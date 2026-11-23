//! Source Miracast (Wi-Fi Display) : processus `archprojeteur cast-session`.
//!
//! 1. Portail de capture d'écran → flux PipeWire (sélecteur Hyprland).
//! 2. Assistant root → groupe Wi-Fi Direct rejoint + adresse IP.
//! 3. L'écran se connecte sur notre port 7236 : échange RTSP M1…M7.
//! 4. GStreamer : H.264 (+ AAC) → MPEG-TS → RTP/UDP vers l'écran.
//! 5. Keep-alive RTSP jusqu'à l'arrêt (SIGTERM / TEARDOWN de l'écran).

use crate::cast::{self, Session};
use crate::hypr;
use crate::settings::{Media, MediaKind, Settings};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

const RTSP_PORT: u16 = 7236;
const RTP_SERVER_PORT: u16 = 19000;

// ───────────────────────── Capture d'écran (portail) ─────────────────────────

struct Capture {
    fd: OwnedFd,
    node: u32,
    // Garder la session du portail ouverte tant qu'on diffuse.
    _proxy: ashpd::desktop::screencast::Screencast,
    _session: ashpd::desktop::Session<ashpd::desktop::screencast::Screencast>,
}

fn capture() -> Result<Capture, String> {
    use ashpd::desktop::PersistMode;
    use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
    futures_lite::future::block_on(async {
        let e = |e: ashpd::Error| format!("portail de capture : {e}");
        let proxy = Screencast::new().await.map_err(e)?;
        let session = proxy.create_session(Default::default()).await.map_err(e)?;
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::EMBEDDED)
                    .set_sources(SourceType::MONITOR | SourceType::WINDOW)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .map_err(e)?;
        let resp = proxy
            .start(&session, None, Default::default())
            .await
            .map_err(e)?
            .response()
            .map_err(|_| "partage d'écran annulé".to_string())?;
        let node = resp.streams().first().ok_or("aucun écran choisi")?.pipe_wire_node_id();
        let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await.map_err(e)?;
        Ok(Capture { fd, node, _proxy: proxy, _session: session })
    })
}

// ───────────────────────────── RTSP minimal ─────────────────────────────

#[derive(Debug, Default)]
struct Msg {
    first: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Msg {
    fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }
    fn cseq(&self) -> u32 {
        self.header("CSeq").and_then(|v| v.trim().parse().ok()).unwrap_or(0)
    }
    fn is_response(&self) -> bool {
        self.first.starts_with("RTSP/")
    }
    fn method(&self) -> &str {
        self.first.split(' ').next().unwrap_or("")
    }
    fn ok(&self) -> bool {
        self.first.split(' ').nth(1) == Some("200")
    }
    /// Corps « clé: valeur » (text/parameters).
    fn param(&self, key: &str) -> Option<&str> {
        self.body.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(key).then(|| v.trim())
        })
    }
}

struct Rtsp {
    stream: TcpStream,
    buf: Vec<u8>,
    cseq: u32,
}

impl Rtsp {
    fn new(stream: TcpStream) -> Self {
        stream.set_read_timeout(Some(Duration::from_millis(300))).ok();
        stream.set_nodelay(true).ok();
        Self { stream, buf: Vec::new(), cseq: 0 }
    }

    /// Lit un message complet, `Ok(None)` si rien pour l'instant.
    fn poll(&mut self) -> Result<Option<Msg>, String> {
        if let Some(m) = self.take_msg() {
            return Ok(Some(m));
        }
        let mut tmp = [0u8; 4096];
        match self.stream.read(&mut tmp) {
            Ok(0) => Err("l'écran a fermé la connexion".into()),
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                Ok(self.take_msg())
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => Ok(None),
            Err(e) => Err(format!("RTSP : {e}")),
        }
    }

    fn take_msg(&mut self) -> Option<Msg> {
        let end = self.buf.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&self.buf[..end]).to_string();
        let mut lines = head.split("\r\n");
        let mut m = Msg { first: lines.next()?.trim().to_string(), ..Default::default() };
        for l in lines {
            if let Some((k, v)) = l.split_once(':') {
                m.headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        let len: usize = m.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
        if self.buf.len() < end + 4 + len {
            return None;
        }
        m.body = String::from_utf8_lossy(&self.buf[end + 4..end + 4 + len]).to_string();
        self.buf.drain(..end + 4 + len);
        eprintln!("<< {}\n{}", m.first, m.body.trim_end());
        Some(m)
    }

    fn write(&mut self, text: &str) -> Result<(), String> {
        eprintln!(">> {}", text.lines().next().unwrap_or(""));
        self.stream.write_all(text.as_bytes()).map_err(|e| format!("RTSP : {e}"))
    }

    fn request(&mut self, method: &str, url: &str, extra: &[(&str, &str)], body: &str) -> Result<u32, String> {
        self.cseq += 1;
        let mut s = format!("{method} {url} RTSP/1.0\r\nCSeq: {}\r\n", self.cseq);
        for (k, v) in extra {
            s += &format!("{k}: {v}\r\n");
        }
        if !body.is_empty() {
            s += &format!("Content-Type: text/parameters\r\nContent-Length: {}\r\n", body.len());
        }
        s += "\r\n";
        s += body;
        self.write(&s)?;
        Ok(self.cseq)
    }

    fn respond(&mut self, cseq: u32, extra: &[(&str, &str)]) -> Result<(), String> {
        let mut s = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n");
        for (k, v) in extra {
            s += &format!("{k}: {v}\r\n");
        }
        s += "\r\n";
        self.write(&s)
    }
}

// ───────────────────────────── Session WFD ─────────────────────────────

#[derive(Default)]
struct Wfd {
    session_id: String,
    sink_rtp_port: u16,
    playing: bool,
    teardown: bool,
}

/// Répond aux requêtes envoyées par l'écran (M2, M6, M7, keep-alive, IDR…).
fn handle_request(r: &mut Rtsp, w: &mut Wfd, m: &Msg) -> Result<(), String> {
    let cseq = m.cseq();
    let session = format!("{};timeout=30", w.session_id);
    match m.method() {
        "OPTIONS" => r.respond(cseq, &[("Public", "org.wfa.wfd1.0, SETUP, TEARDOWN, PLAY, PAUSE, GET_PARAMETER, SET_PARAMETER")]),
        "SETUP" => {
            if let Some(t) = m.header("Transport") {
                if let Some(p) = t.split(';').find_map(|p| p.trim().strip_prefix("client_port=")) {
                    w.sink_rtp_port = p.split('-').next().and_then(|x| x.parse().ok()).unwrap_or(w.sink_rtp_port);
                }
            }
            let transport = format!(
                "RTP/AVP/UDP;unicast;client_port={};server_port={}-{}",
                w.sink_rtp_port,
                RTP_SERVER_PORT,
                RTP_SERVER_PORT + 1
            );
            r.respond(cseq, &[("Session", &session), ("Transport", &transport)])
        }
        "PLAY" => {
            w.playing = true;
            r.respond(cseq, &[("Session", &session), ("Range", "npt=now-")])
        }
        "PAUSE" => r.respond(cseq, &[("Session", &session)]),
        "TEARDOWN" => {
            w.teardown = true;
            r.respond(cseq, &[("Session", &session)])
        }
        _ => r.respond(cseq, &[]), // GET/SET_PARAMETER (keep-alive, wfd_idr_request…)
    }
}

/// Envoie une requête et attend sa réponse, en traitant les requêtes de l'écran entre-temps.
fn transact(r: &mut Rtsp, w: &mut Wfd, method: &str, url: &str, extra: &[(&str, &str)], body: &str) -> Result<Msg, String> {
    let id = r.request(method, url, extra, body)?;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(10) {
        if let Some(m) = r.poll()? {
            if m.is_response() && m.cseq() == id {
                return if m.ok() { Ok(m) } else { Err(format!("l'écran a répondu « {} » à {method}", m.first)) };
            } else if !m.is_response() {
                handle_request(r, w, &m)?;
            }
        }
    }
    Err(format!("pas de réponse de l'écran à {method}"))
}

struct Video {
    width: u32,
    height: u32,
    format: String,
    bitrate: u32,
}

/// Choix du format vidéo parmi ceux de l'écran (champ CEA de wfd_video_formats).
fn choose_video(sink: Option<&str>, allow_1080p: bool) -> Video {
    let mut cea = 0x01u32;
    let mut level = 0x01u32;
    if let Some(v) = sink.filter(|v| *v != "none") {
        let f: Vec<&str> = v.split_whitespace().collect();
        // native, preferred, puis profil, niveau, CEA, …
        level = f.get(3).and_then(|x| u32::from_str_radix(x.trim_end_matches(','), 16).ok()).unwrap_or(1);
        cea = f.get(4).and_then(|x| u32::from_str_radix(x, 16).ok()).unwrap_or(1);
    }
    let (w, h, bit, lvl, rate) = if allow_1080p && cea & 0x80 != 0 && level >= 0x04 {
        (1920, 1080, 0x80, 0x04, 8000) // 1080p30, niveau 4
    } else if cea & 0x20 != 0 {
        (1280, 720, 0x20, 0x01, 5000) // 720p30, niveau 3.1
    } else {
        (640, 480, 0x01, 0x01, 2500) // 480p60, obligatoire
    };
    Video {
        width: w,
        height: h,
        format: format!("00 00 01 {lvl:02x} {bit:08x} 00000000 00000000 00 0000 0000 00 none none"),
        bitrate: rate,
    }
}

fn want_audio(sink: Option<&str>) -> bool {
    sink.is_some_and(|a| a.contains("AAC")) && gst_has("fdkaacenc")
}

fn gst_has(element: &str) -> bool {
    Command::new("gst-inspect-1.0").args(["--exists", element]).status().is_ok_and(|s| s.success())
}

/// Ce qu'on envoie à l'écran.
enum Source<'a> {
    Screen(&'a Capture),
    File(&'a Media),
    Test,
}

fn spawn_gst(src: &Source, v: &Video, audio: bool, local_ip: &str, peer_ip: &str, port: u16) -> Result<Child, String> {
    let level = if v.height >= 1080 { "4" } else { "3.1" };
    // Un fichier vidéo n'est pas « live » : udpsink doit cadencer au temps réel.
    let sync = matches!(src, Source::File(m) if m.kind == MediaKind::Video);
    let video_src = match src {
        Source::Screen(c) => format!("pipewiresrc fd=3 path={} do-timestamp=true keepalive-time=100 always-copy=true", c.node),
        Source::File(m) if m.kind == MediaKind::Image => {
            "filesrc @LOCATION@ ! decodebin ! videoconvert ! imagefreeze is-live=true".to_string()
        }
        Source::File(_) => "filesrc @LOCATION@ ! decodebin name=dec dec. ! queue".to_string(),
        Source::Test => "videotestsrc is-live=true pattern=ball".to_string(),
    };
    let mut desc = format!(
        "{video_src} ! \
         videoconvert ! videoscale add-borders=true ! videorate ! \
         video/x-raw,format=I420,width={w},height={h},framerate=30/1,pixel-aspect-ratio=1/1 ! \
         {vqueue} ! \
         x264enc tune=zerolatency speed-preset=ultrafast bitrate={br} vbv-buf-capacity=300 key-int-max=30 bframes=0 cabac=false byte-stream=true ! \
         video/x-h264,profile=constrained-baseline,level=(string){level},stream-format=byte-stream ! \
         h264parse config-interval=-1 ! queue ! mux.sink_4113 \
         mpegtsmux name=mux alignment=7 pat-interval=9000 pmt-interval=9000 ! \
         rtpmp2tpay pt=33 mtu=1400 ! \
         udpsink host={peer} port={port} bind-address={local} bind-port={sport} sync={sync} async=false",
        // Source en direct : on jette les images en retard (latence minimale).
        // Fichier : lu plus vite que le temps réel, il ne faut rien jeter.
        vqueue = if sync { "queue" } else { "queue max-size-buffers=2 leaky=downstream" },
        w = v.width,
        h = v.height,
        br = v.bitrate,
        peer = peer_ip,
        local = local_ip,
        sport = RTP_SERVER_PORT,
    );
    if audio {
        let audio_src = match src {
            Source::File(_) => "dec. ! queue",
            _ => "pulsesrc device=@DEFAULT_MONITOR@ do-timestamp=true",
        };
        desc += &format!(
            " {audio_src} ! audioconvert ! audioresample ! \
             audio/x-raw,rate=48000,channels=2 ! fdkaacenc bitrate=128000 ! aacparse ! \
             audio/mpeg,mpegversion=4,stream-format=adts ! queue ! mux.sink_4352"
        );
    }

    // Découpage en arguments ; le chemin du fichier reste un seul argument
    // (gst-launch recolle les arguments puis interprète les guillemets).
    let args: Vec<String> = desc
        .split_whitespace()
        .map(|t| match (t, src) {
            ("@LOCATION@", Source::File(m)) => {
                let p = m.path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
                format!("location=\"{p}\"")
            }
            _ => t.to_string(),
        })
        .collect();
    eprintln!("gst-launch-1.0 {}", args.join(" "));

    let mut cmd = Command::new("gst-launch-1.0");
    cmd.arg("-q").args(&args).stdin(Stdio::null());
    if let Source::Screen(c) = src {
        let raw = c.fd.as_raw_fd();
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                // fd PipeWire du portail → fd 3 dans gst-launch (dup2 retire CLOEXEC).
                if libc::dup2(raw, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    cmd.spawn().map_err(|e| format!("gst-launch-1.0 : {e}"))
}

/// Attend la connexion RTSP de l'écran (ou s'y connecte si elle ne vient pas).
fn rtsp_connect(local_ip: &str, peer_ip: &str, stop: &AtomicBool) -> Result<TcpStream, String> {
    let listener = TcpListener::bind((local_ip, RTSP_PORT)).map_err(|e| format!("port {RTSP_PORT} : {e}"))?;
    listener.set_nonblocking(true).ok();
    let peer: SocketAddr = format!("{peer_ip}:{RTSP_PORT}").parse().map_err(|_| "IP de l'écran invalide")?;
    let t = Instant::now();
    let mut last_try = Instant::now();
    while t.elapsed() < Duration::from_secs(30) && !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((s, from)) => {
                eprintln!("connexion RTSP de {from}");
                s.set_nonblocking(false).ok();
                return Ok(s);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("accept : {e}")),
        }
        // Repli : certains écrans attendent que la source se connecte.
        if t.elapsed() > Duration::from_secs(6) && last_try.elapsed() > Duration::from_secs(3) {
            last_try = Instant::now();
            if let Ok(s) = TcpStream::connect_timeout(&peer, Duration::from_secs(1)) {
                eprintln!("connecté en RTSP à {peer}");
                return Ok(s);
            }
        }
        sleep(Duration::from_millis(150));
    }
    Err("l'écran n'a pas ouvert la session Miracast".into())
}

pub fn run_session(mac: &str, name: &str) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone()).ok();
    signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone()).ok();

    let settings = Settings::load();
    let media = settings.active_media();
    let mut st = Session::new(mac, name);
    match &media {
        Some(m) => st.set("connecting", &format!("Fichier : {}", m.file_name())),
        None => st.set("capture", "Choisis l'écran à partager"),
    }

    // Mode test (développement) : ARCHPROJ_TEST_PEER="ip_locale,ip_ecran",
    // mire GStreamer au lieu du portail et pas de Wi-Fi Direct.
    let test = std::env::var("ARCHPROJ_TEST_PEER").ok();

    // 1. Capture
    let cap = if test.is_some() || media.is_some() { None } else { Some(capture()?) };
    let source = match (&cap, &media) {
        (Some(c), _) => Source::Screen(c),
        (None, Some(m)) => Source::File(m),
        (None, None) => Source::Test,
    };
    let media_audio = media.as_ref().map(|m| m.has_audio());

    // 2. Wi-Fi Direct
    st.set("connecting", "Connexion Wi-Fi Direct…");
    let (local_ip, peer_ip) = match &test {
        Some(t) => {
            let (a, b) = t.split_once(',').ok_or("ARCHPROJ_TEST_PEER=ip_locale,ip_ecran")?;
            (a.to_string(), b.to_string())
        }
        None => {
            hypr::notify(&format!("Connexion à {name}… (accepte sur l'écran si demandé)"), true);
            let joined = cast::helper(&["connect", mac])?;
            eprintln!("groupe : {joined}");
            (
                joined["ip"].as_str().unwrap_or_default().to_string(),
                joined["peer_ip"].as_str().unwrap_or_default().to_string(),
            )
        }
    };

    let result = (|| -> Result<(), String> {
        // 3. RTSP
        st.set("connecting", "Ouverture de la session Miracast…");
        let mut r = Rtsp::new(rtsp_connect(&local_ip, &peer_ip, &stop)?);
        let mut w = Wfd {
            session_id: format!("{:08X}", std::process::id().wrapping_mul(2654435761)),
            sink_rtp_port: 1028,
            ..Default::default()
        };

        transact(&mut r, &mut w, "OPTIONS", "*", &[("Require", "org.wfa.wfd1.0")], "")?; // M1
        // M2 : l'écran envoie à son tour OPTIONS, on y répond avant M3.
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            if let Some(m) = r.poll()? {
                if !m.is_response() {
                    let is_options = m.method() == "OPTIONS";
                    handle_request(&mut r, &mut w, &m)?;
                    if is_options {
                        break;
                    }
                }
            }
        }
        let caps = transact(
            &mut r,
            &mut w,
            "GET_PARAMETER",
            "rtsp://localhost/wfd1.0",
            &[],
            "wfd_video_formats\r\nwfd_audio_codecs\r\nwfd_client_rtp_ports\r\n",
        )?; // M3
        if let Some(p) = caps.param("wfd_client_rtp_ports") {
            w.sink_rtp_port = p.split_whitespace().nth(1).and_then(|x| x.parse().ok()).unwrap_or(1028);
        }
        let video = choose_video(caps.param("wfd_video_formats"), settings.allow_1080p);
        // Son : réglage utilisateur, et pour un fichier seulement s'il a une piste audio.
        let audio = want_audio(caps.param("wfd_audio_codecs")) && settings.audio && media_audio.unwrap_or(true);

        let mut m4 = format!("wfd_video_formats: {}\r\n", video.format);
        if audio {
            m4 += "wfd_audio_codecs: AAC 00000001 00\r\n";
        }
        m4 += &format!("wfd_presentation_URL: rtsp://{local_ip}/wfd1.0/streamid=0 none\r\n");
        m4 += &format!("wfd_client_rtp_ports: RTP/AVP/UDP;unicast {} 0 mode=play\r\n", w.sink_rtp_port);
        transact(&mut r, &mut w, "SET_PARAMETER", "rtsp://localhost/wfd1.0", &[], &m4)?; // M4
        transact(&mut r, &mut w, "SET_PARAMETER", "rtsp://localhost/wfd1.0", &[], "wfd_trigger_method: SETUP\r\n")?; // M5

        // M6 (SETUP) puis M7 (PLAY), envoyés par l'écran.
        let t = Instant::now();
        while !w.playing {
            if let Some(m) = r.poll()? {
                if !m.is_response() {
                    handle_request(&mut r, &mut w, &m)?;
                }
            }
            if w.teardown || t.elapsed() > Duration::from_secs(15) {
                return Err("l'écran n'a pas démarré la lecture".into());
            }
        }

        // 4. Flux
        let mut gst = spawn_gst(&source, &video, audio, &local_ip, &peer_ip, w.sink_rtp_port)?;
        let what = media.as_ref().map(|m| format!("{} · ", m.file_name())).unwrap_or_default();
        st.set("streaming", &format!("{what}{}×{}{}", video.width, video.height, if audio { " + son" } else { "" }));
        hypr::notify(&format!("Diffusion vers {name}"), true);

        // 5. Keep-alive jusqu'à l'arrêt
        let mut last_ka = Instant::now();
        let mut last_answer = Instant::now();
        let end = loop {
            if stop.load(Ordering::Relaxed) {
                let _ = r.request("SET_PARAMETER", "rtsp://localhost/wfd1.0", &[], "wfd_trigger_method: TEARDOWN\r\n");
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(2) && !w.teardown {
                    match r.poll() {
                        Ok(Some(m)) if !m.is_response() => {
                            let _ = handle_request(&mut r, &mut w, &m);
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                break Ok(());
            }
            match r.poll() {
                Ok(Some(m)) => {
                    last_answer = Instant::now();
                    if !m.is_response() {
                        handle_request(&mut r, &mut w, &m)?;
                    }
                }
                Ok(None) => {}
                Err(e) => break Err(e),
            }
            if w.teardown {
                break Ok(());
            }
            if last_ka.elapsed() > Duration::from_secs(20) {
                last_ka = Instant::now();
                let sid = w.session_id.clone();
                r.request("GET_PARAMETER", "rtsp://localhost/wfd1.0", &[("Session", &sid)], "")?;
            }
            if last_answer.elapsed() > Duration::from_secs(70) {
                break Err("l'écran ne répond plus".into());
            }
            if let Ok(Some(status)) = gst.try_wait() {
                // Fichier terminé : on le relance en boucle sans couper la session.
                if status.success() && media.as_ref().is_some_and(|m| m.looping) {
                    gst = spawn_gst(&source, &video, audio, &local_ip, &peer_ip, w.sink_rtp_port)?;
                    continue;
                }
                break Err(format!("GStreamer s'est arrêté ({status})"));
            }
        };
        let _ = gst.kill();
        let _ = gst.wait();
        end
    })();

    if test.is_none() {
        let _ = cast::helper(&["leave"]);
    }
    result
}

//! Caster (Win+K) : découverte des écrans Miracast via NetworkManager et
//! pilotage de la session de diffusion (processus `cast-session`, voir wfd.rs).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use zbus::blocking::{Connection, Proxy};
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedObjectPath;

pub const HELPER: &str = "/usr/local/libexec/archprojeteur-p2p";

const NM: &str = "org.freedesktop.NetworkManager";

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

// ───────────────────────────── Assistant root ─────────────────────────────

/// L'assistant root est-il installé et autorisé sans mot de passe ?
pub fn helper_ready() -> bool {
    std::path::Path::new(HELPER).exists()
        && Command::new("sudo")
            .args(["-n", "-l", HELPER, "p2p-helper", "leave"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

pub fn helper(args: &[&str]) -> Result<serde_json::Value, String> {
    if !helper_ready() {
        return Err("assistant Wi-Fi Direct non installé (sudo ./install-helper.sh)".into());
    }
    let out = Command::new("sudo")
        .arg("-n")
        .arg(HELPER)
        .arg("p2p-helper")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("sudo : {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    eprint!("{}", String::from_utf8_lossy(&out.stderr)); // traces → journal de session
    let v: serde_json::Value = text
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str(l).ok())
        .ok_or_else(|| format!("réponse de l'assistant illisible : {} {}", text, String::from_utf8_lossy(&out.stderr)))?;
    match v.get("error").and_then(|e| e.as_str()) {
        Some(e) => Err(e.to_string()),
        None => Ok(v),
    }
}

// ───────────────────────────── Découverte ─────────────────────────────

#[derive(Debug, Clone)]
pub struct WirelessDisplay {
    pub mac: String,
    pub name: String,
    pub strength: u8,
    /// Un autre appareil diffuse déjà dessus (sous-élément WFD « session info »).
    pub busy: bool,
}

/// Analyse les IE Wi-Fi Display : `Some(busy)` si c'est un écran (sink).
fn parse_wfd(ies: &[u8]) -> Option<bool> {
    let mut i = 0;
    let mut sink = false;
    let mut busy = false;
    while i + 3 <= ies.len() {
        let id = ies[i];
        let len = u16::from_be_bytes([ies[i + 1], ies[i + 2]]) as usize;
        let body = ies.get(i + 3..i + 3 + len)?;
        match id {
            0 if len >= 2 => {
                let info = u16::from_be_bytes([body[0], body[1]]);
                sink = info & 0x3 != 0; // 01 sink principal, 10 secondaire, 11 double rôle
            }
            9 if len > 0 => busy = true,
            _ => {}
        }
        i += 3 + len;
    }
    sink.then_some(busy)
}

pub struct Discovery {
    conn: Connection,
    device: OwnedObjectPath,
}

impl Discovery {
    pub fn new() -> Result<Self, String> {
        let conn = Connection::system().map_err(|e| format!("D-Bus système : {e}"))?;
        let nm = proxy(&conn, "/org/freedesktop/NetworkManager", NM)?;
        let devices: Vec<OwnedObjectPath> = nm.get_property("AllDevices").map_err(|e| e.to_string())?;
        drop(nm);
        let device = devices
            .into_iter()
            .find(|d| {
                proxy(&conn, d.as_str(), "org.freedesktop.NetworkManager.Device")
                    .ok()
                    .and_then(|p| p.get_property::<u32>("DeviceType").ok())
                    == Some(30)
            })
            .ok_or("pas de Wi-Fi Direct sur ce PC (NetworkManager)")?;
        Ok(Self { conn, device })
    }

    pub fn start_find(&self) -> Result<(), String> {
        let p = proxy(&self.conn, self.device.as_str(), "org.freedesktop.NetworkManager.Device.WifiP2P")?;
        let mut opts: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
        opts.insert("timeout", 30i32.into());
        p.call::<_, _, ()>("StartFind", &(opts,)).map_err(|e| format!("recherche Wi-Fi Direct : {e}"))
    }

    pub fn displays(&self) -> Result<Vec<WirelessDisplay>, String> {
        let p = proxy(&self.conn, self.device.as_str(), "org.freedesktop.NetworkManager.Device.WifiP2P")?;
        let peers: Vec<OwnedObjectPath> = p.get_property("Peers").map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for pe in peers {
            let pp = proxy(&self.conn, pe.as_str(), "org.freedesktop.NetworkManager.WifiP2PPeer")?;
            let ies: Vec<u8> = pp.get_property("WfdIEs").unwrap_or_default();
            let Some(busy) = parse_wfd(&ies) else { continue };
            let name: String = pp.get_property("Name").unwrap_or_default();
            let mac: String = pp.get_property("HwAddress").unwrap_or_default();
            if mac.is_empty() {
                continue;
            }
            out.push(WirelessDisplay {
                name: if name.is_empty() { mac.clone() } else { name },
                mac: mac.to_lowercase(),
                strength: pp.get_property("Strength").unwrap_or(0),
                busy,
            });
        }
        out.sort_by_key(|d| d.name.to_lowercase());
        Ok(out)
    }
}

fn proxy<'a>(conn: &'a Connection, path: &'a str, iface: &'a str) -> Result<Proxy<'a>, String> {
    zbus::blocking::proxy::Builder::new(conn)
        .destination(NM)
        .and_then(|b| b.path(path))
        .and_then(|b| b.interface(iface))
        .map(|b| b.cache_properties(CacheProperties::No))
        .and_then(|b| b.build())
        .map_err(|e| format!("D-Bus : {e}"))
}

// ───────────────────────────── Session ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub mac: String,
    pub name: String,
    /// capture | connecting | streaming
    pub state: String,
    pub message: String,
    pub pid: u32,
}

fn session_file() -> PathBuf {
    runtime_dir().join("archprojeteur-cast.json")
}

pub fn log_file() -> PathBuf {
    runtime_dir().join("archprojeteur-cast.log")
}

impl Session {
    pub fn new(mac: &str, name: &str) -> Self {
        Self { mac: mac.into(), name: name.into(), state: "capture".into(), message: String::new(), pid: std::process::id() }
    }

    pub fn set(&mut self, state: &str, message: &str) {
        self.state = state.into();
        self.message = message.into();
        if let Ok(json) = serde_json::to_string(self) {
            let _ = std::fs::write(session_file(), json);
        }
    }

    pub fn clear() {
        let _ = std::fs::remove_file(session_file());
    }
}

/// Session en cours (le processus doit encore tourner).
pub fn current() -> Option<Session> {
    let s: Session = serde_json::from_str(&std::fs::read_to_string(session_file()).ok()?).ok()?;
    std::path::Path::new(&format!("/proc/{}", s.pid)).exists().then_some(s)
}

/// Lance le processus de diffusion en arrière-plan.
pub fn start(mac: &str, name: &str) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    if current().is_some() {
        stop();
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let log = std::fs::File::create(log_file()).map_err(|e| e.to_string())?;
    Command::new(exe)
        .args(["cast-session", mac, name])
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("impossible de lancer la diffusion : {e}"))?;
    Ok(())
}

/// Arrête la diffusion. Renvoie le nom de l'écran s'il y en avait une.
pub fn stop() -> Option<String> {
    let s = current()?;
    unsafe {
        libc::kill(s.pid as i32, libc::SIGTERM);
    }
    for _ in 0..50 {
        if current().is_none() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if current().is_some() {
        unsafe {
            libc::kill(s.pid as i32, libc::SIGKILL);
        }
        let _ = helper(&["leave"]);
    }
    Session::clear();
    Some(s.name)
}

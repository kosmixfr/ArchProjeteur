//! Assistant root (lancé via `sudo -n /usr/local/libexec/archprojeteur-p2p p2p-helper …`).
//!
//! NetworkManager ne sait que *négocier* un groupe Wi-Fi Direct ; les écrans
//! Miracast des salles sont déjà propriétaires de leur groupe et doivent être
//! *rejoints* (ce que fait Windows). On pilote donc wpa_supplicant directement.
//!
//!   p2p-helper connect <MAC>   rejoint / négocie, obtient une IP, affiche du JSON
//!   p2p-helper leave           quitte tous les groupes Wi-Fi Direct

use serde::Serialize;
use std::collections::HashSet;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

#[derive(Serialize)]
pub struct Joined {
    pub iface: String,
    pub ip: String,
    pub prefix: u8,
    pub peer_ip: String,
    pub method: String,
}

pub fn main(args: &[String]) -> ! {
    let res = match args.first().map(String::as_str) {
        Some("connect") => match args.get(1) {
            Some(mac) if valid_mac(mac) => connect(&mac.to_lowercase()).map(|j| serde_json::to_string(&j).unwrap()),
            _ => Err("adresse MAC invalide".into()),
        },
        Some("leave") => leave().map(|_| "{\"ok\":true}".to_string()),
        _ => Err("usage : p2p-helper connect <MAC> | leave".into()),
    };
    match res {
        Ok(json) => {
            println!("{json}");
            std::process::exit(0);
        }
        Err(e) => {
            println!("{}", serde_json::json!({ "error": e }));
            std::process::exit(1);
        }
    }
}

fn valid_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6 && parts.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn run(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Nom de l'interface P2P de wpa_supplicant (`p2p-dev-wlan0`).
fn p2p_dev() -> Result<String, String> {
    let out = run("nmcli", &["-t", "-f", "DEVICE,TYPE", "device"]);
    out.lines()
        .find_map(|l| l.strip_suffix(":wifi-p2p").map(str::to_string))
        .ok_or_else(|| "aucune interface Wi-Fi Direct (p2p-dev-*)".into())
}

fn wpa(dev: &str, args: &[&str]) -> String {
    let mut a = vec!["-i", dev];
    a.extend_from_slice(args);
    let out = run("wpa_cli", &a);
    if args.first() != Some(&"status") {
        eprintln!("wpa_cli {} → {}", a.join(" "), out.replace('\n', " | "));
    }
    out
}

/// Interfaces de groupe Wi-Fi Direct existantes (`p2p-wlan0-0`, …).
fn group_ifaces() -> HashSet<String> {
    std::fs::read_dir("/sys/class/net")
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with("p2p-") && !n.starts_with("p2p-dev"))
                .collect()
        })
        .unwrap_or_default()
}

fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

fn connect(mac: &str) -> Result<Joined, String> {
    let dev = p2p_dev()?;

    // Se présenter comme source Miracast (sous-élément WFD « device info » :
    // source, session disponible, port de contrôle 7236, 50 Mb/s).
    wpa(&dev, &["set", "wifi_display", "1"]);
    wpa(&dev, &["wfd_subelem_set", "0", "000600101c440032"]);

    // L'écran doit être dans la table de wpa_supplicant.
    let mut peer = wpa(&dev, &["p2p_peer", mac]);
    if peer.starts_with("FAIL") || peer.is_empty() {
        wpa(&dev, &["p2p_find", "15"]);
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(15) {
            sleep(Duration::from_millis(500));
            peer = wpa(&dev, &["p2p_peer", mac]);
            if !peer.starts_with("FAIL") && !peer.is_empty() {
                break;
            }
        }
        if peer.starts_with("FAIL") || peer.is_empty() {
            return Err("écran introuvable en Wi-Fi Direct".into());
        }
    }
    let group_capab = field(&peer, "group_capab")
        .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0);
    let is_go = group_capab & 0x01 != 0;

    // NetworkManager détruit tout groupe Wi-Fi Direct qu'il n'a pas lancé
    // lui-même (il appelle RemoveInterface sur wpa_supplicant). On lui fait donc
    // lancer la connexion : pendant qu'il est « en train de se connecter », il
    // adopte le groupe qui démarre et fait le DHCP. Pour un écran déjà
    // propriétaire de groupe, on remplace sa négociation par un « join ».
    nm(&["connection", "delete", CON]);
    let wfd_ies = "00:00:06:00:10:1c:44:00:32"; // source, session dispo, port 7236
    let add = nm(&[
        "connection", "add", "type", "wifi-p2p", "con-name", CON, "ifname", &dev,
        "autoconnect", "no", "save", "no",
        "wifi-p2p.peer", &mac.to_uppercase(), "wifi-p2p.wfd-ies", wfd_ies,
        "ipv4.method", "auto", "ipv6.method", "disabled",
    ]);
    if !add.contains("successfully") && !add.contains("ajoutée") {
        return Err(format!("NetworkManager : {add}"));
    }
    let mut up = Command::new("nmcli")
        .env("LC_ALL", "C")
        .args(["--wait", "75", "connection", "up", CON])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("nmcli : {e}"))?;

    let method = if is_go { "join" } else { "negotiate" };
    if is_go {
        // Attendre que NetworkManager ait envoyé sa négociation (état « config »).
        let t = Instant::now();
        loop {
            let st = nm_state(&dev);
            if st == 50 {
                break;
            }
            if st >= 100 || t.elapsed() > Duration::from_secs(30) || up.try_wait().ok().flatten().is_some() {
                let _ = up.kill();
                nm(&["connection", "delete", CON]);
                return Err(format!("NetworkManager n'a pas démarré la connexion (état {st})"));
            }
            sleep(Duration::from_millis(100));
        }
        sleep(Duration::from_millis(600));
        wpa(&dev, &["p2p_cancel"]);
        let res = wpa(&dev, &["p2p_connect", mac, "pbc", "join"]);
        if res.starts_with("FAIL") {
            let _ = up.kill();
            nm(&["connection", "delete", CON]);
            return Err(format!("wpa_supplicant a refusé le join ({res})"));
        }
    }

    // NetworkManager adopte le groupe, fait le DHCP et passe en « activé ».
    let status = up.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = up.stderr.take() {
            use std::io::Read;
            let _ = e.read_to_string(&mut err);
        }
        nm(&["connection", "delete", CON]);
        return Err(format!("connexion Wi-Fi Direct échouée : {}", err.trim()));
    }

    let iface = nm(&["-g", "GENERAL.IP-IFACE", "device", "show", &dev]);
    let addr = run("ip", &["-4", "-o", "addr", "show", "dev", &iface]);
    let cidr = addr
        .split_whitespace()
        .skip_while(|w| *w != "inet")
        .nth(1)
        .ok_or_else(|| format!("pas d'adresse IPv4 sur {iface}"))?
        .to_string();
    let (ip, prefix) = cidr.split_once('/').unwrap_or((&cidr, "24"));
    let prefix: u8 = prefix.parse().unwrap_or(24);

    // IP de l'écran : serveur DHCP du groupe, sinon passerelle, sinon .1 du réseau.
    let dhcp = nm(&["-g", "DHCP4", "device", "show", &dev]);
    let server = dhcp
        .split(['|', '\n'])
        .find_map(|o| o.trim().strip_prefix("dhcp_server_identifier = ").map(|v| v.trim().to_string()));
    let gateway = nm(&["-g", "IP4.GATEWAY", "device", "show", &dev]);
    let peer_ip = server.or((!gateway.is_empty()).then_some(gateway)).unwrap_or_else(|| {
        let a: std::net::Ipv4Addr = ip.parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
        let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
        std::net::Ipv4Addr::from((u32::from(a) & mask) + 1).to_string()
    });

    firewall_open(&iface);

    Ok(Joined { iface, ip: ip.to_string(), prefix, peer_ip, method: method.into() })
}

const CON: &str = "archprojeteur-p2p";

fn nm(args: &[&str]) -> String {
    let out = Command::new("nmcli")
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).trim().to_string())
        .unwrap_or_default();
    eprintln!("nmcli {} → {}", args.join(" "), out.replace('\n', " | "));
    out
}

/// État NetworkManager du périphérique Wi-Fi Direct (50 = config, 100 = activé).
fn nm_state(dev: &str) -> u32 {
    let out = Command::new("nmcli")
        .env("LC_ALL", "C")
        .args(["-g", "GENERAL.STATE", "device", "show", dev])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    out.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// UFW refuse les connexions entrantes : l'écran ne pourrait pas ouvrir la
/// session RTSP sur notre port 7236. On l'autorise sur l'interface du groupe
/// Wi-Fi Direct uniquement, le temps de la diffusion.
fn firewall_open(iface: &str) {
    if !ufw_active() {
        return;
    }
    run(
        "ufw",
        &["insert", "1", "allow", "in", "on", iface, "to", "any", "port", "7236", "proto", "tcp", "comment", "archprojeteur"],
    );
    eprintln!("ufw : port 7236/tcp ouvert sur {iface}");
}

fn firewall_close() {
    if !ufw_active() {
        return;
    }
    // Supprime nos règles (numéros décroissants pour ne pas décaler les suivants).
    let status = run("ufw", &["status", "numbered"]);
    let mut nums: Vec<u32> = status
        .lines()
        .filter(|l| l.contains("archprojeteur"))
        .filter_map(|l| l.trim_start().strip_prefix('[')?.split(']').next()?.trim().parse().ok())
        .collect();
    nums.sort_unstable_by(|a, b| b.cmp(a));
    for n in nums {
        run("ufw", &["--force", "delete", &n.to_string()]);
        eprintln!("ufw : règle {n} retirée");
    }
}

fn ufw_active() -> bool {
    run("ufw", &["status"]).starts_with("Status: active")
}

fn leave() -> Result<(), String> {
    let dev = p2p_dev()?;
    firewall_close();
    nm(&["connection", "down", CON]);
    nm(&["connection", "delete", CON]);
    for g in group_ifaces() {
        wpa(&dev, &["p2p_group_remove", &g]);
    }
    wpa(&dev, &["p2p_cancel"]);
    Ok(())
}

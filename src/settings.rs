//! Réglages persistants (~/.config/archprojeteur/settings.json).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Autoriser le 1080p si l'écran le gère (sinon 720p max).
    pub allow_1080p: bool,
    /// Envoyer le son du PC avec l'image.
    pub audio: bool,
    /// Section cachée débloquée (5 clics sur la version).
    pub troll_unlocked: bool,
    /// Diffuser un fichier au lieu de l'écran.
    pub troll_enabled: bool,
    pub media_path: String,
    pub media_loop: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            allow_1080p: true,
            audio: true,
            troll_unlocked: false,
            troll_enabled: false,
            media_path: String::new(),
            media_loop: true,
        }
    }
}

fn file() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("archprojeteur").join("settings.json")
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read_to_string(file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let f = file();
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(f, json);
        }
    }

    /// Fichier à diffuser à la place de l'écran, s'il est actif et valide.
    pub fn active_media(&self) -> Option<Media> {
        if !self.troll_unlocked || !self.troll_enabled {
            return None;
        }
        Media::from_path(&self.media_path, self.media_loop)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
}

#[derive(Debug, Clone)]
pub struct Media {
    pub path: PathBuf,
    pub kind: MediaKind,
    pub looping: bool,
}

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff"];
const VIDEO_EXT: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "m4v", "gif", "ogv", "flv", "wmv"];

pub fn media_kind(path: &Path) -> Option<MediaKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if IMAGE_EXT.contains(&ext.as_str()) {
        Some(MediaKind::Image)
    } else if VIDEO_EXT.contains(&ext.as_str()) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

impl Media {
    pub fn from_path(path: &str, looping: bool) -> Option<Media> {
        let p = PathBuf::from(path.trim());
        if !p.is_file() {
            return None;
        }
        Some(Media { kind: media_kind(&p)?, path: p, looping })
    }

    pub fn file_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
    }

    /// La vidéo a-t-elle une piste audio ? (sinon le multiplexeur attendrait en vain)
    pub fn has_audio(&self) -> bool {
        if self.kind == MediaKind::Image {
            return false;
        }
        std::process::Command::new("gst-discoverer-1.0")
            .arg("-t")
            .arg("5")
            .arg(&self.path)
            .env("LC_ALL", "C")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.trim_start().starts_with("audio #")))
            .unwrap_or(false)
    }
}

/// Fichiers image/vidéo récents des dossiers habituels (pour le menu rapide).
pub fn recent_media(limit: usize) -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return vec![] };
    let dirs = ["Images", "Pictures", "Vidéos", "Videos", "Téléchargements", "Downloads", "Bureau", "Desktop"];
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(home.join(d)) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if media_kind(&p).is_some() {
                let t = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                found.push((t, p));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().take(limit).map(|(_, p)| p).collect()
}

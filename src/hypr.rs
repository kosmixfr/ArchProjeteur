//! Pilotage des écrans via `hyprctl` (Hyprland, config Lua).
//!
//! `hyprctl eval 'hl.monitor{...}'` ajoute une règle à l'exécution, mais la
//! première règle correspondante gagne : on fait donc toujours un
//! `hyprctl reload` (qui efface les règles ajoutées à chaud et revient à la
//! config utilisateur) avant d'appliquer celles du mode choisi.

use serde::Deserialize;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    PcOnly,
    Duplicate,
    Extend,
    SecondOnly,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::PcOnly, Mode::Duplicate, Mode::Extend, Mode::SecondOnly];

    pub fn title(self) -> &'static str {
        match self {
            Mode::PcOnly => "Écran du PC uniquement",
            Mode::Duplicate => "Dupliquer",
            Mode::Extend => "Étendre",
            Mode::SecondOnly => "Deuxième écran uniquement",
        }
    }

    pub fn from_arg(s: &str) -> Option<Mode> {
        match s {
            "pc" | "internal" | "pc-only" => Some(Mode::PcOnly),
            "duplicate" | "dup" | "mirror" | "clone" => Some(Mode::Duplicate),
            "extend" | "ext" => Some(Mode::Extend),
            "second" | "external" | "second-only" => Some(Mode::SecondOnly),
            _ => None,
        }
    }

    pub fn needs_external(self) -> bool {
        self != Mode::PcOnly
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(rename = "mirrorOf", default)]
    pub mirror_of: String,
}

impl Monitor {
    pub fn is_internal(&self) -> bool {
        let n = self.name.to_ascii_uppercase();
        n.starts_with("EDP") || n.starts_with("LVDS") || n.starts_with("DSI")
    }
}

#[derive(Debug, Clone)]
pub struct Screens {
    pub internal: Option<Monitor>,
    pub externals: Vec<Monitor>,
}

impl Screens {
    pub fn has_external(&self) -> bool {
        !self.externals.is_empty()
    }

    /// Écran « principal » : l'interne s'il existe, sinon le premier externe.
    fn primary(&self) -> Option<&Monitor> {
        self.internal.as_ref().or(self.externals.first())
    }

    pub fn current_mode(&self) -> Mode {
        let internal_on = self.internal.as_ref().is_some_and(|m| !m.disabled);
        let ext_on: Vec<&Monitor> = self.externals.iter().filter(|m| !m.disabled).collect();
        if self.internal.is_some() && !internal_on && !ext_on.is_empty() {
            Mode::SecondOnly
        } else if ext_on.is_empty() {
            Mode::PcOnly
        } else if ext_on.iter().any(|m| m.mirror_of != "none" && !m.mirror_of.is_empty()) {
            Mode::Duplicate
        } else {
            Mode::Extend
        }
    }
}

fn hyprctl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("hyprctl")
        .args(args)
        .output()
        .map_err(|e| format!("impossible de lancer hyprctl : {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || stdout.starts_with("Error") {
        return Err(format!("hyprctl {} → {stdout}", args.join(" ")));
    }
    Ok(stdout)
}

fn eval(lua: &str) -> Result<(), String> {
    let out = hyprctl(&["eval", lua])?;
    if out == "ok" { Ok(()) } else { Err(format!("hyprctl eval → {out}")) }
}

fn lua_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn screens() -> Result<Screens, String> {
    let json = hyprctl(&["-j", "monitors", "all"])?;
    let mut list: Vec<Monitor> =
        serde_json::from_str(&json).map_err(|e| format!("JSON hyprctl invalide : {e}"))?;
    // Les sorties virtuelles (headless / fake) ne sont pas de vrais écrans.
    list.retain(|m| !m.name.starts_with("HEADLESS") && !m.name.starts_with("FALLBACK"));
    list.sort_by_key(|m| m.id);
    let internal_idx = list.iter().position(|m| m.is_internal());
    let internal = internal_idx.map(|i| list.remove(i));
    Ok(Screens { internal, externals: list })
}

fn rule_enable(name: &str, position: &str) -> String {
    format!(
        "hl.monitor({{ output = {}, mode = \"preferred\", position = {}, scale = 1 }})",
        lua_str(name),
        lua_str(position)
    )
}

fn rule_mirror(name: &str, source: &str) -> String {
    format!(
        "hl.monitor({{ output = {}, mode = \"preferred\", position = \"auto\", scale = 1, mirror = {} }})",
        lua_str(name),
        lua_str(source)
    )
}

fn rule_disable(name: &str) -> String {
    format!("hl.monitor({{ output = {}, disabled = true }})", lua_str(name))
}

pub fn apply(mode: Mode) -> Result<(), String> {
    let s = screens()?;
    if mode.needs_external() && !s.has_external() {
        return Err("aucun écran externe détecté".into());
    }

    // Repart de la config utilisateur (efface les règles posées à chaud).
    hyprctl(&["reload"])?;
    sleep(Duration::from_millis(250));

    let mut rules: Vec<String> = Vec::new();
    match mode {
        Mode::PcOnly => {
            for m in &s.externals {
                rules.push(rule_disable(&m.name));
            }
        }
        Mode::Duplicate => {
            let src = s.primary().map(|m| m.name.clone()).unwrap_or_default();
            for m in s.externals.iter().filter(|m| m.name != src) {
                rules.push(rule_mirror(&m.name, &src));
            }
        }
        Mode::Extend => {
            for m in &s.externals {
                rules.push(rule_enable(&m.name, "auto-right"));
            }
        }
        Mode::SecondOnly => {
            // Allume d'abord l'externe, ensuite seulement éteint l'interne :
            // on ne se retrouve jamais sans aucun écran.
            for (i, m) in s.externals.iter().enumerate() {
                let pos = if i == 0 { "0x0" } else { "auto-right" };
                rules.push(rule_enable(&m.name, pos));
            }
            if let Some(int) = &s.internal {
                rules.push(rule_disable(&int.name));
            }
        }
    }

    for r in &rules {
        eval(r)?;
    }

    // Garde-fou : si plus aucun écran n'est allumé, on revient à la config.
    sleep(Duration::from_millis(400));
    if let Ok(after) = screens() {
        let any_on = after.internal.iter().chain(after.externals.iter()).any(|m| !m.disabled);
        if !any_on {
            let _ = hyprctl(&["reload"]);
            return Err("aucun écran actif après changement, configuration restaurée".into());
        }
    }
    Ok(())
}

/// Règles de fenêtre pour que le sélecteur flotte à droite, au-dessus de tout.
/// Posées une seule fois par chargement de config (un `reload` les efface).
pub fn ensure_window_rules(app_id: &str) {
    let cls = lua_str(&format!("^{app_id}$"));
    let lua = format!(
        "hl.window_rule({{ match = {{ class = {cls} }}, float = true, pin = true, stay_focused = true, \
         dim_around = true, animation = \"slide right\", \
         move = {{\"monitor_w-window_w-24\", \"monitor_h-window_h-72\"}} }})"
    );
    let _ = eval(&lua);
}

pub fn notify(msg: &str, ok: bool) {
    // icône 5 = ok, 3 = erreur (hyprctl notify <icon> <ms> <color> <msg>)
    let (icon, color) = if ok { ("5", "rgb(4ea1ff)") } else { ("3", "rgb(ff5555)") };
    let _ = Command::new("hyprctl")
        .args(["notify", icon, "2500", color, msg])
        .output();
}

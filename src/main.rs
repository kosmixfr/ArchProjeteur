//! ArchProjeteur — « Projeter » (Win+P) et « Caster » (Win+K) façon Windows,
//! pour Hyprland.
//!
//!   archprojeteur              panneau, onglet Projeter (modes d'affichage)
//!   archprojeteur cast         panneau, onglet Caster (écrans sans fil)
//!   archprojeteur pc|duplicate|extend|second|next|status
//!   archprojeteur cast-list    liste les écrans sans fil visibles
//!   archprojeteur cast-stop    arrête la diffusion sans fil
//!
//! Relancer la même commande pendant que le panneau est ouvert passe à
//! l'option suivante (comme rappuyer sur Win+P).

mod cast;
mod hypr;
mod p2p_helper;
mod settings;
mod wfd;

use cast::{Session, WirelessDisplay};
use eframe::egui::{self, Color32, CornerRadius, FontId, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use hypr::{Mode, Screens};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const APP_ID: &str = "archprojeteur";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Project,
    Cast,
    Settings,
}

impl Tab {
    fn msg(self) -> &'static str {
        match self {
            Tab::Project => "next:project",
            Tab::Cast => "next:cast",
            Tab::Settings => "next:settings",
        }
    }
}

/// Action choisie dans le panneau, exécutée après sa fermeture.
enum Action {
    SetMode(Mode),
    CastConnect(String, String),
    CastDisconnect(String),
}

fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("archprojeteur.sock")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = args.get(1).cloned().unwrap_or_default();
    match arg.as_str() {
        // Assistant root (via sudo) : rejoindre / quitter un groupe Wi-Fi Direct.
        "p2p-helper" => p2p_helper::main(&args[2..]),
        // Processus de diffusion lancé par le panneau.
        "cast-session" => {
            let (Some(mac), Some(name)) = (args.get(2), args.get(3)) else { fail("usage : cast-session <MAC> <nom>") };
            let res = wfd::run_session(mac, name);
            Session::clear();
            match res {
                Ok(()) => hypr::notify(&format!("Diffusion vers {name} terminée"), true),
                Err(e) => {
                    hypr::notify(&format!("Caster vers {name} : {e}"), false);
                    fail(&e);
                }
            }
        }
        "" | "gui" => run_gui(Tab::Project),
        "cast" => run_gui(Tab::Cast),
        "settings" => run_gui(Tab::Settings),
        "cast-list" => cast_list(),
        "cast-stop" => match cast::stop() {
            None => println!("Aucune diffusion en cours"),
            Some(n) => println!("Diffusion arrêtée : {n}"),
        },
        "status" => match hypr::screens() {
            Ok(s) => {
                println!("Mode actuel : {}", s.current_mode().title());
                for m in s.internal.iter().chain(s.externals.iter()) {
                    let kind = if m.is_internal() { "interne" } else { "externe" };
                    let state = if m.disabled { "éteint" } else { "allumé" };
                    println!("  {} ({kind}, {state}) {}", m.name, m.description);
                }
            }
            Err(e) => fail(&e),
        },
        "next" => match hypr::screens() {
            Ok(s) => {
                let cur = s.current_mode();
                let i = Mode::ALL.iter().position(|m| *m == cur).unwrap_or(0);
                apply_mode(Mode::ALL[(i + 1) % Mode::ALL.len()]);
            }
            Err(e) => fail(&e),
        },
        "-h" | "--help" | "help" => print_help(),
        other => match Mode::from_arg(other) {
            Some(mode) => apply_mode(mode),
            None => {
                print_help();
                std::process::exit(2);
            }
        },
    }
}

fn print_help() {
    println!(
        "archprojeteur — Projeter / Caster façon Windows pour Hyprland\n\n\
         usage : archprojeteur [cast | pc|duplicate|extend|second|next|status | cast-list|cast-stop]\n\
         sans argument : panneau Projeter ; « cast » : panneau Caster (écrans sans fil)\n\
         clavier : Flèches / 1-9, Entrée pour valider, Gauche/Droite change d'onglet, Échap pour fermer"
    );
}

fn fail(e: &str) -> ! {
    eprintln!("archprojeteur : {e}");
    std::process::exit(1);
}

fn apply_mode(mode: Mode) {
    match hypr::apply(mode) {
        Ok(()) => {
            hypr::notify(&format!("Affichage : {}", mode.title()), true);
            println!("{}", mode.title());
        }
        Err(e) => {
            hypr::notify(&format!("Projeter : {e}"), false);
            fail(&e);
        }
    }
}

fn cast_list() {
    let d = cast::Discovery::new().unwrap_or_else(|e| fail(&e));
    d.start_find().unwrap_or_else(|e| fail(&e));
    println!("Recherche des écrans sans fil…");
    std::thread::sleep(Duration::from_secs(8));
    match d.displays() {
        Ok(list) if list.is_empty() => println!("Aucun écran sans fil trouvé"),
        Ok(list) => {
            for w in list {
                let busy = if w.busy { "occupé" } else { "" };
                println!("  {:<28} {}  signal {:>3}%  {busy}", w.name, w.mac, w.strength);
            }
        }
        Err(e) => fail(&e),
    }
    match cast::current() {
        Some(s) => println!("Diffusion en cours : {} ({})", s.name, s.message),
        None => {}
    }
    if !cast::helper_ready() {
        println!("\nAssistant Wi-Fi Direct non installé : sudo ./install-helper.sh");
    }
}

// ───────────────────────────── Panneau ─────────────────────────────

/// État partagé entre le thread de découverte et l'interface.
#[derive(Default)]
struct CastModel {
    started: bool,
    displays: Vec<WirelessDisplay>,
    error: Option<String>,
    helper_ok: Option<bool>,
    session: Option<Session>,
}

fn start_discovery(model: &Arc<Mutex<CastModel>>, ctx: &egui::Context) {
    {
        let mut m = model.lock().unwrap();
        if m.started {
            return;
        }
        m.started = true;
    }
    let model = model.clone();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let ok = cast::helper_ready();
        model.lock().unwrap().helper_ok = Some(ok);
        let disc = match cast::Discovery::new() {
            Ok(d) => d,
            Err(e) => {
                model.lock().unwrap().error = Some(e);
                ctx.request_repaint();
                return;
            }
        };
        let mut last_find: Option<std::time::Instant> = None;
        loop {
            let streaming = cast::current();
            // Pas de recherche pendant une diffusion : elle perturberait le lien.
            if streaming.is_none() && last_find.is_none_or(|t| t.elapsed() > Duration::from_secs(25)) {
                if let Err(e) = disc.start_find() {
                    model.lock().unwrap().error = Some(e);
                }
                last_find = Some(std::time::Instant::now());
            }
            match disc.displays() {
                Ok(list) => {
                    let mut m = model.lock().unwrap();
                    m.displays = list;
                    m.error = None;
                }
                Err(e) => model.lock().unwrap().error = Some(e),
            }
            model.lock().unwrap().session = streaming;
            ctx.request_repaint();
            std::thread::sleep(Duration::from_millis(1000));
        }
    });
}

fn run_gui(tab: Tab) {
    let sock = socket_path();
    // Panneau déjà ouvert ? On lui transmet l'appui (option suivante / onglet).
    if let Ok(mut s) = UnixStream::connect(&sock) {
        let _ = s.write_all(format!("{}\n", tab.msg()).as_bytes());
        return;
    }
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).ok();

    let screens = hypr::screens().unwrap_or_else(|e| fail(&e));
    hypr::ensure_window_rules(APP_ID);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            .with_title("Projeter")
            .with_inner_size([380.0, 470.0])
            .with_resizable(false)
            .with_decorations(false)
            .with_always_on_top(),
        ..Default::default()
    };

    let model = Arc::new(Mutex::new(CastModel::default()));
    let mut action: Option<Action> = None;
    let action_ref = &mut action;
    let model_ui = model.clone();
    let res = eframe::run_native(
        "Projeter",
        options,
        Box::new(move |cc| {
            let (tx, rx) = channel();
            if let Some(l) = listener {
                let ctx = cc.egui_ctx.clone();
                std::thread::spawn(move || {
                    for conn in l.incoming().flatten() {
                        let mut line = String::new();
                        let _ = BufReader::new(conn).read_line(&mut line);
                        if tx.send(line.trim().to_string()).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                });
            }
            Ok(Box::new(App::new(screens, tab, rx, model_ui, action_ref)))
        }),
    );
    let _ = std::fs::remove_file(&sock);
    if let Err(e) = res {
        fail(&format!("erreur d'affichage : {e}"));
    }

    // Exécuté après la fermeture du panneau : ni le `reload` Hyprland ni le
    // sélecteur du portail ne sont gênés par la fenêtre.
    match action {
        Some(Action::SetMode(mode)) => apply_mode(mode),
        Some(Action::CastConnect(mac, name)) => {
            if let Err(e) = cast::start(&mac, &name) {
                hypr::notify(&format!("Caster : {e}"), false);
                fail(&e);
            }
        }
        Some(Action::CastDisconnect(name)) => {
            cast::stop();
            hypr::notify(&format!("Déconnecté de {name}"), true);
        }
        None => {}
    }
}

struct App<'a> {
    tab: Tab,
    screens: Screens,
    current: Mode,
    sel_mode: usize,
    sel_cast: usize,
    rx: Receiver<String>,
    model: Arc<Mutex<CastModel>>,
    action: &'a mut Option<Action>,
    frames: u32,
    session: Option<Session>,
    helper_ok: Option<bool>,
    settings: settings::Settings,
    version_clicks: u32,
    recent: Option<Vec<PathBuf>>,
}

impl<'a> App<'a> {
    fn new(
        screens: Screens,
        tab: Tab,
        rx: Receiver<String>,
        model: Arc<Mutex<CastModel>>,
        action: &'a mut Option<Action>,
    ) -> Self {
        let current = screens.current_mode();
        let sel_mode = Mode::ALL.iter().position(|m| *m == current).unwrap_or(0);
        Self { tab, screens, current, sel_mode, sel_cast: 0, rx, model, action, frames: 0, session: None, helper_ok: None, settings: settings::Settings::load(), version_clicks: 0, recent: None }
    }

    fn mode_enabled(&self, mode: Mode) -> bool {
        !mode.needs_external() || self.screens.has_external()
    }

    fn step(&mut self, delta: i32, n_cast: usize) {
        match self.tab {
            Tab::Project => {
                let n = Mode::ALL.len() as i32;
                for _ in 0..n {
                    self.sel_mode = (self.sel_mode as i32 + delta).rem_euclid(n) as usize;
                    if self.mode_enabled(Mode::ALL[self.sel_mode]) {
                        break;
                    }
                }
            }
            Tab::Cast if n_cast > 0 => {
                self.sel_cast = (self.sel_cast as i32 + delta).rem_euclid(n_cast as i32) as usize;
            }
            Tab::Cast | Tab::Settings => {}
        }
    }

    fn close(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn choose_mode(&mut self, ctx: &egui::Context, idx: usize) {
        let mode = Mode::ALL[idx];
        if !self.mode_enabled(mode) {
            return;
        }
        if mode != self.current {
            *self.action = Some(Action::SetMode(mode));
        }
        self.close(ctx);
    }

    fn choose_display(&mut self, ctx: &egui::Context, d: &WirelessDisplay) {
        let active = self.session.as_ref().is_some_and(|s| s.mac == d.mac);
        *self.action = Some(if active {
            Action::CastDisconnect(d.name.clone())
        } else {
            Action::CastConnect(d.mac.clone(), d.name.clone())
        });
        self.close(ctx);
    }
}

// Palette
const BG: Color32 = Color32::from_rgb(0x1c, 0x1e, 0x26);
const TILE: Color32 = Color32::from_rgb(0x26, 0x29, 0x34);
const TILE_HOVER: Color32 = Color32::from_rgb(0x2e, 0x32, 0x3f);
const ACCENT: Color32 = Color32::from_rgb(0x4e, 0xa1, 0xff);
const OK: Color32 = Color32::from_rgb(0x4c, 0xd1, 0x8a);
const ERR: Color32 = Color32::from_rgb(0xff, 0x6b, 0x6b);
const TEXT: Color32 = Color32::from_rgb(0xe8, 0xea, 0xf0);
const MUTED: Color32 = Color32::from_rgb(0x8a, 0x90, 0xa2);
const DISABLED: Color32 = Color32::from_rgb(0x55, 0x5a, 0x68);

impl eframe::App for App<'_> {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        if self.tab == Tab::Cast {
            start_discovery(&self.model, ctx);
        }
        let (displays, cast_error) = {
            let m = self.model.lock().unwrap();
            self.session = m.session.clone();
            self.helper_ok = m.helper_ok;
            (m.displays.clone(), m.error.clone())
        };
        let n_cast = displays.len();
        if self.sel_cast >= n_cast {
            self.sel_cast = n_cast.saturating_sub(1);
        }

        // Appuis venant d'une 2ᵉ invocation (raccourci pressé de nouveau).
        while let Ok(msg) = self.rx.try_recv() {
            let wanted = match msg.as_str() {
                "next:cast" => Tab::Cast,
                "next:settings" => Tab::Settings,
                _ => Tab::Project,
            };
            if wanted == self.tab {
                self.step(1, n_cast);
            } else {
                self.tab = wanted;
            }
        }

        // Clavier
        let mut pick: Option<usize> = None;
        let mut switch_tab = false;
        let mut close = false;
        let mut delta = 0;
        let typing = self.tab == Tab::Settings; // champ texte : pas de raccourcis
        ctx.input(|i| {
            close = i.key_pressed(Key::Escape);
            if typing {
                return;
            }
            switch_tab = i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::ArrowRight);
            if i.key_pressed(Key::ArrowDown)
                || (i.key_pressed(Key::Tab) && !i.modifiers.shift)
                || i.key_pressed(Key::P)
                || i.key_pressed(Key::K)
            {
                delta = 1;
            }
            if i.key_pressed(Key::ArrowUp) || (i.key_pressed(Key::Tab) && i.modifiers.shift) {
                delta = -1;
            }
            if i.key_pressed(Key::Enter) || i.key_pressed(Key::Space) {
                pick = Some(usize::MAX);
            }
            let digits = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
            for (n, k) in digits.iter().enumerate() {
                if i.key_pressed(*k) {
                    pick = Some(n);
                }
            }
        });
        if close {
            self.close(ctx);
        }
        if switch_tab {
            self.tab = if self.tab == Tab::Project { Tab::Cast } else { Tab::Project };
            // (l'onglet Paramètres ne s'atteint qu'à la souris)
        }
        if delta != 0 {
            self.step(delta, n_cast);
        }
        if let Some(p) = pick {
            match self.tab {
                Tab::Project => {
                    let idx = if p == usize::MAX { self.sel_mode } else { p };
                    if idx < Mode::ALL.len() {
                        self.choose_mode(ctx, idx);
                    }
                }
                Tab::Cast => {
                    let idx = if p == usize::MAX { self.sel_cast } else { p };
                    if let Some(d) = displays.get(idx) {
                        self.choose_display(ctx, d);
                    }
                }
                Tab::Settings => {}
            }
        }

        // Ferme le panneau s'il perd le focus (clic ailleurs).
        self.frames = self.frames.saturating_add(1);
        if self.frames > 10 && ctx.input(|i| !i.viewport().focused.unwrap_or(true)) {
            self.close(ctx);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::same(16)))
            .show(root, |ui| {
                // Onglets
                ui.horizontal(|ui| {
                    for (t, label) in [(Tab::Project, "Projeter"), (Tab::Cast, "Caster"), (Tab::Settings, "Paramètres")] {
                        let active = self.tab == t;
                        let txt = egui::RichText::new(label)
                            .font(FontId::proportional(if active { 21.0 } else { 17.0 }))
                            .color(if active { TEXT } else { MUTED });
                        let r = ui.add(egui::Label::new(txt).sense(Sense::click()));
                        if active {
                            let y = r.rect.max.y + 3.0;
                            ui.painter().line_segment(
                                [Pos2::new(r.rect.min.x, y), Pos2::new(r.rect.min.x + 24.0, y)],
                                Stroke::new(3.0, ACCENT),
                            );
                        }
                        if r.clicked() {
                            self.tab = t;
                        }
                        ui.add_space(10.0);
                    }
                });
                ui.add_space(16.0);

                match self.tab {
                    Tab::Project => self.ui_project(ui, ctx),
                    Tab::Cast => self.ui_cast(ui, ctx, &displays, cast_error),
                    Tab::Settings => self.ui_settings(ui, ctx),
                }
            });

        if self.tab == Tab::Cast {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

impl App<'_> {
    fn ui_project(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        for (idx, mode) in Mode::ALL.iter().copied().enumerate() {
            let enabled = self.mode_enabled(mode);
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 66.0), Sense::click());
            if resp.hovered() && enabled && ctx.input(|i| i.pointer.delta() != Vec2::ZERO) {
                self.sel_mode = idx;
            }
            if resp.clicked() && enabled {
                self.choose_mode(ctx, idx);
            }
            let selected = idx == self.sel_mode && enabled;
            let p = ui.painter();
            tile(p, rect, selected);
            draw_mode_icon(p, Rect::from_min_size(rect.min + Vec2::new(14.0, 13.0), Vec2::new(56.0, 40.0)), mode, enabled);

            let is_current = mode == self.current;
            let sub = if is_current {
                "Mode actuel"
            } else if !enabled {
                "Aucun écran externe branché"
            } else {
                ["1", "2", "3", "4"][idx]
            };
            two_lines(p, rect, 86.0, mode.title(), if enabled { TEXT } else { DISABLED }, sub, if is_current { ACCENT } else { MUTED });
            if is_current {
                p.circle_filled(Pos2::new(rect.max.x - 18.0, rect.center().y), 4.5, ACCENT);
            }
            ui.add_space(8.0);
        }
        ui.add_space(4.0);
        let names: Vec<String> =
            self.screens.internal.iter().chain(self.screens.externals.iter()).map(|m| m.name.clone()).collect();
        footer(ui, &format!("Écrans : {}   ·   Gauche/Droite : onglet · Entrée · Échap", names.join(", ")));
    }

    fn ui_cast(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, displays: &[WirelessDisplay], error: Option<String>) {
        if let Some(e) = error {
            ui.label(egui::RichText::new(format!("Impossible de rechercher des écrans :\n{e}")).color(ERR));
            return;
        }
        if self.helper_ok == Some(false) {
            egui::Frame::new().fill(TILE).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                ui.label(egui::RichText::new("Installation requise (une seule fois)").color(ERR).strong());
                ui.label(egui::RichText::new("Pour rejoindre les écrans de salle, dans le dossier du projet :").color(MUTED).font(FontId::proportional(12.0)));
                ui.label(egui::RichText::new("sudo ./install-helper.sh").monospace().color(TEXT));
            });
            ui.add_space(8.0);
        }

        let status = match &self.session {
            Some(s) => format!("{} — {}", s.name, if s.message.is_empty() { &s.state } else { &s.message }),
            None if displays.is_empty() => "Recherche d'écrans sans fil…".into(),
            None => "Écrans sans fil disponibles".into(),
        };
        ui.horizontal(|ui| {
            if self.session.as_ref().is_none_or(|s| s.state != "streaming") {
                ui.add(egui::Spinner::new().size(14.0).color(MUTED));
            }
            ui.label(egui::RichText::new(status).color(MUTED).font(FontId::proportional(13.0)));
        });
        if let Some(m) = self.settings.active_media() {
            ui.label(
                egui::RichText::new(format!("Mode troll : « {} » au lieu de l'écran", m.file_name()))
                    .color(ACCENT)
                    .font(FontId::proportional(12.0)),
            );
        }
        ui.add_space(10.0);

        let list_h = ui.available_height() - 28.0;
        egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
            for (idx, d) in displays.iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 56.0), Sense::click());
                if resp.hovered() && ctx.input(|i| i.pointer.delta() != Vec2::ZERO) {
                    self.sel_cast = idx;
                }
                if resp.clicked() {
                    self.choose_display(ctx, d);
                }
                let session = self.session.as_ref().filter(|s| s.mac == d.mac);
                let streaming = session.is_some_and(|s| s.state == "streaming");
                let p = ui.painter();
                tile(p, rect, idx == self.sel_cast);
                let color = if streaming { OK } else if session.is_some() { ACCENT } else { TEXT };
                draw_cast_icon(p, Rect::from_min_size(rect.min + Vec2::new(14.0, 12.0), Vec2::new(40.0, 32.0)), color);
                let num = if idx < 9 { format!(" · {}", idx + 1) } else { String::new() };
                let (sub, sc) = match session {
                    Some(_) if streaming => ("Connecté · Entrée pour déconnecter".to_string(), OK),
                    Some(s) => (format!("{} · Entrée pour annuler", s.message), ACCENT),
                    None if d.busy => (format!("Occupé par un autre appareil{num}"), MUTED),
                    None => (format!("Miracast · signal {}%{num}", d.strength), MUTED),
                };
                two_lines(p, rect, 68.0, &d.name, TEXT, &sub, sc);
                ui.add_space(6.0);
            }
        });
        footer(ui, "Entrée : connecter / déconnecter   ·   Gauche/Droite : onglet · Échap");
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        use settings::{Media, MediaKind};
        let mut changed = false;
        let section = |ui: &mut egui::Ui, t: &str| {
            ui.label(egui::RichText::new(t).color(TEXT).strong().font(FontId::proportional(14.0)));
        };

        // Fichier glissé sur la fenêtre → mode troll (si débloqué).
        if self.settings.troll_unlocked {
            if let Some(p) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty())) {
                self.settings.media_path = p.to_string_lossy().to_string();
                self.settings.troll_enabled = true;
                changed = true;
            }
        }

        let body_h = ui.available_height() - 24.0;
        egui::ScrollArea::vertical().max_height(body_h).auto_shrink([false, false]).show(ui, |ui| {
            section(ui, "Diffusion sans fil");
            changed |= ui.checkbox(&mut self.settings.allow_1080p, "Autoriser le 1080p (sinon 720p max)").changed();
            changed |= ui.checkbox(&mut self.settings.audio, "Envoyer le son du PC").changed();
            ui.add_space(10.0);
            section(ui, "Raccourcis");
            ui.label(egui::RichText::new("Super+K : Caster   ·   Super+Shift+K : Projeter").color(MUTED).font(FontId::proportional(12.0)));
            ui.add_space(10.0);
            section(ui, "Assistant Wi-Fi Direct");
            let helper = match self.helper_ok {
                Some(true) => ("Installé", OK),
                Some(false) => ("Non installé : sudo ./install-helper.sh", ERR),
                None => ("Vérification à l'ouverture de Caster", MUTED),
            };
            ui.label(egui::RichText::new(helper.0).color(helper.1).font(FontId::proportional(12.0)));

            if self.settings.troll_unlocked {
                ui.add_space(12.0);
                egui::Frame::new()
                    .fill(TILE)
                    .stroke(Stroke::new(1.5, ACCENT))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(egui::RichText::new("Mode troll").color(ACCENT).strong().font(FontId::proportional(15.0)));
                        changed |= ui
                            .checkbox(&mut self.settings.troll_enabled, "Diffuser un fichier au lieu de l'écran")
                            .changed();
                        ui.label(egui::RichText::new("Image ou vidéo (glisse-la sur cette fenêtre) :").color(MUTED).font(FontId::proportional(12.0)));
                        changed |= ui
                            .add(egui::TextEdit::singleline(&mut self.settings.media_path).hint_text("/chemin/vers/fichier.mp4").desired_width(f32::INFINITY))
                            .changed();

                        let recent = self.recent.get_or_insert_with(|| settings::recent_media(25)).clone();
                        egui::ComboBox::from_id_salt("recent-media")
                            .selected_text("Fichiers récents…")
                            .width(ui.available_width())
                            .show_ui(ui, |ui| {
                                if recent.is_empty() {
                                    ui.label("Rien dans Images / Vidéos / Téléchargements");
                                }
                                for p in &recent {
                                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                                    if ui.selectable_label(false, name).clicked() {
                                        self.settings.media_path = p.to_string_lossy().to_string();
                                        self.settings.troll_enabled = true;
                                        changed = true;
                                    }
                                }
                            });

                        let (txt, col) = if self.settings.media_path.trim().is_empty() {
                            ("Aucun fichier choisi".to_string(), MUTED)
                        } else {
                            match Media::from_path(&self.settings.media_path, self.settings.media_loop) {
                                Some(m) if m.kind == MediaKind::Image => (format!("Image : {}", m.file_name()), OK),
                                Some(m) => (format!("Vidéo : {}", m.file_name()), OK),
                                None => ("Fichier introuvable ou format non géré".to_string(), ERR),
                            }
                        };
                        ui.label(egui::RichText::new(txt).color(col).font(FontId::proportional(12.0)));
                        changed |= ui.checkbox(&mut self.settings.media_loop, "Vidéo en boucle").changed();
                        ui.add_space(4.0);
                        if ui.small_button("Masquer ce mode").clicked() {
                            self.settings.troll_unlocked = false;
                            self.settings.troll_enabled = false;
                            self.version_clicks = 0;
                            changed = true;
                        }
                    });
            }
        });

        // Version : 5 clics débloquent le mode caché.
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("ArchProjeteur v{}", env!("CARGO_PKG_VERSION")))
                        .color(MUTED)
                        .font(FontId::proportional(11.5)),
                )
                .sense(Sense::click()),
            );
            if r.clicked() && !self.settings.troll_unlocked {
                self.version_clicks += 1;
                if self.version_clicks >= 5 {
                    self.settings.troll_unlocked = true;
                    changed = true;
                }
            }
            if !self.settings.troll_unlocked && (2..5).contains(&self.version_clicks) {
                ui.label(egui::RichText::new(format!("encore {}…", 5 - self.version_clicks)).color(MUTED).font(FontId::proportional(11.5)));
            } else if self.settings.troll_unlocked && self.version_clicks >= 5 {
                ui.label(egui::RichText::new("mode secret débloqué").color(ACCENT).font(FontId::proportional(11.5)));
            }
        });

        if changed {
            self.settings.save();
        }
    }
}

fn tile(p: &egui::Painter, rect: Rect, selected: bool) {
    p.rect_filled(rect, CornerRadius::same(10), if selected { TILE_HOVER } else { TILE });
    if selected {
        p.rect_stroke(rect, CornerRadius::same(10), Stroke::new(2.0, ACCENT), StrokeKind::Inside);
    }
}

fn two_lines(p: &egui::Painter, rect: Rect, x: f32, title: &str, tc: Color32, sub: &str, sc: Color32) {
    let short: String = if title.chars().count() > 30 {
        format!("{}…", title.chars().take(29).collect::<String>())
    } else {
        title.to_string()
    };
    p.text(Pos2::new(rect.min.x + x, rect.center().y - 9.0), egui::Align2::LEFT_CENTER, short, FontId::proportional(15.0), tc);
    p.text(Pos2::new(rect.min.x + x, rect.center().y + 11.0), egui::Align2::LEFT_CENTER, sub, FontId::proportional(12.0), sc);
}

fn footer(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).font(FontId::proportional(11.5)).color(MUTED));
}

/// Pictogramme « écran + ondes » pour le Caster.
fn draw_cast_icon(p: &egui::Painter, r: Rect, color: Color32) {
    let screen = Rect::from_min_size(r.min + Vec2::new(4.0, 2.0), Vec2::new(32.0, 22.0));
    let s = Stroke::new(1.6, color);
    p.rect_stroke(screen, CornerRadius::same(2), s, StrokeKind::Middle);
    let base = Pos2::new(screen.min.x + 4.0, screen.max.y - 4.0);
    for rad in [6.0_f32, 11.0] {
        let pts: Vec<Pos2> = (0..=10)
            .map(|i| {
                let a = -std::f32::consts::FRAC_PI_2 + (i as f32 / 10.0) * std::f32::consts::FRAC_PI_2;
                Pos2::new(base.x + rad * a.cos(), base.y + rad * a.sin())
            })
            .collect();
        p.add(egui::Shape::line(pts, s));
    }
    p.circle_filled(base, 1.8, color);
    let foot = Pos2::new(screen.center().x, screen.max.y);
    p.line_segment([foot, foot + Vec2::new(0.0, 5.0)], s);
    p.line_segment([foot + Vec2::new(-6.0, 5.0), foot + Vec2::new(6.0, 5.0)], s);
}

/// Petits pictogrammes : deux écrans, remplis selon le mode.
fn draw_mode_icon(p: &egui::Painter, r: Rect, mode: Mode, enabled: bool) {
    let on = if enabled { ACCENT } else { DISABLED };
    let off = Color32::TRANSPARENT;
    let line = if enabled { TEXT } else { DISABLED };
    let (w, h) = (24.0, 17.0);
    let y = r.center().y - h / 2.0 - 2.0;
    let a = Rect::from_min_size(Pos2::new(r.min.x + 2.0, y), Vec2::new(w, h));
    let b = Rect::from_min_size(Pos2::new(r.max.x - w - 2.0, y), Vec2::new(w, h));
    let (fa, fb) = match mode {
        Mode::PcOnly => (on, off),
        Mode::Duplicate | Mode::Extend => (on, on),
        Mode::SecondOnly => (off, on),
    };
    for (rect, fill) in [(a, fa), (b, fb)] {
        p.rect_filled(rect, CornerRadius::same(2), fill);
        p.rect_stroke(rect, CornerRadius::same(2), Stroke::new(1.5, line), StrokeKind::Middle);
        let foot = Pos2::new(rect.center().x, rect.max.y);
        p.line_segment([foot, foot + Vec2::new(0.0, 5.0)], Stroke::new(1.5, line));
        p.line_segment([foot + Vec2::new(-5.0, 5.0), foot + Vec2::new(5.0, 5.0)], Stroke::new(1.5, line));
    }
    match mode {
        Mode::Duplicate => {
            for rect in [a, b] {
                p.circle_filled(rect.center(), 3.0, BG);
            }
        }
        Mode::Extend => {
            p.line_segment(
                [Pos2::new(a.max.x - 4.0, a.center().y), Pos2::new(b.min.x + 4.0, b.center().y)],
                Stroke::new(3.0, BG),
            );
        }
        _ => {}
    }
}

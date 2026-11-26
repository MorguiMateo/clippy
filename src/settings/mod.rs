//! Ventana principal / ajustes (proceso aparte: `clippy.exe --settings`).
//! Al cerrarla el proceso termina, así que mientras jugás no ocupa nada.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Align, Layout, Margin, RichText};

use crate::app::autostart;
use crate::app::keys::{self, hotkey_name};
use crate::app::mode::{close_tray, find_tray, post_tray, Window, WM_HOTKEY_SUSPEND, WM_RELOAD};
use crate::app::status::{self, Status};
use crate::app::update;
use crate::config::{self, Config, GlobalAction};
use crate::record::audio;
use crate::ui_kit::theme::*;
use crate::ui_kit::watch::Watcher;
use crate::ui_kit::window::{maximize_once, single_instance, window_options};

mod news;
mod pages;

/// Ayuda mientras se espera una tecla nueva (Inicio y Atajos).
const CAPTURE_HINT: &str = "Esc cancels. You can combine it with Ctrl, Shift or Alt.";

pub fn run() {
    let win = Window::Settings;
    let Some(_mutex) = single_instance(win.mutex(), || win.find()) else { return };
    let _ = eframe::run_native(
        win.title(),
        window_options(win.title(), [760.0, 560.0]),
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(SettingsApp::new(cc.egui_ctx.clone())))
        }),
    );
}

/// Resultado de un trabajo en segundo plano (None = todavía no terminó).
type Shared<T> = std::sync::Arc<std::sync::Mutex<Option<T>>>;

struct SettingsApp {
    cfg: Config,
    saved: Config,
    prev: Config,
    last_edit: Option<Instant>,
    /// Atajo global que se está eligiendo.
    capturing: Option<GlobalAction>,
    global_note: Option<String>,
    ignore_clicks_until: Instant,
    autostart: bool,
    main_running: bool,
    /// Con qué graba la grabadora y por qué no puede.
    recorder: Status,
    /// Avisa cuando cambia algo afuera (config, clips, la grabadora): no se sondea.
    watcher: Watcher,
    mtime: Option<SystemTime>,
    clips: (usize, Option<SystemTime>),
    /// Carpeta de los clips y las de cada juego, con sus fechas, la última vez que se contaron.
    clips_seen: Vec<(PathBuf, Option<SystemTime>)>,
    mics: Vec<(String, String)>,
    /// Monitores conectados (nombre de Windows, cómo se muestra), para elegir cuál grabar.
    monitors: Vec<(String, String)>,
    /// Versión nueva disponible y estado de la descarga.
    update: Option<(String, String)>,
    update_status: Shared<Result<(), String>>,
    updating: bool,
    /// "Buscar actualizaciones": la consulta en curso y su resultado (versión nueva, si hay).
    check: Shared<Result<Option<String>, String>>,
    checking: bool,
    /// Lo que dijo la última búsqueda ("Tenés la última versión.", un error). Queda a la vista mientras
    /// se busca de nuevo, así la fila no se achica y vuelve a crecer.
    check_note: Option<String>,
    /// Novedades de la versión recién instalada (versión, texto), hasta que se cierran.
    news: Option<(String, String)>,
    /// Confirmación de "Actualizar": la versión nueva y lo que trae, si el release tiene texto.
    confirm_update: Option<(String, Option<String>)>,
    maximized: bool,
    page: Page,
    confirm_quit: bool,
    /// Se eligió "Personalizada" en la calidad (aunque el valor coincida con uno de los fijos).
    custom_quality: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Home,
    Recording,
    Audio,
    Keys,
    General,
}

impl SettingsApp {
    fn new(ctx: egui::Context) -> Self {
        let cfg = config::load();
        let watcher = Watcher::new(ctx, cfg.output_dir.clone(), true);
        let mut s = Self {
            saved: cfg.clone(),
            prev: cfg.clone(),
            cfg,
            last_edit: None,
            capturing: None,
            global_note: None,
            ignore_clicks_until: Instant::now(),
            autostart: autostart::enabled(),
            main_running: false,
            recorder: Status::default(),
            watcher,
            mtime: config::config_mtime(),
            clips: (0, None),
            clips_seen: Vec::new(),
            mics: audio::capture_devices(),
            monitors: pages::monitors(),
            update: None,
            update_status: Default::default(),
            updating: false,
            check: Default::default(),
            checking: false,
            check_note: None,
            news: None,
            confirm_update: None,
            maximized: false,
            page: Page::Home,
            confirm_quit: false,
            custom_quality: false,
        };
        s.poll_outside();
        s
    }

    fn apply(&mut self) {
        self.cfg.sanitize();
        if config::save(&self.cfg).is_ok() {
            self.saved = self.cfg.clone();
            self.prev = self.cfg.clone();
            self.mtime = config::config_mtime();
            post_tray(WM_RELOAD, 0);
        }
    }

    /// Descarga la versión nueva en segundo plano y la instala (cierra todo y vuelve a abrir Clippy).
    fn start_update(&mut self) {
        let Some((_, url)) = self.update.clone() else { return };
        if self.update_busy() {
            return;
        }
        self.updating = true;
        let st = self.update_status.clone();
        *st.lock().unwrap() = None;
        std::thread::spawn(move || {
            let r = update::download(&url).and_then(|p| update::start_install(&p)).map_err(|e| e.to_string());
            *st.lock().unwrap() = Some(r);
        });
    }

    /// "Buscar actualizaciones": una consulta en segundo plano.
    fn start_check(&mut self) {
        self.checking = true;
        let st = self.check.clone();
        *st.lock().unwrap() = None;
        std::thread::spawn(move || {
            let r = update::check_now().map_err(|e| e.to_string());
            *st.lock().unwrap() = Some(r);
        });
    }

    /// Mientras se busca, mirar si terminó; si hay versión nueva aparece el botón para actualizar.
    fn finish_check(&mut self, ctx: &egui::Context) {
        if !self.checking {
            return;
        }
        let note = match &*self.check.lock().unwrap() {
            Some(Ok(None)) => Some("You have the latest version.".to_string()),
            Some(Ok(Some(_))) => None,
            Some(Err(e)) => Some(format!("Couldn't check: {e}")),
            None => {
                ctx.request_repaint_after(Duration::from_millis(200));
                return;
            }
        };
        self.checking = false;
        self.check_note = note;
        self.update = update::available();
    }

    /// Hay una descarga en curso (todavía sin resultado).
    fn update_busy(&self) -> bool {
        self.updating && self.update_status.lock().unwrap().is_none()
    }

    /// Empieza a esperar la tecla nueva para ese atajo (la bandeja suelta sus atajos mientras tanto).
    fn start_capture(&mut self, a: GlobalAction) {
        if self.capturing.is_some() || Instant::now() <= self.ignore_clicks_until {
            return;
        }
        self.capturing = Some(a);
        self.global_note = None;
        post_tray(WM_HOTKEY_SUSPEND, 1);
    }

    /// Deja de esperar una tecla (la elegiste, apretaste Esc, cambiaste de página o cerraste la
    /// ventana) y la bandeja vuelve a escuchar sus atajos. Si no se estaba esperando, no hace nada.
    fn end_capture(&mut self) {
        if self.capturing.take().is_some() {
            self.ignore_clicks_until = Instant::now() + Duration::from_millis(400);
            post_tray(WM_HOTKEY_SUSPEND, 0);
        }
    }

    /// Mientras se descarga, seguir mirando el estado; si el instalador ya arrancó, cerrar todo
    /// para que pueda reemplazar el programa (en cualquier página, no solo en Inicio).
    fn finish_update(&mut self, ctx: &egui::Context) {
        if !self.updating {
            return;
        }
        let status = self.update_status.lock().unwrap().clone();
        match status {
            Some(Ok(())) => {
                close_tray();
                Window::Editor.close();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            // Falló: el error queda a la vista y se deja de mirar (nada de repintar en reposo).
            Some(Err(_)) => self.updating = false,
            None => ctx.request_repaint_after(Duration::from_millis(200)),
        }
    }

    /// Relee lo que cambia desde afuera (la grabadora, la bandeja, clips nuevos), solo si hubo un aviso.
    fn poll_outside(&mut self) {
        self.watcher.set_clips(&self.cfg.output_dir);
        if !self.watcher.take_changed() {
            return;
        }
        self.main_running = find_tray().is_some();
        self.recorder = status::load();
        // La grabadora escribe update.json cuando encuentra una versión nueva: el botón aparece al toque,
        // aunque esta ventana se haya abierto antes de que terminara la consulta.
        if !self.updating {
            self.update = update::available();
        }
        // Las novedades quedan listas cuando arranca la grabadora recién actualizada (a veces después
        // de abrir esta ventana, si hubo que bajar el texto).
        self.news = update::news();
        let seen = clip_dirs(&self.cfg.output_dir);
        if self.clips_seen != seen {
            self.clips = count_clips(&seen);
            self.clips_seen = seen;
        }
        // Si se cambió algo desde la bandeja (ej. pausar), reflejarlo acá.
        let m = config::config_mtime();
        if m != self.mtime {
            self.mtime = m;
            if self.cfg == self.saved {
                let c = config::load();
                self.cfg = c.clone();
                self.saved = c.clone();
                self.prev = c;
            }
        }
    }

    fn capture_key(&mut self, ctx: &egui::Context) {
        let Some(slot) = self.capturing else { return };
        if let Some((vk, mods)) = keys::pressed_key() {
            if vk != 0x1B {
                // Si otro atajo global usaba la misma tecla, queda libre.
                self.global_note = None;
                for other in GlobalAction::ALL {
                    if other != slot && self.cfg.hotkey(other) == (vk, mods) {
                        let (v, m) = self.cfg.hotkey_mut(other);
                        *v = 0;
                        *m = 0;
                        let (label, key) = (other.label(), hotkey_name(vk, mods));
                        self.global_note = Some(format!("“{label}” no longer has a key because it used {key}."));
                    }
                }
                let (v, m) = self.cfg.hotkey_mut(slot);
                *v = vk;
                *m = mods;
            }
            self.end_capture();
        }
        ctx.request_repaint_after(Duration::from_millis(15));
    }
}

impl Drop for SettingsApp {
    fn drop(&mut self) {
        self.end_capture();
        if self.cfg != self.saved {
            self.apply();
        }
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        maximize_once(ctx, &mut self.maximized, Window::Settings.title());
        self.poll_outside();
        self.capture_key(ctx);
        self.finish_update(ctx);

        egui::SidePanel::left("nav")
            .exact_width(260.0)
            .resizable(false)
            .frame(egui::Frame::none().fill(CARD).stroke(egui::Stroke::new(1.0_f32, LINE)).inner_margin(Margin::symmetric(16.0, 18.0)))
            .show(ctx, |ui| self.nav(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG).inner_margin(Margin::symmetric(36.0, 28.0)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    centered(ui, 860.0, |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                        match self.page {
                            Page::Home => self.home_page(ui),
                            Page::Recording => self.recording_page(ui),
                            Page::Audio => self.audio_page(ui),
                            Page::Keys => self.keys_page(ui),
                            Page::General => self.general_page(ui),
                        }
                    });
                });
            });

        if self.cfg != self.prev {
            self.prev = self.cfg.clone();
            self.last_edit = Some(Instant::now());
        }
        if let Some(t) = self.last_edit {
            if t.elapsed() >= Duration::from_millis(400) {
                self.last_edit = None;
                self.apply();
            } else {
                ctx.request_repaint_after(Duration::from_millis(420));
            }
        }
        self.news_modals(ctx);
        if self.confirm_quit {
            let mut quit = false;
            let title = "Quit Clippy?";
            let sub = "Recording stops and everything closes.";
            let x = modal(ctx, "quit", title, sub, 480.0, |ui| {
                modal_body(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new("The editor and this window close. Anything you haven't saved as a clip is lost.\n\n\
                                 If you only want to close the window, use the X: Clippy keeps recording in the background.")
                            .color(MUTED),
                        )
                        .wrap(),
                    );
                });
                modal_footer(ui, |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if button_sized(ui, "Quit", Kind::Danger, egui::vec2(120.0, 38.0)).clicked() {
                            quit = true;
                        }
                        if button_sized(ui, "Cancel", Kind::Secondary, egui::vec2(120.0, 38.0)).clicked() {
                            self.confirm_quit = false;
                        }
                    });
                });
            });
            if x {
                self.confirm_quit = false;
            }
            if quit {
                if self.cfg != self.saved {
                    self.apply();
                }
                close_tray();
                Window::Editor.close();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

/// La carpeta de los clips y las de adentro (un nivel, como la biblioteca), con la fecha en que cambiaron.
/// La fecha se pide a cada carpeta: la que trae la lista de la de arriba Windows la actualiza tarde.
fn clip_dirs(root: &Path) -> Vec<(PathBuf, Option<SystemTime>)> {
    let subdirs = std::fs::read_dir(root).into_iter().flatten().flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir()));
    std::iter::once(root.to_path_buf())
        .chain(subdirs.map(|e| e.path()))
        .map(|d| {
            let m = std::fs::metadata(&d).and_then(|m| m.modified()).ok();
            (d, m)
        })
        .collect()
}

fn count_clips(dirs: &[(PathBuf, Option<SystemTime>)]) -> (usize, Option<SystemTime>) {
    let mut n = 0;
    let mut last = None;
    for (dir, _) in dirs {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4")) {
                n += 1;
                if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                    if last.is_none_or(|l| m > l) {
                        last = Some(m);
                    }
                }
            }
        }
    }
    (n, last)
}

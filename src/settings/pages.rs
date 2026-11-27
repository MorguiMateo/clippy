//! Páginas de la ventana principal: navegación, Inicio, Grabación, Audio, Atajos y General.
//! Cada ajuste va en una fila `setting` con su nombre, qué hace y el control.

use std::time::SystemTime;

use eframe::egui::{self, Align, Layout, Margin, RichText};

use crate::app::autostart;
use crate::app::keys::hotkey_name;
use crate::app::mode::Mode;
use crate::app::update;
use crate::config::{CaptureMode, Codec, GlobalAction, MAX_MBPS, MAX_SECONDS, MIN_MBPS, MIN_SECONDS, RESOLUTIONS};
use crate::log::app_dir;
use crate::record::{audio, screen, Device, CPU_MAX_FPS, CPU_MAX_SHORT};
use crate::ui_kit::sound;
use crate::ui_kit::theme::*;
use crate::ui_kit::window::open_folder;

use super::{Page, SettingsApp, CAPTURE_HINT};

/// Calidades de video de un toque (Mbps): baja, media y alta.
const QUALITY: [u32; 3] = [15, 30, 60];

fn page_title(ui: &mut egui::Ui, title: &str, sub: &str) {
    ui.label(RichText::new(title).family(bold()).size(28.0).color(TEXT));
    ui.label(RichText::new(sub).color(MUTED));
    ui.add_space(18.0);
}

/// Los monitores conectados, como (nombre de Windows, cómo se muestra).
pub(super) fn monitors() -> Vec<(String, String)> {
    screen::monitors().into_iter().map(|m| (m.name, m.label)).collect()
}

/// Ancho de un deslizador que deja lugar a su valor a la derecha.
fn slider_width(ui: &mut egui::Ui, value_w: f32) {
    ui.spacing_mut().slider_width = (ui.available_width() - value_w).max(80.0);
}

impl SettingsApp {
    /// Barra lateral: logo y versión, estado de la grabación, secciones y acceso al editor.
    pub(super) fn nav(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            app_mark(ui, 30.0);
            ui.label(RichText::new("Clippy").family(bold()).size(20.0).color(TEXT));
            badge(ui, concat!("v", env!("CARGO_PKG_VERSION")), MUTED).on_hover_text("The version you have installed");
        });
        ui.add_space(18.0);

        egui::Frame::none().fill(BG).stroke(egui::Stroke::new(1.0_f32, LINE)).rounding(10.0).inner_margin(Margin::same(14.0)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if self.main_running {
                // Si no puede grabar se ve acá, en todas las páginas, con el motivo (la bandeja avisa una sola vez).
                let error = self.recorder.error.clone().filter(|_| self.cfg.enabled);
                let hidden = self.recorder.hidden.clone().filter(|_| self.cfg.enabled && error.is_none());
                ui.horizontal(|ui| {
                    let (txt, dot, col) = if error.is_some() {
                        ("Can't record", DANGER, DANGER)
                    } else if hidden.is_some() {
                        ("Can't see the game", DANGER, DANGER)
                    } else if self.cfg.enabled {
                        ("Recording", REC, TEXT)
                    } else {
                        ("Paused", MUTED, MUTED)
                    };
                    status_dot(ui, dot);
                    ui.label(RichText::new(txt).family(bold()).color(col));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        toggle(ui, &mut self.cfg.enabled).on_hover_text(if self.cfg.enabled { "Pause" } else { "Resume" });
                    });
                });
                let sub = if let Some(e) = error {
                    format!("{e} Clippy keeps trying by itself.")
                } else if let Some(h) = hidden {
                    h
                } else if self.cfg.enabled {
                    let (key, secs) = (hotkey_name(self.cfg.hotkey_vk, self.cfg.hotkey_mods), fmt_secs(self.cfg.seconds));
                    match self.recording_game() {
                        Some(g) => format!("{g} · {key} saves the last {secs}"),
                        None => format!("{key} saves the last {secs}"),
                    }
                } else {
                    "Nothing is being saved".into()
                };
                ui.add(egui::Label::new(RichText::new(sub).size(12.5).color(MUTED)).wrap());
            } else {
                ui.horizontal(|ui| {
                    status_dot(ui, MUTED);
                    ui.label(RichText::new("The recorder is closed").family(bold()).color(TEXT));
                });
                ui.add_space(4.0);
                if wide_button(ui, "Start recorder", Kind::Primary, 34.0).clicked() {
                    Mode::Tray { background: false }.launch();
                }
            }
        });
        ui.add_space(18.0);

        ui.label(RichText::new("SETTINGS").size(11.0).color(MUTED).extra_letter_spacing(1.2));
        ui.add_space(2.0);
        for (page, label) in [
            (Page::Home, "Home"),
            (Page::Recording, "Recording"),
            (Page::Audio, "Audio"),
            (Page::Keys, "Shortcuts"),
            (Page::General, "General"),
        ] {
            if nav_item(ui, label, self.page == page).clicked() {
                self.page = page;
                self.end_capture();
                // Al entrar a Grabación se vuelven a buscar los monitores (pudo conectarse otro).
                if page == Page::Recording {
                    self.monitors = monitors();
                }
            }
        }

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            if wide_button(ui, "Quit Clippy", Kind::Danger, 36.0).on_hover_text("Stops recording and closes everything").clicked() {
                self.confirm_quit = true;
            }
            ui.add_space(14.0);
            divider(ui);
            ui.add_space(14.0);
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                let w = ui.available_width();
                if icon_button(ui, Glyph::Folder, "Clips folder", Kind::Secondary, egui::vec2(w, 36.0)).clicked() {
                    open_folder(&self.cfg.output_dir);
                }
                // Un solo botón principal: con la grabadora cerrada, lo principal es abrirla.
                let editor = if self.main_running { Kind::Primary } else { Kind::Secondary };
                if icon_button(ui, Glyph::Film, "Open editor", editor, egui::vec2(w, 42.0)).clicked() {
                    Mode::Editor(None).launch();
                }
                self.update_button(ui);
            });
        });
    }

    pub(super) fn home_page(&mut self, ui: &mut egui::Ui) {
        page_title(ui, "Home", "Clippy is always recording in the background. Press your key and the last seconds become a clip.");
        // Lo primero que se busca: con qué tecla se guarda un clip. Va arriba y se cambia ahí mismo.
        self.clip_key_hero(ui);
        ui.add_space(12.0);
        self.editor_hero(ui);
        ui.add_space(22.0);
        self.summary_section(ui);
    }

    pub(super) fn recording_page(&mut self, ui: &mut egui::Ui) {
        page_title(ui, "Recording", "What gets recorded, how it looks and where clips are saved.");
        self.screen_section(ui);
        self.video_section(ui);
        self.storage_section(ui);
    }

    pub(super) fn audio_page(&mut self, ui: &mut egui::Ui) {
        page_title(ui, "Audio", "Which sounds go into your clips. Each one has its own volume.");
        self.audio_section(ui);
    }

    pub(super) fn keys_page(&mut self, ui: &mut egui::Ui) {
        page_title(ui, "Shortcuts", "Keys that work at any time, even inside a game.");
        self.global_keys_section(ui);
    }

    pub(super) fn general_page(&mut self, ui: &mut egui::Ui) {
        page_title(ui, "General", "Startup, sounds, updates and app data.");
        self.general_section(ui);
        self.about_section(ui);
    }

    /// Versión nueva: un botón más al pie de la barra lateral, del tamaño de los otros (como en
    /// Discord o Medal), no un cartel. La bandeja ya avisa con una notificación.
    fn update_button(&mut self, ui: &mut egui::Ui) {
        let Some((v, _)) = self.update.clone() else { return };
        let busy = self.update_busy();
        // El pie se arma de abajo hacia arriba: lo primero que se agrega queda abajo del botón.
        if let Some(Err(e)) = self.update_status.lock().unwrap().clone() {
            let msg = format!("Couldn't update: {e}");
            ui.add(egui::Label::new(RichText::new(msg).size(12.0).color(DANGER)).wrap());
        }
        let label = if busy { format!("Downloading {v}…") } else { format!("Update to {v}") };
        let cur = update::current();
        let tip = format!("You have {cur}.");
        let size = egui::vec2(ui.available_width(), 36.0);
        let r = ui.add_enabled_ui(!busy, |ui| icon_button(ui, Glyph::Download, &label, Kind::Secondary, size)).inner;
        // Primero se confirma, viendo qué trae la versión nueva.
        if r.on_hover_text(&tip).on_disabled_hover_text(&tip).clicked() {
            self.confirm_update = Some((v.clone(), update::notes(&v)));
        }
    }

    /// Tarjeta principal: la tecla para guardar clips, dibujada como una tecla que se toca para cambiar.
    fn clip_key_hero(&mut self, ui: &mut egui::Ui) {
        let a = GlobalAction::SaveClip;
        let (vk, mods) = self.cfg.hotkey(a);
        let waiting = self.capturing == Some(a);
        hero_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 22.0;
                let text = if waiting { "Press…".to_string() } else { hotkey_name(vk, mods) };
                if keycap(ui, &text, waiting).on_hover_text("Click to change the key").clicked() {
                    self.start_capture(a);
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    ui.add_space(4.0);
                    ui.label(RichText::new("Your key to save clips").family(bold()).size(20.0).color(TEXT));
                    let sub = if waiting {
                        format!("Press the new key. {CAPTURE_HINT}")
                    } else {
                        let (key, secs) = (hotkey_name(vk, mods), fmt_secs(self.cfg.seconds));
                        match self.recording_game() {
                            Some(g) => format!("Recording {g}: press {key} at any time and the last {secs} are saved."),
                            None => format!("Press {key} at any time, even in game, and the last {secs} are saved."),
                        }
                    };
                    ui.add(egui::Label::new(RichText::new(sub).color(MUTED)).wrap());
                    let note = match &self.global_note {
                        Some(n) if !waiting => n.clone(),
                        _ => "Click the key to change it.".to_string(),
                    };
                    ui.label(RichText::new(note).size(12.5).color(MUTED));
                });
            });
        });
    }

    /// El juego que se está grabando ahora (solo con la grabadora andando y sin pausa).
    fn recording_game(&self) -> Option<&str> {
        self.recorder.game.as_deref().filter(|_| self.main_running && self.cfg.enabled && self.recorder.error.is_none())
    }

    /// Tarjeta destacada para abrir el editor.
    fn editor_hero(&mut self, ui: &mut egui::Ui) {
        hero_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("Clip editor").family(bold()).size(17.0).color(TEXT));
                    ui.label(RichText::new("Trim, join and export your plays into a single video.").color(MUTED));
                    let info = match self.clips {
                        (0, _) => "No saved clips yet.".to_string(),
                        (n, last) => {
                            let (count, when) = (fmt_count(n), last.map(ago).unwrap_or_default());
                            match n {
                                1 => format!("{count} saved clip · the last one {when}"),
                                _ => format!("{count} saved clips · the last one {when}"),
                            }
                        }
                    };
                    ui.label(RichText::new(info).size(12.5).color(MUTED));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icon_button(ui, Glyph::Film, "Open editor", Kind::Secondary, egui::vec2(150.0, 40.0)).clicked() {
                        Mode::Editor(None).launch();
                    }
                });
            });
        });
    }

    /// Cómo se graba ahora, de un vistazo, con un acceso a cada página.
    fn summary_section(&mut self, ui: &mut egui::Ui) {
        let res = match self.cfg.resolution {
            0 => "native resolution".to_string(),
            r => format!("{r}p"),
        };
        let monitor = self.monitor_label();
        let video = format!("{res} · {} fps · {} · {monitor}", self.cfg.fps, self.cfg.codec.label());
        let sounds: Vec<&str> = [(self.cfg.game_audio, "game"), (self.cfg.desktop_audio, "desktop"), (self.cfg.mic_audio, "microphone")]
            .into_iter()
            .filter_map(|(on, n)| on.then_some(n))
            .collect();
        let audio = if sounds.is_empty() { "No sound".to_string() } else { format!("Records {}", sounds.join(", ")) };
        section(ui, "Right now", |ui| {
            for (i, (title, value, page)) in
                [("Video", video, Page::Recording), ("Audio", audio, Page::Audio)].into_iter().enumerate()
            {
                if i > 0 {
                    divider(ui);
                }
                setting(ui, title, value, |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        if button(ui, "Change", Kind::Secondary).clicked() {
                            self.page = page;
                        }
                    });
                });
            }
        });
    }

    /// Qué monitor se graba, en pocas palabras.
    fn monitor_label(&self) -> String {
        if self.cfg.monitor.is_empty() {
            return "game's monitor".into();
        }
        match self.monitors.iter().find(|(n, _)| *n == self.cfg.monitor) {
            Some((n, _)) => format!("Monitor {}", n.trim_start_matches(|c: char| !c.is_ascii_digit())),
            None => "monitor disconnected".into(),
        }
    }

    pub(super) fn about_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "About", |ui| {
            let about = match &self.check_note {
                Some(note) => note.clone(),
                None => "Clippy also checks by itself once a day and lets you know from the tray.".to_string(),
            };
            setting(ui, concat!("Clippy ", env!("CARGO_PKG_VERSION")), about, |ui| {
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| match self.update.clone() {
                    Some((v, _)) => {
                        let busy = self.update_busy();
                        let label = if busy { "Updating…".to_string() } else { format!("Update to {v}") };
                        if ui.add_enabled_ui(!busy, |ui| button(ui, &label, Kind::Secondary)).inner.clicked() {
                            self.start_update();
                        }
                    }
                    None => {
                        let label = if self.checking { "Checking…" } else { "Check for updates" };
                        // Ancho fijo: al pasar a "Checking…" no se achica ni se corre.
                        let size = egui::vec2(190.0, 0.0);
                        if ui.add_enabled_ui(!self.checking, |ui| button_sized(ui, label, Kind::Secondary, size)).inner.clicked() {
                            self.start_check();
                        }
                    }
                });
            });
            self.finish_check(ui.ctx());
            divider(ui);
            let dir = app_dir().display().to_string();
            setting(ui, "Data and log", format!("Settings and clippy.log, useful if something goes wrong: {dir}"), |ui| {
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    if icon_button(ui, Glyph::Folder, "Open folder", Kind::Secondary, egui::Vec2::ZERO).clicked() {
                        open_folder(&app_dir());
                    }
                });
            });
        });
    }

    /// Qué se graba: toda la pantalla o solo el juego, qué monitor y el cursor.
    fn screen_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Screen", |ui| {
            let about = if self.cfg.capture_mode == CaptureMode::GamesOnly {
                "Records only the fullscreen game. If you minimize it or cover it with another window, that part of the clip goes black and your desktop isn't shown."
            } else {
                "Records everything on the monitor: games, desktop and other windows."
            };
            setting(ui, "What's recorded", about, |ui| {
                let opts = [(CaptureMode::Screen, "Whole screen"), (CaptureMode::GamesOnly, "Games only")];
                seg(ui, &mut self.cfg.capture_mode, &opts);
            });
            divider(ui);
            let about = if self.cfg.monitor.is_empty() {
                "Automatic records the monitor your game is on, and with no game, the one with the window you're using. Pick one to always record that monitor."
            } else {
                "Always records this monitor, even if the game is on another one. Changing it starts the recording again."
            };
            setting(ui, "Monitor", about, |ui| self.monitor_picker(ui));
            // "Solo juegos" graba el juego, y en un juego el cursor no va: ahí no aplica.
            if self.cfg.capture_mode == CaptureMode::Screen {
                divider(ui);
                setting(ui, "Record the cursor", "Draws the mouse pointer in the clip. Only when there's no game on screen: games draw their own.", |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        toggle(ui, &mut self.cfg.cursor);
                    });
                });
            }
        });
    }

    fn monitor_picker(&mut self, ui: &mut egui::Ui) {
        const AUTO: &str = "Automatic (game's monitor)";
        let current = if self.cfg.monitor.is_empty() {
            AUTO.to_string()
        } else {
            self.monitors
                .iter()
                .find(|(n, _)| *n == self.cfg.monitor)
                .map(|(_, l)| l.clone())
                .unwrap_or_else(|| "Disconnected monitor (records automatically)".into())
        };
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button(ui, "Refresh", Kind::Secondary).on_hover_text("Look for connected monitors").clicked() {
                    self.monitors = monitors();
                }
                let w = ui.available_width() - 8.0;
                egui::ComboBox::from_id_salt("monitor")
                    .width(w)
                    .truncate()
                    .selected_text(current.clone())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.cfg.monitor, String::new(), AUTO);
                        for (name, label) in self.monitors.clone() {
                            ui.selectable_value(&mut self.cfg.monitor, name, label);
                        }
                    })
                    .response
                    .on_hover_text(current);
            });
        });
    }

    /// Con qué se codifica, mientras graba. Si es el procesador, por qué y qué cambia.
    fn encoder_row(&self, ui: &mut egui::Ui) {
        let Some(d) = self.recorder.device.as_ref().filter(|_| self.main_running && self.cfg.enabled) else { return };
        let (label, about) = match d {
            Device::Gpu(Some(v)) => (format!("Graphics card ({v})"), "Your graphics card compresses the video, so recording barely affects your game.".to_string()),
            Device::Gpu(None) => ("Graphics card".to_string(), "Your graphics card compresses the video, so recording barely affects your game.".to_string()),
            Device::Cpu => {
                let (p, fps) = (CPU_MAX_SHORT, CPU_MAX_FPS);
                (
                    "Processor".to_string(),
                    format!("Your graphics card has no video encoder or it couldn't be used, so Clippy uses the processor and records up to {p}p at {fps} frames per second so it doesn't slow down your game."),
                )
            }
        };
        divider(ui);
        setting(ui, "Encoding with", about, |ui| {
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                badge(ui, &label, TEXT);
            });
        });
    }

    fn video_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Video", |ui| {
            let max = self.cfg.max_seconds();
            let key = hotkey_name(self.cfg.hotkey_vk, self.cfg.hotkey_mods);
            let mut about = format!("How much is saved when you press {key}: the last {}.", fmt_secs(self.cfg.seconds));
            if max < MAX_SECONDS {
                about += &format!(" With this quality and your PC's RAM, it goes up to {}.", fmt_secs(max));
            }
            setting(ui, "Clip length", about, |ui| {
                slider_width(ui, 70.0);
                ui.add(egui::Slider::new(&mut self.cfg.seconds, MIN_SECONDS..=max).step_by(5.0).custom_formatter(|v, _| fmt_secs(v as u32)));
            });
            divider(ui);
            let custom = self.custom_quality || !QUALITY.contains(&self.cfg.bitrate_mbps);
            let about = if custom {
                "More Mbps, better image and bigger clips. At 1080p and 60 fps, 40 to 60 already looks great; at 1440p or 4K, go for 80 or more."
            } else {
                "How sharp the video looks. Higher quality makes bigger clips and lets fewer seconds fit in RAM."
            };
            setting(ui, "Quality", about, |ui| {
                // 0 = "Personalizada": se elige el bitrate a mano.
                let mut q = if custom { 0 } else { self.cfg.bitrate_mbps };
                let opts = [(QUALITY[0], "Low"), (QUALITY[1], "Medium"), (QUALITY[2], "High"), (0, "Custom")];
                if seg(ui, &mut q, &opts) {
                    self.custom_quality = q == 0;
                    if q != 0 {
                        self.cfg.bitrate_mbps = q;
                    }
                }
                if custom {
                    slider_width(ui, 90.0);
                    ui.add(egui::Slider::new(&mut self.cfg.bitrate_mbps, MIN_MBPS..=MAX_MBPS).suffix(" Mbps"));
                }
            });
            // Más calidad, menos segundos entran en la RAM: se recorta en el mismo cuadro.
            self.cfg.seconds = self.cfg.seconds.min(self.cfg.max_seconds());
            divider(ui);
            setting(
                ui,
                "Resolution",
                "Keeps your monitor's shape: if you play stretched, the clip comes out stretched just as you see it. Lower resolution, lighter clips.",
                |ui| {
                    let labels = RESOLUTIONS.map(|r| if r == 0 { "Native".to_string() } else { format!("{r}p") });
                    let opts: Vec<(u32, &str)> = RESOLUTIONS.iter().zip(&labels).map(|(&r, l)| (r, l.as_str())).collect();
                    seg(ui, &mut self.cfg.resolution, &opts);
                },
            );
            divider(ui);
            setting(
                ui,
                "Frames per second",
                "60 looks smooth in most games. 120 is smoother in fast games but makes bigger clips; 30 makes the lightest ones.",
                |ui| {
                    seg(ui, &mut self.cfg.fps, &[(30, "30"), (60, "60"), (120, "120")]);
                },
            );
            divider(ui);
            let about = match self.cfg.codec {
                Codec::H264 => "MP4 video (H.264): plays on any PC, phone, Discord or WhatsApp.",
                Codec::Hevc => "MP4 video (H.265): the same image in clips a third lighter. Some PCs need “HEVC Video Extensions” (Microsoft Store) to play it, and Discord or WhatsApp may not play it. If your graphics card doesn't support it, it records in H.264.",
            };
            setting(ui, "Format", about, |ui| {
                let opts = [(Codec::H264, Codec::H264.label()), (Codec::Hevc, Codec::Hevc.label())];
                seg(ui, &mut self.cfg.codec, &opts);
            });
            self.encoder_row(ui);
        });
    }

    /// Dónde se guardan los clips y cuánta memoria ocupa grabar.
    fn storage_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Storage", |ui| {
            let path = self.cfg.output_dir.display().to_string();
            setting(ui, "Clips folder", format!("Where clips are saved: {path}"), |ui| {
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    if button(ui, "Change", Kind::Secondary).clicked() {
                        if let Some(p) = rfd::FileDialog::new().set_title("Folder for clips").set_directory(&self.cfg.output_dir).pick_folder() {
                            self.cfg.output_dir = p;
                        }
                    }
                    if icon_button(ui, Glyph::Folder, "Open", Kind::Ghost, egui::Vec2::ZERO).clicked() {
                        open_folder(&self.cfg.output_dir);
                    }
                });
            });
            divider(ui);
            setting(
                ui,
                "Memory",
                "The last seconds are kept in RAM, never on disk, until you save. Approximate, as Task Manager shows it: it goes up a bit with more seconds, higher quality or lots of motion.",
                |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        badge(ui, &format!("About {}", fmt_bytes(self.cfg.typical_ram_mb() as f64 * 1e6)), TEXT);
                    });
                },
            );
        });
    }

    pub(super) fn global_keys_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Global shortcuts", |ui| {
            for a in GlobalAction::ALL {
                // Guardar clip siempre tiene tecla; los demás se pueden quitar.
                let optional = a != GlobalAction::SaveClip;
                if optional {
                    divider(ui);
                }
                setting(ui, a.label(), a.about(), |ui| {
                    let (vk, mods) = self.cfg.hotkey(a);
                    let capturing = self.capturing == Some(a);
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        if optional && vk != 0 && !capturing && button(ui, "Remove", Kind::Ghost).clicked() {
                            let (v, m) = self.cfg.hotkey_mut(a);
                            *v = 0;
                            *m = 0;
                        }
                        let text = if capturing {
                            "Press a key…".to_string()
                        } else if vk == 0 {
                            "Not set".to_string()
                        } else {
                            hotkey_name(vk, mods)
                        };
                        let kind = if capturing { Kind::Selected } else { Kind::Secondary };
                        let w = ui.available_width();
                        if button_sized(ui, &text, kind, egui::vec2(w, 34.0)).on_hover_text("Click, then press the new key").clicked() {
                            self.start_capture(a);
                        }
                    });
                });
            }
        });
        let note = if self.capturing.is_some() {
            CAPTURE_HINT.to_string()
        } else if let Some(n) = &self.global_note {
            n.clone()
        } else {
            "Editor shortcuts are changed from the editor, in “Editor settings” (the gear).".to_string()
        };
        ui.label(RichText::new(note).size(12.5).color(MUTED));
    }

    fn audio_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Sources", |ui| {
            let about = if self.cfg.game_audio {
                "The sound of the game you have in fullscreen, detected automatically. While there isn't one, everything goes to Desktop."
            } else {
                "Muted: the game won't be heard in clips."
            };
            volume_setting(ui, "Game", about, &mut self.cfg.game_audio, &mut self.cfg.game_volume, 100);
            divider(ui);
            let about = if self.cfg.desktop_audio {
                "Everything else you hear: Discord, music, the browser."
            } else {
                "Muted: Discord, music and other apps won't be heard in clips."
            };
            volume_setting(ui, "Desktop", about, &mut self.cfg.desktop_audio, &mut self.cfg.desktop_volume, 100);
            divider(ui);
            let about = if self.cfg.mic_audio {
                "Your voice is mixed into the clip. You can also turn it on or off from the tray icon."
            } else {
                "Off: your voice isn't in clips."
            };
            volume_setting(ui, "Microphone", about, &mut self.cfg.mic_audio, &mut self.cfg.mic_volume, 200);
            if self.cfg.mic_audio {
                divider(ui);
                setting(ui, "Microphone device", "Which microphone records your voice. Windows default follows the one set in Windows sound settings.", |ui| {
                    self.mic_picker(ui);
                });
            }
        });
    }

    fn mic_picker(&mut self, ui: &mut egui::Ui) {
        let default = "Windows default";
        let current = if self.cfg.mic_device.is_empty() {
            default.to_string()
        } else {
            self.mics
                .iter()
                .find(|(id, _)| *id == self.cfg.mic_device)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| "Microphone disconnected".into())
        };
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button(ui, "Refresh", Kind::Secondary).on_hover_text("Look for connected microphones").clicked() {
                    self.mics = audio::capture_devices();
                }
                let w = ui.available_width() - 8.0;
                egui::ComboBox::from_id_salt("mic")
                    .width(w)
                    .truncate()
                    .selected_text(current.clone())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.cfg.mic_device, String::new(), default);
                        for (id, name) in self.mics.clone() {
                            ui.selectable_value(&mut self.cfg.mic_device, id, name);
                        }
                    })
                    .response
                    .on_hover_text(current);
            });
        });
    }

    pub(super) fn general_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Preferences", |ui| {
            setting(ui, "Start with Windows", "Opens Clippy in the tray when you turn on your PC, so it's always ready to save a clip.", |ui| {
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    let mut a = self.autostart;
                    if toggle(ui, &mut a).changed() {
                        autostart::set(a);
                        self.autostart = autostart::enabled();
                    }
                });
            });
            divider(ui);
            setting(
                ui,
                "Sound on save",
                "Plays a short sound when a clip is saved, so you know it worked without leaving the game. It isn't heard in the clip.",
                |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        toggle(ui, &mut self.cfg.sound_on_save);
                        if button(ui, "Test sound", Kind::Ghost).clicked() {
                            sound::play_saved();
                        }
                    });
                },
            );
            divider(ui);
            setting(
                ui,
                "Notification on save",
                "Shows a Windows notification with each saved clip: click it to open the editor. While you play in full screen, Windows may keep it in the notification center.",
                |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        toggle(ui, &mut self.cfg.notify_on_save);
                    });
                },
            );
        });
    }
}

/// Ajuste de audio: interruptor y volumen (hasta `max` %).
fn volume_setting(ui: &mut egui::Ui, title: &str, about: &str, on: &mut bool, vol: &mut u32, max: u32) {
    setting(ui, title, about, |ui| {
        ui.horizontal(|ui| {
            toggle(ui, on).on_hover_text(if *on { "Mute" } else { "Unmute" });
            ui.add_space(8.0);
            ui.add_enabled_ui(*on, |ui| {
                slider_width(ui, 70.0);
                ui.add(egui::Slider::new(vol, 0..=max).suffix(" %"));
            });
        });
    });
}

fn ago(t: SystemTime) -> String {
    let s = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    // La unidad en singular y plural.
    let ago = |n: u64, [one, many]: [&str; 2]| {
        let unit = if n == 1 { one } else { many };
        format!("{n} {unit} ago")
    };
    match s {
        0..=59 => "a moment ago".into(),
        60..=3599 => ago(s / 60, ["min", "min"]),
        3600..=86_399 => ago(s / 3600, ["h", "h"]),
        86_400..=172_799 => "yesterday".into(),
        172_800..=604_799 => ago(s / 86_400, ["day", "days"]),
        604_800..=2_591_999 => ago(s / 604_800, ["week", "weeks"]),
        2_592_000..=31_535_999 => ago(s / 2_592_000, ["month", "months"]),
        _ => ago(s / 31_536_000, ["year", "years"]),
    }
}

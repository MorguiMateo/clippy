//! Preferencias propias del editor (%APPDATA%\Clippy\editor.json): atajos de teclado, últimas
//! opciones de exportación, tamaño de los paneles, el imán y los favoritos y el orden de la biblioteca. Solo el
//! editor las lee y las escribe.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::export::Quality;
use super::library::Sort;
use super::shortcuts::Keys;
use crate::log::app_dir;

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(default)]
pub(super) struct Prefs {
    /// Atajos que el usuario cambió (id de acción -> combinación).
    #[serde(alias = "editor_keys")]
    pub(super) keys: Keys,
    /// Últimas opciones de exportación (0 = como el original).
    pub(super) export_height: u32,
    pub(super) export_fps: u32,
    pub(super) export_quality: Quality,
    pub(super) export_size_mb: u32,
    /// Tamaño de los paneles (puntos), como los dejó el usuario arrastrando sus bordes.
    pub(super) library_w: f32,
    pub(super) props_w: f32,
    pub(super) timeline_h: f32,
    /// Parte del alto de las pistas que usan las de video (el resto es de las de audio).
    pub(super) lane_split: f32,
    /// Imán: las piezas de cada pista quedan pegadas una detrás de la otra.
    pub(super) magnet: bool,
    /// Videos marcados con la estrella en la biblioteca (siguen al archivo si se mueve desde el editor).
    pub(super) favorites: HashSet<PathBuf>,
    /// En qué orden se ven los videos de la biblioteca.
    pub(super) library_sort: Sort,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            keys: Keys::new(),
            export_height: 0,
            export_fps: 0,
            export_quality: Quality::Recommended,
            export_size_mb: 25,
            library_w: 320.0,
            props_w: 290.0,
            timeline_h: 268.0,
            lane_split: 0.6,
            magnet: true,
            favorites: HashSet::new(),
            library_sort: Sort::Newest,
        }
    }
}

fn path() -> PathBuf {
    app_dir().join("editor.json")
}

fn read(p: &Path) -> Option<Prefs> {
    let s = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(s.trim_start_matches('\u{feff}')).ok()
}

impl Prefs {
    pub(super) fn load() -> Prefs {
        if let Some(p) = read(&path()) {
            return p;
        }
        // Antes (hasta 0.2.0) se guardaban en config.json, con los mismos nombres salvo `editor_keys`.
        let p = read(&crate::config::config_path()).unwrap_or_default();
        p.save();
        p
    }

    pub(super) fn save(&self) {
        let _ = std::fs::create_dir_all(app_dir());
        let tmp = app_dir().join("editor.json.tmp");
        if std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, path());
        }
    }
}

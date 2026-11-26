//! Carpeta de datos de la app (%APPDATA%\Clippy), registro (clippy.log) y marcas de tiempo.

use std::io::Write;
use std::path::PathBuf;

use windows::Win32::System::SystemInformation::GetLocalTime;

pub fn app_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Clippy")
}

/// Con milisegundos: así se mide cuánto tarda cada paso (atajo -> grabadora -> clip guardado).
fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

pub fn file_stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

pub fn log(msg: &str) {
    let dir = app_dir();
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join("clippy.log");
    if let Ok(m) = std::fs::metadata(&p) {
        if m.len() > 512 * 1024 {
            let _ = std::fs::remove_file(&p);
        }
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        Ok(mut f) => {
            let _ = writeln!(f, "[{}] {}", timestamp(), msg);
        }
        // Si otro programa tiene el registro tomado (antivirus, indexador), la línea no se pierde del todo.
        Err(_) => eprintln!("[{}] {msg}", timestamp()),
    }
    #[cfg(debug_assertions)]
    eprintln!("{msg}");
}

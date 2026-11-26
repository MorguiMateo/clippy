//! Qué es "el juego": el proceso de la ventana activa que ocupa todo su monitor
//! (pantalla completa o ventana sin bordes), salvo programas que no son juegos: los de la carpeta de Windows
//! (una consola o el explorador a pantalla completa) y los de `NOT_GAMES`.
//! Lo usan la elección del monitor a grabar, el modo "Solo juegos" (imagen del clip), la carpeta del clip y el
//! canal de audio "Juego". Una vez detectado queda fijo mientras siga abierto, aunque hagas Alt+Tab.

use windows::core::{w, HSTRING, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindow, IsZoomed,
};

/// Programas fuera de la carpeta de Windows que pueden estar en pantalla completa sin ser juegos (en minúsculas,
/// sin ".exe"). Los de adentro (explorador, consolas, menú Inicio, pantalla de bloqueo) no hace falta listarlos.
const NOT_GAMES: &[&str] = &[
    "chrome", "msedge", "firefox", "opera", "brave", "vivaldi",
    "vlc", "mpc-hc", "mpc-hc64", "mpc-be", "mpc-be64", "mpv", "potplayer", "potplayer64", "potplayermini", "potplayermini64",
    "microsoft.media.player", "video.ui", "wmplayer", "powerpnt", "windowsterminal",
    // Herramientas de captura de pantalla: tapan todo el monitor mientras elegís qué recortar.
    "snippingtool", "lightshot", "sharex", "greenshot", "snagitcapture", "snagit32",
    "flameshot", "snipaste", "gyazo", "gyazowin", "picpick", "screenpresso",
    "clippy",
];

const FS_EVERY: i64 = 2_500_000; // 250 ms
const ALIVE_EVERY: i64 = 30_000_000; // 3 s
const STILL_ACTIVE: u32 = 259;

struct Proc {
    pid: u32,
    /// Nombre del juego para mostrar y para su carpeta de clips (ver `display_name`).
    name: String,
    handle: HANDLE,
    /// Última ventana del juego que estuvo adelante en pantalla completa (dice en qué monitor está).
    window: HWND,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[derive(Default)]
pub struct Detector {
    game: Option<Proc>,
    /// El juego está adelante en pantalla completa ahora mismo.
    in_front: bool,
    /// El juego se ve en su monitor (ver `visible`).
    visible: bool,
    /// Último proceso en pantalla completa que resultó no ser juego (para no consultarlo de nuevo).
    not_game: u32,
    next_fs: i64,
    next_alive: i64,
}

impl Detector {
    /// Revisa la ventana activa (cada 250 ms) y si el juego sigue abierto (cada 3 s).
    /// Devuelve true si cambió el juego.
    pub fn poll(&mut self, now: i64) -> bool {
        let mut changed = false;
        let checked = now >= self.next_alive || now >= self.next_fs;
        if now >= self.next_alive {
            self.next_alive = now + ALIVE_EVERY;
            if self.game.as_ref().is_some_and(|g| !alive(g)) {
                self.game = None;
                changed = true;
            }
        }
        if now >= self.next_fs {
            self.next_fs = now + FS_EVERY;
            self.in_front = false;
            if let Some((pid, window)) = fullscreen_window() {
                if let Some(g) = self.game.as_mut().filter(|g| g.pid == pid) {
                    g.window = window;
                    self.in_front = true;
                } else if pid != self.not_game {
                    match open_game(pid, window) {
                        Some(p) => {
                            self.game = Some(p);
                            self.in_front = true;
                            changed = true;
                        }
                        None => self.not_game = pid,
                    }
                }
            }
        }
        if checked {
            self.visible = self.sees();
        }
        changed
    }

    pub fn pid(&self) -> Option<u32> {
        self.game.as_ref().map(|g| g.pid)
    }

    /// Nombre del juego (ej. "VALORANT").
    pub fn name(&self) -> Option<String> {
        self.game.as_ref().map(|g| g.name.clone())
    }

    /// Monitor del juego, si está abierto y no minimizado.
    pub fn monitor(&self) -> Option<HMONITOR> {
        let w = self.game.as_ref()?.window;
        unsafe {
            if !IsWindow(w).as_bool() || IsIconic(w).as_bool() {
                return None;
            }
            let m = MonitorFromWindow(w, MONITOR_DEFAULTTONULL);
            (!m.is_invalid()).then_some(m)
        }
    }

    /// El juego está adelante en pantalla completa ahora mismo (ahí manda imágenes todo el tiempo).
    pub fn in_front(&self) -> bool {
        self.in_front
    }

    /// El juego se ve en su monitor: está adelante, o la ventana activa está en otro monitor
    /// (con dos o más pantallas, mirar Discord en la otra no tapa el juego). Se averigua con cada revisión de
    /// `poll` (cada 250 ms), no en cada cuadro.
    pub fn visible(&self) -> bool {
        self.visible
    }

    fn sees(&self) -> bool {
        let Some(m) = self.monitor() else { return false };
        self.in_front || foreground().is_none_or(|h| unsafe { MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST) } != m)
    }
}

/// Monitor de la ventana activa (el escritorio y la barra de tareas no cuentan: el escritorio abarca todos).
pub fn active_monitor() -> Option<HMONITOR> {
    let m = unsafe { MonitorFromWindow(foreground()?, MONITOR_DEFAULTTONULL) };
    (!m.is_invalid()).then_some(m)
}

/// Ventana activa, salvo el escritorio y las barras de tareas.
fn foreground() -> Option<HWND> {
    unsafe {
        let h = GetForegroundWindow();
        if h.is_invalid() {
            return None;
        }
        let mut cls = [0u16; 64];
        let n = GetClassNameW(h, &mut cls).max(0) as usize;
        let cls = String::from_utf16_lossy(&cls[..n]);
        (!matches!(cls.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")).then_some(h)
    }
}

fn is_game(exe: &str) -> bool {
    !exe.is_empty() && !NOT_GAMES.iter().any(|n| exe.eq_ignore_ascii_case(n))
}

/// El proceso `pid`, si es un juego.
fn open_game(pid: u32, window: HWND) -> Option<Proc> {
    if pid == 0 || pid == std::process::id() {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).unwrap_or_default();
        // Si no es un juego, al soltarlo se cierra el proceso abierto.
        let mut p = Proc { pid, name: String::new(), handle, window };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let path = if !handle.is_invalid() && QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            // Algunos anticheats no dejan abrir el proceso: el nombre sale de la lista de procesos.
            exe_name(pid)?
        };
        let exe = exe_stem(&path);
        if !is_game(&exe) || in_windows_dir(&path) {
            return None;
        }
        p.name = display_name(&path).unwrap_or(exe);
        Some(p)
    }
}

/// "C:\\...\\VALORANT-Win64-Shipping.exe" -> "VALORANT-Win64-Shipping".
fn exe_stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or("");
    match file.len().checked_sub(4) {
        Some(n) if file.is_char_boundary(n) && file[n..].eq_ignore_ascii_case(".exe") => file[..n].to_string(),
        _ => file.to_string(),
    }
}

/// El exe está adentro de la carpeta de Windows (System32, SysWOW64, SystemApps, WinSxS…): es parte del sistema,
/// no un juego. La carpeta se le pregunta a Windows; `path` viene de `QueryFullProcessImageNameW`, con la ruta
/// completa. Si el nombre salió de la lista de procesos no hay ruta y no se puede saber.
fn in_windows_dir(path: &str) -> bool {
    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemWindowsDirectoryW(Some(&mut buf)) } as usize;
    if n == 0 || n > buf.len() {
        return false;
    }
    let dir = String::from_utf16_lossy(&buf[..n]).trim_end_matches('\\').to_lowercase();
    path.to_lowercase().strip_prefix(&dir).is_some_and(|rest| rest.starts_with('\\'))
}

/// El nombre que el juego publica en su exe, sin una lista para cada uno: el ProductName (ej. "VALORANT") o, si no
/// tiene o es el de Windows ("Microsoft® Windows® Operating System", que cambia según el idioma pero siempre dice
/// "Windows®"), la FileDescription. None si no trae ninguno.
fn display_name(path: &str) -> Option<String> {
    let info = VersionInfo::read(path)?;
    info.get("ProductName").filter(|n| !n.contains("Windows®") && !n.contains("Windows(R)")).or_else(|| info.get("FileDescription"))
}

/// Recurso de versión de un exe (lo que muestra Propiedades > Detalles).
struct VersionInfo {
    data: Vec<u64>,
    /// (idioma, página de códigos) de cada tabla de textos.
    translations: Vec<[u16; 2]>,
}

impl VersionInfo {
    fn read(path: &str) -> Option<Self> {
        if !std::path::Path::new(path).is_absolute() {
            return None;
        }
        unsafe {
            let path = HSTRING::from(path);
            let size = GetFileVersionInfoSizeW(&path, None);
            if size == 0 {
                return None;
            }
            let mut data = vec![0u64; (size as usize).div_ceil(8)];
            GetFileVersionInfoW(&path, 0, size, data.as_mut_ptr().cast()).ok()?;
            let mut value = std::ptr::null_mut();
            let mut len = 0;
            if !VerQueryValueW(data.as_ptr().cast(), w!("\\VarFileInfo\\Translation"), &mut value, &mut len).as_bool() || value.is_null() {
                return None;
            }
            let translations = std::slice::from_raw_parts(value.cast::<u16>(), len as usize / 2).as_chunks::<2>().0.to_vec();
            Some(Self { data, translations })
        }
    }

    /// El texto `key` (ej. "ProductName") de la primera tabla que lo tenga, sin espacios de más. None si está vacío.
    fn get(&self, key: &str) -> Option<String> {
        self.translations.iter().find_map(|t| unsafe {
            let key = HSTRING::from(format!("\\StringFileInfo\\{:04x}{:04x}\\{key}", t[0], t[1]));
            let mut value = std::ptr::null_mut();
            let mut len = 0;
            if !VerQueryValueW(self.data.as_ptr().cast(), &key, &mut value, &mut len).as_bool() || value.is_null() || len <= 1 {
                return None;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(value.cast::<u16>(), len as usize));
            let text = text.trim_matches('\0').trim();
            (!text.is_empty()).then(|| text.to_string())
        })
    }
}

fn alive(p: &Proc) -> bool {
    if p.handle.is_invalid() {
        // Mientras su ventana exista, el proceso sigue: no hace falta recorrer la lista de procesos.
        let mut owner = 0u32;
        if unsafe { IsWindow(p.window).as_bool() && GetWindowThreadProcessId(p.window, Some(&mut owner)) != 0 } && owner == p.pid {
            return true;
        }
        return exe_name(p.pid).is_some();
    }
    let mut code = 0u32;
    unsafe { GetExitCodeProcess(p.handle, &mut code).is_ok() && code == STILL_ACTIVE }
}

/// Nombre del ejecutable ("juego.exe") del proceso `pid`, buscándolo en la lista de procesos. None si ya no está.
fn exe_name(pid: u32) -> Option<String> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = None;
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                if e.th32ProcessID == pid {
                    let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                    found = Some(String::from_utf16_lossy(&e.szExeFile[..n]));
                    break;
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// (PID, ventana) de la ventana activa si ocupa todo su monitor. El escritorio, las ventanas normales y las
/// maximizadas no cuentan: en un monitor sin barra de tareas una ventana maximizada también lo tapa entero,
/// y apps sin barra de título estándar (Discord, Spotify) parecerían juegos. Un juego sin bordes no se
/// maximiza: se agranda al tamaño del monitor.
fn fullscreen_window() -> Option<(u32, HWND)> {
    unsafe {
        let h = foreground()?;
        if IsZoomed(h).as_bool() {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(h, &mut r).ok()?;
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            return None;
        }
        let m = mi.rcMonitor;
        if !(r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom) {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        (pid != 0).then_some((pid, h))
    }
}

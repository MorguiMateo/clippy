//! Captura de un monitor con DXGI Desktop Duplication. Cuál monitor lo decide `recorder`
//! (el del juego o, si no hay juego, el de la ventana activa) y se puede cambiar sin cortar el buffer.
//! La imagen nunca sale de la GPU: se copia a una textura propia y listo.
//! No se inyecta nada en el juego (compatible con anticheats como Vanguard).

use anyhow::{anyhow, Result};
use windows::core::Interface;
use std::collections::VecDeque;

use windows::Win32::Foundation::{HMODULE, LUID, POINT, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTOPRIMARY};

use super::cursor::Cursor;
use super::screen::{self, wide};
use crate::log::log;

/// Una duplicación que no se pudo abrir se reintenta enseguida durante `QUICK_FOR` (un juego que cambia el modo
/// de video al entrar en pantalla completa la corta un segundo) y después cada 5 s (100 ns).
const QUICK_RETRY: i64 = 2_500_000;
const QUICK_FOR: i64 = 30_000_000;
const RETRY: i64 = 50_000_000;

/// Un juego adelante manda imágenes todo el tiempo y cada una cambia el monitor entero. Si en este lapso llegan menos de
/// `GAME_MIN` así, no se está viendo el juego: la captura quedó trabada, o el juego no se deja capturar y solo llegan las
/// del escritorio de atrás (100 ns).
const STALL: i64 = 50_000_000;
/// 5 por segundo: muy por debajo de un juego en el menú con los fps limitados y muy por encima de lo que el escritorio
/// se repinta entero (casi nunca: cambian el reloj o una ventana).
const GAME_MIN: usize = 25;
/// Qué parte del monitor tiene que cambiar en una imagen para que sea del juego (en décimos).
const GAME_COVER: u64 = 9;
/// Imágenes del juego que tiene que haber recibido una sesión para darla por andando (2 s a 60 fps).
const WORKED: u32 = 120;

/// Qué hizo `Capture::aim`.
pub enum Aim {
    /// Sigue igual (ya era ese monitor, o no se puede cambiar sin reiniciar y no hacía falta).
    Same,
    Switched,
    /// Hay que reiniciar la captura: el monitor está en otra GPU o el video tiene que tomar su resolución.
    Restart,
    /// Se perdió la placa al abrir ese monitor: hay que volver a empezar grabando este (el de antes).
    Lost(HMONITOR),
}

/// Se perdió la placa de video (reinicio del driver, cuelgue, placa desconectada): ese dispositivo no vuelve y la
/// sesión empieza de nuevo con otro. El motivo ya quedó en el log; no llega a la pantalla.
#[derive(Debug)]
pub struct DeviceLost;

impl std::fmt::Display for DeviceLost {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("se perdió la placa de video")
    }
}

impl std::error::Error for DeviceLost {}

/// Última imagen del monitor (BGRA, sin rotar), en la GPU.
pub struct Frame {
    pub tex: ID3D11Texture2D,
    pub rotation: DXGI_MODE_ROTATION,
    /// La forma con la que se ve en el monitor (ya girada): la del panel si Windows la estira
    /// (resolución estirada), si no la de la imagen.
    pub shape: (u32, u32),
}

/// Un monitor visto desde una GPU.
struct Output {
    monitor: HMONITOR,
    output: IDXGIOutput1,
    name: String,
    /// Tamaño del video para este monitor (ver `video_size`).
    width: u32,
    height: u32,
}

pub struct Capture {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub luid: LUID,
    /// Tamaño del video, fijo durante la sesión (antes de achicarlo según los ajustes): otro monitor
    /// se escala adentro sin deformarse.
    pub width: u32,
    pub height: u32,
    /// Es una textura nueva cuando cambia el tamaño o la rotación de la imagen.
    pub latest: Option<Frame>,
    /// Sube cada vez que cambia lo que hay en `latest` (imagen nueva o cursor): si no subió, convertirla de nuevo daría
    /// lo mismo.
    pub version: u64,
    adapter: IDXGIAdapter1,
    gpu: String,
    out: Output,
    dup: Option<IDXGIOutputDuplication>,
    rotation: DXGI_MODE_ROTATION,
    /// El modo de video del monitor al abrir la duplicación (un juego en pantalla completa exclusiva pone el suyo).
    mode: (u32, u32),
    /// La forma del panel si Windows estira la imagen hasta llenarlo (None si la encaja con bandas o no se sabe).
    stretch: Option<(u32, u32)>,
    retry_at: i64,
    /// Desde cuándo no abre la duplicación.
    failing_since: Option<i64>,
    /// Ya se avisó en el log que no abre (para no repetirlo cada 5 s).
    failing: bool,
    /// Último monitor que no se pudo seguir sin reiniciar (para avisarlo una sola vez).
    skipped: Option<HMONITOR>,
    /// Monitores que hicieron perder la placa al pasar a ellos: no se vuelven a probar.
    lost_on: Vec<HMONITOR>,
    /// Imágenes del juego (ver `GAME_COVER`) recibidas en la sesión (hasta `WORKED`), cuándo llegaron las últimas
    /// `GAME_MIN` y cuántas veces se perdió la duplicación desde la última imagen nueva.
    game_frames: u32,
    game_at: VecDeque<i64>,
    dup_lost: u32,
    /// Para leer qué partes cambiaron en cada imagen (`GetFrameDirtyRects`) sin pedir memoria cada vez.
    dirty: Vec<RECT>,
    #[cfg(feature = "devtools")]
    stats: Stats,
    /// Se pidió el cursor (`set_cursor`) y con qué se dibuja (None si apagado o si no se pudo preparar).
    cursor_on: bool,
    cursor: Option<Cursor>,
}

impl Capture {
    /// Captura `monitor` (si no hay, ya no existe o está en `lost_on`, el principal u otro). Prueba cada GPU que lo ve:
    /// en notebooks con dos GPUs la que lo enumera primero no siempre puede duplicarlo.
    pub fn new(monitor: Option<HMONITOR>, lost_on: &[HMONITOR]) -> Result<Self> {
        #[cfg(feature = "devtools")]
        SESSIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut all = Vec::new();
        let mut i = 0;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
            i += 1;
            all.extend(outputs(&adapter).into_iter().map(|o| (adapter.clone(), o)));
        }
        let primary = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
        let usable = |m: &HMONITOR| !lost_on.contains(m) && all.iter().any(|(_, o)| o.monitor == *m);
        let want = monitor.into_iter().chain([primary]).chain(all.iter().map(|(_, o)| o.monitor)).find(usable).unwrap_or(primary);
        let mut last = anyhow!("no monitor found to record");
        for (adapter, out) in all.into_iter().filter(|(_, o)| o.monitor == want) {
            let name = out.name.clone();
            match Self::open_on(adapter, out, lost_on) {
                Ok(c) => return Ok(c),
                Err(e) => {
                    log(&format!("no se pudo capturar {name}: {e}"));
                    last = e;
                }
            }
        }
        Err(last)
    }

    fn open_on(adapter: IDXGIAdapter1, out: Output, lost_on: &[HMONITOR]) -> Result<Self> {
        unsafe {
            let adesc = adapter.GetDesc1()?;
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.ok_or_else(|| anyhow!("sin dispositivo D3D11"))?;
            let context = context.ok_or_else(|| anyhow!("sin contexto D3D11"))?;
            // El encoder de Media Foundation usa el mismo dispositivo desde sus propios hilos.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let mut cap = Capture {
                device,
                context,
                luid: adesc.AdapterLuid,
                width: out.width,
                height: out.height,
                latest: None,
                version: 0,
                adapter,
                gpu: wide(&adesc.Description),
                out,
                dup: None,
                rotation: DXGI_MODE_ROTATION::default(),
                mode: (0, 0),
                stretch: None,
                retry_at: 0,
                failing_since: None,
                failing: false,
                skipped: None,
                lost_on: lost_on.to_vec(),
                game_frames: 0,
                game_at: VecDeque::with_capacity(GAME_MIN + 1),
                dup_lost: 0,
                dirty: Vec::new(),
                #[cfg(feature = "devtools")]
                stats: Stats::default(),
                cursor_on: false,
                cursor: None,
            };
            cap.open()?;
            Ok(cap)
        }
    }

    /// El monitor que se graba.
    pub fn monitor(&self) -> HMONITOR {
        self.out.monitor
    }

    /// Con el juego adelante desde `since`: si hace más de `STALL` y no llegan sus imágenes (menos de `GAME_MIN` en los
    /// últimos `STALL`), cuántas veces se perdió la duplicación desde la última imagen nueva.
    pub fn stalled(&self, now: i64, since: i64) -> Option<u32> {
        (now - since > STALL && !self.sees_game(now)).then_some(self.dup_lost)
    }

    /// Con el juego adelante desde `since`: hace más de `STALL` y llegan sus imágenes. Unas sueltas del escritorio,
    /// aunque lo cambien entero, no alcanzan.
    pub fn flowing(&self, now: i64, since: i64) -> bool {
        now - since > STALL && self.sees_game(now)
    }

    fn sees_game(&self, now: i64) -> bool {
        self.game_at.iter().filter(|&&t| now - t <= STALL).count() >= GAME_MIN
    }

    /// Llegó una imagen nueva; `game` si cambió casi todo el monitor, como cada cuadro de un juego.
    fn arrived(&mut self, now: i64, game: bool) {
        self.dup_lost = 0;
        if game {
            self.game_frames = (self.game_frames + 1).min(WORKED);
            self.game_at.push_back(now);
            if self.game_at.len() > GAME_MIN {
                self.game_at.pop_front();
            }
        }
    }

    /// La imagen tomada (antes de `ReleaseFrame`) cambió al menos `GAME_COVER` décimos del monitor. Si Windows no dice
    /// qué cambió, cuenta como del juego: mejor no avisar de más.
    fn covers(&mut self, dup: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) -> bool {
        if info.TotalMetadataBufferSize == 0 {
            return true;
        }
        let size = std::mem::size_of::<RECT>() as u32;
        let mut need = 0u32;
        let mut got = unsafe { dup.GetFrameDirtyRects(self.dirty.capacity() as u32 * size, self.dirty.as_mut_ptr(), &mut need) };
        if got.as_ref().is_err_and(|e| e.code() == DXGI_ERROR_MORE_DATA) {
            self.dirty.reserve(need.div_ceil(size) as usize);
            got = unsafe { dup.GetFrameDirtyRects(self.dirty.capacity() as u32 * size, self.dirty.as_mut_ptr(), &mut need) };
        }
        if got.is_err() {
            return true;
        }
        // SAFETY: GetFrameDirtyRects escribió `need` bytes de RECT en el buffer, que tiene lugar para todos.
        unsafe { self.dirty.set_len((need / size) as usize) };
        // Casi nunca se pisan; si se pisan, se cuentan dos veces (pasa en el escritorio, no en un juego).
        let area: u64 = self.dirty.iter().map(|r| (r.right - r.left).max(0) as u64 * (r.bottom - r.top).max(0) as u64).sum();
        self.dirty.clear();
        area * 10 >= self.mode.0 as u64 * self.mode.1 as u64 * GAME_COVER
    }

    /// El modo de video del monitor no es el del escritorio guardado en el registro (ni girado): lo puso un juego en
    /// pantalla completa exclusiva, y Desktop Duplication no ve lo que dibuja.
    pub fn mode_changed(&self) -> bool {
        let (w, h) = self.mode;
        screen::desktop(&self.out.name).is_some_and(|d| d != (w, h) && d != (h, w))
    }

    /// La sesión llegó a capturar el juego (si después se traba, es que algo la trabó, no que nunca anduvo).
    pub fn worked(&self) -> bool {
        self.game_frames >= WORKED
    }

    /// "\\.\DISPLAY2 (NVIDIA GeForce RTX 2060)", para el log.
    pub fn describe(&self) -> String {
        format!("{} ({})", self.out.name, self.gpu)
    }

    /// Si se perdió la placa, por qué (`GetDeviceRemovedReason`), para el log.
    fn removed(&self) -> Option<String> {
        let code = unsafe { self.device.GetDeviceRemovedReason() }.err()?.code();
        let why = match code {
            DXGI_ERROR_DEVICE_HUNG => "se colgó con comandos de Clippy",
            DXGI_ERROR_DEVICE_REMOVED => "se desconectó la placa o se actualizó el driver",
            // Lo que reciben las apps que no la colgaron cuando Windows reinicia la placa (TDR).
            DXGI_ERROR_DEVICE_RESET => "se reinició la placa por un cuelgue o un reinicio del driver",
            DXGI_ERROR_DRIVER_INTERNAL_ERROR => "error interno del driver",
            _ => "motivo desconocido",
        };
        Some(format!("{why}, {code}"))
    }

    /// Anota que se perdió la placa (`why`, y `after`: el error que lo mostró) y devuelve `DeviceLost`.
    fn lost(&self, why: &str, after: &dyn std::fmt::Display) -> anyhow::Error {
        log(&format!("se perdió la placa de video grabando {} ({why}; {after}): se vuelve a empezar con otro dispositivo", self.describe()));
        DeviceLost.into()
    }

    /// Si `r` falló porque se perdió la placa, devuelve `DeviceLost` (la conversión y el encoder usan este dispositivo).
    pub fn check<T>(&self, r: Result<T>) -> Result<T> {
        r.map_err(|e| match self.removed() {
            Some(why) => self.lost(&why, &e),
            None => e,
        })
    }

    fn open(&mut self) -> Result<()> {
        self.dup = None;
        let dup = unsafe { self.out.output.DuplicateOutput(&self.device)? };
        let desc = unsafe { dup.GetDesc() };
        self.rotation = desc.Rotation;
        self.mode = (desc.ModeDesc.Width, desc.ModeDesc.Height);
        self.dup = Some(dup);
        if let Some(c) = &mut self.cursor {
            c.reset();
        }
        // Se vuelve a abrir cada vez que un juego cambia el modo de video: ahí se ve si se estira.
        self.stretch = screen::all().into_iter().find(|s| s.name == self.out.name && !s.keeps_shape).map(|s| s.panel);
        Ok(())
    }

    /// Pasa a grabar `monitor`. Con `native` el video tiene que quedar a la resolución de ese
    /// monitor (el del juego): si no coincide, pide reiniciar. Sin `native`, un monitor de otra
    /// resolución se escala adentro del video y uno de otra GPU se deja pasar (reiniciar perdería el buffer).
    pub fn aim(&mut self, monitor: HMONITOR, native: bool) -> Aim {
        let same_size = |o: &Output| (o.width, o.height) == (self.width, self.height);
        if monitor == self.out.monitor {
            // Volvió al que se graba: la próxima vez que pase al salteado se vuelve a probar.
            self.skipped = None;
            if !native || same_size(&self.out) {
                return Aim::Same;
            }
        }
        // Ya se vio que no se puede seguir sin reiniciar: probarlo de nuevo cada 250 ms consultaría los monitores y
        // cerraría y reabriría la duplicación del que se graba.
        if !native && self.skipped == Some(monitor) {
            return Aim::Same;
        }
        if self.lost_on.contains(&monitor) {
            // Ni por el juego: se volvería a perder la placa.
            return self.skip(monitor, false, "hizo perder la placa de video");
        }
        let Some(out) = outputs(&self.adapter).into_iter().find(|o| o.monitor == monitor) else {
            return self.skip(monitor, native, "está en otra GPU");
        };
        let restart = native && !same_size(&out);
        let prev = std::mem::replace(&mut self.out, out);
        // Se abre aunque haya que reiniciar: así se sabe antes si ese monitor hace perder la placa.
        match self.open() {
            Ok(()) if restart => {
                log(&format!("{} tiene otra resolución ({}x{}): reiniciando la captura", self.out.name, self.out.width, self.out.height));
                Aim::Restart
            }
            Ok(()) => {
                log(&format!("grabando {}", self.describe()));
                (self.failing, self.failing_since) = (false, None);
                self.skipped = None;
                Aim::Switched
            }
            Err(e) => {
                // Se vuelve al monitor de antes; `poll` reabre su duplicación.
                let failed = std::mem::replace(&mut self.out, prev);
                self.dup = None;
                self.retry_at = 0;
                if let Some(why) = self.removed() {
                    log(&format!(
                        "se perdió la placa de video al abrir {} ({why}; {e}): se vuelve a empezar grabando {} y no se prueba más ese monitor",
                        failed.name, self.out.name
                    ));
                    return Aim::Lost(self.out.monitor);
                }
                self.skip(monitor, native, &format!("no se pudo capturar con esta GPU ({e})"))
            }
        }
    }

    /// Prende o apaga el seguimiento del cursor. Apagado no se pide ni su forma. Al prenderlo se vuelve a abrir la
    /// duplicación: Desktop Duplication manda la forma solo cuando cambia, y al abrirse manda la de ahora.
    pub fn set_cursor(&mut self, on: bool) {
        if on == std::mem::replace(&mut self.cursor_on, on) {
            return;
        }
        if !on {
            self.draw_cursor(false);
            self.cursor = None;
            log("cursor del mouse: apagado");
            return;
        }
        match Cursor::new(&self.device) {
            Ok(c) => {
                self.cursor = Some(c);
                (self.dup, self.retry_at) = (None, 0);
                log("cursor del mouse: se dibuja mientras no haya un juego");
            }
            Err(e) => log(&format!("cursor del mouse: no se puede dibujar ({e})")),
        }
    }

    /// Deja la imagen con el cursor si `show` (y se pidió con `set_cursor`), o sin él. Si no se puede dibujar, se
    /// sigue grabando sin cursor.
    pub fn draw_cursor(&mut self, show: bool) {
        let (Some(c), Some(f)) = (&mut self.cursor, &self.latest) else { return };
        match c.draw(&self.context, f, show) {
            Ok(changed) => self.version += changed as u64,
            Err(e) => {
                log(&format!("cursor del mouse: no se puede dibujar ({e})"));
                self.cursor = None;
            }
        }
    }

    fn skip(&mut self, monitor: HMONITOR, native: bool, why: &str) -> Aim {
        if native {
            log(&format!("el monitor del juego {why}: reiniciando la captura"));
            return Aim::Restart;
        }
        if self.skipped != Some(monitor) {
            self.skipped = Some(monitor);
            log(&format!("la ventana activa está en un monitor que {why}: sigo grabando {}", self.out.name));
        }
        Aim::Same
    }

    /// Toma la imagen más reciente si hubo cambios (no bloquea). Devuelve `DeviceLost` si se perdió la placa.
    pub fn poll(&mut self, now: i64) -> Result<()> {
        #[cfg(feature = "devtools")]
        if test_lost(now) {
            return Err(self.lost("prueba CLIPPY_TEST_LOST", &"sin error"));
        }
        if self.dup.is_none() {
            if now < self.retry_at {
                return Ok(());
            }
            // Escritorio seguro (UAC, bloqueo), cambio de modo u otra app que ocupa la duplicación. Si se perdió la
            // placa, reintentar con este dispositivo no sirve.
            if let Err(e) = self.open() {
                if let Some(why) = self.removed() {
                    return Err(self.lost(&why, &e));
                }
                let since = *self.failing_since.get_or_insert(now);
                let quick = now - since < QUICK_FOR;
                self.retry_at = now + if quick { QUICK_RETRY } else { RETRY };
                if !quick && !self.failing {
                    self.failing = true;
                    log(&format!("no se puede capturar {} ({e}): el video queda congelado, reintento cada 5 s", self.out.name));
                }
                return Ok(());
            }
            self.failing_since = None;
            if std::mem::take(&mut self.failing) {
                log(&format!("captura de {} recuperada", self.out.name));
            }
        }
        let dup = self.dup.clone().unwrap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        let got = unsafe { dup.AcquireNextFrame(0, &mut info, &mut res) };
        #[cfg(feature = "devtools")]
        let got = test_hidden(test_stall(now, got, &dup), &dup, self.latest.is_some());
        #[cfg(feature = "devtools")]
        if test_covered() && self.latest.is_some() && now - self.stats.covered_at >= COVERED_EVERY {
            // Una imagen suelta del escritorio que lo cambia entero: lo peor que puede llegar con el juego tapado.
            self.stats.covered_at = now;
            self.arrived(now, true);
        }
        match got {
            Ok(()) => {
                if let Some(c) = &mut self.cursor {
                    c.update(&dup, &info);
                }
                let r = if info.LastPresentTime != 0 {
                    match res {
                        Some(res) => res.cast::<ID3D11Texture2D>().map_err(Into::into).and_then(|t| self.copy(&t)),
                        None => Ok(()),
                    }
                } else {
                    Ok(())
                };
                let game = info.LastPresentTime != 0 && self.covers(&dup, &info);
                unsafe {
                    let _ = dup.ReleaseFrame();
                }
                self.check(r)?;
                if info.LastPresentTime != 0 {
                    #[cfg(feature = "devtools")]
                    self.stats.add(&info, game);
                    self.arrived(now, game);
                }
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
            Err(e) => {
                if let Some(why) = self.removed() {
                    return Err(self.lost(&why, &e));
                }
                // Se pierde en cada cambio de modo o Alt+Tab y se reabre en el cuadro siguiente. Si queda trabada,
                // lo nota `stalled`.
                if e.code() == DXGI_ERROR_ACCESS_LOST {
                    self.dup_lost += 1;
                } else {
                    log(&format!("duplicación de {}: {e}", self.out.name));
                }
                self.dup = None;
                self.retry_at = now;
            }
        }
        Ok(())
    }

    fn copy(&mut self, frame: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            frame.GetDesc(&mut d);
            let turned = matches!(self.rotation, DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270);
            let image = if turned { (d.Height, d.Width) } else { (d.Width, d.Height) };
            let shape = self.stretch.unwrap_or(image);
            let fits = self.latest.as_ref().is_some_and(|f| {
                let mut l = D3D11_TEXTURE2D_DESC::default();
                f.tex.GetDesc(&mut l);
                (f.rotation, f.shape) == (self.rotation, shape) && (l.Width, l.Height, l.Format) == (d.Width, d.Height, d.Format)
            });
            if !fits {
                d.MipLevels = 1;
                d.ArraySize = 1;
                d.Usage = D3D11_USAGE_DEFAULT;
                d.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
                d.CPUAccessFlags = 0;
                d.MiscFlags = 0;
                let mut t = None;
                self.device.CreateTexture2D(&d, None, Some(&mut t))?;
                let tex = t.ok_or_else(|| anyhow!("sin textura de captura"))?;
                if shape.0 as u64 * image.1 as u64 != shape.1 as u64 * image.0 as u64 {
                    log(&format!("{} a {}x{}: estirado a la forma del monitor ({}x{})", self.out.name, image.0, image.1, shape.0, shape.1));
                }
                self.latest = Some(Frame { tex, rotation: self.rotation, shape });
            }
            if let Some(f) = &self.latest {
                self.context.CopyResource(&f.tex, frame);
                self.version += 1;
                if let Some(c) = &mut self.cursor {
                    c.forget();
                }
            }
        }
        Ok(())
    }
}

/// Prueba (CLIPPY_TEST_LOST=<segundos>): a esos segundos de grabar se pierde la placa, una sola vez, como en un
/// reinicio del driver.
#[cfg(feature = "devtools")]
fn test_lost(now: i64) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    let at = std::env::var("CLIPPY_TEST_LOST").ok().and_then(|s| s.parse::<i64>().ok());
    at.is_some_and(|s| now >= s * 10_000_000) && !DONE.swap(true, Ordering::Relaxed)
}

/// Prueba: cuántas veces se creó la captura (la primera sesión es la que se traba con `CLIPPY_TEST_STALL`).
#[cfg(feature = "devtools")]
static SESSIONS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Prueba (CLIPPY_TEST_STALL=<segundos>): pasado ese tiempo de la primera sesión, la duplicación se pierde apenas se
/// abre, como con algunos juegos en pantalla completa exclusiva en Windows 11 24H2.
#[cfg(feature = "devtools")]
fn test_stall(now: i64, got: windows::core::Result<()>, dup: &IDXGIOutputDuplication) -> windows::core::Result<()> {
    let after = std::env::var("CLIPPY_TEST_STALL").ok().and_then(|s| s.parse::<i64>().ok());
    let first = SESSIONS.load(std::sync::atomic::Ordering::Relaxed) == 1;
    if !after.is_some_and(|s| first && now >= s * 10_000_000) {
        return got;
    }
    if got.is_ok() {
        unsafe {
            let _ = dup.ReleaseFrame();
        }
    }
    Err(DXGI_ERROR_ACCESS_LOST.into())
}

/// Prueba (CLIPPY_TEST_HIDDEN=1): un juego adelante en el monitor que se graba que Windows no le pasa a Clippy.
/// Desktop Duplication no recibe nada después de la primera imagen de cada sesión, como con una captura trabada.
/// Prueba (CLIPPY_TEST_COVERED=1): lo mismo, pero cada `COVERED_EVERY` llega una imagen que cambia todo el monitor,
/// como con un juego que no se deja capturar y el escritorio de atrás que se repinta.
#[cfg(feature = "devtools")]
pub fn test_hidden_game() -> bool {
    std::env::var_os("CLIPPY_TEST_HIDDEN").is_some() || test_covered()
}

#[cfg(feature = "devtools")]
fn test_covered() -> bool {
    std::env::var_os("CLIPPY_TEST_COVERED").is_some()
}

#[cfg(feature = "devtools")]
const COVERED_EVERY: i64 = 20_000_000;

/// Prueba: qué imágenes llegan, para medirlas con el juego adelante (`Capture::log_rate`).
#[cfg(feature = "devtools")]
#[derive(Default)]
struct Stats {
    /// Desde cuándo se cuenta (None: todavía no se empezó).
    since: Option<i64>,
    /// Imágenes nuevas tomadas, cuadros que Windows juntó en ellas y las que cambian casi todo el monitor (del juego).
    new: u32,
    accumulated: u32,
    game: u32,
    /// Última imagen de mentira de CLIPPY_TEST_COVERED.
    covered_at: i64,
}

#[cfg(feature = "devtools")]
impl Stats {
    fn add(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO, game: bool) {
        self.new += 1;
        self.accumulated += info.AccumulatedFrames;
        self.game += game as u32;
    }
}

#[cfg(feature = "devtools")]
impl Capture {
    /// Prueba (CLIPPY_LOG_FRAMES=1): con el juego adelante, cada segundo anota cuántas imágenes llegaron y cuántas
    /// cambian todo el monitor, para comparar el juego que se ve con el tapado por el escritorio.
    pub fn log_rate(&mut self, now: i64) {
        if std::env::var_os("CLIPPY_LOG_FRAMES").is_none() {
            return;
        }
        let Some(since) = self.stats.since else {
            self.stats = Stats { since: Some(now), covered_at: self.stats.covered_at, ..Default::default() };
            return;
        };
        if now - since < 10_000_000 {
            return;
        }
        let s = &self.stats;
        let secs = (now - since) as f64 / 1e7;
        log(&format!(
            "imágenes con el juego adelante en {}: {:.0}/s nuevas ({:.0}/s cuadros juntados), {:.0}/s cambian todo el monitor; se ve el juego: {}",
            self.out.name,
            s.new as f64 / secs,
            s.accumulated as f64 / secs,
            s.game as f64 / secs,
            self.sees_game(now)
        ));
        self.stats = Stats { since: Some(now), covered_at: s.covered_at, ..Default::default() };
    }

    /// Prueba: el juego dejó de estar adelante; la próxima medición empieza de cero.
    pub fn log_rate_stop(&mut self) {
        self.stats.since = None;
    }
}

#[cfg(feature = "devtools")]
fn test_hidden(got: windows::core::Result<()>, dup: &IDXGIOutputDuplication, first_taken: bool) -> windows::core::Result<()> {
    if !test_hidden_game() || !first_taken {
        return got;
    }
    if got.is_ok() {
        unsafe {
            let _ = dup.ReleaseFrame();
        }
    }
    Err(DXGI_ERROR_WAIT_TIMEOUT.into())
}

/// Monitores conectados al escritorio que ve esta GPU.
fn outputs(adapter: &IDXGIAdapter1) -> Vec<Output> {
    let screens = screen::all();
    let mut v = Vec::new();
    let mut j = 0;
    while let Ok(output) = unsafe { adapter.EnumOutputs(j) } {
        j += 1;
        let (Ok(d), Ok(output)) = (unsafe { output.GetDesc() }, output.cast::<IDXGIOutput1>()) else { continue };
        if !d.AttachedToDesktop.as_bool() {
            continue;
        }
        let name = wide(&d.DeviceName);
        let r = d.DesktopCoordinates;
        let desktop = screen::desktop(&name).unwrap_or(((r.right - r.left) as u32, (r.bottom - r.top) as u32));
        let panel = screens.iter().find(|s| s.name == name).map(|s| s.panel);
        let (width, height) = video_size(desktop, panel);
        v.push(Output { monitor: d.Monitor, output, name, width, height });
    }
    v
}

/// Tamaño del video para un monitor (pares, como lo pide H.264): la forma del panel, así una resolución
/// estirada sale estirada como la ve el jugador, a la resolución del escritorio de Windows (la que eligió el
/// usuario, no la que pone un juego): no se agranda la imagen, un panel 4K con el escritorio a 1080p graba 1080p.
fn video_size(desktop: (u32, u32), panel: Option<(u32, u32)>) -> (u32, u32) {
    let (w, h) = match panel {
        Some((pw, ph)) => {
            let short = desktop.0.min(desktop.1).min(pw.min(ph)) as u64;
            let (pw, ph) = (pw as u64, ph as u64);
            if pw >= ph { (short * pw / ph, short) } else { (short, short * ph / pw) }
        }
        None => (desktop.0 as u64, desktop.1 as u64),
    };
    ((w as u32).max(2) & !1, (h as u32).max(2) & !1)
}

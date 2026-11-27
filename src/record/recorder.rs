//! Hilo de grabación: captura -> NV12 -> encoder -> buffer en RAM. Guarda clips a pedido.
//! Se despierta exactamente `fps` veces por segundo con un temporizador de alta resolución.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use windows::Win32::Foundation::{FILETIME, HANDLE};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Memory::{HeapOptimizeResources, HeapSetInformation};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetProcessTimes, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};

use super::audio::{self, Audio, AudioBuf, Source};
use super::capture::{Aim, Capture, DeviceLost};
use super::clock::{now_100ns, Timer};
use super::convert::Converter;
use super::encoder::{Bytes, Device, Encoder, Packet};
use super::track::{self, Input, Track};
use super::{game, mux, screen};
use crate::config::{CaptureMode, Config};
use crate::editor::library::{clean_name, next_stem};
use crate::log::log;
use crate::mf::{frames_at, time_of, Mf};
use crate::ui_kit::sound;

pub enum Cmd {
    /// Guardar un clip; lleva cuándo se pidió (para anotar cuánto tardó en llegar).
    Save(Instant),
    /// Cambios que no necesitan reiniciar el video (duración, audio, sonido, carpeta).
    Update(Config),
    Stop,
}

/// La bandeja solo usa algunos datos; los modos de prueba (`devtools`) los imprimen todos.
#[cfg_attr(not(feature = "devtools"), allow(dead_code))]
pub enum Event {
    /// Empezó a grabar, codificando con esto.
    Started(Device),
    Saved(std::path::PathBuf),
    SaveFailed(String),
    Error(String),
    /// Cambió el juego detectado (nombre del ejecutable sin ".exe").
    Game(Option<String>),
    /// El juego está adelante y Windows no le pasa sus imágenes a Clippy: los clips solo tienen el escritorio. Lleva
    /// el motivo, o None cuando se ve de nuevo o cambia el juego.
    GameHidden(Option<String>),
}

pub type Notify = Arc<dyn Fn(Event) + Send + Sync>;

pub struct Recorder {
    tx: mpsc::Sender<Cmd>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Recorder {
    pub fn start(cfg: Config, notify: Notify) -> Self {
        // Lo que se usó para guardar el clip (las muestras y la cola del Sink Writer, el MP4 a medio escribir) ya se
        // soltó cuando el hilo que guarda avisa que terminó.
        let notify: Notify = Arc::new(move |ev| {
            if matches!(ev, Event::Saved(_)) {
                give_back_memory();
            }
            notify(ev)
        });
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("clippy-rec".into())
            .spawn(move || thread_main(cfg, rx, notify))
            .ok();
        Self { tx, handle }
    }

    pub fn save(&self) {
        if self.tx.send(Cmd::Save(Instant::now())).is_err() {
            log("guardar: se descarta, el hilo de grabación ya terminó");
        }
    }

    pub fn update(&self, cfg: Config) {
        let _ = self.tx.send(Cmd::Update(cfg));
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

enum End {
    Stop,
    /// Volver a empezar grabando este monitor (el del juego).
    Restart(HMONITOR),
    /// Se perdió la placa al pasar a `on`: volver a empezar grabando `back` (el de antes) y no probar más `on`.
    Lost { on: HMONITOR, back: HMONITOR },
}

/// Qué monitor graba cada sesión al empezar.
#[derive(Default)]
struct Target {
    /// El del juego que pidió reiniciar (ya se graba a su resolución: no se vuelve a pedir) o el de antes de perder
    /// la placa. Si no hay, el del juego o el de la ventana activa.
    monitor: Option<HMONITOR>,
    /// Los que hicieron perder la placa al pasar a ellos: no se vuelven a probar.
    lost_on: Vec<HMONITOR>,
    /// El juego está adelante y no llegan sus imágenes (ya se avisó). Sigue entre sesiones: reiniciar no lo arregla
    /// enseguida, así que se reintenta cada vez más espaciado.
    hidden: Option<Hidden>,
}

struct Hidden {
    /// El juego del aviso: si cambia, el aviso ya no vale.
    game: Option<String>,
    retry_at: Instant,
    wait: Duration,
}

/// Con el juego oculto, cuánto se espera hasta el primer reintento y hasta cuánto se va duplicando la espera.
const HIDDEN_RETRY: Duration = Duration::from_secs(30);
const HIDDEN_RETRY_MAX: Duration = Duration::from_secs(300);

/// Avisa que no se ve `game` en `on` (para el log) y por qué. Se reintenta en `HIDDEN_RETRY`.
fn hide(target: &mut Target, notify: &Notify, game: Option<String>, why: Why, on: &str) {
    log(&format!("el juego está adelante y no llegan sus imágenes de {on} ({}): se avisa", why.log()));
    notify(Event::GameHidden(Some(why.reason(game.as_deref()))));
    target.hidden = Some(Hidden { game, retry_at: Instant::now() + HIDDEN_RETRY, wait: HIDDEN_RETRY });
}

/// Deja de avisar que no se ve el juego (`why` va al log).
fn unhide(target: &mut Target, notify: &Notify, why: &str) {
    if target.hidden.take().is_some() {
        log(&format!("{why}: se deja de avisar que no se ve el juego"));
        notify(Event::GameHidden(None));
    }
}

/// Por qué no se ve el juego.
#[derive(Clone, Copy)]
enum Why {
    /// El juego pidió que no se capture su ventana (`game::Detector::blocks_capture`).
    Blocked,
    /// El modo de video del monitor no es el del escritorio: pantalla completa exclusiva.
    Exclusive,
    /// No llegan sus imágenes y no se sabe por qué.
    Unknown,
}

impl Why {
    /// Para el log.
    fn log(self) -> &'static str {
        match self {
            Why::Blocked => "el juego pidió que no se capture su ventana",
            Why::Exclusive => "el modo de video no es el del escritorio, pantalla completa exclusiva",
            Why::Unknown => "el modo de video es el del escritorio",
        }
    }

    /// Para la pantalla.
    fn reason(self, game: Option<&str>) -> String {
        // Al principio de la frase, "The game".
        let (g, first) = game.map_or(("the game", "The game"), |g| (g, g));
        match self {
            Why::Blocked => format!(
                "{first} is keeping its window out of screen capture, so clips only have what's behind it. Restarting {g} usually fixes it."
            ),
            Why::Exclusive => format!(
                "{first} is in exclusive full screen, so Windows doesn't show it to Clippy and clips only have the desktop. Set {g}'s display mode to borderless or windowed fullscreen."
            ),
            Why::Unknown => format!("Windows isn't showing {g} to Clippy, so clips only have the desktop. Clippy keeps trying by itself."),
        }
    }
}

/// Cada cuánto se vuelve a intentar grabar después de un error.
const RETRY: Duration = Duration::from_secs(3);

fn thread_main(mut cfg: Config, rx: Receiver<Cmd>, notify: Notify) {
    let _mf = Mf::init();
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    let mut target = Target::default();
    // El último error avisado: mientras se repita en cada reintento, no se vuelve a anotar ni avisar.
    let mut failing = None;
    // Una vez que el encoder rechazó un tamaño que no es múltiplo de 16, se graba siempre alineado.
    let mut align = false;
    loop {
        let mut untested = None;
        match session(&mut cfg, &rx, &notify, &mut target, &mut failing, align, &mut untested) {
            Ok(End::Stop) => break,
            Ok(End::Restart(m)) => target.monitor = Some(m),
            Ok(End::Lost { on, back }) => {
                target.lost_on.push(on);
                target.monitor = Some(back);
            }
            // Quick Sync acepta el tamaño al configurarse y falla con el primer cuadro si no está alineado: Intel pide
            // ancho múltiplo de 16 y algunos drivers de 2022 (HD 620) ancho y alto múltiplos de 4. 1366x768 no cumple
            // ninguno de los dos: se reintenta al toque, alineado a 16 (con el mismo monitor).
            Err(e) if !e.is::<DeviceLost>() && untested.is_some_and(|s| align16(s) != s) => {
                let (w, h) = untested.unwrap();
                let (aw, ah) = align16((w, h));
                log(&format!("el encoder no acepta {w}x{h} ({e:?}): se graba a {aw}x{ah}"));
                align = true;
            }
            Err(e) => {
                target.monitor = None;
                let msg = e.to_string();
                // Si se perdió la placa ya se anotó por qué: se vuelve a grabar con otro dispositivo sin avisar (si no
                // arranca, eso sí se avisa).
                if !e.is::<DeviceLost>() && failing.as_ref() != Some(&msg) {
                    log(&format!("error de captura: {e:?} (se reintenta cada {} s)", RETRY.as_secs()));
                    notify(Event::Error(msg.clone()));
                    failing = Some(msg);
                }
                match rx.recv_timeout(RETRY) {
                    Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                    Ok(Cmd::Update(c)) => cfg = c,
                    Ok(Cmd::Save(_)) => {
                        let m = "Clippy isn't recording right now";
                        log(&format!("guardar: se descarta, {m}"));
                        notify(Event::SaveFailed(m.into()));
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
        }
    }
    // En pausa (o antes de volver a empezar con otros ajustes) los buffers ya se soltaron.
    give_back_memory();
}

/// Le devuelve a Windows la memoria que los heaps del proceso tienen libre. El heap se queda con lo que se suelta para
/// volver a usarlo, así que después de un pico (guardar un clip) o de soltar los buffers (dejar de grabar) seguiría
/// contando como RAM privada aunque nadie lo use. Windows 8.1 o más nuevo; en los anteriores no hace nada.
fn give_back_memory() {
    // HEAP_OPTIMIZE_RESOURCES_INFORMATION { Version: 1, Flags: 0 }; sin heap: todos los del proceso.
    let info = [1u32, 0];
    unsafe {
        let _ = HeapSetInformation(HANDLE::default(), HeapOptimizeResources, Some(info.as_ptr().cast()), size_of_val(&info));
    }
}

/// Milisegundos desde que se abrió este proceso, para el log del arranque.
fn since_launch_ms() -> i64 {
    let t = |f: FILETIME| ((f.dwHighDateTime as i64) << 32 | f.dwLowDateTime as i64) / 10_000;
    let [mut created, mut exit, mut kernel, mut user] = [FILETIME::default(); 4];
    unsafe {
        if GetProcessTimes(GetCurrentProcess(), &mut created, &mut exit, &mut kernel, &mut user).is_err() {
            return -1;
        }
        t(GetSystemTimeAsFileTime()) - t(created)
    }
}

/// RAM privada del proceso (la del Administrador de tareas), en MB.
fn private_mb() -> usize {
    let mut m = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    unsafe {
        let _ = K32GetProcessMemoryInfo(GetCurrentProcess(), &mut m as *mut _ as *mut PROCESS_MEMORY_COUNTERS, m.cb);
    }
    m.PrivateUsage >> 20
}

/// Lo que tarda el aviso de "clip guardado" en empezar a sonar y llegar a la captura (100 ns):
/// se silencia en el canal escritorio junto con el sonido.
const SOUND_LATENCY: i64 = 2_050_000;

/// Una fuente de audio que no se pudo abrir se reintenta cada 5 s.
const AUDIO_RETRY: i64 = 50_000_000;

/// Un canal de audio (juego, escritorio o micrófono) con su propio buffer.
#[derive(Default)]
struct Channel {
    audio: Option<Audio>,
    buf: AudioBuf,
    /// Fuentes que no se pudieron abrir y desde cuándo se pueden reintentar.
    failed: Vec<(Source, i64)>,
}

impl Channel {
    /// Deja abierta `want` o, si no se puede, `fallback`. La fuente nueva se abre antes de soltar
    /// la anterior: un reintento que falla no corta el audio que ya se estaba grabando.
    fn sync(&mut self, want: Option<Source>, fallback: Option<Source>, now: i64, name: &str) {
        let sources = [want, fallback];
        let sources = || sources.iter().flatten();
        if sources().next().is_none() {
            self.audio = None;
            self.buf = AudioBuf::default();
            return;
        }
        // Solo se recuerdan las fallas de lo que se pide ahora (no se acumulan juegos viejos).
        self.failed.retain(|(s, _)| sources().any(|w| w == s));
        for src in sources() {
            if self.audio.as_ref().is_some_and(|a| a.source == *src) {
                return;
            }
            if self.failed.iter().any(|(s, at)| s == src && now < *at) {
                continue;
            }
            match Audio::open(src.clone()) {
                Ok(a) => {
                    log(&format!("audio {name}: {src:?}"));
                    self.failed.retain(|(s, _)| s != src);
                    self.audio = Some(a);
                    return;
                }
                Err(e) => match self.failed.iter_mut().find(|(s, _)| s == src) {
                    Some(f) => f.1 = now + AUDIO_RETRY,
                    None => {
                        log(&format!("no se pudo abrir el audio {name} ({src:?}): {e}"));
                        self.failed.push((src.clone(), now + AUDIO_RETRY));
                    }
                },
            }
        }
        self.audio = None;
    }

    fn poll(&mut self, t0: i64) {
        if let Some(a) = self.audio.as_mut() {
            if let Err(e) = a.poll(&mut self.buf, t0) {
                log(&format!("audio: {e}"));
                self.audio = None;
            }
        }
    }
}

struct AudioState {
    game_ch: Channel,
    desktop: Channel,
    mic: Channel,
    game: game::Detector,
    output: audio::DefaultOutput,
    next_check: i64,
    /// Ventanas (100 ns) donde sonó el aviso de "clip guardado": se silencian en el canal escritorio.
    masks: Vec<(i64, i64)>,
    /// La pista de los clips, mezclada y comprimida mientras se graba (None con el audio apagado).
    track: Option<Track>,
    /// Si no se pudo abrir el encoder de audio, desde cuándo se reintenta.
    track_retry: i64,
}

/// Los canales prendidos, con su volumen, para la mezcla.
fn inputs<'a>(cfg: &Config, game: &'a Channel, mic: &'a Channel, desktop: &'a Channel) -> [Option<Input<'a>>; 3] {
    let input = |on: bool, c: &'a Channel, volume: u32, masked: bool| on.then(|| Input { buf: &c.buf, gain: volume as f32 / 100.0, masked });
    [
        input(cfg.game_audio, game, cfg.game_volume, false),
        input(cfg.mic_audio, mic, cfg.mic_volume, false),
        input(cfg.desktop_audio, desktop, cfg.desktop_volume, true),
    ]
}

impl AudioState {
    fn sync(&mut self, cfg: &Config, now: i64) {
        if now >= self.next_check {
            self.next_check = now + 30_000_000;
            // Si cambió el dispositivo de salida por defecto, reabrir la captura del sistema.
            if let Some(a) = &self.desktop.audio {
                if a.source == Source::System && a.device_id != self.output.id() {
                    self.desktop.audio = None;
                }
            }
        }
        let pid = self.game.pid();
        let game = pid.filter(|_| cfg.game_audio).map(Source::Only);
        self.game_ch.sync(game, None, now, "juego");
        self.mic.sync(cfg.mic_audio.then(|| Source::Mic(cfg.mic_device.clone())), None, now, "micrófono");
        // Escritorio = todo menos el juego; si no se puede excluir al juego, todo el sistema.
        let (desk, fallback) = match pid {
            Some(p) => (Source::AllBut(p), Some(Source::System)),
            None => (Source::System, None),
        };
        let on = cfg.desktop_audio;
        self.desktop.sync(on.then_some(desk), fallback.filter(|_| on), now, "escritorio");
    }

    /// Pasa a cada canal lo que llegó de su captura.
    fn poll(&mut self, t0: i64) {
        self.game_ch.poll(t0);
        self.desktop.poll(t0);
        self.mic.poll(t0);
    }

    /// Mezcla y comprime la pista de los clips hasta el frame `to` y suelta de los canales lo que ya se mezcló.
    fn advance(&mut self, cfg: &Config, to: i64, now: i64) {
        // Si el hilo estuvo frenado más que el buffer entero (la PC se suspendió), no se comprime ese silencio: se
        // empieza una pista nueva.
        let stalled = frames_at(cfg.seconds as i64 * 10_000_000 + 20_000_000);
        if !cfg.audio_on() || self.track.as_ref().is_some_and(|t| to - t.pos() > stalled) {
            self.track = None;
        } else if self.track.is_none() && now >= self.track_retry {
            match Track::new(to.max(0)) {
                Ok(t) => self.track = Some(t),
                Err(e) => {
                    log(&format!("no se pudo abrir el encoder de audio: {e:?}"));
                    self.track_retry = now + AUDIO_RETRY;
                }
            }
        }
        if let Some(t) = &mut self.track {
            if let Err(e) = t.advance(to, &inputs(cfg, &self.game_ch, &self.mic, &self.desktop), &self.masks) {
                log(&format!("no se pudo comprimir el audio: {e:?}"));
                self.track = None;
                self.track_retry = now + AUDIO_RETRY;
            }
        }
        let done = self.track.as_ref().map_or(to, |t| t.pos());
        for c in [&mut self.game_ch, &mut self.desktop, &mut self.mic] {
            c.buf.trim_before(done);
        }
        self.masks.retain(|m| m.1 > time_of(done));
    }
}

/// Cada cuánto se revisa qué monitor grabar (100 ns).
const AIM_EVERY: i64 = 2_500_000;

/// Cuánto después de empezar a convertir un monitor se controla la imagen para el log (100 ns).
const PROBE_AFTER: i64 = 20_000_000;

/// El tamaño más grande que entra en `(w, h)` con los dos lados múltiplos de 16 (la imagen se encaja adentro).
fn align16((w, h): (u32, u32)) -> (u32, u32) {
    ((w & !15).max(16), (h & !15).max(16))
}

/// `failing`: el error que se avisó; se olvida al volver a grabar.
/// `align`: grabar a un tamaño múltiplo de 16. `untested`: el tamaño del video mientras el encoder todavía no
/// entregó ningún cuadro (None hasta que se configura y otra vez cuando codifica).
fn session(
    cfg: &mut Config,
    rx: &Receiver<Cmd>,
    notify: &Notify,
    target: &mut Target,
    failing: &mut Option<String>,
    align: bool,
    untested: &mut Option<(u32, u32)>,
) -> Result<End> {
    let begun = std::time::Instant::now();
    let mut game = game::Detector::default();
    game.poll(0);
    // Qué monitor grabar: el elegido en los ajustes; en automático, el del juego; si no hay juego, el de la ventana
    // activa; si no, el principal.
    let chosen = screen::find(&cfg.monitor);
    let mut cap = Capture::new(chosen.or(target.monitor).or_else(|| game.monitor()).or_else(game::active_monitor), &target.lost_on)?;
    cap.set_cursor(cfg.cursor_on());
    let mut size = cfg.video_size((cap.width, cap.height));
    if align {
        size = align16(size);
    }
    // Si ni se pudo configurar (no hay encoder, otra placa), no es por el tamaño: se avisa como siempre.
    let mut enc = Encoder::new(&cap.device, cap.luid, size.0, size.1, cfg.fps.max(1), cfg.codec, cfg.bitrate_mbps)?;
    // El encoder por procesador puede pedir menos que lo elegido.
    let (w, h, fps) = (enc.width, enc.height, enc.fps);
    let frame_dur = 10_000_000i64 / fps as i64;
    // Imágenes convertidas que se reservan en la placa. El encoder de la placa retiene algunas en su cola y necesita
    // margen; el de procesador copia cada una al recibirla, así que alcanza con las que esperan en la fila.
    let pool = if enc.device() == Device::Cpu { 4 } else { 8 };
    *untested = Some((w, h));
    // Prueba (CLIPPY_TEST_ALIGN16): un encoder que, como Quick Sync, falla en el primer cuadro si el tamaño no
    // es múltiplo de 16.
    #[cfg(feature = "devtools")]
    let reject = std::env::var_os("CLIPPY_TEST_ALIGN16").is_some() && align16((w, h)) != (w, h);
    log(&format!(
        "grabando {} a {w}x{h} @ {fps} fps, {} {} Mbps, buffer {} s (máximo en esta PC: {} s), encoder: {}",
        cap.describe(),
        enc.codec.label(),
        enc.codec.bitrate(cfg.bitrate_mbps) / 1_000_000,
        cfg.seconds,
        cfg.max_seconds(),
        enc.name
    ));
    *failing = None;
    notify(Event::Started(enc.device()));

    let mut conv: Option<Converter> = None;
    let timer = Timer::new();
    let t0 = now_100ns();
    let mut au = AudioState {
        game_ch: Channel::default(),
        desktop: Channel::default(),
        mic: Channel::default(),
        game,
        output: audio::DefaultOutput::new(),
        next_check: 0,
        masks: Vec::new(),
        track: None,
        track_retry: 0,
    };
    notify(Event::Game(au.game.name()));
    let mut game_monitor = target.monitor;
    let mut next_aim = 0i64;
    // El juego está en el monitor que se graba (se revisa con `next_aim`): con un monitor elegido puede estar en otro.
    let mut game_on_cap = false;
    // Cuándo controlar la imagen del monitor actual (una vez por cada `Converter`).
    let mut probe_at: Option<i64> = None;
    au.sync(cfg, 0);
    let mut next_frame = 0i64;
    let mut packets: VecDeque<Packet> = VecDeque::new();
    let mut out = Vec::new();
    // Desde cuándo el juego está adelante en el monitor que se graba.
    let mut front_since: Option<i64> = None;
    #[cfg(feature = "devtools")]
    let mut perf = crate::perf::Loop::default();

    loop {
        #[cfg(feature = "devtools")]
        let woke = now_100ns();
        loop {
            match rx.try_recv() {
                Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => return Ok(End::Stop),
                Ok(Cmd::Save(asked)) => {
                    log(&format!("guardar: la grabadora tomó la orden {} ms después del pedido", asked.elapsed().as_millis()));
                    let now = now_100ns() - t0;
                    // Lo último que llegó de cada captura, para completar la pista hasta el final del clip.
                    au.poll(t0);
                    if save_clip(cfg, &packets, &enc, &au, now, frame_dur, notify) && cfg.sound_on_save {
                        sound::play_saved();
                        au.masks.push((now, now + sound::SAVED_LEN + SOUND_LATENCY));
                    }
                }
                Ok(Cmd::Update(c)) => {
                    *cfg = c;
                    cap.set_cursor(cfg.cursor_on());
                    au.sync(cfg, now_100ns() - t0);
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        let keep = cfg.seconds as i64 * 10_000_000;

        let now = now_100ns() - t0;
        if au.game.poll(now) {
            let name = au.game.name();
            log(&format!("juego detectado: {}", name.as_deref().unwrap_or("ninguno")));
            notify(Event::Game(name));
        }
        if now >= next_aim {
            next_aim = now + AIM_EVERY;
            // Cuando el juego aparece o se muda de monitor, el video toma la resolución de ese monitor.
            // Con un monitor elegido se graba siempre ese, y el juego cuenta solo si está ahí.
            let chosen = screen::find(&cfg.monitor);
            let game_at = au.game.monitor();
            let gm = game_at.filter(|m| chosen.is_none_or(|c| c == *m));
            let native = gm.is_some() && gm != game_monitor;
            game_monitor = gm;
            if let Some(m) = chosen.or(gm).or_else(game::active_monitor) {
                match cap.aim(m, native) {
                    Aim::Restart => return Ok(End::Restart(m)),
                    Aim::Lost(back) => return Ok(End::Lost { on: m, back }),
                    Aim::Same | Aim::Switched => {}
                }
            }
            game_on_cap = game_at == Some(cap.monitor());
            if target.hidden.as_ref().is_some_and(|h| h.game != au.game.name()) {
                unhide(target, notify, "cambió el juego");
            }
            let front = au.game.in_front() && gm == Some(cap.monitor());
            #[cfg(feature = "devtools")]
            let front = front || super::capture::test_hidden_game();
            // Si el juego pidió que no se capture su ventana, llega lo que hay detrás: se avisa enseguida y no se reintenta
            // (reiniciar la captura no lo cambia). Si no, un juego adelante manda imágenes todo el tiempo y cada una
            // cambia el monitor entero. Si no llegan, la captura quedó trabada (puede pasar al volver de un Alt+Tab con
            // resolución estirada): reabrirla con el mismo dispositivo no alcanza y el video repetiría la última imagen
            // para siempre. Se empieza de nuevo, como al cambiar un ajuste de video. Si nunca anduvo, o ya se reinició y
            // sigue igual, Windows no le pasa el juego a Clippy: se avisa y se reintenta cada vez más espaciado, sin
            // perder el buffer en cada traba.
            if front {
                let since = *front_since.get_or_insert(now);
                #[cfg(feature = "devtools")]
                cap.log_rate(now);
                if au.game.blocks_capture() {
                    if target.hidden.is_none() {
                        hide(target, notify, au.game.name(), Why::Blocked, &cap.describe());
                    }
                } else if cap.flowing(now, since) {
                    unhide(target, notify, &format!("llegan imágenes del juego otra vez en {}", cap.describe()));
                } else if let Some(lost) = cap.stalled(now, since) {
                    if cap.worked() && target.hidden.is_none() {
                        log(&format!(
                            "el juego está adelante y no llegan sus imágenes de {} (se perdió {lost} veces): se reinicia la captura",
                            cap.describe()
                        ));
                        return Ok(End::Restart(cap.monitor()));
                    }
                    match &mut target.hidden {
                        None => {
                            let why = if cap.mode_changed() { Why::Exclusive } else { Why::Unknown };
                            hide(target, notify, au.game.name(), why, &format!("{} (se perdió {lost} veces)", cap.describe()));
                        }
                        Some(h) if Instant::now() >= h.retry_at => {
                            h.wait = (h.wait * 2).min(HIDDEN_RETRY_MAX);
                            h.retry_at = Instant::now() + h.wait;
                            log(&format!(
                                "sigue sin verse el juego en {}: se reinicia la captura (próximo intento en {} s)",
                                cap.describe(),
                                h.wait.as_secs()
                            ));
                            return Ok(End::Restart(cap.monitor()));
                        }
                        Some(_) => {}
                    }
                }
            } else {
                front_since = None;
                #[cfg(feature = "devtools")]
                cap.log_rate_stop();
            }
        }
        let visible = cfg.capture_mode == CaptureMode::Screen || (au.game.visible() && game_on_cap);
        // En "Solo juegos" sin el juego a la vista el video va en negro: no se copia una pantalla que se descartaría.
        // La primera imagen sí hace falta, para preparar la conversión.
        if visible || conv.is_none() {
            cap.poll(now)?;
        }
        if let Some(f) = &cap.latest {
            if !conv.as_ref().is_some_and(|c| c.is_for(f)) {
                conv = Some(cap.check(Converter::new(&cap.device, &cap.context, f, w, h, fps, pool))?);
                probe_at = Some(now + PROBE_AFTER);
            }
        }
        au.sync(cfg, now);
        au.poll(t0);

        cap.check(enc.poll(&mut out))?;
        #[cfg(feature = "devtools")]
        {
            let got_at = now_100ns() - t0;
            for p in &out {
                perf.encoded(p.pts, p.data.len(), p.key, got_at);
            }
        }
        let added = out.len();
        packets.extend(out.drain(..));
        seal_gops(&mut packets, added);
        if !packets.is_empty() && untested.take().is_some() {
            log(&format!(
                "primer cuadro codificado: {} ms después de empezar la sesión, {} ms después de abrir Clippy, RAM privada {} MB",
                begun.elapsed().as_millis(),
                since_launch_ms(),
                private_mb()
            ));
        }

        if now >= next_frame {
            let pts = (now / frame_dur) * frame_dur;
            #[cfg(feature = "devtools")]
            if next_frame > 0 {
                perf.missed((pts - next_frame) / frame_dur);
            }
            if let Some(c) = conv.as_mut() {
                // Al encoder, en orden, los cuadros que la GPU ya terminó de convertir (sin esperarla: con el juego al
                // 100 % la conversión queda en la cola detrás de sus cuadros). Si el encoder está ocupado, esperan.
                while enc.pending_input > 0 {
                    let Some(r) = cap.check(c.ready())? else {
                        #[cfg(feature = "devtools")]
                        if c.queued() > 0 {
                            perf.gpu_late();
                        }
                        break;
                    };
                    if r.converted && probe_at.is_some_and(|t| now >= t) {
                        probe_at = None;
                        log(&c.probe(&r.tex));
                    }
                    #[cfg(feature = "devtools")]
                    if reject {
                        anyhow::bail!("encoder de prueba: {w}x{h} no es múltiplo de 16 (0x80004005)");
                    }
                    #[cfg(feature = "devtools")]
                    let fed_at = now_100ns() - t0;
                    cap.check(enc.feed(&r.tex, r.pts, frame_dur))?;
                    #[cfg(feature = "devtools")]
                    perf.fed(r.pts, fed_at);
                    c.pop();
                }
                #[cfg(feature = "devtools")]
                {
                    if enc.pending_input == 0 && c.queued() > 0 {
                        perf.encoder_busy();
                    }
                    if c.full() {
                        perf.dropped();
                    }
                }
                if visible {
                    // Con un juego, nunca: muchos dibujan su propio cursor y el de Windows queda quieto en el medio.
                    cap.draw_cursor(au.game.pid().is_none());
                    cap.check(c.convert(pts, cap.version))?;
                } else {
                    cap.check(c.black(pts))?;
                }
            }
            next_frame = pts + frame_dur;
        }

        trim_video(&mut packets, now - keep);
        au.advance(cfg, frames_at(now) - track::LAG, now);
        if let Some(t) = &mut au.track {
            t.trim_before(now - keep - 20_000_000);
        }

        #[cfg(feature = "devtools")]
        perf.end(now, now_100ns() - woke, fps, || {
            let mb = |b: usize| b as f64 / 1_048_576.0;
            let video = packets.iter().map(|p| p.data.len()).sum();
            let audio = au.game_ch.buf.bytes() + au.desktop.buf.bytes() + au.mic.buf.bytes() + au.track.as_ref().map_or(0, |t| t.bytes());
            (mb(video), packets.len(), mb(audio))
        });
        timer.sleep(next_frame - (now_100ns() - t0));
    }
}

/// Junta en un bloque cada GOP que se cerró con uno de los últimos `added` paquetes (un fotograma clave nuevo cierra
/// el GOP anterior). Ver `Bytes`.
fn seal_gops(p: &mut VecDeque<Packet>, added: usize) {
    let n = p.len();
    for k in n - added..n {
        if !p[k].key {
            continue;
        }
        if let Some(j) = (0..k).rev().find(|&i| p[i].key) {
            Bytes::seal(p.range_mut(j..k).collect());
        }
    }
}

/// Descarta GOPs completos que ya quedaron fuera de la ventana.
fn trim_video(p: &mut VecDeque<Packet>, min_pts: i64) {
    while p.front().is_some_and(|f| !f.key) {
        p.pop_front();
    }
    while let Some(second) = p.iter().skip(1).position(|x| x.key).map(|i| i + 1) {
        if p[second].pts > min_pts {
            break;
        }
        p.drain(..second);
    }
}

/// Arma el trabajo de guardado y lo lanza en otro hilo. Devuelve false si no había nada que guardar.
fn save_clip(cfg: &Config, packets: &VecDeque<Packet>, enc: &Encoder, au: &AudioState, now: i64, frame_dur: i64, notify: &Notify) -> bool {
    let started = std::time::Instant::now();
    let keep = cfg.seconds as i64 * 10_000_000;
    let no_video = || "there's no video in the buffer yet";
    let fail = |m: &str| {
        log(&format!("guardar: se descarta, {m}"));
        notify(Event::SaveFailed(m.into()));
        false
    };
    // En "Solo juegos" sin un juego abierto, todo el buffer es negro: no se guarda un clip vacío.
    if cfg.capture_mode == CaptureMode::GamesOnly && au.game.pid().is_none() {
        return fail("No game detected. In \"Games only\" mode, only a game in full screen is recorded");
    }
    let Some(video_type) = enc.type_blob() else { return fail(no_video()) };
    let min = now - keep;
    let start = packets
        .iter()
        .rposition(|p| p.key && p.pts <= min)
        .or_else(|| packets.iter().position(|p| p.key));
    let Some(start) = start else { return fail(no_video()) };
    // Comparte los bytes con el buffer (cada paquete apunta a su bloque): no copia el video.
    let pk: Vec<Packet> = packets.iter().skip(start).cloned().collect();
    let base = pk[0].pts;
    let end = pk.last().map_or(base, |p| p.pts) + frame_dur;
    // La pista va un poco detrás del presente: lo que le falta hasta el final del video se mezcla ahora con el audio
    // que ya llegó (sin moverla) y se comprime en el hilo que guarda.
    let audio = au.track.as_ref().and_then(|t| t.clip(base, end, &inputs(cfg, &au.game_ch, &au.mic, &au.desktop), &au.masks));
    log(&format!("clip: {} cuadros, {} fotogramas clave", pk.len(), pk.iter().filter(|p| p.key).count()));
    if au.game.pid().is_none() && cfg.game_audio {
        log("clip sin canal de juego: no se detectó ningún juego");
    }

    // Mientras el juego esté abierto, sus clips van a una carpeta con su nombre adentro de la de los clips (aunque se
    // guarde con Alt+Tab en el escritorio). Sin juego, a la de los clips.
    // El nombre del archivo también lleva el juego y un número que sigue al del último clip, para que se sepa de dónde
    // es aunque se mueva de carpeta ("VALORANT 4").
    let game = au.game.name();
    let game_dir = game.as_deref().map(clean_name).filter(|g| !g.is_empty());
    let dir = game_dir.map_or_else(|| cfg.output_dir.clone(), |g| cfg.output_dir.join(g));
    let path = dir.join(format!("{}.mp4", next_stem(&dir, game.as_deref())));
    let notify = notify.clone();
    // Cuánto se frenó el hilo de grabación, cuánto tardó comprimir la cola del audio ("mezcla") y cuánto escribir el MP4
    // (lo lee tools/medir.ps1).
    let held = started.elapsed();
    std::thread::spawn(move || {
        let ms = |d: std::time::Duration| d.as_millis();
        let t = std::time::Instant::now();
        let audio = audio.and_then(|a| {
            let _mf = Mf::init();
            a.complete()
        });
        let mixed = t.elapsed();
        if let Some(a) = &audio {
            let (first, last) = (&a.packets[0], &a.packets[a.packets.len() - 1]);
            let ms = |t: i64| t as f64 / 10_000.0;
            log(&format!(
                "audio del clip: {} paquetes AAC, de {:+.1} ms a {:+.1} ms del video",
                a.packets.len(),
                ms(first.pts - base),
                ms(last.pts + time_of(super::aac::AAC_FRAME as i64) - end)
            ));
        }
        let t = std::time::Instant::now();
        let job = mux::Job { path, video_type, packets: pk, base, frame_dur, audio };
        match mux::write(job) {
            Ok(p) => {
                let (held, mixed, written) = (ms(held), ms(mixed), ms(t.elapsed()));
                log(&format!("clip guardado: {} (grabación frenada {held} ms, mezcla {mixed} ms, MP4 {written} ms)", p.display()));
                #[cfg(feature = "devtools")]
                crate::perf::saved(held as u64, mixed as u64, written as u64);
                notify(Event::Saved(p));
            }
            Err(e) => {
                log(&format!("no se pudo guardar el clip: {e:?}"));
                notify(Event::SaveFailed(e.to_string()));
            }
        }
    });
    true
}

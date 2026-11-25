//! Autoevaluación del rendimiento de la grabadora (solo con `devtools`).
//!
//! `--test-soak <minutos> <carpeta> [escenario] [segundos entre clips]` graba como en el uso real, guarda un clip cada
//! tanto (60 s si no se dice) y cada segundo mide a la grabadora desde adentro: procesador, RAM, memoria de video, uso de
//! cada motor de la placa, handles, hilos y objetos de Windows, más lo que cuenta el propio hilo de grabación (cuadros que
//! llegan al encoder, cuadros perdidos, cuánto trabaja por cuadro, tamaño de los buffers). Al terminar busca fugas
//! (lo que sigue creciendo después de llenarse el buffer) y escribe en `<carpeta>`:
//! - `perf.jsonl`: una muestra por segundo.
//! - `informe.json` y `informe.md`: el resumen, los avisos y el veredicto (también va a la consola).
//!
//! Sale con 0 si todo está bien, 3 si hay algo para revisar y 1 si no pudo medir.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter3, IDXGIFactory1, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W,
    PDH_FMT_DOUBLE, PDH_MORE_DATA,
};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetGuiResources, GetProcessHandleCount, GetProcessTimes, GetThreadTimes, GR_GDIOBJECTS,
    GR_USEROBJECTS,
};

use crate::record::recorder::{Event, Notify, Recorder};

// --- Lo que cuenta el hilo de grabación ----------------------------------------------------------------------------

/// El último segundo del hilo de grabación.
#[derive(Clone, Default, Serialize)]
pub struct Rec {
    /// Cuadros por segundo pedidos.
    pub fps: u32,
    /// Cuadros que recibió el encoder.
    pub fed: u32,
    /// Cuadros salteados porque el hilo se despertó tarde.
    pub missed: u32,
    /// Por qué no llegaron cuadros al encoder (vueltas en este segundo):
    /// - `gpu_late`: el más viejo de la fila seguía sin convertir (la placa no llegó a hacer la conversión);
    /// - `encoder_busy`: había cuadros listos pero el encoder no pidió más;
    /// - `dropped`: la fila de conversiones estaba llena y el cuadro nuevo se descartó (ese se pierde).
    pub gpu_late: u32,
    pub encoder_busy: u32,
    pub dropped: u32,
    /// Cuánto trabajó el hilo por vuelta (ms): promedio y máximo.
    pub busy_avg_ms: f64,
    pub busy_max_ms: f64,
    /// Video y audio en el buffer (MB) y cuántos paquetes de video.
    pub video_mb: f64,
    pub packets: usize,
    pub audio_mb: f64,
    /// Bytes que sacó el encoder en el segundo, tamaño del cuadro más grande (y si fue fotograma clave),
    /// y tiempo máximo (ms) entre mandar un cuadro al encoder y recibir su paquete.
    pub enc_bytes: usize,
    pub enc_max_frame_bytes: usize,
    pub enc_max_frame_key: bool,
    pub enc_lat_max_ms: f64,
}

/// Un clip guardado: cuánto se frenó la grabación, cuánto tardó la mezcla del audio y cuánto la escritura del MP4 (ms).
#[derive(Clone, Serialize)]
pub struct Save {
    pub held_ms: u64,
    pub mix_ms: u64,
    pub mp4_ms: u64,
}

static REC: Mutex<Option<Rec>> = Mutex::new(None);
static SAVES: Mutex<Vec<Save>> = Mutex::new(Vec::new());

/// Lo llama el hilo que guarda cada clip.
pub fn saved(held_ms: u64, mix_ms: u64, mp4_ms: u64) {
    SAVES.lock().unwrap().push(Save { held_ms, mix_ms, mp4_ms });
}

/// La cuenta del hilo de grabación entre una publicación y la siguiente (tiempos en 100 ns).
#[derive(Default)]
pub struct Loop {
    since: i64,
    fed: u32,
    missed: u32,
    gpu_late: u32,
    encoder_busy: u32,
    dropped: u32,
    turns: u32,
    busy_sum: i64,
    busy_max: i64,
    /// (pts, instante en 100 ns) de cada cuadro entregado al encoder que todavía no devolvió paquete.
    fed_at: VecDeque<(i64, i64)>,
    enc_bytes: usize,
    enc_max_frame_bytes: usize,
    enc_max_frame_key: bool,
    enc_lat_max: i64,
}

impl Loop {
    pub fn fed(&mut self, pts: i64, now: i64) {
        self.fed += 1;
        if self.fed_at.len() >= 128 {
            self.fed_at.pop_front();
        }
        self.fed_at.push_back((pts, now));
    }

    /// Un paquete que devolvió el encoder en `now` (100 ns).
    pub fn encoded(&mut self, pts: i64, bytes: usize, key: bool, now: i64) {
        self.enc_bytes += bytes;
        if bytes > self.enc_max_frame_bytes {
            self.enc_max_frame_bytes = bytes;
            self.enc_max_frame_key = key;
        }
        while self.fed_at.front().is_some_and(|&(p, _)| p < pts) {
            self.fed_at.pop_front();
        }
        if self.fed_at.front().is_some_and(|&(p, _)| p == pts) {
            let (_, at) = self.fed_at.pop_front().unwrap();
            self.enc_lat_max = self.enc_lat_max.max(now - at);
        }
    }

    pub fn missed(&mut self, frames: i64) {
        self.missed += frames.max(0) as u32;
    }

    pub fn gpu_late(&mut self) {
        self.gpu_late += 1;
    }

    pub fn encoder_busy(&mut self) {
        self.encoder_busy += 1;
    }

    pub fn dropped(&mut self) {
        self.dropped += 1;
    }

    /// Fin de una vuelta que trabajó `busy`. Una vez por segundo publica lo contado y el tamaño de los buffers
    /// (`buffers`: MB de video, paquetes, MB de audio).
    pub fn end(&mut self, now: i64, busy: i64, fps: u32, buffers: impl FnOnce() -> (f64, usize, f64)) {
        self.turns += 1;
        self.busy_sum += busy;
        self.busy_max = self.busy_max.max(busy);
        if now - self.since < 10_000_000 {
            return;
        }
        let (video_mb, packets, audio_mb) = buffers();
        let ms = |t: i64| t as f64 / 10_000.0;
        *REC.lock().unwrap() = Some(Rec {
            fps,
            fed: self.fed,
            missed: self.missed,
            gpu_late: self.gpu_late,
            encoder_busy: self.encoder_busy,
            dropped: self.dropped,
            busy_avg_ms: ms(self.busy_sum / self.turns.max(1) as i64),
            busy_max_ms: ms(self.busy_max),
            video_mb,
            packets,
            audio_mb,
            enc_bytes: self.enc_bytes,
            enc_max_frame_bytes: self.enc_max_frame_bytes,
            enc_max_frame_key: self.enc_max_frame_key,
            enc_lat_max_ms: ms(self.enc_lat_max),
        });
        let fed_at = std::mem::take(&mut self.fed_at);
        *self = Loop { since: now, fed_at, ..Default::default() };
    }
}

// --- Lo que se mide del proceso --------------------------------------------------------------------------------------

/// Una muestra por segundo.
#[derive(Clone, Serialize)]
struct Sample {
    /// Segundos desde que empezó la grabación.
    t: f64,
    /// Procesador de la grabadora sin contar esta medición, y el de la medición (% de toda la CPU).
    cpu: f64,
    cpu_muestreo: f64,
    privada_mb: f64,
    ws_mb: f64,
    vram_mb: f64,
    handles: u32,
    hilos: u32,
    gdi: u32,
    user: u32,
    /// Uso de la placa: el motor más ocupado y cada tipo de motor (%).
    gpu: f64,
    gpu_3d: f64,
    gpu_copia: f64,
    gpu_encoder: f64,
    gpu_conversion: f64,
    gpu_otros: f64,
    /// Uso total de la placa de todos los procesos (%): motor 3D y encoder.
    gpu_total_3d: f64,
    gpu_total_encoder: f64,
    /// Se estaba guardando un clip.
    guardando: bool,
    rec: Option<Rec>,
}

/// Mide este proceso.
struct Meter {
    cores: f64,
    pid: u32,
    at: Instant,
    cpu: u64,
    own: u64,
    adapters: Vec<IDXGIAdapter3>,
    gpu: Option<Gpu>,
}

fn ticks(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// (procesador de todo el proceso, de este hilo), en 100 ns.
fn cpu_times() -> (u64, u64) {
    let [mut c, mut e, mut k, mut u, mut tc, mut te, mut tk, mut tu] = [FILETIME::default(); 8];
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
        let _ = GetThreadTimes(GetCurrentThread(), &mut tc, &mut te, &mut tk, &mut tu);
    }
    (ticks(k) + ticks(u), ticks(tk) + ticks(tu))
}

fn thread_count(pid: u32) -> u32 {
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else { return 0 };
        let mut e = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut n = 0;
        if Thread32First(snap, &mut e).is_ok() {
            loop {
                n += (e.th32OwnerProcessID == pid) as u32;
                if Thread32Next(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        n
    }
}

impl Meter {
    fn new() -> Self {
        let adapters = unsafe {
            let mut v = Vec::new();
            if let Ok(f) = CreateDXGIFactory1::<IDXGIFactory1>() {
                let mut i = 0;
                while let Ok(a) = f.EnumAdapters1(i) {
                    i += 1;
                    if let Ok(a) = a.cast::<IDXGIAdapter3>() {
                        v.push(a);
                    }
                }
            }
            v
        };
        let (cpu, own) = cpu_times();
        Meter {
            cores: std::thread::available_parallelism().map_or(1, |n| n.get()) as f64,
            pid: std::process::id(),
            at: Instant::now(),
            cpu,
            own,
            adapters,
            gpu: Gpu::open(),
        }
    }

    fn sample(&mut self, t: f64, guardando: bool) -> Sample {
        let wall = self.at.elapsed().as_secs_f64().max(0.001) * 10_000_000.0;
        self.at = Instant::now();
        let (cpu, own) = cpu_times();
        let pct = |d: u64| d as f64 / wall / self.cores * 100.0;
        let own_d = own.saturating_sub(self.own);
        let all_d = cpu.saturating_sub(self.cpu);
        (self.cpu, self.own) = (cpu, own);

        let mut mem = PROCESS_MEMORY_COUNTERS_EX { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
        let mut handles = 0u32;
        let (gdi, user, vram) = unsafe {
            let p = GetCurrentProcess();
            let _ = K32GetProcessMemoryInfo(p, &mut mem as *mut _ as *mut PROCESS_MEMORY_COUNTERS, mem.cb);
            let _ = GetProcessHandleCount(p, &mut handles);
            let vram: u64 = self
                .adapters
                .iter()
                .filter_map(|a| {
                    let mut i = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                    a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut i).ok().map(|_| i.CurrentUsage)
                })
                .sum();
            (GetGuiResources(p, GR_GDIOBJECTS), GetGuiResources(p, GR_USEROBJECTS), vram)
        };
        let (g, gpu_total_3d, gpu_total_encoder) = self.gpu.as_mut().map(|g| g.read(self.pid)).unwrap_or_default();
        let mb = |b: u64| b as f64 / 1_048_576.0;
        Sample {
            t,
            cpu: pct(all_d.saturating_sub(own_d)),
            cpu_muestreo: pct(own_d),
            privada_mb: mb(mem.PrivateUsage as u64),
            ws_mb: mb(mem.WorkingSetSize as u64),
            vram_mb: mb(vram),
            handles,
            hilos: thread_count(self.pid),
            gdi,
            user,
            gpu: g.iter().cloned().fold(0.0, f64::max).min(100.0),
            gpu_3d: g[0],
            gpu_copia: g[1],
            gpu_encoder: g[2],
            gpu_conversion: g[3],
            gpu_otros: g[4],
            gpu_total_3d,
            gpu_total_encoder,
            guardando,
            rec: REC.lock().unwrap().clone(),
        }
    }
}

/// Uso de cada motor de la placa por proceso, con los contadores de Windows que usa el Administrador de tareas
/// (por su nombre en inglés: andan en cualquier idioma).
struct Gpu {
    query: isize,
    counter: isize,
    buf: Vec<u64>,
}

impl Gpu {
    fn open() -> Option<Gpu> {
        unsafe {
            let (mut query, mut counter) = (0isize, 0isize);
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            if PdhAddEnglishCounterW(query, w!("\\GPU Engine(*)\\Utilization Percentage"), 0, &mut counter) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            // El uso sale de la diferencia entre dos lecturas: esta es la primera.
            PdhCollectQueryData(query);
            Some(Gpu { query, counter, buf: Vec::new() })
        }
    }

    /// (3D, copia, encoder, conversión y otros de `pid`; total 3D de todos los procesos; total encoder de todos).
    fn read(&mut self, pid: u32) -> ([f64; 5], f64, f64) {
        let mut own = [0.0; 5];
        let mut total_3d: HashMap<String, f64> = HashMap::new();
        let mut total_enc: HashMap<String, f64> = HashMap::new();
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return (own, 0.0, 0.0);
            }
            let (mut size, mut count) = (0u32, 0u32);
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None) != PDH_MORE_DATA {
                return (own, 0.0, 0.0);
            }
            self.buf.resize((size as usize).div_ceil(8), 0);
            let items = self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
                return (own, 0.0, 0.0);
            }
            let prefix = format!("pid_{pid}_");
            for it in std::slice::from_raw_parts(items, count as usize) {
                // 0 o 1: dato válido (o nuevo).
                if it.FmtValue.CStatus > 1 {
                    continue;
                }
                let val = it.FmtValue.Anonymous.doubleValue;
                if !val.is_finite() || val < 0.0 {
                    continue;
                }
                let name = it.szName.to_string().unwrap_or_default();
                let kind = match name.rsplit("engtype_").next().unwrap_or("") {
                    "3D" => 0,
                    "Copy" => 1,
                    "VideoEncode" => 2,
                    "VideoProcessing" => 3,
                    _ => 4,
                };
                if name.starts_with(&prefix) {
                    own[kind] += val;
                }
                // Todos los procesos que comparten el mismo motor físico tienen el mismo sufijo `_luid_...`.
                if kind == 0 || kind == 2 {
                    let eng = name.find("_luid_").map_or(name.as_str(), |i| &name[i..]);
                    let map = if kind == 0 { &mut total_3d } else { &mut total_enc };
                    *map.entry(eng.to_string()).or_insert(0.0) += val;
                }
            }
        }
        let peak = |m: HashMap<String, f64>| m.into_values().fold(0.0f64, f64::max).min(100.0);
        (own, peak(total_3d), peak(total_enc))
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

// --- La prueba -------------------------------------------------------------------------------------------------------

pub fn soak(args: &[String]) -> i32 {
    let minutes: f64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(10.0);
    let Some(dir) = args.get(1).map(std::path::PathBuf::from) else {
        println!("uso: --test-soak <minutos> <carpeta> [escenario|-] [segundos entre clips]");
        return 1;
    };
    // "-": el escenario de config.json, para poder dar los segundos entre clips.
    let scenario = args.get(2).map(|s| s.as_str()).filter(|s| *s != "-");
    let every: u64 = args.get(3).and_then(|s| s.parse().ok()).filter(|&s| s >= 5).unwrap_or(60);
    if std::fs::create_dir_all(&dir).is_err() {
        println!("ERROR no se pudo crear {}", dir.display());
        return 1;
    }
    let cfg = crate::devtools::test_config(dir.clone(), scenario);
    let warmup = cfg.seconds as f64 + 15.0;
    let mut meter = Meter::new();
    // Una vuelta corta antes de medir: lo que Windows carga una sola vez (las DLL de Media Foundation y del encoder de
    // la placa, ~95 handles) ya queda cargado, y `al_cerrar` muestra solo lo que deja la grabadora. Su clip va aparte.
    let mut first = cfg.clone();
    first.output_dir = dir.join("calentamiento");
    let _ = std::fs::create_dir_all(&first.output_dir);
    println!("calentamiento: grabadora 5 s y un clip ({})", if session_once(&first, 5) { "guardado" } else { "NO guardado" });
    let before = meter.sample(0.0, false);

    let events: Arc<Mutex<Vec<(Instant, Event)>>> = Arc::default();
    let sink = events.clone();
    let notify: Notify = Arc::new(move |e| sink.lock().unwrap().push((Instant::now(), e)));
    let start = Instant::now();
    let rec = Recorder::start(cfg.clone(), notify);
    let Ok(file) = std::fs::File::create(dir.join("perf.jsonl")) else {
        println!("ERROR no se pudo escribir en {}", dir.display());
        return 1;
    };
    let mut out = std::io::BufWriter::new(file);
    println!("midiendo {minutes} min (un clip cada {every} s; el buffer se llena en {} s)", cfg.seconds);

    let mut samples = Vec::new();
    let mut log = vec![format!("un clip cada {every} s")];
    let (mut encoder, mut saving, mut ok, mut failed) = (String::new(), false, 0u32, Vec::new());
    let mut next_save = every as f64;
    let total = minutes * 60.0;
    let mut tick = 1.0;
    // Después del final se espera a que termine el último clip que se estaba guardando.
    while tick <= total || (saving && tick <= total + 30.0) {
        let wait = tick - start.elapsed().as_secs_f64();
        if wait > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(wait));
        }
        for (at, e) in std::mem::take(&mut *events.lock().unwrap()) {
            let t = at.saturating_duration_since(start).as_secs_f64();
            match e {
                Event::Started(d) => encoder = format!("{d:?}"),
                Event::Game(g) => log.push(format!("{t:.0} s: juego {}", g.as_deref().unwrap_or("ninguno"))),
                Event::Saved(_) => (saving, ok) = (false, ok + 1),
                Event::SaveFailed(m) => {
                    saving = false;
                    failed.push(format!("{t:.0} s: {m}"));
                }
                Event::Error(m) => failed.push(format!("{t:.0} s: error de grabación: {m}")),
            }
        }
        let s = meter.sample(start.elapsed().as_secs_f64(), saving);
        let _ = writeln!(out, "{}", serde_json::to_string(&s).unwrap_or_default());
        let _ = out.flush();
        live(&s, samples.last());
        samples.push(s);
        if tick >= next_save && tick <= total {
            rec.save();
            saving = true;
            next_save += every as f64;
        }
        tick += 1.0;
    }
    drop(rec);
    // Lo que queda después de cerrar la grabadora (hilos y handles que no se soltaron).
    std::thread::sleep(Duration::from_secs(2));
    let after = meter.sample(start.elapsed().as_secs_f64(), false);

    let report = analyze(&Input { scenario, minutes, warmup, encoder, ok, failed, log, before, after }, &samples);
    let md = report.markdown();
    let _ = std::fs::write(dir.join("informe.json"), serde_json::to_string_pretty(&report).unwrap_or_default());
    let _ = std::fs::write(dir.join("informe.md"), &md);
    println!("{md}");
    if report.veredicto == "OK" {
        0
    } else {
        3
    }
}

/// `--test-restart [vueltas] <carpeta>`: arranca la grabadora, graba 15 s, guarda un clip y la cierra, varias veces en
/// el mismo proceso (3 si no se dice). Después de cada vuelta cuenta hilos y handles: si lo que queda después de cerrar
/// es igual en cada vuelta, son cosas que Windows carga una sola vez (las DLL de Media Foundation o del encoder de la
/// placa); si crece en cada vuelta, cada sesión deja algo sin soltar. Sale con 0 si no crece y 3 si crece.
pub fn restart(args: &[String]) -> i32 {
    let (rounds, dir) = match args.first().and_then(|s| s.parse::<u32>().ok()) {
        Some(n) => (n.max(2), args.get(1)),
        None => (3, args.first()),
    };
    let Some(dir) = dir.map(std::path::PathBuf::from) else {
        println!("uso: --test-restart [vueltas] <carpeta>");
        return 1;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        println!("ERROR no se pudo crear {}", dir.display());
        return 1;
    }
    let cfg = crate::devtools::test_config(dir, None);
    let mut meter = Meter::new();
    let before = meter.sample(0.0, false);
    println!("antes de grabar: {} handles, {} hilos", before.handles, before.hilos);
    let mut after = Vec::new();
    for round in 1..=rounds {
        let saved = session_once(&cfg, 15);
        let s = meter.sample(0.0, false);
        println!(
            "vuelta {round}: {} handles ({:+} contra antes de grabar), {} hilos ({:+}), clip {}",
            s.handles,
            s.handles as i64 - before.handles as i64,
            s.hilos,
            s.hilos as i64 - before.hilos as i64,
            if saved { "guardado" } else { "NO guardado" }
        );
        after.push(s);
    }
    // Desde la segunda vuelta: lo de la primera puede ser lo que se carga una sola vez.
    let per_round = |f: fn(&Sample) -> u32| (f(&after[after.len() - 1]) as f64 - f(&after[0]) as f64) / (after.len() - 1) as f64;
    let (h, t) = (per_round(|s| s.handles), per_round(|s| s.hilos));
    if h > 5.0 || t > 0.5 {
        println!("REVISAR: cada sesión deja {h:.1} handles y {t:.1} hilos sin soltar");
        3
    } else {
        println!("OK: lo que queda después de cerrar es de una sola vez ({h:+.1} handles y {t:+.1} hilos por vuelta desde la segunda)");
        0
    }
}

/// Arranca la grabadora, graba `secs` segundos, guarda un clip, la cierra y espera 2 s a que se suelte todo. Devuelve
/// si el clip se guardó.
fn session_once(cfg: &crate::config::Config, secs: u64) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let notify: Notify = Arc::new(move |e| {
        let _ = tx.send(e);
    });
    let rec = Recorder::start(cfg.clone(), notify);
    std::thread::sleep(Duration::from_secs(secs));
    rec.save();
    let saved = (0..10).any(|_| matches!(rx.recv_timeout(Duration::from_secs(1)), Ok(Event::Saved(_))));
    drop(rec);
    std::thread::sleep(Duration::from_secs(2));
    saved
}

/// Lo que se ve en la consola mientras graba (para seguirla en vivo): un "ESTADO" por minuto y un "AVISO" cuando algo
/// se sale de lo normal.
fn live(s: &Sample, prev: Option<&Sample>) {
    let t = s.t.round();
    if (t as u64).is_multiple_of(60) {
        println!(
            "ESTADO {t} s: CPU {:.1} %, RAM {:.0} MB, VRAM {:.0} MB, placa {:.1} %, {} handles, {} hilos",
            s.cpu, s.privada_mb, s.vram_mb, s.gpu, s.handles, s.hilos
        );
    }
    let Some(r) = s.rec.as_ref().filter(|_| s.t >= 3.0) else { return };
    let budget = 1000.0 / r.fps.max(1) as f64;
    if (r.fed as f64) < r.fps as f64 * 0.9 {
        println!(
            "AVISO {t} s: el encoder recibió {} cuadros de {} (conversión sin terminar {}, encoder sin pedir {}, descartados con la fila llena {})",
            r.fed, r.fps, r.gpu_late, r.encoder_busy, r.dropped
        );
    }
    if r.missed > 0 {
        println!("AVISO {t} s: se saltearon {} cuadros por despertarse tarde", r.missed);
    }
    if r.busy_max_ms > budget {
        println!("AVISO {t} s: una vuelta del hilo de grabación tardó {:.1} ms (un cuadro dura {budget:.1} ms)", r.busy_max_ms);
    }
    if let Some(p) = prev.filter(|p| !s.guardando && !p.guardando && s.privada_mb - p.privada_mb > 50.0) {
        println!("AVISO {t} s: la RAM privada subió {:.0} MB en un segundo sin guardar un clip", s.privada_mb - p.privada_mb);
    }
}

// --- El informe ------------------------------------------------------------------------------------------------------

struct Input<'a> {
    scenario: Option<&'a str>,
    minutes: f64,
    warmup: f64,
    encoder: String,
    ok: u32,
    failed: Vec<String>,
    log: Vec<String>,
    before: Sample,
    after: Sample,
}

#[derive(Serialize)]
struct Stat {
    prom: f64,
    p95: f64,
    max: f64,
}

fn stat(v: impl Iterator<Item = f64>) -> Stat {
    let mut v: Vec<f64> = v.filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return Stat { prom: 0.0, p95: 0.0, max: 0.0 };
    }
    v.sort_by(f64::total_cmp);
    let r = |x: f64| (x * 100.0).round() / 100.0;
    Stat { prom: r(v.iter().sum::<f64>() / v.len() as f64), p95: r(v[(v.len() - 1) * 95 / 100]), max: r(v[v.len() - 1]) }
}

/// Los segundos con tirones y sus causas (sumadas en esos segundos; ver `Rec`).
#[derive(Serialize)]
struct Tirones {
    segundos: u32,
    conversion_sin_terminar: u32,
    encoder_sin_pedir: u32,
    descartados_fila_llena: u32,
    /// Lo que más se repitió.
    causa: &'static str,
    /// Cuándo (segundos desde el arranque).
    cuando: Vec<u32>,
    /// Qué pasaba en la placa (todos los procesos) y en el encoder durante esos segundos.
    gpu_total_3d_pct: Stat,
    gpu_total_encoder_pct: Stat,
    enc_mbps: Stat,
    enc_cuadro_max_kb: Stat,
    /// En cuántos de los segundos con tirones el cuadro más grande fue un fotograma clave.
    cuadro_max_clave: u32,
    enc_latencia_max_ms: Stat,
}

/// Un recurso que se revisa por fugas.
#[derive(Serialize)]
struct Leak {
    recurso: &'static str,
    unidad: &'static str,
    /// Al terminar de llenarse el buffer y al final (el piso de cada tramo de 30 s: sin los picos de guardar).
    inicio: f64,
    fin: f64,
    pendiente_por_min: f64,
    veredicto: &'static str,
    /// Tramos que no se miraron porque el buffer se había achicado (ver `analyze`).
    tramos_descartados: usize,
}

#[derive(Serialize)]
struct Report {
    escenario: String,
    minutos: f64,
    encoder: String,
    fps_objetivo: u32,
    clips_ok: u32,
    clips_fallidos: Vec<String>,
    cpu_pct: Stat,
    cpu_guardando_pct: Stat,
    cpu_muestreo_pct: f64,
    gpu_pct: Stat,
    gpu_por_motor_prom: [(&'static str, f64); 4],
    gpu_total_3d_pct: Stat,
    gpu_total_encoder_pct: Stat,
    ram_privada_mb: Stat,
    vram_mb: Stat,
    fps_real: Stat,
    cuadros_perdidos: u32,
    /// Segundos con menos del 90 % de los cuadros al encoder, y qué pasó en esos segundos.
    tirones: Tirones,
    enc_latencia_max_ms: Stat,
    trabajo_por_cuadro_ms: Stat,
    presupuesto_por_cuadro_ms: f64,
    guardado_frenada_ms: Stat,
    guardado_mezcla_ms: Stat,
    guardado_mp4_ms: Stat,
    fugas: Vec<Leak>,
    al_cerrar: String,
    eventos: Vec<String>,
    avisos: Vec<String>,
    veredicto: &'static str,
}

/// Piso (mínimo) de cada tramo de 30 s después del calentamiento: (minuto del tramo, valor).
fn floors(samples: &[&Sample], f: impl Fn(&Sample) -> f64) -> Vec<(f64, f64)> {
    let Some(first) = samples.first() else { return Vec::new() };
    let mut out: Vec<(f64, f64)> = Vec::new();
    let mut window = -1i64;
    for s in samples {
        let w = ((s.t - first.t) / 30.0) as i64;
        let v = f(s);
        if w != window {
            window = w;
            out.push((s.t / 60.0, v));
        } else if let Some(last) = out.last_mut() {
            last.1 = last.1.min(v);
        }
    }
    out
}

/// Envolvente inferior: en cada punto, el mínimo de ese punto y todos los siguientes.
/// Así un salto temporal que después vuelve a bajar (el heap reteniendo memoria un rato) no suma a la pendiente;
/// solo cuenta una subida que no vuelve a bajar.
fn lower_envelope(p: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = p.to_vec();
    let mut min = f64::INFINITY;
    for pt in out.iter_mut().rev() {
        min = min.min(pt.1);
        pt.1 = min;
    }
    out
}

/// Pendiente por minuto (mínimos cuadrados).
fn slope(p: &[(f64, f64)]) -> f64 {
    let n = p.len() as f64;
    let (mx, my) = (p.iter().map(|x| x.0).sum::<f64>() / n, p.iter().map(|x| x.1).sum::<f64>() / n);
    let num: f64 = p.iter().map(|(x, y)| (x - mx) * (y - my)).sum();
    let den: f64 = p.iter().map(|(x, _)| (x - mx).powi(2)).sum();
    if den > 0.0 {
        num / den
    } else {
        0.0
    }
}

fn analyze(i: &Input, samples: &[Sample]) -> Report {
    let steady: Vec<&Sample> = samples.iter().filter(|s| s.t >= i.warmup).collect();
    let recs: Vec<&Rec> = samples.iter().filter(|s| s.t >= 3.0).filter_map(|s| s.rec.as_ref()).collect();
    let fps = recs.first().map_or(0, |r| r.fps);
    let budget = if fps > 0 { 1000.0 / fps as f64 } else { 0.0 };
    let saves = SAVES.lock().unwrap().clone();
    let mut avisos = Vec::new();

    // Tramos con el buffer achicado: el piso del buffer de video y audio quedó por debajo del 75 % del más alto hasta ese
    // momento (la imagen se quedó quieta y pesa menos). Ahí el heap no devuelve toda la memoria que soltó el buffer y
    // "RAM fuera de los buffers" sube de un escalón aunque no haya fuga, así que esos tramos no se miran para ese recurso.
    let buffers = |s: &Sample| s.rec.as_ref().map_or(0.0, |r| r.video_mb + r.audio_mb);
    let mut top = 0.0f64;
    let shrunk: Vec<bool> = floors(&steady, buffers)
        .iter()
        .map(|&(_, b)| {
            top = top.max(b);
            b < top * 0.75
        })
        .collect();

    // Recurso, unidad, cómo se lee, cuánto puede subir por minuto y en total sin que sea una fuga, si se descartan los
    // tramos con el buffer achicado (y se usa envolvente inferior), y duración mínima en minutos para dar veredicto.
    type Get = fn(&Sample) -> f64;
    let checks: [(&'static str, &'static str, Get, f64, f64, bool, f64); 7] = [
        ("RAM privada", "MB", |s| s.privada_mb, 0.5, 5.0, false, 0.0),
        ("RAM fuera de los buffers de video y audio", "MB", |s| s.privada_mb - s.rec.as_ref().map_or(0.0, |r| r.video_mb + r.audio_mb), 0.5, 5.0, true, 30.0),
        ("memoria de video", "MB", |s| s.vram_mb, 0.5, 5.0, false, 0.0),
        ("handles", "", |s| s.handles as f64, 2.0, 20.0, false, 0.0),
        ("hilos", "", |s| s.hilos as f64, 0.2, 3.0, false, 0.0),
        ("objetos GDI", "", |s| s.gdi as f64, 1.0, 10.0, false, 0.0),
        ("objetos USER", "", |s| s.user as f64, 1.0, 10.0, false, 0.0),
    ];
    let mut fugas = Vec::new();
    for (recurso, unidad, get, per_min, total, skip_shrunk, min_minutes) in checks {
        let all = floors(&steady, get);
        let f: Vec<(f64, f64)> = all.iter().zip(&shrunk).filter(|(_, &s)| !(skip_shrunk && s)).map(|(x, _)| *x).collect();
        let tramos_descartados = all.len() - f.len();
        let f = if skip_shrunk { lower_envelope(&f) } else { f };
        let r = |x: f64| (x * 100.0).round() / 100.0;
        let (inicio, fin) = (f.first().map_or(0.0, |x| x.1), f.last().map_or(0.0, |x| x.1));
        let m = if f.len() >= 3 { slope(&f) } else { 0.0 };
        let veredicto = if f.len() < 4 || i.minutes < min_minutes {
            "corrida corta para saber"
        } else if m > per_min && fin - inicio > total {
            avisos.push(format!("Posible fuga de {recurso}: sube {:.2} {unidad}/min ({inicio:.1} → {fin:.1}).", m));
            "posible fuga"
        } else {
            "estable"
        };
        fugas.push(Leak { recurso, unidad, inicio: r(inicio), fin: r(fin), pendiente_por_min: r(m), veredicto, tramos_descartados });
    }

    let fps_real = stat(recs.iter().map(|r| r.fed as f64));
    let dips: Vec<(&Sample, &Rec)> = samples
        .iter()
        .filter(|s| s.t >= 3.0)
        .filter_map(|s| s.rec.as_ref().map(|r| (s, r)))
        .filter(|(_, r)| (r.fed as f64) < r.fps as f64 * 0.9)
        .collect();
    let sum = |f: fn(&Rec) -> u32| dips.iter().map(|(_, r)| f(r)).sum::<u32>();
    let (late, busy_enc, dropped) = (sum(|r| r.gpu_late), sum(|r| r.encoder_busy), sum(|r| r.dropped));
    let causa = if dips.is_empty() {
        "sin tirones"
    } else if dropped >= late.max(busy_enc) && dropped > 0 {
        "la fila de conversiones se llena: la placa convierte tarde y se descartan cuadros"
    } else if late >= busy_enc && late > 0 {
        "la conversión no termina a tiempo (la placa la atiende detrás del juego)"
    } else if busy_enc > 0 {
        "el encoder no pide cuadros (encoder saturado)"
    } else {
        "ninguna de las tres: no llegaron imágenes nuevas a convertir"
    };
    if !dips.is_empty() {
        avisos.push(format!("{} segundos con tirones (menos del 90 % de los cuadros al encoder). Causa: {causa}.", dips.len()));
    }
    let tirones = Tirones {
        segundos: dips.len() as u32,
        conversion_sin_terminar: late,
        encoder_sin_pedir: busy_enc,
        descartados_fila_llena: dropped,
        causa,
        cuando: dips.iter().map(|(s, _)| s.t.round() as u32).collect(),
        gpu_total_3d_pct: stat(dips.iter().map(|(s, _)| s.gpu_total_3d)),
        gpu_total_encoder_pct: stat(dips.iter().map(|(s, _)| s.gpu_total_encoder)),
        enc_mbps: stat(dips.iter().map(|(_, r)| r.enc_bytes as f64 * 8.0 / 1_000_000.0)),
        enc_cuadro_max_kb: stat(dips.iter().map(|(_, r)| r.enc_max_frame_bytes as f64 / 1024.0)),
        cuadro_max_clave: dips.iter().filter(|(_, r)| r.enc_max_frame_key).count() as u32,
        enc_latencia_max_ms: stat(dips.iter().map(|(_, r)| r.enc_lat_max_ms)),
    };
    let missed: u32 = recs.iter().map(|r| r.missed).sum();
    let busy = stat(recs.iter().map(|r| r.busy_max_ms));
    if fps > 0 && fps_real.prom < fps as f64 * 0.97 {
        avisos.push(format!("El encoder recibe {:.1} cuadros por segundo de {fps}: el video pierde cuadros.", fps_real.prom));
    }
    if missed > 0 {
        avisos.push(format!("El hilo de grabación se despertó tarde y salteó {missed} cuadros."));
    }
    if budget > 0.0 && busy.max > budget {
        avisos.push(format!("Alguna vuelta del hilo de grabación tardó {:.1} ms, más que un cuadro ({budget:.1} ms).", busy.max));
    } else if budget > 0.0 && busy.p95 > budget * 0.5 {
        avisos.push(format!("El hilo de grabación usa más de la mitad del tiempo de un cuadro ({:.1} de {budget:.1} ms).", busy.p95));
    }
    let held = stat(saves.iter().map(|s| s.held_ms as f64));
    if budget > 0.0 && held.max > budget {
        avisos.push(format!("Guardar un clip frenó la grabación {:.0} ms (más que un cuadro).", held.max));
    }
    let cpu = stat(steady.iter().filter(|s| !s.guardando).map(|s| s.cpu));
    if cpu.prom > 5.0 {
        avisos.push(format!("La grabadora usa {:.1} % del procesador en promedio grabando.", cpu.prom));
    }
    if !i.failed.is_empty() {
        avisos.push(format!("{} clips o errores fallaron (ver clips_fallidos).", i.failed.len()));
    }
    let left = i.after.hilos as i64 - i.before.hilos as i64;
    let left_h = i.after.handles as i64 - i.before.handles as i64;
    let al_cerrar = format!(
        "al cerrar la grabadora quedaron {left:+} hilos y {left_h:+} handles respecto de antes de empezar (ya con lo que Windows carga una sola vez, por la vuelta de calentamiento)"
    );
    if left > 2 || left_h > 20 {
        avisos.push(format!(
            "Al cerrar la grabadora no se soltó todo: {left:+} hilos y {left_h:+} handles. Confirmalo con --test-restart: si crece en cada vuelta, cada sesión deja algo."
        ));
    }
    if i.minutes * 60.0 < i.warmup + 120.0 {
        avisos.push(format!("La corrida es corta: para buscar fugas conviene grabar al menos {:.0} min.", (i.warmup + 300.0) / 60.0));
    }
    let leak = fugas.iter().any(|f| f.veredicto == "posible fuga");
    let veredicto = if leak || !i.failed.is_empty() || (fps > 0 && fps_real.prom < fps as f64 * 0.97) { "REVISAR" } else { "OK" };

    let motor = |f: fn(&Sample) -> f64| stat(steady.iter().map(|s| f(s))).prom;
    Report {
        escenario: i.scenario.unwrap_or("config.json").to_string(),
        minutos: i.minutes,
        encoder: i.encoder.clone(),
        fps_objetivo: fps,
        clips_ok: i.ok,
        clips_fallidos: i.failed.clone(),
        cpu_pct: cpu,
        cpu_guardando_pct: stat(samples.iter().filter(|s| s.guardando).map(|s| s.cpu)),
        cpu_muestreo_pct: stat(samples.iter().map(|s| s.cpu_muestreo)).prom,
        gpu_pct: stat(steady.iter().map(|s| s.gpu)),
        gpu_por_motor_prom: [
            ("3d", motor(|s| s.gpu_3d)),
            ("copia", motor(|s| s.gpu_copia)),
            ("encoder", motor(|s| s.gpu_encoder)),
            ("conversion", motor(|s| s.gpu_conversion)),
        ],
        gpu_total_3d_pct: stat(steady.iter().map(|s| s.gpu_total_3d)),
        gpu_total_encoder_pct: stat(steady.iter().map(|s| s.gpu_total_encoder)),
        ram_privada_mb: stat(steady.iter().map(|s| s.privada_mb)),
        vram_mb: stat(steady.iter().map(|s| s.vram_mb)),
        fps_real,
        cuadros_perdidos: missed,
        tirones,
        enc_latencia_max_ms: stat(recs.iter().map(|r| r.enc_lat_max_ms)),
        trabajo_por_cuadro_ms: busy,
        presupuesto_por_cuadro_ms: (budget * 100.0).round() / 100.0,
        guardado_frenada_ms: held,
        guardado_mezcla_ms: stat(saves.iter().map(|s| s.mix_ms as f64)),
        guardado_mp4_ms: stat(saves.iter().map(|s| s.mp4_ms as f64)),
        fugas,
        al_cerrar,
        eventos: i.log.clone(),
        avisos,
        veredicto,
    }
}

impl Report {
    fn markdown(&self) -> String {
        let st = |s: &Stat| format!("{} prom · {} p95 · {} máx", s.prom, s.p95, s.max);
        let mut m = format!(
            "# Rendimiento de la grabadora: {}\n\n{} min, escenario {}, encoder {}, {} clips guardados.\n\n",
            self.veredicto, self.minutos, self.escenario, self.encoder, self.clips_ok
        );
        m += "## Avisos\n\n";
        if self.avisos.is_empty() {
            m += "Ninguno.\n";
        }
        for a in &self.avisos {
            m += &format!("- {a}\n");
        }
        m += "\n## Consumo grabando (después de llenarse el buffer)\n\n";
        m += &format!("- Procesador: {} %\n", st(&self.cpu_pct));
        m += &format!("- Procesador al guardar: {} %\n", st(&self.cpu_guardando_pct));
        m += &format!("- Placa (motor más ocupado): {} %\n", st(&self.gpu_pct));
        let motores: Vec<String> = self.gpu_por_motor_prom.iter().map(|(k, v)| format!("{k} {v} %")).collect();
        m += &format!("- Placa por motor (promedio): {}\n", motores.join(", "));
        m += &format!(
            "- Placa total (todos los procesos): 3D {} % · encoder {} %\n",
            st(&self.gpu_total_3d_pct),
            st(&self.gpu_total_encoder_pct)
        );
        m += &format!("- RAM privada: {} MB\n", st(&self.ram_privada_mb));
        m += &format!("- Memoria de video: {} MB\n", st(&self.vram_mb));
        m += &format!("- Esta medición usó {:.2} % del procesador (ya descontado arriba)\n", self.cpu_muestreo_pct);
        m += "\n## Video\n\n";
        m += &format!("- Cuadros por segundo al encoder: {} (pedidos: {})\n", st(&self.fps_real), self.fps_objetivo);
        m += &format!("- Cuadros salteados por despertarse tarde: {}\n", self.cuadros_perdidos);
        let tr = &self.tirones;
        m += &format!(
            "- Segundos con tirones: {} ({}). Conversión sin terminar {}, encoder sin pedir {}, descartados con la fila llena {}\n",
            tr.segundos, tr.causa, tr.conversion_sin_terminar, tr.encoder_sin_pedir, tr.descartados_fila_llena
        );
        if tr.segundos > 0 {
            m += &format!(
                "  - En esos segundos: placa total 3D {} % · encoder total {} % · salida {} Mbps · cuadro más grande {} KB (clave en {}/{}) · latencia del encoder {} ms\n",
                st(&tr.gpu_total_3d_pct),
                st(&tr.gpu_total_encoder_pct),
                st(&tr.enc_mbps),
                st(&tr.enc_cuadro_max_kb),
                tr.cuadro_max_clave,
                tr.segundos,
                st(&tr.enc_latencia_max_ms),
            );
        }
        m += &format!("- Latencia del encoder (máximo de cada segundo): {} ms\n", st(&self.enc_latencia_max_ms));
        m += &format!(
            "- Trabajo del hilo por vuelta (máximo de cada segundo): {} ms, de {} ms por cuadro\n",
            st(&self.trabajo_por_cuadro_ms),
            self.presupuesto_por_cuadro_ms
        );
        m += "\n## Guardar un clip\n\n";
        m += &format!("- Grabación frenada: {} ms\n", st(&self.guardado_frenada_ms));
        m += &format!("- Mezcla del audio: {} ms\n", st(&self.guardado_mezcla_ms));
        m += &format!("- Escritura del MP4: {} ms\n", st(&self.guardado_mp4_ms));
        for f in &self.clips_fallidos {
            m += &format!("- Falló: {f}\n");
        }
        m += "\n## Fugas (piso de cada tramo de 30 s, sin los picos de guardar)\n\n| Recurso | Al llenarse el buffer | Al final | Por minuto | Veredicto |\n|---|---|---|---|---|\n";
        for f in &self.fugas {
            m += &format!("| {} | {} {} | {} {} | {:+} | {} |\n", f.recurso, f.inicio, f.unidad, f.fin, f.unidad, f.pendiente_por_min, f.veredicto);
        }
        for f in self.fugas.iter().filter(|f| f.tramos_descartados > 0) {
            m += &format!(
                "\n{}: no se miraron {} tramos de 30 s con el buffer achicado (la imagen se quedó quieta y el heap no devuelve todo lo que soltó el buffer).\n",
                capitalize(f.recurso),
                f.tramos_descartados
            );
        }
        if self.minutos < 30.0 {
            m += "\nRAM fuera de los buffers de video y audio: hace falta una corrida de al menos 30 min para confirmar fuga (en corridas más cortas el heap puede subir y bajar ~40 MB sin que haya fuga).\n";
        }
        m += &format!("\n{}.\n", capitalize(&self.al_cerrar));
        if !self.eventos.is_empty() {
            m += "\n## Eventos\n\n";
            for e in &self.eventos {
                m += &format!("- {e}\n");
            }
        }
        m
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

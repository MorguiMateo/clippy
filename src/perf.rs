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
    /// Cuánto trabajó el hilo por vuelta (ms): promedio y máximo.
    pub busy_avg_ms: f64,
    pub busy_max_ms: f64,
    /// Video y audio en el buffer (MB) y cuántos paquetes de video.
    pub video_mb: f64,
    pub packets: usize,
    pub audio_mb: f64,
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
    turns: u32,
    busy_sum: i64,
    busy_max: i64,
}

impl Loop {
    pub fn fed(&mut self) {
        self.fed += 1;
    }

    pub fn missed(&mut self, frames: i64) {
        self.missed += frames.max(0) as u32;
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
            busy_avg_ms: ms(self.busy_sum / self.turns.max(1) as i64),
            busy_max_ms: ms(self.busy_max),
            video_mb,
            packets,
            audio_mb,
        });
        *self = Loop { since: now, ..Default::default() };
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
        let g = self.gpu.as_mut().map(|g| g.read(self.pid)).unwrap_or_default();
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

    /// 3D, copia, encoder, conversión y otros (%) de los motores del proceso `pid`.
    fn read(&mut self, pid: u32) -> [f64; 5] {
        let mut out = [0.0; 5];
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return out;
            }
            let (mut size, mut count) = (0u32, 0u32);
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None) != PDH_MORE_DATA {
                return out;
            }
            self.buf.resize((size as usize).div_ceil(8), 0);
            let items = self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
                return out;
            }
            let prefix = format!("pid_{pid}_");
            for it in std::slice::from_raw_parts(items, count as usize) {
                // 0 o 1: dato válido (o nuevo).
                if it.FmtValue.CStatus > 1 {
                    continue;
                }
                let name = it.szName.to_string().unwrap_or_default();
                if !name.starts_with(&prefix) {
                    continue;
                }
                let kind = match name.rsplit("engtype_").next().unwrap_or("") {
                    "3D" => 0,
                    "Copy" => 1,
                    "VideoEncode" => 2,
                    "VideoProcessing" => 3,
                    _ => 4,
                };
                out[kind] += it.FmtValue.Anonymous.doubleValue;
            }
        }
        out
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
        println!("AVISO {t} s: el encoder recibió {} cuadros de {}", r.fed, r.fps);
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
    ram_privada_mb: Stat,
    vram_mb: Stat,
    fps_real: Stat,
    cuadros_perdidos: u32,
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

    // Recurso, unidad, cómo se lee, cuánto puede subir por minuto y en total sin que sea una fuga.
    type Get = fn(&Sample) -> f64;
    let checks: [(&'static str, &'static str, Get, f64, f64); 7] = [
        ("RAM privada", "MB", |s| s.privada_mb, 0.5, 5.0),
        ("RAM fuera de los buffers de video y audio", "MB", |s| s.privada_mb - s.rec.as_ref().map_or(0.0, |r| r.video_mb + r.audio_mb), 0.5, 5.0),
        ("memoria de video", "MB", |s| s.vram_mb, 0.5, 5.0),
        ("handles", "", |s| s.handles as f64, 2.0, 20.0),
        ("hilos", "", |s| s.hilos as f64, 0.2, 3.0),
        ("objetos GDI", "", |s| s.gdi as f64, 1.0, 10.0),
        ("objetos USER", "", |s| s.user as f64, 1.0, 10.0),
    ];
    let mut fugas = Vec::new();
    for (recurso, unidad, get, per_min, total) in checks {
        let f = floors(&steady, get);
        let r = |x: f64| (x * 100.0).round() / 100.0;
        let (inicio, fin) = (f.first().map_or(0.0, |x| x.1), f.last().map_or(0.0, |x| x.1));
        let m = if f.len() >= 3 { slope(&f) } else { 0.0 };
        let veredicto = if f.len() < 4 {
            "corrida corta para saber"
        } else if m > per_min && fin - inicio > total {
            avisos.push(format!("Posible fuga de {recurso}: sube {:.2} {unidad}/min ({inicio:.1} → {fin:.1}).", m));
            "posible fuga"
        } else {
            "estable"
        };
        fugas.push(Leak { recurso, unidad, inicio: r(inicio), fin: r(fin), pendiente_por_min: r(m), veredicto });
    }

    let fps_real = stat(recs.iter().map(|r| r.fed as f64));
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
    let al_cerrar = format!("al cerrar la grabadora quedaron {left:+} hilos y {left_h:+} handles respecto de antes de empezar");
    if left > 2 || left_h > 50 {
        avisos.push(format!("Al cerrar la grabadora no se soltó todo: {left:+} hilos y {left_h:+} handles."));
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
        ram_privada_mb: stat(steady.iter().map(|s| s.privada_mb)),
        vram_mb: stat(steady.iter().map(|s| s.vram_mb)),
        fps_real,
        cuadros_perdidos: missed,
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
        m += &format!("- RAM privada: {} MB\n", st(&self.ram_privada_mb));
        m += &format!("- Memoria de video: {} MB\n", st(&self.vram_mb));
        m += &format!("- Esta medición usó {:.2} % del procesador (ya descontado arriba)\n", self.cpu_muestreo_pct);
        m += "\n## Video\n\n";
        m += &format!("- Cuadros por segundo al encoder: {} (pedidos: {})\n", st(&self.fps_real), self.fps_objetivo);
        m += &format!("- Cuadros salteados por despertarse tarde: {}\n", self.cuadros_perdidos);
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

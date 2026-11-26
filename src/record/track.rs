//! La pista de audio de los clips, armada mientras se graba: mezcla juego, micrófono y escritorio con sus volúmenes,
//! la comprime a AAC y guarda los paquetes en un buffer que se recorta igual que el del video. Guardar un clip solo
//! copia paquetes (antes se mezclaba y comprimía todo el PCM del clip al guardar: más tiempo, procesador y RAM).
//!
//! La mezcla va `LAG` detrás del presente: el audio de cada captura llega hasta ~40 ms tarde (paquetes de 10 ms más una
//! vuelta del hilo de grabación) y se ubica por su marca QPC. Al guardar, la cola (de `pos` al final del video) se mezcla
//! con lo que ya llegó y se comprime con un encoder aparte solo para ese clip (`Tail`): la pista sigue `LAG` detrás y el
//! audio de esa cola que llegue tarde entra igual en los clips siguientes, sin cortes.

use std::collections::VecDeque;

use anyhow::Result;

use super::aac::{Aac, AAC_FRAME};
use super::audio::AudioBuf;
use super::encoder::{Bytes, Packet};
use crate::clipcache::{Wave, WaveBuilder, WAVE_BUCKET};
use crate::log::log;
use crate::mf::{frames_at, time_of, RATE};

/// Cuánto detrás del presente va la mezcla (frames): lo que puede tardar en llegar el audio de una captura.
pub const LAG: i64 = RATE / 4;

/// Paquetes AAC que se juntan en un bloque (~1 s): como con el video, el buffer suelta bloques enteros y no deja miles
/// de pedazos chicos en el heap.
const SEAL: usize = 47;

/// Muestras por valor de la forma de onda.
const PER: i64 = frames_at(WAVE_BUCKET);

/// Cuánta mezcla ya comprimida se guarda (frames): el encoder de la cola arranca ahí para llegar al empalme con el mismo
/// estado que el de la pista (sus primeros paquetes, que salen distintos, se tiran).
const HISTORY: i64 = 8 * AAC_FRAME as i64;

/// Un canal que entra a la mezcla (None: apagado).
pub struct Input<'a> {
    pub buf: &'a AudioBuf,
    pub gain: f32,
    /// Si se le silencian los avisos de "clip guardado" (el canal escritorio).
    pub masked: bool,
}

/// Lo que se lleva un clip de la pista.
pub struct Clip {
    pub type_blob: Vec<u8>,
    pub packets: Vec<Packet>,
    pub wave: Wave,
    /// Lo que falta comprimir hasta el final del video (se hace en el hilo que guarda, con `complete`).
    tail: Option<Tail>,
}

/// La cola de un clip: la mezcla que la pista todavía no comprimió, más la ya comprimida de antes para que el encoder
/// nuevo llegue al empalme como el de la pista.
struct Tail {
    pcm: Vec<i16>,
    /// Frame (desde t0) donde empieza `pcm`: a un número entero de paquetes del comienzo de la pista, así el encoder
    /// nuevo arma los paquetes en los mismos tramos.
    at: i64,
    /// Dónde termina el último paquete de la pista (100 ns): de ahí en adelante van los de la cola (None: la pista
    /// todavía no sacó ninguno).
    splice: Option<i64>,
    from: i64,
    to: i64,
}

pub struct Track {
    aac: Aac,
    /// Hasta qué frame (desde t0) ya se mezcló y comprimió.
    pos: i64,
    packets: VecDeque<Packet>,
    /// Paquetes del final que todavía no se juntaron en un bloque.
    loose: usize,
    /// Forma de onda de la mezcla: un valor (pico, RMS) cada `PER` frames desde `start`.
    wave: WaveBuilder,
    start: i64,
    buckets: VecDeque<(u8, u8)>,
    /// Índice (desde `start`) del primer valor de `buckets`.
    first_bucket: i64,
    /// La mezcla ya comprimida de los últimos `HISTORY` frames (o un poco más), desde `history_at`.
    history: VecDeque<i16>,
    history_at: i64,
    mix: Vec<f32>,
    pcm: Vec<i16>,
    out: Vec<Packet>,
}

impl Track {
    /// Una pista que empieza en el frame `pos`.
    pub fn new(pos: i64) -> Result<Self> {
        Ok(Self {
            aac: Aac::new()?,
            pos,
            packets: VecDeque::new(),
            loose: 0,
            wave: WaveBuilder::new(),
            start: pos,
            buckets: VecDeque::new(),
            first_bucket: 0,
            history: VecDeque::new(),
            history_at: pos,
            mix: vec![0.0; AAC_FRAME * 2],
            pcm: Vec::with_capacity(AAC_FRAME * 2),
            out: Vec::new(),
        })
    }

    pub fn pos(&self) -> i64 {
        self.pos
    }

    /// Mezcla y comprime hasta el frame `to`, en tramos de un paquete AAC. `masks`: dónde sonó el aviso de guardado
    /// (100 ns, en orden).
    pub fn advance(&mut self, to: i64, inputs: &[Option<Input>], masks: &[(i64, i64)]) -> Result<()> {
        while self.pos < to {
            let n = ((to - self.pos) as usize).min(AAC_FRAME);
            mix(&mut self.mix[..n * 2], self.pos, inputs, masks, &mut self.pcm);
            self.aac.encode(&self.pcm, time_of(self.pos), &mut self.out)?;
            for c in self.pcm.as_chunks::<2>().0 {
                self.wave.push(c[0], c[1]);
            }
            self.buckets.extend(self.wave.take());
            self.history.extend(&self.pcm);
            self.pos += n as i64;
        }
        // Se suelta de a paquetes enteros, así `history_at` queda en el borde de uno.
        while self.pos - self.history_at >= HISTORY + AAC_FRAME as i64 {
            self.history.drain(..AAC_FRAME * 2);
            self.history_at += AAC_FRAME as i64;
        }
        self.loose += self.out.len();
        self.packets.extend(self.out.drain(..));
        while self.loose >= SEAL {
            let n = self.packets.len();
            let from = n - self.loose;
            Bytes::seal(self.packets.range_mut(from..from + SEAL).collect());
            self.loose -= SEAL;
        }
        Ok(())
    }

    /// Descarta lo que termina antes de `t` (100 ns).
    pub fn trim_before(&mut self, t: i64) {
        let dur = time_of(AAC_FRAME as i64);
        while self.packets.len() > self.loose && self.packets.front().is_some_and(|p| p.pts + dur <= t) {
            self.packets.pop_front();
        }
        let drop = ((frames_at(t) - self.start) / PER - self.first_bucket).clamp(0, self.buckets.len() as i64);
        self.buckets.drain(..drop as usize);
        self.first_bucket += drop;
    }

    /// Los paquetes y la forma de onda del tramo `from..to` (100 ns, el del video del clip). Los paquetes empiezan en
    /// el primero que no arranca antes de `from` (como mucho un paquete, 21 ms, después) y comparten los bloques del
    /// buffer, sin copiar el audio. Lo que la pista todavía no mezcló se mezcla acá con lo que ya llegó, sin tocar la
    /// pista, y queda para comprimir en `Clip::complete`.
    pub fn clip(&self, from: i64, to: i64, inputs: &[Option<Input>], masks: &[(i64, i64)]) -> Option<Clip> {
        let first = self.packets.partition_point(|p| p.pts < from);
        let last = self.packets.partition_point(|p| p.pts < to);
        let packets: Vec<Packet> = self.packets.range(first..last).cloned().collect();
        let n = (frames_at(to) - self.pos).max(0) as usize;
        if packets.is_empty() && n == 0 {
            return None;
        }
        let mut pcm = Vec::with_capacity(self.history.len() + n * 2);
        pcm.extend(&self.history);
        let mut fresh = Vec::with_capacity(n * 2);
        mix(&mut vec![0.0; n * 2], self.pos, inputs, masks, &mut fresh);
        pcm.extend(&fresh);
        // La forma de onda de la cola sigue la de la pista (con su valor a medio armar).
        let mut wave = self.wave.clone();
        for c in fresh.as_chunks::<2>().0 {
            wave.push(c[0], c[1]);
        }
        let tail_buckets: Vec<_> = wave.take().collect();
        let splice = self.packets.back().map(|p| p.pts + time_of(AAC_FRAME as i64));
        // Valores de la forma de onda desde `from`; si la pista empezó después (se prendió el audio a mitad del clip),
        // silencio hasta ahí.
        let bucket = |t: i64| (frames_at(t) - self.start).div_euclid(PER) - self.first_bucket;
        let (b0, b1) = (bucket(from), bucket(to));
        let pad = (-b0).clamp(0, (b1 - b0).max(0));
        let all = (self.buckets.len() + tail_buckets.len()) as i64;
        let (h0, h1) = (b0.clamp(0, all) as usize, b1.clamp(0, all) as usize);
        let have = self.buckets.iter().chain(&tail_buckets).skip(h0).take(h1.saturating_sub(h0));
        let wave = std::iter::repeat_n((0, 0), pad as usize).chain(have.copied()).collect();
        let tail = Tail { pcm, at: self.history_at, splice, from, to };
        Some(Clip { type_blob: self.aac.type_blob.clone(), packets, wave, tail: Some(tail) })
    }

    /// Memoria que ocupan los paquetes (bytes), para medir el rendimiento.
    #[cfg(feature = "devtools")]
    pub fn bytes(&self) -> usize {
        self.packets.iter().map(|p| p.data.len()).sum::<usize>() + self.buckets.len() * 2 + self.history.len() * 2
    }
}

impl Clip {
    /// Comprime la cola con un encoder nuevo y la empalma detrás de los paquetes de la pista. Si no se puede, el clip
    /// queda con lo que ya tenía (termina unos cientos de ms antes que el video). None si no queda audio.
    pub fn complete(mut self) -> Option<Self> {
        if let Some(tail) = self.tail.take() {
            match tail.encode() {
                Ok(p) => self.packets.extend(p),
                Err(e) => log(&format!("no se pudo comprimir el final del audio del clip: {e:?}")),
            }
        }
        (!self.packets.is_empty()).then_some(self)
    }
}

impl Tail {
    fn encode(self) -> Result<Vec<Packet>> {
        let mut aac = Aac::new()?;
        let mut out = Vec::new();
        for (i, c) in self.pcm.chunks(AAC_FRAME * 2).enumerate() {
            aac.encode(c, time_of(self.at + (i * AAC_FRAME) as i64), &mut out)?;
        }
        aac.drain(&mut out)?;
        // Los paquetes de los dos encoders caen en los mismos tramos (salvo redondeo de la marca): la cola arranca en el
        // que sigue al último de la pista, sin hueco ni solapamiento.
        let half = time_of(AAC_FRAME as i64) / 2;
        let after = self.splice.map_or(self.from, |s| (s - half).max(self.from));
        out.retain(|p| p.pts >= after && p.pts < self.to);
        Ok(out)
    }
}

/// Mezcla en `mix` (estéreo intercalado desde el frame `from`) los canales con sus volúmenes y la deja en `pcm` como
/// 16 bits. `masks`: dónde sonó el aviso de guardado (100 ns, en orden).
fn mix(mix: &mut [f32], from: i64, inputs: &[Option<Input>], masks: &[(i64, i64)], pcm: &mut Vec<i16>) {
    let to = from + (mix.len() / 2) as i64;
    mix.fill(0.0);
    let at = |f: i64| ((f - from) * 2) as usize;
    for i in inputs.iter().flatten() {
        if !i.masked {
            i.buf.mix_into(mix, from, i.gain);
            continue;
        }
        // Solo los tramos donde no sonó el aviso.
        let mut a = from;
        for (m0, m1) in masks.iter().map(|&(m0, m1)| (frames_at(m0).clamp(from, to), frames_at(m1).clamp(from, to))).chain([(to, to)]) {
            if m0 > a {
                i.buf.mix_into(&mut mix[at(a)..at(m0)], a, i.gain);
            }
            a = a.max(m1);
        }
    }
    pcm.clear();
    pcm.extend(mix.iter().map(|x| x.clamp(i16::MIN as f32, i16::MAX as f32) as i16));
}

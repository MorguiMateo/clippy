//! Conversión BGRA -> NV12 con el procesador de video de la GPU (sin shaders ni CPU).
//! También endereza monitores verticales, estira la resolución estirada y encaja un monitor de otra forma sin deformarlo.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;

use anyhow::Result;
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::capture::Frame;

/// Conversiones mandadas a la GPU que todavía no se le pasaron al encoder. Si se llena (la placa va atrasada con
/// un juego al 100 %), ese cuadro no se convierte. El resto del pool queda para las que el encoder sigue leyendo.
const IN_FLIGHT: usize = 3;

/// Una imagen convertida del pool, con lo que avisa cuándo la GPU terminó de escribirla.
struct Slot {
    tex: ID3D11Texture2D,
    view: ID3D11VideoProcessorOutputView,
    done: ID3D11Query,
}

pub struct Converter {
    /// La imagen que convierte (si `Capture` cambia de textura, hace falta otro `Converter`).
    src: ID3D11Texture2D,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vctx: ID3D11VideoContext,
    vp: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    pool: Vec<Slot>,
    next: usize,
    /// La última conversión: de qué versión de la imagen (`Capture::version`) y en qué lugar del pool quedó.
    last: Option<(u64, usize)>,
    /// Tamaño del video (para crear el cuadro negro).
    size: (u32, u32),
    /// Cuadro negro del modo "Solo juegos": se crea la primera vez que hace falta.
    black: Option<ID3D11Texture2D>,
    /// Cuadros en orden para el encoder: (lugar del pool, o None si es el negro; tiempo).
    queue: VecDeque<(Option<usize>, i64)>,
}

/// Un cuadro listo para el encoder.
pub struct Ready {
    pub tex: ID3D11Texture2D,
    pub pts: i64,
    /// Es una imagen convertida (no el cuadro negro).
    pub converted: bool,
}

impl Converter {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        frame: &Frame,
        w: u32,
        h: u32,
        fps: u32,
        pool_size: usize,
    ) -> Result<Self> {
        unsafe {
            let vdev: ID3D11VideoDevice = device.cast()?;
            let vctx: ID3D11VideoContext = context.cast()?;
            let src = &frame.tex;
            let mut sd = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut sd);

            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let cd = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: sd.Width,
                InputHeight: sd.Height,
                OutputFrameRate: rate,
                OutputWidth: w,
                OutputHeight: h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let en = vdev.CreateVideoProcessorEnumerator(&cd)?;
            let vp = vdev.CreateVideoProcessor(&en, 0)?;

            let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut input = None;
            vdev.CreateVideoProcessorInputView(src, &en, &ivd, Some(&mut input))?;

            let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut pool = Vec::with_capacity(pool_size);
            for _ in 0..pool_size {
                let mut td = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                if device.CreateTexture2D(&td, None, Some(&mut t)).is_err() {
                    td.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
                    device.CreateTexture2D(&td, None, Some(&mut t))?;
                }
                let t = t.unwrap();
                let mut ov = None;
                vdev.CreateVideoProcessorOutputView(&t, &en, &ovd, Some(&mut ov))?;
                let mut done = None;
                device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut done))?;
                pool.push(Slot { tex: t, view: ov.unwrap(), done: done.unwrap() });
            }

            // Colores correctos: RGB completo -> YUV BT.709 rango limitado.
            if let Ok(v1) = vctx.cast::<ID3D11VideoContext1>() {
                v1.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                v1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);

            // Monitor vertical: la imagen llega acostada (DXGI_MODE_ROTATION va de 1 a 4, la del
            // procesador de 0 a 3, en el mismo orden).
            let quarter = frame.rotation.0 - 1;
            if quarter > 0 {
                vctx.VideoProcessorSetStreamRotation(&vp, 0, true, D3D11_VIDEO_PROCESSOR_ROTATION(quarter));
            }
            // Encajar con la forma con la que se ve en el monitor: estirada si Windows la estira (resolución
            // estirada); un monitor con otra forma queda con bandas negras.
            let (sw, sh) = frame.shape;
            let scale = (w as f64 / sw as f64).min(h as f64 / sh as f64);
            let dw = (sw as f64 * scale).round() as i32 & !1;
            let dh = (sh as f64 * scale).round() as i32 & !1;
            let (x, y) = (((w as i32 - dw) / 2) & !1, ((h as i32 - dh) / 2) & !1);
            let dest = RECT { left: x, top: y, right: x + dw, bottom: y + dh };
            vctx.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&dest));
            let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
            vctx.VideoProcessorSetOutputBackgroundColor(&vp, false, &black);

            Ok(Self {
                src: src.clone(),
                device: device.clone(),
                context: context.clone(),
                vctx,
                vp,
                input: input.unwrap(),
                pool,
                next: 0,
                last: None,
                size: (w, h),
                black: None,
                queue: VecDeque::new(),
            })
        }
    }

    pub fn is_for(&self, frame: &Frame) -> bool {
        self.src == frame.tex
    }

    /// Pone en la fila el cuadro negro (modo "Solo juegos" sin el juego en pantalla).
    pub fn black(&mut self, pts: i64) -> Result<()> {
        if self.queue.len() >= IN_FLIGHT {
            return Ok(());
        }
        if self.black.is_none() {
            self.black = Some(self.black_texture()?);
        }
        self.queue.push_back((None, pts));
        Ok(())
    }

    /// Cuadro negro en NV12 (Y=16, UV=128).
    fn black_texture(&self) -> Result<ID3D11Texture2D> {
        let (w, h) = self.size;
        let (bw, bh) = (w as usize, h as usize);
        let mut px = vec![16u8; bw * bh];
        px.resize(bw * bh * 3 / 2, 128);
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: px.as_ptr() as *const _, SysMemPitch: w, SysMemSlicePitch: 0 };
        let bd = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut black = None;
        unsafe { self.device.CreateTexture2D(&bd, Some(&init), Some(&mut black))? };
        black.ok_or_else(|| anyhow::anyhow!("sin cuadro negro"))
    }

    /// Manda a convertir la imagen actual a una textura del pool, sin esperar a la GPU: `ready` la entrega cuando
    /// termina. Si la placa va atrasada (`IN_FLIGHT` sin terminar), ese cuadro se saltea.
    /// Si la imagen no cambió desde la última conversión (`version`), vuelve a mandar esa sin convertirla: el escritorio
    /// quieto o un juego a menos cuadros por segundo que el video no ocupan a la placa en cada cuadro.
    pub fn convert(&mut self, pts: i64, version: u64) -> Result<()> {
        if self.queue.len() >= IN_FLIGHT {
            return Ok(());
        }
        // El pool avanza solo al convertir: esa imagen sigue intacta hasta que se convierta otra encima.
        if let Some((_, i)) = self.last.filter(|&(v, _)| v == version) {
            self.queue.push_back((Some(i), pts));
            return Ok(());
        }
        let i = self.next;
        self.next = (self.next + 1) % self.pool.len();
        let slot = &self.pool[i];
        unsafe {
            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
                ..Default::default()
            }];
            let r = self.vctx.VideoProcessorBlt(&self.vp, &slot.view, 0, &streams);
            ManuallyDrop::drop(&mut streams[0].pInputSurface);
            r?;
            self.context.End(&slot.done);
            // Que la GPU la empiece ya, no cuando se pregunte si terminó.
            self.context.Flush();
        }
        self.queue.push_back((Some(i), pts));
        self.last = Some((version, i));
        Ok(())
    }

    /// El cuadro más viejo de la fila, si ya está escrito. Nunca entrega una imagen a medio convertir: hay encoders
    /// (sospecha con AMD) que la leen sin esperar a la conversión y graban cuadros vacíos (verdes).
    pub fn ready(&self) -> Result<Option<Ready>> {
        let Some(&(slot, pts)) = self.queue.front() else { return Ok(None) };
        // El negro se crea antes de ponerlo en la fila.
        let Some(i) = slot else { return Ok(self.black.clone().map(|tex| Ready { tex, pts, converted: false })) };
        let s = &self.pool[i];
        let mut done = BOOL(0);
        let size = std::mem::size_of::<BOOL>() as u32;
        unsafe { self.context.GetData(&s.done, Some(&mut done as *mut BOOL as *mut _), size, D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32)? };
        Ok(done.as_bool().then(|| Ready { tex: s.tex.clone(), pts, converted: true }))
    }

    /// Cuadros en la fila (mandados a convertir y todavía sin pasar al encoder).
    #[cfg(feature = "devtools")]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// La fila está llena: el próximo `convert` o `black` saltea su cuadro.
    #[cfg(feature = "devtools")]
    pub fn full(&self) -> bool {
        self.queue.len() >= IN_FLIGHT
    }

    /// Saca de la fila el cuadro que entregó `ready`.
    pub fn pop(&mut self) {
        self.queue.pop_front();
    }

    /// Brillo promedio de la imagen capturada y de la convertida, para el log: muestra en qué paso se
    /// pierde la imagen si un clip sale negro o verde (verde = NV12 en ceros, sin escribir).
    pub fn probe(&self, converted: &ID3D11Texture2D) -> String {
        let src = self.read(&self.src, |d, m| {
            if d.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                return format!("formato {:?}", d.Format);
            }
            let [b, g, r] = mean(d.Width / 16, d.Height / 16, |x, y, c| m.at(y * 16, x * 16 * 4 + c));
            format!("RGB medio ({r}, {g}, {b})")
        });
        let nv12 = self.read(converted, |d, m| {
            let [y] = mean(d.Width / 16, d.Height / 16, |x, y, _| m.at(y * 16, x * 16));
            // En NV12 el plano de color (U y V intercalados) va después de las filas de Y.
            let [u, v] = mean(d.Width / 32, d.Height / 32, |x, y, c| m.at(d.Height + y * 16, x * 32 + c));
            format!("Y {y}, U {u}, V {v}")
        });
        format!("control de imagen: captura {src}; convertida {nv12}")
    }

    /// Copia `tex` a la CPU y la pasa a `f`.
    fn read(&self, tex: &ID3D11Texture2D, f: impl Fn(&D3D11_TEXTURE2D_DESC, &Mapped) -> String) -> String {
        unsafe {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut d);
            let mut sd = d;
            sd.MipLevels = 1;
            sd.ArraySize = 1;
            sd.Usage = D3D11_USAGE_STAGING;
            sd.BindFlags = 0;
            sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            sd.MiscFlags = 0;
            let mut staging = None;
            if let Err(e) = self.device.CreateTexture2D(&sd, None, Some(&mut staging)) {
                return format!("no se pudo leer ({e})");
            }
            let staging = staging.unwrap();
            self.context.CopyResource(&staging, tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            if let Err(e) = self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m)) {
                return format!("no se pudo leer ({e})");
            }
            let s = f(&d, &Mapped { data: m.pData as *const u8, pitch: m.RowPitch as usize });
            self.context.Unmap(&staging, 0);
            s
        }
    }
}

struct Mapped {
    data: *const u8,
    pitch: usize,
}

impl Mapped {
    fn at(&self, row: u32, byte: u32) -> u8 {
        unsafe { *self.data.add(row as usize * self.pitch + byte as usize) }
    }
}

/// Promedio de `N` canales en una grilla de `w` x `h` puntos.
fn mean<const N: usize>(w: u32, h: u32, px: impl Fn(u32, u32, u32) -> u8) -> [u64; N] {
    let mut sum = [0u64; N];
    for y in 0..h {
        for x in 0..w {
            for (c, s) in sum.iter_mut().enumerate() {
                *s += px(x, y, c as u32) as u64;
            }
        }
    }
    sum.map(|s| s / (w as u64 * h as u64).max(1))
}

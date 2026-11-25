//! Codificador H.264 o H.265 de Media Foundation. Primero el de la placa de video (NVENC / AMF / QuickSync), que
//! recibe texturas NV12 de la GPU directo, sin copias a la CPU. Si la placa no tiene H.265, H.264. Si no tiene
//! ninguno (RX 6400, máquinas virtuales, controladores viejos), el H.264 de Windows por procesador: la conversión a
//! NV12 sigue en la GPU y solo la imagen ya convertida pasa a la memoria.

use std::mem::ManuallyDrop;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use windows::core::{Interface, GUID, VARIANT};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};

use crate::config::{fit_short, Codec};
use crate::i18n::tr;
use crate::log::log;

#[derive(Clone)]
pub struct Packet {
    pub data: Bytes,
    pub pts: i64,
    pub key: bool,
}

/// Los bytes de un paquete de video. Recién salido del encoder es un bloque propio; cuando su GOP se cierra (llega el
/// fotograma clave siguiente), `seal` junta todo el GOP en un solo bloque grande y cada paquete pasa a ser un tramo.
/// Como el buffer se recorta de a GOPs enteros, cada bloque se suelta entero: la memoria vuelve a Windows en vez de
/// quedar en el heap como miles de pedazos de 60 KB cuando el buffer se achica.
#[derive(Clone)]
pub struct Bytes {
    block: Arc<Vec<u8>>,
    start: usize,
    end: usize,
}

impl Bytes {
    fn own(data: Vec<u8>) -> Bytes {
        let end = data.len();
        Bytes { block: Arc::new(data), start: 0, end }
    }

    /// Junta los paquetes de un GOP cerrado en un solo bloque, sin cambiar sus bytes.
    pub fn seal(gop: Vec<&mut Packet>) {
        let mut block = Vec::with_capacity(gop.iter().map(|p| p.data.len()).sum());
        let mut ranges = Vec::with_capacity(gop.len());
        for p in &gop {
            let start = block.len();
            block.extend_from_slice(&p.data);
            ranges.push((start, block.len()));
        }
        let block = Arc::new(block);
        for (p, (start, end)) in gop.into_iter().zip(ranges) {
            p.data = Bytes { block: block.clone(), start, end };
        }
    }
}

impl std::ops::Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.block[self.start..self.end]
    }
}

/// Con qué se codifica el video (lo muestra la ventana principal).
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum Device {
    /// La placa de video, con su fabricante si se sabe.
    Gpu(Option<String>),
    /// El procesador: la placa no tiene encoder de video.
    Cpu,
}

/// Lo más que se le pide al encoder por procesador (lado corto y cuadros por segundo): 1080p a 60 ocupa un
/// núcleo y medio de un Ryzen 5 5600X, 1080p a 30 un poco más de medio. El procesador también lo usa el juego.
pub const CPU_MAX_SHORT: u32 = 1080;
pub const CPU_MAX_FPS: u32 = 30;

/// Cada cuánto se pide un fotograma clave (100 ns): permite recortar el buffer al segundo exacto.
const KEY_EVERY: i64 = 10_000_000;

/// Si pasa esto sin fotogramas clave, el buffer no se puede recortar ni el clip empezar bien (100 ns).
const KEY_LATE: i64 = 5 * KEY_EVERY;

pub struct Encoder {
    mft: IMFTransform,
    api: Option<ICodecAPI>,
    /// Antes de esto no se vuelve a pedir un fotograma clave a mano.
    next_key: i64,
    last_key: i64,
    /// Ya se avisó en el log que el encoder no genera fotogramas clave.
    warned_keys: bool,
    path: Path,
    pub pending_input: u32,
    /// SPS y PPS del último fotograma clave: los que de verdad usa el video.
    seq_header: Option<Vec<u8>>,
    /// Donde escribe sus paquetes un encoder que no trae muestras propias (el de procesador): se crea una vez y se
    /// reusa, en vez de pedir 4 MB de memoria en cada consulta.
    out_sample: Option<IMFSample>,
    pub name: String,
    /// Con qué formato codifica (H.265 si se pidió y la placa lo tiene).
    pub codec: Codec,
    /// Tamaño y cuadros por segundo con que codifica (el de procesador puede usar menos que lo pedido).
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Cómo le llegan las imágenes al encoder.
enum Path {
    /// Encoder de la placa (asíncrono): lee las texturas de la GPU y avisa con eventos cuándo quiere otra.
    Gpu { events: IMFMediaEventGenerator, _mgr: IMFDXGIDeviceManager, vendor: Option<String> },
    /// Encoder por procesador (sincrónico): cada imagen se copia a la memoria.
    Cpu(Readback),
}

impl Encoder {
    /// Usa el encoder `codec` de la placa que captura (si no lo tiene, el H.264); si no hay o no arranca, el H.264
    /// de Windows por procesador. `quality_mbps`: la calidad elegida (ver `Codec::bitrate`).
    pub fn new(device: &ID3D11Device, luid: LUID, w: u32, h: u32, fps: u32, codec: Codec, quality_mbps: u32) -> Result<Self> {
        unsafe {
            let codecs = if codec == Codec::Hevc { &[Codec::Hevc, Codec::H264][..] } else { &[Codec::H264] };
            for &c in codecs {
                // Si ni se pueden buscar (máquinas virtuales, el adaptador básico de Windows), también va el procesador.
                let gpu_encoders = gpu_encoders(luid, c).unwrap_or_else(|e| {
                    log(&format!("no se pudieron buscar encoders {} en la placa: {e}", c.label()));
                    Vec::new()
                });
                for (act, name, vendor) in gpu_encoders {
                    let gpu = act.ActivateObject::<IMFTransform>().map_err(Into::into).and_then(|mft| {
                        let path = gpu_path(&mft, device, &name, vendor)?;
                        Self::open(mft, path, name.clone(), c, w, h, fps, c.bitrate(quality_mbps))
                    });
                    match gpu {
                        Ok(e) => return Ok(e),
                        Err(e) => log(&format!("no se pudo usar {name}: {e}")),
                    }
                }
                if c == Codec::Hevc {
                    log("sin encoder H.265 en la placa: se graba en H.264");
                }
            }
            log("sin encoder de video en la placa: se codifica con el procesador");
            let mft: IMFTransform = CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER).map_err(|e| {
                log(&format!("no está el encoder H.264 de Windows: {e}"));
                anyhow!(tr!(
                    "Windows no tiene el codificador de video H.264. Si es una edición N de Windows, instalá el «Paquete de características multimedia» desde Configuración > Aplicaciones > Características opcionales.",
                    "Windows has no H.264 video encoder. If this is an N edition of Windows, install the “Media Feature Pack” from Settings > Apps > Optional features.",
                    "O Windows não tem o codificador de vídeo H.264. Se for uma edição N do Windows, instale o “Pacote de Recursos de Mídia” em Configurações > Aplicativos > Recursos opcionais."
                ))
            })?;
            let (w, h) = fit_short((w, h), CPU_MAX_SHORT);
            let path = Path::Cpu(Readback::new(device, w, h)?);
            let bitrate = Codec::H264.bitrate(quality_mbps);
            Self::open(mft, path, "H.264 de Windows (procesador)".into(), Codec::H264, w, h, fps.min(CPU_MAX_FPS), bitrate)
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn open(mft: IMFTransform, path: Path, name: String, codec: Codec, w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let cpu = matches!(path, Path::Cpu(_));
        let api = mft.cast::<ICodecAPI>().ok();
        if let Some(api) = &api {
            let set = |g: &GUID, v: VARIANT| {
                let _ = api.SetValue(g, &v);
            };
            set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32));
            set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
            // Un fotograma clave por segundo (si no aparecen, `feed` los pide a mano: hay encoders que ignoran esto).
            set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps));
            set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
            set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
            if cpu {
                // Lo más rápido: el procesador también lo usa el juego.
                set(&CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(0u32));
            }
        }

        let ot = MFCreateMediaType()?;
        ot.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        ot.SetGUID(&MF_MT_SUBTYPE, &subtype(codec))?;
        ot.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        set_video_common(&ot, w, h, fps)?;
        let profile = match codec {
            Codec::H264 => eAVEncH264VProfile_High.0 as u32,
            Codec::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
        };
        ot.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
        mft.SetOutputType(0, &ot, 0)?;

        let it = MFCreateMediaType()?;
        it.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        it.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        set_video_common(&it, w, h, fps)?;
        if cpu {
            it.SetUINT32(&MF_MT_DEFAULT_STRIDE, w)?;
        }
        mft.SetInputType(0, &it, 0)?;

        let info = mft.GetOutputStreamInfo(0)?;
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;
        let out_sample = if provides {
            None
        } else {
            let s = MFCreateSample()?;
            s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(4 * 1024 * 1024))?)?;
            Some(s)
        };

        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

        Ok(Self {
            mft,
            api,
            next_key: 0,
            last_key: 0,
            warned_keys: false,
            path,
            pending_input: 0,
            seq_header: None,
            out_sample,
            name,
            codec,
            width: w,
            height: h,
            fps,
        })
    }

    /// Con qué codifica.
    pub fn device(&self) -> Device {
        match &self.path {
            Path::Gpu { vendor, .. } => Device::Gpu(vendor.clone()),
            Path::Cpu(_) => Device::Cpu,
        }
    }

    /// Junta los paquetes listos sin bloquear y anota cuántas imágenes más acepta el encoder.
    pub fn poll(&mut self, out: &mut Vec<Packet>) -> Result<()> {
        let events = match &self.path {
            Path::Gpu { events, .. } => events.clone(),
            // El de procesador codifica al recibir la imagen: lo que sale ya está, y siempre acepta otra.
            Path::Cpu(_) => {
                while self.pull(out)? {}
                self.pending_input = 1;
                return Ok(());
            }
        };
        loop {
            let ev = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(e) => return Err(e.into()),
            };
            let t = unsafe { ev.GetType()? };
            if t == METransformNeedInput.0 as u32 {
                self.pending_input += 1;
            } else if t == METransformHaveOutput.0 as u32 {
                self.pull(out)?;
            }
        }
        Ok(())
    }

    /// Saca un paquete del encoder, si tiene uno listo.
    fn pull(&mut self, out: &mut Vec<Packet>) -> Result<bool> {
        unsafe {
            let own = match &self.out_sample {
                Some(s) => {
                    // Sin las marcas del paquete anterior (ej. fotograma clave) y vacía.
                    s.DeleteAllItems()?;
                    s.GetBufferByIndex(0)?.SetCurrentLength(0)?;
                    Some(s.clone())
                }
                None => None,
            };
            let mut buf = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut buf, &mut status);
            let sample = ManuallyDrop::take(&mut buf[0].pSample);
            drop(ManuallyDrop::take(&mut buf[0].pEvents));
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    return Ok(true);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(e) => return Err(e.into()),
            }
            let Some(sample) = sample else { return Ok(false) };

            let pts = sample.GetSampleTime().unwrap_or(0);
            let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let mb = sample.ConvertToContiguousBuffer()?;
            let mut p = std::ptr::null_mut();
            let mut len = 0u32;
            mb.Lock(&mut p, None, Some(&mut len))?;
            let data = Bytes::own(std::slice::from_raw_parts(p, len as usize).to_vec());
            mb.Unlock()?;

            // Un clip tiene que empezar en una imagen IDR: con otra imagen "limpia" (I sin IDR, que algunos
            // encoders marcan igual) los reproductores muestran negro o verde hasta el próximo IDR.
            // Si la salida no es Annex B no se puede mirar adentro y vale la marca del encoder.
            let key = if is_annex_b(&data) { is_keyframe(&data, self.codec) } else { clean };
            if key {
                self.last_key = pts;
            } else if pts - self.last_key > KEY_LATE && !self.warned_keys {
                self.warned_keys = true;
                log(&format!("el encoder {} no genera fotogramas clave: los clips pueden salir negros o verdes", self.name));
            }
            if key {
                let ps = Some(param_sets(&data, self.codec)).filter(|ps| !ps.is_empty());
                if self.seq_header.is_none() {
                    let published = self.published_header();
                    let hex = |h: &Option<Vec<u8>>| h.as_ref().map_or("no".into(), |h| h.iter().map(|b| format!("{b:02x}")).collect::<String>());
                    let same = if ps.is_some() && ps == published { " (iguales)" } else { "" };
                    log(&format!("encabezado {}{same}: en el video {}, publicado por el encoder {}", self.codec.label(), hex(&ps), hex(&published)));
                }
                if ps.is_some() {
                    self.seq_header = ps;
                }
            }
            out.push(Packet { data, pts, key });
        }
        Ok(true)
    }

    pub fn feed(&mut self, tex: &ID3D11Texture2D, pts: i64, dur: i64) -> Result<()> {
        unsafe {
            // Solo si el encoder no generó uno solo (con margen: los paquetes salen unos cuadros después).
            if pts >= self.next_key && pts - self.last_key > KEY_EVERY * 3 / 2 {
                self.next_key = pts + KEY_EVERY;
                if let Some(api) = &self.api {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                }
            }
            let (buf, pts) = match &mut self.path {
                Path::Gpu { .. } => {
                    let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
                    if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                        if let Ok(len) = b2.GetContiguousLength() {
                            let _ = buf.SetCurrentLength(len);
                        }
                    }
                    (buf, pts)
                }
                // Sale la imagen anterior (la de esta queda copiándose): la primera vez, ninguna.
                Path::Cpu(r) => match r.read(tex, pts)? {
                    Some(prev) => prev,
                    None => {
                        self.pending_input -= 1;
                        return Ok(());
                    }
                },
            };
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(pts)?;
            s.SetSampleDuration(dur)?;
            self.mft.ProcessInput(0, &s, 0)?;
        }
        self.pending_input -= 1;
        Ok(())
    }

    /// SPS y PPS que el encoder publica en su tipo de salida. El de AMD no coincide con lo que
    /// codifica (dice POC tipo 2 y los cuadros usan tipo 0): con ese encabezado el MP4 sale negro o verde.
    fn published_header(&self) -> Option<Vec<u8>> {
        unsafe {
            let t = self.mft.GetOutputCurrentType(0).ok()?;
            let mut v = vec![0u8; t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()? as usize];
            t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut v, None).ok()?;
            Some(v)
        }
    }

    /// Tipo de salida serializado (para reconstruirlo en el hilo que escribe el MP4), con el encabezado
    /// que viene en el video si lo hay, y si no el que publica el encoder ahora.
    pub fn type_blob(&self) -> Option<Vec<u8>> {
        unsafe {
            let src = self.mft.GetOutputCurrentType(0).ok()?;
            let t = MFCreateMediaType().ok()?;
            src.CopyAllItems(&t).ok()?;
            if let Some(sh) = &self.seq_header {
                t.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, sh).ok()?;
            }
            let size = MFGetAttributesAsBlobSize(&t).ok()?;
            let mut v = vec![0u8; size as usize];
            MFGetAttributesAsBlob(&t, &mut v).ok()?;
            Some(v)
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(sd) = self.mft.cast::<IMFShutdown>() {
                let _ = sd.Shutdown();
            }
        }
    }
}

unsafe fn set_video_common(t: &IMFMediaType, w: u32, h: u32, fps: u32) -> Result<()> {
    t.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
    t.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
    let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
    let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
    let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
    Ok(())
}

/// Prepara un encoder de la placa: asíncrono y leyendo las texturas de la GPU que captura.
unsafe fn gpu_path(mft: &IMFTransform, device: &ID3D11Device, name: &str, vendor: Option<String>) -> Result<Path> {
    let attrs = mft.GetAttributes()?;
    if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
        bail!("el encoder {name} no es asíncrono");
    }
    attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
    let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

    let mut token = 0u32;
    let mut mgr = None;
    MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
    let mgr = mgr.unwrap();
    mgr.ResetDevice(device, token)?;
    mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)?;
    Ok(Path::Gpu { events: mft.cast()?, _mgr: mgr, vendor })
}

fn subtype(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => MFVideoFormat_H264,
        Codec::Hevc => MFVideoFormat_HEVC,
    }
}

/// Encoders `codec` de la placa que captura (importa en notebooks con dos GPUs), con nombre y fabricante.
unsafe fn gpu_encoders(luid: LUID, codec: Codec) -> Result<Vec<(IMFActivate, String, Option<String>)>> {
    // Prueba: en una placa que no existe la búsqueda falla, y se tiene que seguir con el procesador.
    let luid = if skip_gpu() { LUID { LowPart: u32::MAX, HighPart: i32::MAX } } else { luid };
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec) };
    let mut attrs = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.unwrap();
    let luid_bytes = std::slice::from_raw_parts(&luid as *const LUID as *const u8, std::mem::size_of::<LUID>());
    attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, luid_bytes)?;

    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnum2(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input),
        Some(&output),
        &attrs,
        &mut ptr,
        &mut count,
    )?;
    if ptr.is_null() {
        return Ok(Vec::new());
    }
    let v = std::slice::from_raw_parts_mut(ptr, count as usize)
        .iter_mut()
        .filter_map(|a| a.take())
        .map(|a| {
            let name = string_attr(&a, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "encoder".into());
            let vendor = string_attr(&a, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute).as_deref().and_then(vendor_name);
            (a, name, vendor)
        })
        .collect();
    CoTaskMemFree(Some(ptr as *const _));
    Ok(v)
}

/// Con `devtools`, la variable CLIPPY_CPU_ENCODER hace que no se encuentre ningún encoder en la placa.
#[cfg(feature = "devtools")]
fn skip_gpu() -> bool {
    std::env::var_os("CLIPPY_CPU_ENCODER").is_some()
}

#[cfg(not(feature = "devtools"))]
fn skip_gpu() -> bool {
    false
}

/// Fabricante a partir del id PCI que publica el encoder ("VEN_10DE").
fn vendor_name(id: &str) -> Option<String> {
    let name = match u32::from_str_radix(id.trim().trim_start_matches("VEN_"), 16).ok()? {
        0x10DE => "NVIDIA",
        0x1002 | 0x1022 => "AMD",
        0x8086 => "Intel",
        0x5143 => "Qualcomm",
        _ => return None,
    };
    Some(name.into())
}

unsafe fn string_attr(act: &IMFActivate, key: &GUID) -> Option<String> {
    let mut p = windows::core::PWSTR::null();
    let mut len = 0u32;
    act.GetAllocatedString(key, &mut p, &mut len).ok()?;
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const _));
    s
}

/// Copia las imágenes NV12 ya convertidas en la GPU a la memoria, para el encoder por procesador.
/// Usa dos copias que se turnan: mientras la placa copia una imagen, se lee la anterior, que ya terminó. Leer la
/// recién copiada frenaría el hilo de grabación hasta que la placa, ocupada con el juego, llegue a copiarla.
struct Readback {
    staging: [ID3D11Texture2D; 2],
    /// La copia que se está escribiendo y el tiempo de su imagen (se lee en la próxima).
    pending: Option<(usize, i64)>,
    context: ID3D11DeviceContext,
    w: usize,
    h: usize,
}

impl Readback {
    unsafe fn new(device: &ID3D11Device, w: u32, h: u32) -> Result<Self> {
        let d = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let one = || -> Result<ID3D11Texture2D> {
            let mut t = None;
            device.CreateTexture2D(&d, None, Some(&mut t))?;
            t.ok_or_else(|| anyhow!("sin textura de lectura"))
        };
        Ok(Self { staging: [one()?, one()?], pending: None, context: device.GetImmediateContext()?, w: w as usize, h: h as usize })
    }

    /// Manda a copiar `tex` (de tiempo `pts`) y devuelve la imagen anterior con su tiempo, en un buffer de memoria con
    /// las filas juntas: el plano Y y después el UV intercalado.
    unsafe fn read(&mut self, tex: &ID3D11Texture2D, pts: i64) -> Result<Option<(IMFMediaBuffer, i64)>> {
        let i = self.pending.map_or(0, |(j, _)| 1 - j);
        self.context.CopyResource(&self.staging[i], tex);
        let Some((prev, prev_pts)) = self.pending.replace((i, pts)) else { return Ok(None) };
        let staging = &self.staging[prev];
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let len = self.w * self.h * 3 / 2;
        let copy = || -> Result<IMFMediaBuffer> {
            let buf = MFCreateMemoryBuffer(len as u32)?;
            let mut dst = std::ptr::null_mut();
            buf.Lock(&mut dst, None, None)?;
            // En la textura mapeada el plano UV empieza después de `h` filas de `RowPitch` bytes.
            for row in 0..self.h * 3 / 2 {
                std::ptr::copy_nonoverlapping((m.pData as *const u8).add(row * m.RowPitch as usize), dst.add(row * self.w), self.w);
            }
            buf.Unlock()?;
            buf.SetCurrentLength(len as u32)?;
            Ok(buf)
        };
        let r = copy();
        self.context.Unmap(staging, 0);
        r.map(|buf| Some((buf, prev_pts)))
    }
}

// --- Utilidades Annex-B -------------------------------------------------------

fn nals(d: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() { starts[k + 1] - 3 } else { d.len() };
        while e > s && d[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&d[s..e]);
        }
    }
    out
}

/// Tipo de NAL: en H.264, los 5 bits bajos del primer byte; en H.265, los 6 del medio.
fn nal_type(n: &[u8], codec: Codec) -> u8 {
    match codec {
        Codec::H264 => n[0] & 0x1f,
        Codec::Hevc => (n[0] >> 1) & 0x3f,
    }
}

/// NAL con imagen: H.264 1 a 5, H.265 0 a 31.
fn is_slice(t: u8, codec: Codec) -> bool {
    match codec {
        Codec::H264 => (1..=5).contains(&t),
        Codec::Hevc => t <= 31,
    }
}

/// ¿Empieza con un código de inicio (00 00 01 o 00 00 00 01)?
fn is_annex_b(d: &[u8]) -> bool {
    d.starts_with(&[0, 0, 1]) || d.starts_with(&[0, 0, 0, 1])
}

/// Dónde empieza el código de inicio del primer NAL con imagen y su tipo. Los encabezados (SPS, PPS, SEI) van
/// antes: se lee hasta ahí y no la imagen entera, que es casi todo el paquete.
fn first_slice(d: &[u8], codec: Codec) -> Option<(usize, u8)> {
    let mut i = 0;
    while i + 3 < d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            let t = nal_type(&d[i + 3..], codec);
            if is_slice(t, codec) {
                return Some((i, t));
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    None
}

/// ¿Se puede empezar el clip acá? H.264: imagen IDR (5). H.265: IDR, CRA o BLA (16 a 21); sin imágenes B, las
/// tres arrancan limpias.
fn is_keyframe(d: &[u8], codec: Codec) -> bool {
    first_slice(d, codec).is_some_and(|(_, t)| match codec {
        Codec::H264 => t == 5,
        Codec::Hevc => (16..=21).contains(&t),
    })
}

/// Encabezados del video para el MP4 (van antes de la imagen): H.264 SPS y PPS (7, 8); H.265 VPS, SPS y PPS
/// (32 a 34).
fn param_sets(d: &[u8], codec: Codec) -> Vec<u8> {
    let headers = &d[..first_slice(d, codec).map_or(d.len(), |(i, _)| i)];
    let mut v = Vec::new();
    for n in nals(headers) {
        let t = nal_type(n, codec);
        let param = match codec {
            Codec::H264 => matches!(t, 7 | 8),
            Codec::Hevc => (32..=34).contains(&t),
        };
        if param {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(n);
        }
    }
    v
}

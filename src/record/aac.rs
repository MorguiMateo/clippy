//! Encoder AAC de Media Foundation (el de Windows, el mismo que usaba el Sink Writer al guardar): comprime la mezcla
//! del clip mientras se graba, así guardar solo copia paquetes.

use std::mem::ManuallyDrop;

use anyhow::Result;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use super::encoder::{Bytes, Packet};
use crate::mf::{self, time_of, FRAME_BYTES};

/// Muestras de cada paquete AAC.
pub const AAC_FRAME: usize = 1024;

pub struct Aac {
    mft: IMFTransform,
    /// Donde el encoder escribe cada paquete (se reusa).
    out: IMFMediaBuffer,
    /// El tipo de salida (con el `AudioSpecificConfig`), para escribir la pista tal cual en el MP4.
    pub type_blob: Vec<u8>,
    /// Dónde va el próximo paquete si el encoder no le pone marca de tiempo (100 ns).
    next: Option<i64>,
}

// Un MFT sincrónico de COM multihilo: lo usa solo el hilo de grabación, pero se crea antes de que arranque la sesión.
unsafe impl Send for Aac {}

impl Aac {
    pub fn new() -> Result<Self> {
        unsafe {
            let mft: IMFTransform = CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER)?;
            // Lo mismo que le pedía el Sink Writer: PCM de Clippy a la entrada, AAC de 192 kbps a la salida.
            mft.SetInputType(0, &mf::pcm_type()?, 0)?;
            mft.SetOutputType(0, &mf::aac_type()?, 0)?;
            let cur = mft.GetOutputCurrentType(0)?;
            let mut type_blob = vec![0u8; MFGetAttributesAsBlobSize(&cur)? as usize];
            MFGetAttributesAsBlob(&cur, &mut type_blob)?;
            let info = mft.GetOutputStreamInfo(0)?;
            // El encoder de Windows no da sus muestras: se le presta un buffer del tamaño que pide.
            let out = MFCreateMemoryBuffer(info.cbSize.max(8192))?;
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            Ok(Self { mft, out, type_blob, next: None })
        }
    }

    /// Comprime `pcm` (estéreo intercalado que empieza en `time`, 100 ns) y suma a `out` los paquetes que salgan, con la
    /// marca de tiempo que les pone el encoder (así el retardo propio del AAC queda igual que antes).
    pub fn encode(&mut self, pcm: &[i16], time: i64, out: &mut Vec<Packet>) -> Result<()> {
        unsafe {
            let bytes = std::slice::from_raw_parts(pcm.as_ptr() as *const u8, pcm.len() * 2);
            debug_assert_eq!(bytes.len() % FRAME_BYTES, 0);
            self.next.get_or_insert(time);
            self.mft.ProcessInput(0, &mf::pcm_sample(bytes, time)?, 0)?;
            while self.pull(out)? {}
        }
        Ok(())
    }

    /// Saca un paquete, si hay uno listo.
    unsafe fn pull(&mut self, out: &mut Vec<Packet>) -> Result<bool> {
        // Una muestra nueva en cada vuelta (el buffer es el mismo): así no queda la marca de tiempo del paquete anterior.
        self.out.SetCurrentLength(0)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&self.out)?;
        let mut buf = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(Some(sample.clone())),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let r = self.mft.ProcessOutput(0, &mut buf, &mut status);
        drop(ManuallyDrop::take(&mut buf[0].pSample));
        drop(ManuallyDrop::take(&mut buf[0].pEvents));
        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
            Err(e) => return Err(e.into()),
        }
        let pts = sample.GetSampleTime().ok().or(self.next).unwrap_or(0);
        self.next = Some(pts + time_of(AAC_FRAME as i64));
        let mb = &self.out;
        let mut p = std::ptr::null_mut();
        let mut len = 0u32;
        mb.Lock(&mut p, None, Some(&mut len))?;
        let data = std::slice::from_raw_parts(p, len as usize).to_vec();
        mb.Unlock()?;
        if !data.is_empty() {
            out.push(Packet { data: Bytes::own(data), pts, key: true });
        }
        Ok(true)
    }
}

//! Escribe el clip a MP4 con el Sink Writer de Media Foundation. Video y audio ya vienen comprimidos (el audio lo
//! comprime `track` mientras se graba): se copian tal cual, sin recodificar.

use std::path::PathBuf;

use anyhow::Result;
use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::*;

use super::aac::AAC_FRAME;
use super::encoder::Packet;
use super::track;
use crate::mf::{self, time_of, Mf};

pub struct Job {
    pub path: PathBuf,
    pub video_type: Vec<u8>,
    pub packets: Vec<Packet>,
    pub base: i64,
    pub frame_dur: i64,
    pub audio: Option<track::Clip>,
}

pub fn write(job: Job) -> Result<PathBuf> {
    let mf = Mf::init();
    let r = write_inner(&job);
    drop(mf);
    if r.is_err() {
        let _ = std::fs::remove_file(&job.path);
    } else {
        // Forma de onda y fotogramas clave listos para el editor (así abre el clip al instante).
        let kf: Vec<i64> = job.packets.iter().filter(|p| p.key).map(|p| p.pts - job.base).collect();
        crate::clipcache::save(&job.path, &job.audio.map(|a| a.wave).unwrap_or_default(), &kf);
    }
    r.map(|_| job.path)
}

unsafe fn media_type(blob: &[u8]) -> Result<IMFMediaType> {
    let t = MFCreateMediaType()?;
    MFInitAttributesFromBlob(&t, blob)?;
    Ok(t)
}

unsafe fn sample(p: &Packet, time: i64, dur: i64) -> Result<IMFSample> {
    let s = mf::sample(&p.data, time, dur.max(1))?;
    if p.key {
        s.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
    }
    Ok(s)
}

/// Duración de cada paquete: hasta el siguiente (el último, `last`).
fn durations(p: &[Packet], base: i64, last: i64) -> impl Iterator<Item = (&Packet, i64, i64)> {
    p.iter().enumerate().map(move |(i, x)| (x, x.pts - base, p.get(i + 1).map_or(last, |n| n.pts - x.pts)))
}

fn write_inner(job: &Job) -> Result<()> {
    if let Some(dir) = job.path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 2)?;
        let attrs = attrs.unwrap();
        attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;

        let url = HSTRING::from(job.path.as_os_str());
        let writer = MFCreateSinkWriterFromURL(&url, None, &attrs)?;
        let vtype = media_type(&job.video_type)?;
        let vs = writer.AddStream(&vtype)?;
        writer.SetInputMediaType(vs, &vtype, None)?;
        let audio = match &job.audio {
            Some(a) => {
                let atype = media_type(&a.type_blob)?;
                let s = writer.AddStream(&atype)?;
                writer.SetInputMediaType(s, &atype, None)?;
                Some((s, &a.packets[..]))
            }
            None => None,
        };
        writer.BeginWriting()?;

        // En orden de tiempo, intercalados.
        let mut video = durations(&job.packets, job.base, job.frame_dur).peekable();
        let mut sound = audio.map(|(s, p)| (s, durations(p, job.base, time_of(AAC_FRAME as i64)).peekable()));
        loop {
            let vt = video.peek().map(|v| v.1);
            let at = sound.as_mut().and_then(|(_, a)| a.peek().map(|x| x.1));
            let write_audio = match (vt, at) {
                (None, None) => break,
                (Some(v), Some(a)) => a < v,
                (None, Some(_)) => true,
                (Some(_), None) => false,
            };
            if write_audio {
                let (s, a) = sound.as_mut().unwrap();
                let (p, t, d) = a.next().unwrap();
                writer.WriteSample(*s, &sample(p, t, d)?)?;
            } else {
                let (p, t, d) = video.next().unwrap();
                writer.WriteSample(vs, &sample(p, t, d)?)?;
            }
        }
        writer.Finalize()?;
    }
    Ok(())
}

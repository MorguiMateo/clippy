//! Grabación: captura de pantalla y audio, codificación en la GPU, buffer en RAM y guardado de clips.

mod aac;
pub mod audio;
mod capture;
pub mod clock;
mod convert;
mod cursor;
mod encoder;
mod game;
mod mux;
mod screen;
mod track;
pub mod recorder;

pub use encoder::{Device, CPU_MAX_FPS, CPU_MAX_SHORT};

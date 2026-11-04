//! ffcine core types shared by every other ffcine crate: the [`Error`] type, param tracks and
//! expressions ([`anim`]), colour spaces
//! ([`ColorSpace`]), 3D LUTs ([`Lut`]), probed media info ([`MediaInfo`]) and 2D geometry
//! ([`geom`]), camera input profiles and their LUTs ([`camera`]), and libav-free probes for camera
//! files: ISO BMFF ([`bmff`]), WAV ([`wav`]) and picture profile detection ([`profile`]). No GPU or
//! ffmpeg dependency (feature `ffmpeg` only adds `Error::Ffmpeg`).

pub mod anim;
pub mod bmff;
pub mod camera;
pub mod color;
mod error;
pub mod geom;
pub mod lut;
pub mod probe;
pub mod profile;
pub mod wav;

pub use color::ColorSpace;
pub use error::{Error, Result};
pub use lut::{Lut, LutType};
pub use probe::{MediaInfo, Rational};
pub use profile::{Confidence, PictureProfile, ProfileGuess};

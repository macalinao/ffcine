//! Backends. Only ffmpeg (libav* I/O) + libplacebo (Vulkan processing) exists today.

// libav's `AVBufferRef::data` is a byte pointer to suitably aligned contexts; FFI setup
// functions read best as one sequence.
#![allow(clippy::cast_ptr_alignment, clippy::too_many_lines)]

#[cfg(feature = "ffmpeg")]
mod ffmpeg;
#[cfg(feature = "placebo")]
pub mod placebo;

#[cfg(all(feature = "ffmpeg", feature = "placebo"))]
mod av;
#[cfg(all(feature = "ffmpeg", feature = "placebo"))]
mod denoise;
#[cfg(all(feature = "ffmpeg", feature = "placebo"))]
pub mod doctor;
#[cfg(all(feature = "ffmpeg", feature = "placebo"))]
mod gpu;
#[cfg(all(feature = "ffmpeg", feature = "placebo"))]
mod hw;
#[cfg(all(feature = "ffmpeg", feature = "placebo"))]
mod io;
#[cfg(feature = "ffmpeg")]
mod retime;
#[cfg(feature = "ffmpeg")]
mod tiles;

#[cfg(feature = "ffmpeg")]
pub use ffmpeg::Ffmpeg;

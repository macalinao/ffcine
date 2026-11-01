//! Audio (roadmap step 6, `docs/design.md#roadmap`; usage in `docs/audio.md`): one audio graph
//! rendered per timeline, not per shot, with no GPU.
//!
//! - [`sync`]: lines up timecode-less recorder WAVs (Zoom H2essential) with the camera clips'
//!   scratch audio by cross-correlation, with drift, and writes them as audio clips.
//! - [`schema`]: `ffcine.audio.clean/1` (high-pass, wind, de-click, denoise, dialogue polish via
//!   libavfilter), `ffcine.audio.level/1` (R128 levelling, gain, ducking, fades) and buses.
//! - [`mix`]: [`render_audio`] mixes the timeline's audio tracks into dialogue, music, ambience
//!   and SFX buses (music ducked under dialogue), then a master at -14 LUFS under a 4x
//!   oversampled true-peak limiter.
//!
//! ```no_run
//! use ffcine_audio::{MixOptions, render_audio};
//! use ffcine_timeline::Timeline;
//!
//! let tl = Timeline::from_file("edit.otio")?;
//! let mix = render_audio(&tl, &MixOptions::default())?;
//! mix.master.write_wav("mix.wav".as_ref())?;
//! # Ok::<(), ffcine_core::Error>(())
//! ```

// Sample maths converts between indices, times and gains everywhere; DSP reads best with short
// names (a, b, k, n, x, y).
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names
)]

pub mod beats;
pub mod buffer;
pub mod decode;
pub mod dsp;
pub mod mix;
pub mod place;
pub mod schema;
pub mod sync;

pub use beats::Beats;
pub use buffer::Audio;
pub use mix::{ClipDefaults, Master, Mix, MixOptions, render_audio, sources, write_mix};
pub use schema::{Bus, Clean, Denoise, Level, LevelMode, SyncInfo};
pub use sync::{ClipSync, Match, SyncOptions, WriteOptions, sync_files, write_timeline};

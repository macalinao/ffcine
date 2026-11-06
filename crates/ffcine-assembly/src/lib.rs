//! Assembly (roadmap step 8, `docs/design.md#roadmap`, [`docs/assembly.md`]): renders a whole
//! timeline into one master with a content-keyed segment cache.
//!
//! - [`plan`](mod@plan) flattens the picture track into segments (one per shot, dissolve overlap and gap)
//!   and the audio tracks into one `ffcine_audio` mix, each keyed by a hash of its content
//!   ([`recipe`]), never by position. The mix key has nothing of the picture in it.
//! - [`exec`] renders only segments missing from the cache, joins them by stream copy (one shared
//!   encoder config, every segment starting on an IDR), and muxes the audio; an audio-only change
//!   is a remux of the cached picture. Every cache entry is written atomically, so an interrupted
//!   render resumes where it stopped.
//! - [`diff`] lists which segments an edit re-renders.
//! - [`proxy`] finds or makes the low-res copies `review` renders decode.
//!
//! Text (`ffcine.text`, via `ffcine-text`) is drawn in the segments it overlaps: segments are cut
//! at text in/out points and keyed by the text they show.
//!
//! [`docs/assembly.md`]: https://github.com/talaagent/ffcine/blob/master/docs/assembly.md

pub mod comp;
pub mod diff;
pub mod exec;
mod filter_prep;
pub mod plan;
pub mod profile;
pub mod proxy;
pub mod recipe;

pub use diff::Diff;
pub use exec::{Detector, Grader, Progress, Renderer, Report};
pub use plan::{
    AudioPlan, COMP_GENERATOR, CompMedia, Filters, Plan, PlanError, Ramp, Segment, Shot, TextLayer,
    Unit, has_comp_clips, plan, plan_comp, plan_with,
};
pub use profile::Profile;
pub use recipe::{AUDIO_REVISION, FsFingerprinter, Key, RENDER_REVISION};

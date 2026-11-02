//! Reframe (roadmap step 5, `docs/design.md#roadmap`, `docs/reframe.md`): keyframed crop windows
//! for punch-ins, eased push-ins, pans and vertical aspect windows (9:16 reels, 4:5 feed) from
//! 16:9 sources, evaluated in Rust per frame and applied as the crop/scale of the libplacebo grade
//! pass. Replaces the prototype's per-frame libplacebo crop expressions (`render.py` `gpu_geo`),
//! without their pixel rounding: windows are `f64` until libplacebo resamples them sub-pixel.
//!
//! - [`Reframe`]: the `ffcine.geo.reframe/1` effect (prototype fields, keyframes, per-profile
//!   overrides), evaluated into a [`Frame`] per output frame.
//! - [`Profile`]: the output frame (one timeline renders once per profile), with a [`SafeArea`]
//!   overlay for review.
//! - [`pass::apply`]: puts the frames into a [`ffcine_render::Grade`], composed with the
//!   stabilizer's warp ([`ffcine_core::geom::Mat3`]) when there is one.

pub mod geo;
pub mod keyframe;
pub mod pass;
pub mod profile;
pub mod reframe;
pub mod window;

pub use geo::{Geo, Mapped};
pub use keyframe::{Interp, Key, Keyed};
pub use profile::{Profile, SafeArea};
pub use reframe::{Fit, Frame, Punch, Reframe, Shot, Slide};
pub use window::Rect;

#[cfg(test)]
mod tests;

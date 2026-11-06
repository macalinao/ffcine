//! Compositions (`ffcine.comp/1`, `docs/compositing.md`): a canvas and layers, bottom first,
//! whose parameters are param tracks ([`ffcine_core::anim`]). A comp is read and validated once
//! ([`Comp`]), cut into pieces at layer in/out points ([`Comp::pieces`]), and each piece is
//! lowered on the CPU into what the GPU pass takes: a base input and grade plus
//! [`ffcine_render::Layer`]s with every track sampled per frame ([`Comp::lower`]). The piece's
//! key hashes those sampled values, not the track definitions.
//!
//! Built (steps 2 and 3 of the plan): `video`, `still`, `solid`, `text` and nested `comp`
//! layers, placement (`size`, `fit`, `content`, `crop`, `transform`), `radius`, shape `mask`,
//! `blend`, `fade`, the canvas background and fade, and base promotion. Text lowers to
//! [`ffcine_render::Overlay`]s drawn under the next layer up; a nested comp to a
//! [`ffcine_render::LayerSource::Scene`] composited in the same pass. Per-layer grades, time
//! remapping and filter graphs are rejected with the step that adds them.

mod lower;
mod model;
mod time;

pub use lower::{Base, Media, Piece};
pub use model::{
    Canvas, Comp, CompLayer, Fit, MaskShape, SCHEMA, Source, is_comp, is_comp_file, limits,
};
pub use time::pick as frame_pick;

#[cfg(test)]
mod tests;

//! Compositions (`ffcine.comp/1`, `docs/compositing.md`): a canvas and layers, bottom first,
//! whose parameters are param tracks ([`ffcine_core::anim`]). A comp is read and validated once
//! ([`Comp`]), cut into pieces at layer in/out points ([`Comp::pieces`]), and each piece is
//! lowered on the CPU into what the GPU pass takes: a base input and grade plus
//! [`ffcine_render::Layer`]s with every track sampled per frame ([`Comp::lower`]). The piece's
//! key hashes those sampled values, not the track definitions.
//!
//! Built (step 2 of the plan): `video` and `solid` layers, placement (`size`, `fit`,
//! `transform`), `radius`, shape `mask`, `blend`, `fade`, the canvas background and fade, and
//! base promotion. Stills, text, nested comps, per-layer grades, time remapping and filter graphs
//! are rejected with the step that adds them.

mod lower;
mod model;

pub use lower::{Base, Media, Piece};
pub use model::{Canvas, Comp, CompLayer, Fit, MaskShape, SCHEMA, Source, is_comp};

#[cfg(test)]
mod tests;

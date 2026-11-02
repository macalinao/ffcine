//! Frame filters that are ours rather than libplacebo render params: a motion-compensated
//! temporal denoiser, a spatial NLM, and film grain. `docs/denoise.md`,
//! `docs/design.md#frame-filters`.
//!
//! A [`Filter`] says where it runs ([`Point`]) and what it reads ([`Footprint`], motion), and
//! [`FilterChain::plan`] groups a chain into dispatches: a pointwise filter folds into the
//! dispatch before it; one that gathers neighbours (spatial or temporal) needs the whole previous
//! result written out first, so it starts a new dispatch.
//!
//! How they run (`ffcine-render`, `backend/denoise.rs`), all on the grade pass's `pl_gpu` (so
//! ffmpeg's `VkDevice`, its frames), nothing leaving VRAM:
//!
//! - [`Point::Source`] (denoise) runs as compute dispatches on the decoded planes right before
//!   `pl_render_image`, into textures the render then reads as its source frame. Not a `pl_hook`:
//!   temporal filters need frames *after* the current one, and a hook can't delay its output.
//! - [`Point::Output`] (grain) is pointwise, so it becomes an mpv `.hook` on `OUTPUT` and fuses
//!   into the render pass itself.
//!
//! Kernels are generated GLSL ([`denoise`], [`grain`]); this crate has no GPU dependency.

pub mod denoise;
pub mod effect;
pub mod grain;

pub use denoise::{Denoise, Motion, Overrides, Preset, Spatial, Temporal};
pub use grain::Grain;

use std::fmt;

/// Where in the libplacebo grade pass a filter runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Point {
    /// Source planes before colour conversion, tone mapping and scaling (`PL_HOOK_NATIVE`):
    /// denoise, deband.
    Source,
    /// Graded, scaled output before dither (`PL_HOOK_OUTPUT`): grain, sharpen.
    Output,
}

/// What a filter reads around the pixel it writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Footprint {
    /// Only the same pixel of the current frame; fuses into the dispatch before it.
    Pixel,
    /// Pixels within `radius` of it in the current frame.
    Spatial { radius: u32 },
    /// Pixels within `radius` in the current frame and the `past` frames before and `future`
    /// frames after it (a ring of textures, motion-compensated when [`Filter::motion`] is set).
    /// `future` frames delay the output by as many frames.
    Temporal { past: u32, future: u32, radius: u32 },
}

impl Footprint {
    /// Whether the filter reads anything but its own pixel, so can't fuse into the previous
    /// dispatch.
    pub const fn gathers(self) -> bool {
        !matches!(self, Self::Pixel)
    }

    /// Previous frames it needs.
    pub const fn past(self) -> u32 {
        match self {
            Self::Temporal { past, .. } => past,
            _ => 0,
        }
    }

    /// Later frames it needs (output delay).
    pub const fn future(self) -> u32 {
        match self {
            Self::Temporal { future, .. } => future,
            _ => 0,
        }
    }
}

/// One frame filter.
pub trait Filter: fmt::Debug + Send + Sync {
    /// Short name for logs and the timeline (`denoise`, `grain`, ...).
    fn name(&self) -> &'static str;
    fn point(&self) -> Point;
    fn footprint(&self) -> Footprint;
    /// Whether it needs motion vectors to the other frames it reads.
    fn motion(&self) -> bool {
        false
    }
}

/// One compute dispatch: a gathering filter (or the frame as is) followed by pointwise ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pass {
    pub point: Point,
    /// Indices into the chain, in order.
    pub filters: Vec<usize>,
    /// Past frames to keep for this pass.
    pub past: u32,
    /// Frames of lookahead.
    pub future: u32,
    pub motion: bool,
}

/// Filters in the order they apply within each [`Point`].
#[derive(Debug, Default)]
pub struct FilterChain {
    pub filters: Vec<Box<dyn Filter>>,
}

impl FilterChain {
    #[must_use]
    pub fn push(mut self, filter: impl Filter + 'static) -> Self {
        self.filters.push(Box::new(filter));
        self
    }

    /// Groups the filters into dispatches, `Source` passes first.
    pub fn plan(&self) -> Vec<Pass> {
        let mut order: Vec<usize> = (0..self.filters.len()).collect();
        order.sort_by_key(|&i| self.filters[i].point());
        let mut passes: Vec<Pass> = Vec::new();
        for i in order {
            let f = &self.filters[i];
            let fp = f.footprint();
            match passes.last_mut() {
                Some(p) if p.point == f.point() && !fp.gathers() => p.filters.push(i),
                _ => passes.push(Pass {
                    point: f.point(),
                    filters: vec![i],
                    past: fp.past(),
                    future: fp.future(),
                    motion: f.motion(),
                }),
            }
        }
        passes
    }
}

impl Filter for Temporal {
    fn name(&self) -> &'static str {
        "denoise.temporal"
    }
    fn point(&self) -> Point {
        Point::Source
    }
    fn footprint(&self) -> Footprint {
        Footprint::Temporal {
            past: self.radius,
            future: self.radius,
            radius: 1,
        }
    }
    fn motion(&self) -> bool {
        true
    }
}

impl Filter for Spatial {
    fn name(&self) -> &'static str {
        "denoise.spatial"
    }
    fn point(&self) -> Point {
        Point::Source
    }
    fn footprint(&self) -> Footprint {
        Footprint::Spatial {
            radius: self.radius + 1,
        }
    }
}

impl Filter for Grain {
    fn name(&self) -> &'static str {
        "grain"
    }
    fn point(&self) -> Point {
        Point::Output
    }
    fn footprint(&self) -> Footprint {
        Footprint::Pixel
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Stub(&'static str, Point, Footprint);

    impl Filter for Stub {
        fn name(&self) -> &'static str {
            self.0
        }
        fn point(&self) -> Point {
            self.1
        }
        fn footprint(&self) -> Footprint {
            self.2
        }
        fn motion(&self) -> bool {
            self.2.past() > 0
        }
    }

    fn filters(p: &Pass) -> Vec<usize> {
        p.filters.clone()
    }

    #[test]
    fn pointwise_filters_fuse_behind_a_gather() {
        let chain = FilterChain::default()
            .push(Stub(
                "denoise",
                Point::Source,
                Footprint::Temporal {
                    past: 2,
                    future: 2,
                    radius: 3,
                },
            ))
            .push(Stub("lift", Point::Source, Footprint::Pixel))
            .push(Stub("grain", Point::Output, Footprint::Pixel));
        let plan = chain.plan();
        assert_eq!(plan.len(), 2);
        assert_eq!(
            (filters(&plan[0]), plan[0].past, plan[0].motion),
            (vec![0, 1], 2, true)
        );
        assert_eq!((plan[1].point, filters(&plan[1])), (Point::Output, vec![2]));
    }

    #[test]
    fn each_gather_starts_a_dispatch_and_points_sort() {
        let chain = FilterChain::default()
            .push(Stub(
                "sharpen",
                Point::Output,
                Footprint::Spatial { radius: 2 },
            ))
            .push(Stub(
                "deband",
                Point::Source,
                Footprint::Spatial { radius: 16 },
            ))
            .push(Stub(
                "denoise",
                Point::Source,
                Footprint::Spatial { radius: 7 },
            ));
        let plan: Vec<_> = chain.plan().iter().map(filters).collect();
        assert_eq!(plan, [vec![1], vec![2], vec![0]]);
    }
}

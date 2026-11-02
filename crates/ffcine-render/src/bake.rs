//! The LUT baker: a camera LUT, a grade LUT and the look ([`LookParams`]) evaluated over a lattice
//! into one 3D LUT, with no ffmpeg in the loop. The result goes on the source as one
//! `pl_custom_lut` (or out as a `.cube` for other tools), in the order the GPU pass runs them:
//! camera LUT, grade LUT, look.
//!
//! LUTs are sampled trilinearly, as libplacebo samples them, so a baked LUT matches the unbaked
//! chain up to the lattice spacing.

use ffcine_core::lut::Lut;

use crate::shaders::LookParams;

/// The default lattice: 65^3, the prototype's bake size (and DJI's and Canon's LUTs').
pub const DEFAULT_SIZE: usize = 65;

/// What to bake, applied in this order.
#[derive(Debug, Clone, Default)]
pub struct Bake<'a> {
    /// Log -> Rec.709 camera LUT ([`ffcine_core::camera`]).
    pub camera: Option<&'a Lut>,
    /// A creative or show LUT, on the Rec.709 picture.
    pub lut: Option<&'a Lut>,
    /// The post-LUT look.
    pub look: LookParams,
}

impl Bake<'_> {
    /// One value through the chain.
    pub fn eval(&self, rgb: [f32; 3]) -> [f32; 3] {
        let mut v = rgb;
        for l in [self.camera, self.lut].into_iter().flatten() {
            v = l.sample(v);
        }
        if self.look.is_identity() {
            v
        } else {
            self.look.apply(v)
        }
    }

    /// The chain as a `size`^3 LUT on 0..1 input.
    pub fn bake(&self, size: usize) -> Lut {
        let mut lut = Lut::identity(size.max(2));
        for e in &mut lut.data {
            *e = self.eval(*e);
        }
        let parts: Vec<&str> = [self.camera, self.lut]
            .into_iter()
            .flatten()
            .map(|l| l.title.as_deref().unwrap_or("LUT"))
            .chain((!self.look.is_identity()).then_some("look"))
            .collect();
        lut.title = Some(format!("ffcine bake: {}", parts.join(" + ")));
        lut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(f: impl Fn(f32) -> f32) -> Lut {
        let mut l = Lut::identity(17);
        for e in &mut l.data {
            *e = e.map(&f);
        }
        l
    }

    #[test]
    fn identity_bakes_to_identity() {
        let b = Bake::default().bake(9);
        assert_eq!(b.data, Lut::identity(9).data);
    }

    #[test]
    fn chain_order_and_accuracy() {
        let cam = mono(|x| x * x);
        let grade = mono(|x| 0.5 + 0.5 * x);
        let look = LookParams {
            saturation: 0.8,
            contrast: 0.1,
            ..LookParams::default()
        };
        let b = Bake {
            camera: Some(&cam),
            lut: Some(&grade),
            look,
        };
        let baked = b.bake(DEFAULT_SIZE);
        // The baked LUT against the chain, off the lattice.
        for v in [[0.1, 0.4, 0.9], [0.33, 0.33, 0.7], [0.95, 0.05, 0.5]] {
            let want = b.eval(v);
            let got = baked.sample(v);
            for c in 0..3 {
                assert!(
                    (want[c] - got[c]).abs() < 0.01,
                    "{v:?}: {want:?} vs {got:?}"
                );
            }
        }
        // Camera before grade: grade(cam(0.5)) = 0.625, not cam(grade(0.5)) = 0.5625.
        let plain = Bake {
            camera: Some(&cam),
            lut: Some(&grade),
            look: LookParams::default(),
        };
        assert!((plain.eval([0.5; 3])[0] - 0.625).abs() < 0.01);
        assert!(baked.title.unwrap().ends_with("+ look"));
    }
}

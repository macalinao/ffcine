//! Geometry maths: prototype equivalence, smoothness, aspect windows, profiles, warp composition.
#![allow(clippy::needless_pass_by_value)]

use ffcine_core::geom;
use ffcine_render::Grade;
use serde_json::{Value, json};

use crate::pass::{self, GEOMETRY_HOOK, SAFE_AREA_HOOK};
use crate::{Fit, Profile, Rect, Reframe, Shot};

const UHD: Shot = Shot {
    width: 3840,
    height: 2160,
    frames: 120,
    rate: 24000.0 / 1001.0,
    start: 0.0,
};

fn reframe(v: Value, profile: Option<&str>) -> Reframe {
    Reframe::parse(v.as_object().unwrap(), profile).unwrap()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

#[test]
fn static_zoom_is_the_prototype_crop_unrounded() {
    let r = reframe(json!({"zoom": 1.3, "zoom_center": [0.4, 0.6]}), None);
    let f = r.frame(0, &UHD, &Profile::preset("landscape").unwrap());
    // crop=iw/z:ih/z:(iw-iw/z)*cx:(ih-ih/z)*cy, without trunc(/2)*2 or round().
    let (w, h) = (3840.0 / 1.3, 2160.0 / 1.3);
    assert!(close(f.crop.w, w) && close(f.crop.h, h));
    assert!(close(f.crop.x, (3840.0 - w) * 0.4) && close(f.crop.y, (2160.0 - h) * 0.6));
    assert!(f.place.is_none());
    assert!(!r.is_animated());
}

#[test]
fn push_is_smooth_and_keeps_aspect() {
    let r = reframe(
        json!({"push": [1.0, 1.12], "zoom_center": [0.5, 0.4]}),
        None,
    );
    let out = Profile::preset("hd").unwrap();
    let frames = r.frames(&UHD, &out);
    assert_eq!(frames.len(), 120);
    assert!(close(frames[0].crop.w, 3840.0));
    assert!(close(frames[119].crop.w, 3840.0 / 1.12));
    let mut fractional = 0;
    for w in frames.windows(3) {
        let [a, b, c] = [w[0].crop, w[1].crop, w[2].crop];
        // Monotone zoom-in, aspect exact, inside the source.
        assert!(b.w < a.w && c.w < b.w);
        assert!((b.aspect() - 16.0 / 9.0).abs() < 1e-12);
        assert!(b.x >= 0.0 && b.x + b.w <= 3840.0 + 1e-9);
        // No wobble: position and size move together (second differences tiny, the same sign
        // pattern as the size), where the prototype's even-rounded size steps 2 px at a time.
        let d2w = (c.w - b.w) - (b.w - a.w);
        let d2x = (c.x - b.x) - (b.x - a.x);
        assert!(d2w.abs() < 0.2 && d2x.abs() < 0.2, "{d2w} {d2x}");
        // x tracks w exactly: x = (iw - w) * cx.
        assert!(close(b.x, (3840.0 - b.w) * 0.5) && close(b.y, (2160.0 - b.h) * 0.4));
        if b.w.fract() != 0.0 {
            fractional += 1;
        }
    }
    assert!(fractional > 100);
}

#[test]
fn punches_cap_and_cut_on_frames() {
    let r = reframe(
        json!({"zoom": 1.1, "punch": [
            {"at": 1.0, "to": 2.0, "zoom": 2.0, "center": [0.2, 0.3]},
            {"at": 3.0}
        ]}),
        None,
    );
    let out = Profile::preset("landscape").unwrap();
    let at = |i| r.frame(i, &UHD, &out).crop;
    let a = 24; // 1 s at 23.976
    assert!(close(at(a - 1).w, 3840.0 / 1.1));
    // Capped at max_punch (1.4), times the base zoom.
    let p = at(a);
    assert!(close(p.w, 3840.0 / (1.4 * 1.1)));
    assert!(close(p.x, (3840.0 - p.w) * 0.2));
    assert!(close(at(48).w, 3840.0 / 1.1));
    // Open-ended, default zoom 1.25, base centre.
    let q = at(100);
    assert!(close(q.w, 3840.0 / (1.25 * 1.1)) && close(q.x, (3840.0 - q.w) * 0.5));
}

#[test]
fn vertical_windows_slide_and_pan() {
    let reel = Profile::preset("reel").unwrap();
    let feed = Profile::preset("feed").unwrap();
    let r = reframe(json!({"crop_x": 0.3}), None);
    let f = r.frame(0, &UHD, &reel).crop;
    assert!(close(f.w, 1215.0) && close(f.h, 2160.0) && close(f.x, (3840.0 - 1215.0) * 0.3));
    let f = r.frame(0, &UHD, &feed).crop;
    assert!(close(f.w, 1728.0) && close(f.x, (3840.0 - 1728.0) * 0.3));

    // [from, to]: an eased pan over the shot.
    let r = reframe(json!({"crop_x": [0.2, 0.8]}), None);
    assert!(r.is_animated());
    let x = |i| r.frame(i, &UHD, &reel).crop.x / (3840.0 - 1215.0);
    assert!(close(x(0), 0.2));
    assert!(close(x(60), 0.5));
    assert!(x(119) > 0.79 && x(119) < 0.8);

    // The prototype's [[t, x], ...] and the keyframe form agree.
    let proto = reframe(json!({"crop_x": [[1.0, 0.2], [3.0, 0.9]]}), None);
    let rt = |s: f64| json!({"OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": s * 24.0});
    let keys = reframe(
        json!({"crop_x": [
            {"t": rt(1.0), "v": 0.2, "interp": "ease"},
            {"t": rt(3.0), "v": 0.9, "interp": "linear"}
        ]}),
        None,
    );
    for i in [0, 30, 48, 60, 100] {
        assert!(close(
            proto.frame(i, &UHD, &reel).crop.x,
            keys.frame(i, &UHD, &reel).crop.x
        ));
    }

    // Landscape outputs from a 16:9 source keep the whole frame.
    let f = r.frame(10, &UHD, &Profile::preset("hd").unwrap()).crop;
    assert_eq!(f, Rect::frame(3840.0, 2160.0));
}

#[test]
fn keyframed_zoom_and_centre() {
    let rt = |s: f64| json!({"OTIO_SCHEMA": "RationalTime.1", "rate": 24.0, "value": s * 24.0});
    let r = reframe(
        json!({
            "zoom": [{"t": rt(0.0), "v": 1.0, "interp": "ease"}, {"t": rt(4.0), "v": 1.5}],
            "zoom_center": [{"t": rt(0.0), "v": [0.5, 0.5]}, {"t": rt(4.0), "v": [0.8, 0.2]}]
        }),
        None,
    );
    assert!(r.is_animated());
    let out = Profile::preset("landscape").unwrap();
    let shot = Shot { rate: 24.0, ..UHD };
    let f = r.frame(48, &shot, &out).crop; // t = 2 s: halfway
    assert!(close(f.w, 3840.0 / 1.25));
    assert!(close(f.x, (3840.0 - f.w) * 0.65) && close(f.y, (2160.0 - f.h) * 0.35));
    // Keys are source time: a clip starting 2 s into its media is halfway on its first frame.
    let late = Shot { start: 2.0, ..shot };
    assert_eq!(r.frame(0, &late, &out).crop, f);
    // The prototype's crop_x keys stay clip-relative.
    let proto = reframe(json!({"crop_x": [[0.0, 0.0], [1.0, 1.0]]}), None);
    let reel = Profile::preset("reel").unwrap();
    assert_eq!(proto.frame(0, &late, &reel).crop.x, 0.0);
}

#[test]
fn profiles_override_per_deliverable() {
    let v = json!({
        "zoom": 1.1, "crop_x": 0.5,
        "profiles": {"reel": {"crop_x": 0.2}, "feed": {"crop_x": 0.7, "zoom": 1.0}}
    });
    let reel = reframe(v.clone(), Some("reel"));
    let feed = reframe(v.clone(), Some("feed"));
    let base = reframe(v, Some("landscape"));
    let rp = Profile::preset("reel").unwrap();
    let fp = Profile::preset("feed").unwrap();
    let r = reel.frame(0, &UHD, &rp).crop;
    let aspect = Rect::frame(3840.0, 2160.0).fit_aspect(9.0 / 16.0, [0.2, 0.5]);
    assert_eq!(r, aspect.zoom(1.1, [0.5, 0.5]));
    let f = feed.frame(0, &UHD, &fp).crop;
    assert!(close(f.w, 1728.0) && close(f.x, (3840.0 - 1728.0) * 0.7));
    let centred = Rect::frame(3840.0, 2160.0).fit_aspect(9.0 / 16.0, [0.5, 0.5]);
    assert_eq!(base.frame(0, &UHD, &rp).crop, centred.zoom(1.1, [0.5, 0.5]));
}

#[test]
fn contain_places_the_whole_picture() {
    let r = reframe(json!({"fit": "contain", "fit_y": 0.42}), None);
    assert_eq!(r.fit, Fit::Contain);
    let f = r.frame(0, &UHD, &Profile::preset("reel").unwrap());
    assert_eq!(f.crop, Rect::frame(3840.0, 2160.0));
    let p = f.place.unwrap();
    assert!(close(p.w, 1080.0) && close(p.h, 608.0));
    assert!(close(p.y, (1920.0f64 * 0.42 - 607.5 / 2.0).round()));
    // A blurred fill around it, in pixels of the placed picture; `fill_blur: 0` is black.
    assert!(close(f.blur.unwrap(), 0.04 * 608.0));
    let mut g = Grade::default();
    pass::apply(
        &mut g,
        &[f],
        &[],
        (3840, 2160),
        &Profile::preset("reel").unwrap(),
    );
    assert_eq!(g.border, ffcine_render::Border::Blur(0.04 * 608.0));
    let black = reframe(json!({"fit": "contain", "fill_blur": 0}), None);
    assert_eq!(
        black.frame(0, &UHD, &Profile::preset("reel").unwrap()).blur,
        None
    );
    assert!(Reframe::parse(json!({"fill_blur": -1}).as_object().unwrap(), None).is_err());
    // Cover has nothing around it.
    assert_eq!(
        reframe(json!({}), None)
            .frame(0, &UHD, &Profile::preset("reel").unwrap())
            .blur,
        None
    );
    // Resolve's sizing mode maps onto the same.
    assert_eq!(reframe(json!({"mode": "fit"}), None).fit, Fit::Contain);
}

#[test]
fn panel_focus_defaults_the_window() {
    let r = reframe(json!({"focus": [0.2, 0.7], "zoom": 1.2}), None).focused();
    let panel = Profile::new("panel", 954, 1080);
    let f = r.frame(0, &UHD, &panel);
    // The panel-aspect window slid to 0.2 of the slack, then zoomed about the focus.
    let aw = 2160.0 * 954.0 / 1080.0;
    let ax = (3840.0 - aw) * 0.2;
    assert!(close(f.crop.w, aw / 1.2));
    assert!(close(f.crop.x, ax + (aw - aw / 1.2) * 0.2));
    assert!(close(f.crop.y, (2160.0 - 2160.0 / 1.2) * 0.7));
    // Explicit fields win over the focus; without one nothing changes.
    let explicit = reframe(json!({"focus": [0.2, 0.7], "crop_x": 0.9}), None).focused();
    assert_eq!(
        explicit.crop_x,
        crate::Slide::Keyed(crate::Keyed::Const(0.9))
    );
    assert_eq!(reframe(json!({}), None).focused(), reframe(json!({}), None));
}

#[test]
fn warp_composes_with_the_window() {
    let reel = Profile::preset("reel").unwrap();
    let r = reframe(json!({"crop_x": [0.2, 0.8], "push": [1.0, 1.1]}), None);
    let frames = r.frames(&UHD, &reel);
    let (iw, ih) = pass::intermediate_size(&frames);
    assert_eq!((iw, ih), (1215, 2160));
    let warps: Vec<_> = (0..120)
        .map(|i| geom::similarity([1920.0, 1080.0], f64::from(i) * 0.1, -2.0, 0.01, 1.03))
        .collect();
    let mats = pass::compose(&frames, &warps, iw, ih);
    assert_eq!(mats.len(), 120);
    for (i, m) in mats.iter().enumerate() {
        // Intermediate pixel -> window pixel -> stabilized source pixel.
        let q = [300.5, 1700.25];
        let c = frames[i].crop;
        let in_window = [
            c.x + q[0] * c.w / f64::from(iw),
            c.y + q[1] * c.h / f64::from(ih),
        ];
        let want = geom::apply(&warps[i], in_window);
        let got = geom::apply(m, q);
        assert!(close(got[0], want[0]) && close(got[1], want[1]));
    }
    // Identity warps leave the window's own map.
    let id = pass::compose(&frames, &[geom::IDENTITY], iw, ih);
    assert_eq!(id[7], frames[7].crop.to_mat3(f64::from(iw), f64::from(ih)));
}

#[test]
fn apply_to_grade() {
    let mut reel = Profile::preset("reel").unwrap();
    reel.safe_area = Some(reel.default_safe_area());
    let r = reframe(json!({"crop_x": [0.2, 0.8]}), None);
    let frames = r.frames(&UHD, &reel);

    let mut g = Grade::default();
    pass::apply(&mut g, &frames, &[], (3840, 2160), &reel);
    assert_eq!(g.size.map(|s| (s.width, s.height)), Some((1080, 1920)));
    assert_eq!(g.crops.len(), 120);
    assert_eq!(g.crop_at(500), g.crops.last().copied());
    assert_eq!(g.hooks.len(), 1);
    assert_eq!(g.hooks[0].name, SAFE_AREA_HOOK);

    let mut g = Grade::default();
    pass::apply(
        &mut g,
        &frames,
        &[geom::translation(3.0, 1.0)],
        (3840, 2160),
        &reel,
    );
    assert!(g.crops.is_empty() && g.crop.is_none());
    let h = &g.hooks[0];
    assert_eq!(h.name, GEOMETRY_HOOK);
    assert_eq!(h.tracks.len(), 9);
    assert!(h.tracks.iter().all(|t| t.values.len() == 120));
    // m02 = warp tx + window x.
    let m02 = &h.tracks[2];
    assert!((f64::from(m02.values[0]) - (frames[0].crop.x + 3.0)).abs() < 1e-3);
    assert!(h.source.contains("//!WIDTH 1215\n//!HEIGHT 2160\n"));
    assert_eq!(h.source.matches("//!PARAM m").count(), 9);
    for line in h.source.lines().chain(g.hooks[1].source.lines()) {
        if let Some(i) = line.find("//!") {
            assert_eq!(i, 0, "{line}");
        }
    }
}

#[test]
fn reads_the_effect_from_a_clip() {
    use ffcine_timeline::schema::{Effect, GenericEffect};
    let params = json!({"zoom": 1.2, "profiles": {"reel": {"crop_x": 0.1}}});
    let e = Effect::Effect(GenericEffect::ffcine(
        "ffcine.geo.reframe/1",
        params.as_object().unwrap().clone(),
    ));
    let r = Reframe::from_effects(&[e], Some("reel")).unwrap().unwrap();
    assert!(r.zoom.is_some());
    assert!(matches!(r.crop_x, crate::Slide::Keyed(crate::Keyed::Const(x)) if close(x, 0.1)));
    assert!(Reframe::from_effects(&[], None).unwrap().is_none());
}

#[test]
fn mapped_hook_takes_the_whole_frame() {
    let reel = Profile {
        safe_area: Some(crate::SafeArea::REELS),
        ..Profile::preset("reel").unwrap()
    };
    let geo = crate::Geo {
        crop: Some(
            crate::geo::Crop::parse(json!({"left": 0.1, "feather": 0.01}).as_object().unwrap())
                .unwrap(),
        ),
        transform: Some(
            crate::geo::Transform::parse(json!({"rotate_deg": 5.0}).as_object().unwrap()).unwrap(),
        ),
    };
    let r = reframe(json!({"fit": "contain", "push": [1.0, 1.1]}), None);
    let mapped: Vec<_> = (0..120).map(|i| geo.frame(&r, i, &UHD, &reel).1).collect();
    let mut g = Grade {
        crop: Some(ffcine_render::Crop {
            x: 1.0,
            y: 1.0,
            width: 2.0,
            height: 2.0,
        }),
        ..Grade::default()
    };
    pass::apply_mapped(&mut g, &mapped, &[], (3840, 2160), &reel);
    assert_eq!((g.crop, g.place, g.warp.as_ref()), (None, None, None));
    assert_eq!(g.crops, []);
    assert_eq!(g.hooks.len(), 2);
    let h = &g.hooks[0];
    assert_eq!(h.name, GEOMETRY_HOOK);
    // Output aspect at the source's scale: 3840 source px over 1080 output px.
    let (iw, ih) = pass::mapped_size(&mapped, (1080, 1920));
    assert_eq!((iw, ih), (3840, 6827));
    assert!(
        h.source
            .contains(&format!("//!WIDTH {iw}\n//!HEIGHT {ih}\n"))
    );
    assert_eq!(h.tracks.len(), 23);
    assert!(h.tracks.iter().all(|t| t.values.len() == 120));
    let mx0 = h.tracks.iter().find(|t| t.name == "mx0").unwrap();
    assert!((f64::from(mx0.values[0]) - 384.0).abs() < 1e-3);
    assert_eq!(g.hooks[1].name, SAFE_AREA_HOOK);
}

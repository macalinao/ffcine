//! The libavfilter node's cache keys (`docs/compositing.md#libavfilter-escape-hatch`, "Cache
//! invalidation tests"): per piece, as the planner keys segments.

use std::path::Path;
use std::sync::Arc;

use ffcine_core::Result;
use ffcine_render::filter_node::Prep;
use ffcine_render::{LayerSource, Size};
use serde_json::{Value, json};

use crate::{Comp, Media};

/// 1920x1080, 30 fps, 300 frames; identities are paths; `ffmpeg_build` as given.
struct Fake(&'static str);

impl Media for Fake {
    fn dims(&self, _: &Path) -> Result<(u32, u32)> {
        Ok((1920, 1080))
    }
    fn identity(&self, path: &Path) -> Value {
        json!(path)
    }
    fn video_rate(&self, _: &Path) -> Result<(f64, u64)> {
        Ok((30.0, 300))
    }
    fn ffmpeg_build(&self) -> String {
        self.0.into()
    }
}

const OUT: Size = Size {
    width: 1080,
    height: 1920,
};

/// A 6 s, 30 fps comp: `layers`, over a solid from 2 s to 4 s that cuts it in three pieces.
#[allow(clippy::needless_pass_by_value)]
fn comp(layers: Value) -> Comp {
    let mut all = vec![json!({ "source": { "solid": "white" }, "in": 2.0, "out": 4.0 })];
    all.extend(layers.as_array().expect("array").iter().cloned());
    Comp::from_value(
        &json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [1080, 1920], "fps": "30", "duration": 6.0 },
            "layers": all,
        }),
        Path::new("/media"),
    )
    .expect("comp")
}

/// A filtered video layer with `filter` and the other `keys`.
#[allow(clippy::needless_pass_by_value)]
fn layer(filter: Value, keys: Value) -> Value {
    let mut l = json!({ "source": { "video": "a.mov" }, "filter": filter });
    for (k, v) in keys.as_object().expect("object") {
        l[k] = v.clone();
    }
    l
}

fn keys_with(c: &Comp, m: &Fake) -> Vec<Value> {
    c.pieces()
        .into_iter()
        .map(|r| c.lower(r, OUT, m).expect("lower").recipe)
        .collect()
}

fn keys(c: &Comp) -> Vec<Value> {
    keys_with(c, &Fake("ffmpeg 1"))
}

/// The filter nodes' preps of each piece.
fn preps(c: &Comp) -> Vec<Vec<Arc<Prep>>> {
    c.pieces()
        .into_iter()
        .map(|r| {
            c.lower(r, OUT, &Fake("ffmpeg 1"))
                .expect("lower")
                .layers
                .iter()
                .filter_map(|l| match &l.source {
                    LayerSource::Filtered(p) => Some(Arc::clone(p)),
                    _ => None,
                })
                .collect()
        })
        .collect()
}

fn graph(g: &str) -> Value {
    json!({ "graph": g })
}

#[test]
fn a_graph_edit_rerenders_only_its_layers_pieces() {
    let c = |g: &str| comp(json!([layer(graph(g), json!({ "out": 4.0 }))]));
    let (a, b) = (keys(&c("tmix=frames=3")), keys(&c("tmix=frames=5")));
    assert_eq!(a.len(), 3);
    assert_ne!(a[0], b[0]);
    assert_ne!(a[1], b[1]);
    assert_eq!(a[2], b[2], "the piece without the layer");
    // A layer without a filter keeps its key beside one.
    let plain = json!({ "source": { "video": "b.mov" }, "transform": { "position": [0, 0] } });
    let with = |g: &str| {
        comp(json!([
            plain.clone(),
            layer(graph(g), json!({ "in": 4.0 }))
        ]))
    };
    let (a, b) = (keys(&with("eq=gamma=1.1")), keys(&with("eq=gamma=1.2")));
    assert_eq!(a[..2], b[..2]);
    assert_ne!(a[2], b[2]);
}

#[test]
fn moving_a_layer_clocked_node_keeps_its_intermediate() {
    let at = |clock: &str, t: f64| {
        let c = comp(json!([layer(
            json!({ "graph": "eq=eval=frame:gamma=1+0.1*sin(t)", "clock": clock }),
            json!({ "in": t, "out": t + 1.5 }),
        )]));
        preps(&c)
            .concat()
            .iter()
            .map(|p| p.key.clone())
            .collect::<Vec<_>>()
    };
    // Inside the solid's first piece both times: one piece holds the whole layer.
    assert_eq!(at("layer", 0.2), at("layer", 0.4));
    assert_eq!(at("layer", 0.2).len(), 1);
    assert_ne!(at("comp", 0.2), at("comp", 0.4));
}

#[test]
fn trimming_the_tail() {
    let c = |filter: Value, out: f64| keys(&comp(json!([layer(filter, json!({ "out": out }))])));
    let (a, b) = (c(graph("eq=gamma=1.2"), 6.0), c(graph("eq=gamma=1.2"), 5.5));
    assert_eq!(a.len(), 3);
    assert_eq!(a[..2], b[..2], "stateless: only the last piece");
    assert_ne!(a[2], b[2]);
    let whole = json!({ "graph": "eq=gamma=1.2", "whole_clip": true });
    let (a, b) = (c(whole.clone(), 6.0), c(whole, 5.5));
    for i in 0..3 {
        assert_ne!(a[i], b[i], "whole clip: piece {i}");
    }
}

#[test]
fn tmix_keys_its_pre_roll_and_follows_the_cut() {
    let at = |cut: f64| {
        let c = Comp::from_value(
            &json!({
                "schema": "ffcine.comp/1",
                "canvas": { "size": [1080, 1920], "fps": "30", "duration": 6.0 },
                "layers": [
                    { "source": { "solid": "white" }, "in": cut, "out": 4.0 },
                    layer(graph("tmix=frames=5"), json!({})),
                ],
            }),
            Path::new("/media"),
        )
        .expect("comp");
        (keys(&c), preps(&c))
    };
    let (ka, a) = at(2.0);
    let p = &a[1][0];
    // The piece from frame 60 feeds source frames 56..120: 4 frames of pre-roll, dropped.
    assert_eq!(p.map.len(), 64);
    assert_eq!(p.start.as_secs_f64(), (56.0 - 0.5) / 30.0);
    assert_eq!((p.store.clone(), p.show.clone()), (4..64, 0..60));
    assert_eq!(p.pts, 56);
    // The first piece has nothing before it to roll.
    assert_eq!(a[0][0].map.len(), 60);
    let (kb, b) = at(2.5);
    let p = &b[1][0];
    assert_eq!(p.map.len(), 49);
    assert_eq!(p.pts, 71);
    assert_ne!(ka[0], kb[0]);
    assert_ne!(ka[1], kb[1]);
    assert_eq!(ka[2], kb[2], "the piece after the moved cut is the same");
}

#[test]
fn ffmpeg_build_and_table_revision() {
    let c = comp(json!([
        layer(graph("tmix=frames=3"), json!({ "out": 2.0 })),
        layer(graph("eq=gamma=1.2"), json!({ "in": 2.0, "out": 4.0 })),
        { "source": { "video": "b.mov" }, "in": 4.0 },
    ]));
    let a = keys_with(&c, &Fake("ffmpeg 1"));
    let b = keys_with(&c, &Fake("ffmpeg 2"));
    assert_ne!(a[0], b[0]);
    assert_ne!(a[1], b[1]);
    assert_eq!(a[2], b[2], "no filter node, no ffmpeg in the key");
    crate::lower::TABLE.set(ffcine_render::filter_node::FILTER_TABLE_REVISION + 1);
    let t = keys(&c);
    crate::lower::TABLE.set(ffcine_render::filter_node::FILTER_TABLE_REVISION);
    assert_ne!(a[0], t[0], "stateful");
    assert_eq!(a[1], t[1], "stateless");
    assert_eq!(a[2], t[2]);
}

#[test]
fn equivalent_expressions_and_later_edits() {
    let c = |speed: Value| {
        keys(&comp(json!([layer(
            graph("tmix=frames=3"),
            json!({ "time": { "speed": speed } }),
        )])))
    };
    assert_eq!(c(json!(1.5)), c(json!("3 / 2")));
    assert_eq!(
        c(json!(1.5)),
        c(json!([{ "t": 0, "v": 1.5 }, { "t": 3, "v": 1.5 }]))
    );
    let (a, b) = (c(json!("if(lt < 4, 1, 1)")), c(json!("if(lt < 4, 1, 2)")));
    assert_eq!(a[..2], b[..2]);
    assert_ne!(a[2], b[2]);
}

#[test]
fn n_and_timing_are_checked() {
    let err = |filter: Value| {
        Comp::from_value(
            &json!({
                "schema": "ffcine.comp/1",
                "canvas": { "size": [1080, 1920], "fps": "30", "duration": 6.0 },
                "layers": [layer(filter, json!({}))],
            }),
            Path::new("/media"),
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
    };
    assert!(err(graph("eq=eval=frame:gamma=1+mod(n\\,2)")).contains("filter.graph"));
    assert_eq!(
        err(json!({ "graph": "eq=eval=frame:gamma=1+mod(n\\,2)", "whole_clip": true })),
        ""
    );
    assert!(err(graph("minterpolate=fps=60")).contains("timing"));
    assert_eq!(
        err(json!({ "graph": "minterpolate=fps=60", "timing": "free" })),
        ""
    );
    assert!(err(graph("[in]split[a][b];[a][b]blend=all_mode=average")).contains("[out]"));
}

/// [`Fake`], hashing small files by their bytes.
struct Contents;

impl Media for Contents {
    fn dims(&self, p: &Path) -> Result<(u32, u32)> {
        Fake("ffmpeg 1").dims(p)
    }
    fn identity(&self, path: &Path) -> Value {
        json!(path)
    }
    fn video_rate(&self, p: &Path) -> Result<(f64, u64)> {
        Fake("ffmpeg 1").video_rate(p)
    }
    fn content_hash(&self, path: &Path) -> Value {
        json!(std::fs::read(path).expect("read"))
    }
}

/// A comp in `dir` at `fps` with one filtered layer.
fn comp_in(dir: &Path, fps: &str, filter: &Value) -> std::result::Result<Comp, String> {
    Comp::from_value(
        &json!({
            "schema": "ffcine.comp/1",
            "canvas": { "size": [1080, 1920], "fps": fps, "duration": 2.0 },
            "layers": [layer(filter.clone(), json!({}))],
        }),
        dir,
    )
    .map_err(|e| e.to_string())
}

fn first_key(c: &Comp) -> Value {
    let r = c.pieces()[0].clone();
    c.lower(r, OUT, &Contents).expect("lower").recipe
}

/// A unique scratch directory for a test.
fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

#[test]
#[allow(clippy::many_single_char_names)]
fn a_referenced_lut_keys_by_its_contents_from_the_comps_dir() {
    let dir = scratch("lut-key");
    let lut = dir.join("look.cube");
    std::fs::write(&lut, "LUT_3D_SIZE 2\n# a\n").expect("write");
    let g = graph("lut3d=file=look.cube");
    let c = comp_in(&dir, "30", &g).expect("comp");
    let a = first_key(&c);
    // The graph ffmpeg runs names the LUT by its absolute path.
    let p = c
        .lower(c.pieces()[0].clone(), OUT, &Contents)
        .expect("lower");
    let run = p
        .layers
        .iter()
        .find_map(|l| match &l.source {
            LayerSource::Filtered(p) => Some(p.graph.clone()),
            _ => None,
        })
        .expect("a filter prep");
    assert!(run.contains(&lut.to_string_lossy().into_owned()), "{run}");
    assert_eq!(a, first_key(&comp_in(&dir, "30", &g).expect("comp")));
    std::fs::write(&lut, "LUT_3D_SIZE 2\n# b\n").expect("write");
    assert_ne!(a, first_key(&comp_in(&dir, "30", &g).expect("comp")));
    // Missing: an error at comp check.
    std::fs::remove_file(&lut).expect("remove");
    let e = comp_in(&dir, "30", &g).err().unwrap_or_default();
    assert!(
        e.contains("filter.graph") && e.contains("no such file"),
        "{e}"
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn canvas_fps_is_in_the_key() {
    let dir = scratch("fps-key");
    let g = graph("eq=eval=frame:gamma=1+0.1*sin(t)");
    let a = first_key(&comp_in(&dir, "30", &g).expect("comp"));
    let b = first_key(&comp_in(&dir, "30000/1001", &g).expect("comp"));
    assert_ne!(a, b);
    // Unfiltered layers key as before: the node's keys are only in filtered layers.
    assert!(a.to_string().contains("\"fps\":30"), "{a}");
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn writers_sources_and_urls_are_refused_at_check() {
    let dir = scratch("policy");
    let e = |g: &str| comp_in(&dir, "30", &graph(g)).err().unwrap_or_default();
    for g in [
        "[in]movie=b.mov[m];[in][m]overlay[out]",
        "metadata=mode=print:file=meta.txt",
        "metadata=mode=print\\:file=meta.txt",
        "metadata=mode=print:fi\\le=meta.txt",
        "psnr=stats_file=psnr.log",
        "vidstabdetect=result=t.trf",
        "sendcmd=f=c.txt,eq",
        "paletteuse=debug_kdtree=k.dot",
        "deshake=filename=d.log",
        "ssim360=stats_file=s.log",
    ] {
        let e = e(g);
        assert!(
            e.contains("filter.graph") && e.contains("allowlist"),
            "{g}: {e}"
        );
    }
    assert!(e("curves=vintage:pl\\ot=p.gp").contains("writes"));
    assert!(e("eq=gamma=1.1:/gamma=g.txt").contains("plain option name"));
    assert!(e("lut3d=file=https\\\\\\://example.com/a.cube").contains("URL"));
    assert_eq!(e("eq=gamma=1.1"), "");
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

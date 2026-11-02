//! The Python prototype's `edit.yaml` (semantics: `render.py` there; field map:
//! `docs/timeline.md`).
//!
//! Shots become clips on `V1` (freezes and Live Photo stills as hold clips after them, dissolves
//! and other transitions as `SMPTE_Dissolve` transitions, split screens as nested stacks), shot
//! overlays go to `PiP` tracks, camera audio to dialogue tracks, and captions, cards, titles,
//! sound effects, music cues and ambience beds to their own tracks. Times follow `render.py`'s
//! frame maths, beat snapping included when [`ImportOptions::beats`] gives the music's beats
//! (without them cuts, cards and titles land unsnapped, reported).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value, json};

use super::{ImportOptions, ImportReport, Imported};
use ffcine_core::{Error, Result};
use ffcine_timeline::metadata::HasMetadata;
use ffcine_timeline::project::{
    ClipGrade, GradeParams, MixParams, Output, OutputMode, Project, Rules,
};
use ffcine_timeline::{
    Clip, Effect, ExternalReference, Gap, GeneratorReference, GenericEffect, Item, LinearTimeWarp,
    Marker, MediaReference, MissingReference, RationalTime, Stack, TimeRange, Timeline, Track,
    Transition, track_kind,
};

/// `metadata.ffcine.foreign` key.
const APP: &str = "edit_yaml";
/// `render.py`'s `FPS`.
const DEFAULT_RATE: f64 = 24000.0 / 1001.0;
/// `sfx.duck_files` default in `render.py`.
const DUCK_FILES: &str = "scratch|giggle|laugh|boing|vine|bruh|oh_no|cha_ching";

/// Shot fields that make up `ffcine.geo.reframe/1`.
const REFRAME_KEYS: &[&str] = &[
    "zoom",
    "zoom_center",
    "punch",
    "push",
    "push_center",
    "pan",
    "fit",
    "fit_y",
    "crop_x",
    "crop_y",
];
/// Split-screen fields that make up `ffcine.comp.layout/1`.
const LAYOUT_KEYS: &[&str] = &[
    "layout",
    "gap",
    "margin",
    "radius",
    "weights",
    "stagger",
    "slide",
    "enter",
    "first_static",
    "audio_panel",
];
/// Top-level sections that become `project.styles.<name>`.
const STYLE_KEYS: &[(&str, &str)] = &[
    ("style", "base"),
    ("title", "title"),
    ("cards", "card"),
    ("locations", "location"),
    ("captions", "caption"),
    ("endcard", "endcard"),
    ("end_screen", "endscreen"),
    ("endscreen", "endscreen"),
];

pub fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let text = std::fs::read_to_string(input).map_err(|source| Error::Io {
        path: input.to_owned(),
        source,
    })?;
    let bad = |msg: String| Error::Import {
        path: input.to_owned(),
        msg,
    };
    let cfg: Value = yaml_serde::from_str(&text).map_err(|e| bad(e.to_string()))?;
    let Value::Object(cfg) = cfg else {
        return Err(bad("not a YAML mapping".into()));
    };
    let input_abs = std::path::absolute(input).map_err(|source| Error::Io {
        path: input.to_owned(),
        source,
    })?;
    let base = input_abs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut c = Converter {
        base,
        opts: *opts,
        rate: DEFAULT_RATE,
        report: ImportReport::default(),
        unmapped: Vec::new(),
        sources: Map::new(),
        styles: Map::new(),
        beats: BTreeMap::new(),
    };
    let timeline = c.run(Obj(cfg)).map_err(bad)?;
    let mut report = c.report;
    report.unmapped(c.unmapped);
    Ok(Imported { timeline, report })
}

/// A YAML mapping whose keys are taken as they get mapped; what's left is foreign.
#[derive(Debug, Default, Clone)]
struct Obj(Map<String, Value>);

impl Obj {
    fn from_value(v: Option<Value>) -> Self {
        match v {
            Some(Value::Object(m)) => Obj(m),
            _ => Obj::default(),
        }
    }

    /// Takes `k`; `null` counts as absent.
    fn take(&mut self, k: &str) -> Option<Value> {
        self.0.shift_remove(k).filter(|v| !v.is_null())
    }

    fn get(&self, k: &str) -> Option<&Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    fn f64(&mut self, k: &str) -> Option<f64> {
        self.take(k).as_ref().and_then(num)
    }

    fn str(&mut self, k: &str) -> Option<String> {
        self.take(k).map(|v| text(&v))
    }

    fn truthy(&mut self, k: &str) -> bool {
        self.take(k).is_some_and(|v| truthy(&v))
    }

    fn obj(&mut self, k: &str) -> Obj {
        Obj::from_value(self.take(k))
    }

    /// Takes the given keys that are present.
    fn take_keys(&mut self, keys: &[&str]) -> Map<String, Value> {
        keys.iter()
            .filter_map(|k| self.take(k).map(|v| ((*k).to_owned(), v)))
            .collect()
    }
}

/// The prototype's mix settings -> `project.mix`: `audio` (`dialogue_lufs`, `nat_lufs`, `xfade`,
/// `highpass`, `clean`, `clean_nat`), `music` (`duck_db`, `nat_duck_db`, `duck_attack`,
/// `duck_release`) and `output.audio` (`lufs`, `true_peak`). Taken out of `cfg`; the rest stays.
fn mix_params(cfg: &mut Obj) -> serde_json::Result<MixParams> {
    const AUDIO: &[&str] = &[
        "dialogue_lufs",
        "nat_lufs",
        "xfade",
        "highpass",
        "clean",
        "clean_nat",
    ];
    const MUSIC: &[&str] = &["duck_db", "nat_duck_db", "duck_attack", "duck_release"];
    const OUTPUT: &[&str] = &["lufs", "true_peak"];
    let mut m = Map::new();
    let mut take = |o: Option<&mut Value>, keys: &[&str]| {
        let Some(Value::Object(o)) = o else { return };
        for k in keys {
            if let Some(v) = o.shift_remove(*k).as_ref().and_then(num) {
                m.insert((*k).to_owned(), v.into());
            }
        }
    };
    take(cfg.0.get_mut("audio"), AUDIO);
    take(cfg.0.get_mut("music"), MUSIC);
    take(
        cfg.0.get_mut("output").and_then(|o| o.get_mut("audio")),
        OUTPUT,
    );
    let empty = |v: Option<&Value>| matches!(v, Some(Value::Object(o)) if o.is_empty());
    if empty(cfg.get("audio")) {
        cfg.take("audio");
    }
    if let Some(Value::Object(o)) = cfg.0.get_mut("output")
        && empty(o.get("audio"))
    {
        o.shift_remove("audio");
    }
    serde_json::from_value(Value::Object(m))
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python truthiness of a YAML value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

fn fps(v: &Value) -> Option<f64> {
    if let Some(s) = v.as_str()
        && let Some((n, d)) = s.split_once('/')
    {
        let (n, d): (f64, f64) = (n.trim().parse().ok()?, d.trim().parse().ok()?);
        return (d != 0.0).then_some(n / d);
    }
    num(v)
}

/// Lexically normalised `base/p` (no filesystem access).
fn join(base: &Path, p: &str) -> PathBuf {
    let joined = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        base.join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// DJI D-Log M: takes the camera LUT.
    Dji,
    Video,
    Photo,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Dji => "dji",
            Kind::Video => "video",
            Kind::Photo => "photo",
        }
    }
}

/// A resolved `src`.
#[derive(Debug, Clone)]
struct Source {
    src: String,
    path: Option<PathBuf>,
    kind: Kind,
}

/// `render.py` `speed_points`: `[(u, speed)]` over the source range.
fn speed_points(ramp: Option<&Value>, speed: f64) -> Vec<(f64, f64)> {
    let pts: Vec<(f64, f64)> = ramp
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some((num(p.first()?)?, num(p.get(1)?)?))
                })
                .collect()
        })
        .unwrap_or_default();
    if pts.is_empty() {
        return vec![(0.0, speed), (1.0, speed)];
    }
    let mut pts = pts;
    if pts[0].0 > 0.0 {
        pts.insert(0, (0.0, pts[0].1));
    }
    if let Some(&(u, s)) = pts.last()
        && u < 1.0
    {
        pts.push((1.0, s));
    }
    pts
}

/// `render.py` `ramp_map`: output length of a piecewise-linear speed ramp over `src_len`.
fn ramp_out_len(pts: &[(f64, f64)], src_len: f64) -> f64 {
    let mut out = 0.0;
    for w in pts.windows(2) {
        let ((u0, a), (u1, b)) = (w[0], w[1]);
        let (t0, t1) = (u0 * src_len, u1 * src_len);
        if t1 - t0 <= 1e-6 {
            continue;
        }
        let m = (b - a) / (t1 - t0);
        out += if m.abs() < 1e-9 {
            (t1 - t0) / a
        } else {
            (b / a).ln() / m
        };
    }
    out
}

fn is_const(pts: &[(f64, f64)]) -> bool {
    pts.iter().all(|p| (p.1 - pts[0].1).abs() < 1e-9)
}

/// A split-screen panel.
#[derive(Debug, Clone)]
struct Panel {
    source: Source,
    in_s: f64,
    out_s: Option<f64>,
    still: Option<Source>,
    still_dur: f64,
    speed: f64,
    look: Option<String>,
    reframe: Map<String, Value>,
    rest: Map<String, Value>,
}

/// A shot, parsed, with its `render.py` timing.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // edit.yaml's flags
struct Shot {
    id: String,
    source: Source,
    in_s: f64,
    out_s: f64,
    audio: String,
    pts: Vec<(f64, f64)>,
    hold_s: f64,
    dissolve_s: f64,
    still: Option<Source>,
    label: Option<String>,
    grade: Option<ClipGrade>,
    blend: Option<f64>,
    stabilize: Option<Value>,
    denoise: Option<Value>,
    reframe: Map<String, Value>,
    layout: Option<(Map<String, Value>, Vec<Panel>)>,
    chapter: Option<String>,
    card: Option<String>,
    location: Option<String>,
    location_at: Option<f64>,
    subs: Vec<(f64, f64, String)>,
    sfx: Vec<Obj>,
    level: Map<String, Value>,
    clean: Option<f64>,
    audio_lead: f64,
    audio_tail: f64,
    caption_y: Option<f64>,
    ai: bool,
    no_auto_sfx: bool,
    snap: bool,
    vignette: Option<f64>,
    darken: Option<f64>,
    fade_in: f64,
    fade_out: f64,
    flash: bool,
    /// `ffcine.transition/1` of a transition other than a plain dissolve.
    transition: Option<Map<String, Value>>,
    overlays: Vec<Obj>,
    rest: Map<String, Value>,
    // Timing, in timeline frames.
    dissolve: i64,
    start: i64,
    base_n: i64,
    n: i64,
    /// `base_n` before beat snapping (the ramp's output length).
    play_n: i64,
    /// Frames the cut moved to land on a beat.
    snapped: i64,
}

impl Shot {
    fn speed(&self) -> f64 {
        self.pts[0].1
    }
}

/// An item to place on a track at an absolute frame.
struct Placed {
    start: i64,
    clip: Clip,
}

struct Converter {
    base: PathBuf,
    opts: ImportOptions,
    rate: f64,
    report: ImportReport,
    unmapped: Vec<String>,
    sources: Map<String, Value>,
    /// `project.styles`, for overlay timing.
    styles: Map<String, Value>,
    /// Beat times per music track name.
    beats: BTreeMap<String, Vec<f64>>,
}

/// A music cue, timed by `Converter::time_shots`.
struct Cue {
    shot: usize,
    offset: f64,
    track: String,
    /// `in` seconds; `None` for `in: end`.
    in_v: Option<f64>,
    snap_in: bool,
    /// Timeline frame, once its shot is timed.
    start: Option<i64>,
    /// Media in point after `snap_in` / `in: end`.
    in_s: Option<f64>,
    /// The rest of the cue.
    o: Obj,
}

/// The value in `v` nearest `x`.
fn nearest(v: &[f64], x: f64) -> Option<f64> {
    v.iter()
        .copied()
        .min_by(|a, b| (a - x).abs().total_cmp(&(b - x).abs()))
}

type Res<T> = std::result::Result<T, String>;

/// A card/location sound effect: `(frame, event, shot id, default gain)`.
type AutoSfx = (i64, Obj, String, f64);

impl Converter {
    #[allow(clippy::cast_possible_truncation)]
    fn s2f(&self, s: f64) -> i64 {
        (s * self.rate).round() as i64
    }

    #[allow(clippy::cast_precision_loss)]
    fn f2s(&self, f: i64) -> f64 {
        f as f64 / self.rate
    }

    #[allow(clippy::cast_precision_loss)]
    fn frames(&self, f: i64) -> RationalTime {
        RationalTime::new(f as f64, self.rate)
    }

    fn secs(&self, s: f64) -> RationalTime {
        RationalTime::from_seconds(s, self.rate)
    }

    fn keep_foreign(&mut self, prefix: &str, rest: &Map<String, Value>) {
        self.unmapped
            .extend(rest.keys().map(|k| format!("{prefix}{k}")));
    }

    #[allow(clippy::too_many_lines)] // a flat walk over the sections
    fn run(&mut self, mut cfg: Obj) -> Res<Timeline> {
        let mut foreign = Map::new();
        if let Some(r) = self.opts.rate {
            self.rate = r;
        } else if let Some(r) = cfg.get("output").and_then(|o| o.get("fps")).and_then(fps) {
            self.rate = r;
        }
        if let Some(Value::Object(s)) = cfg.get("sources") {
            self.sources = s.clone();
        }

        let end_screen = cfg
            .get("end_screen")
            .or_else(|| cfg.get("endscreen"))
            .cloned();
        let project = self.project(&mut cfg)?;
        let shots_v = cfg.take("shots").ok_or("no `shots`")?;
        let Value::Array(shots_v) = shots_v else {
            return Err("`shots` is not a list".into());
        };

        let mut skipped = Vec::new();
        let mut shots = Vec::new();
        for (i, v) in shots_v.into_iter().enumerate() {
            let Value::Object(m) = v else {
                return Err(format!("shots[{i}] is not a mapping"));
            };
            let mut o = Obj(m);
            if o.get("skip").is_some_and(truthy) {
                skipped.push(Value::Object(o.0));
                continue;
            }
            o.take("skip");
            let shot = self.shot(o, shots.len() + 1, end_screen.as_ref())?;
            shots.push(shot);
        }
        if !skipped.is_empty() {
            self.report
                .warn(format!("{} shot(s) with `skip` left out", skipped.len()));
            foreign.insert("skipped_shots".into(), Value::Array(skipped));
            self.unmapped.push("shots[].skip".into());
        }
        let by_id: BTreeMap<String, usize> = shots
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.clone(), i))
            .collect();

        let mut music_cfg = cfg.obj("music");
        let mut sfx_cfg = cfg.obj("sfx");
        let tracks = music_cfg.obj("tracks").0;
        let mut cues = self.cues(&mut music_cfg, &by_id);
        self.load_beats(&tracks, &cues);
        let snap_cuts = music_cfg.get("snap_cuts").is_none_or(truthy);
        let snap_max = music_cfg.get("snap_max").and_then(num).unwrap_or(0.45);
        let unsnapped = cues.iter().any(|c| !self.beats.contains_key(&c.track));
        let total = self.time_shots(
            &mut shots,
            &mut cues,
            &tracks,
            snap_cuts.then_some(snap_max),
        );
        if unsnapped {
            let n = shots
                .iter()
                .enumerate()
                .filter(|(i, s)| {
                    s.snap && matches!(s.audio.as_str(), "music" | "nat") && i + 1 < shots.len()
                })
                .count();
            if n > 0 && snap_cuts {
                self.report.warn(format!(
                    "beat snapping not applied where a cue's track has no beats (no beat \
                     analysis): up to {n} cut(s) may sit up to music.snap_max off the \
                     prototype's, and cards/titles snap_to_beats likewise"
                ));
            }
        } else if !cues.is_empty() {
            // Applied: the snapped timing is the timeline.
            music_cfg.take("snap_cuts");
            music_cfg.take("snap_max");
        }

        let video = self.video_track(&shots);
        let pips = self.pips(&shots);
        let dialogue = self.dialogue(&shots);
        let captions = self.captions(&shots);
        let (overlays, overlay_sfx) = self.overlays(&shots, &by_id, &cues);
        let sfx = self.sfx(&shots, overlay_sfx, &mut sfx_cfg);
        let music = self.music(&mut music_cfg, &tracks, cues, total);
        let ambience = self.ambience(cfg.take("ambience"), &shots, &by_id);
        let grain = self.grain(cfg.take("grain"), &mut foreign);

        let name = cfg
            .get("project")
            .and_then(|p| p.get("title"))
            .or_else(|| cfg.get("youtube").and_then(|y| y.get("title")))
            .map(text)
            .or_else(|| {
                self.base
                    .parent()
                    .and_then(Path::file_name)
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let mut t = Timeline::new(name);
        t.global_start_time = Some(self.frames(0));
        // Text sizes and positions are relative to the frame render.py drew them on.
        let frame = self
            .opts
            .frame_size
            .or(project.output.preview.size.map(|[w, h]| (w, h)));
        if let Some((w, h)) = frame {
            t.ffcine_mut().insert("frame_size".into(), json!([w, h]));
        }
        let mut stack = vec![Item::Track(video)];
        for (prefix, kind, placed) in [
            ("PiP", track_kind::VIDEO, pips),
            ("Overlays", track_kind::VIDEO, overlays),
            ("Captions", track_kind::VIDEO, captions),
            ("Dialogue", track_kind::AUDIO, dialogue),
            ("Ambience", track_kind::AUDIO, ambience),
            ("Music", track_kind::AUDIO, music),
            ("SFX", track_kind::AUDIO, sfx),
        ] {
            stack.extend(
                self.lay_out(prefix, kind, placed)
                    .into_iter()
                    .map(Item::Track),
            );
        }
        t.tracks.children = stack;
        t.tracks.effects.extend(grain);
        t.set_project(&project).map_err(|e| e.to_string())?;

        for (k, rest) in [("music", music_cfg), ("sfx", sfx_cfg)] {
            if !rest.0.is_empty() {
                self.keep_foreign(&format!("{k}."), &rest.0);
                foreign.insert(k.into(), Value::Object(rest.0));
            }
        }
        self.keep_foreign("", &cfg.0);
        foreign.extend(cfg.0);
        if !foreign.is_empty() {
            t.set_foreign(APP, Value::Object(foreign));
        }
        Ok(t)
    }

    /// `grade`, `looks`, `luts`, `iphone`, styles -> `metadata.ffcine.project`.
    fn project(&mut self, cfg: &mut Obj) -> Res<Project> {
        let mut p = Project {
            rate: Some(self.rate),
            ..Project::default()
        };
        let mut grade = cfg.obj("grade");
        p.default_look = grade.str("look");
        let light = grade.f64("light");
        p.grade = grade_params(grade.0).map_err(|e| format!("grade: {e}"))?;
        for (name, v) in cfg.obj("looks").0 {
            let g = match v {
                Value::Object(m) => grade_params(m).map_err(|e| format!("looks.{name}: {e}"))?,
                _ => GradeParams::default(),
            };
            p.looks.insert(name, g);
        }
        for (alias, v) in cfg.obj("luts").0 {
            let path = join(&self.base, &text(&v));
            p.luts.insert(alias, path.to_string_lossy().into_owned());
        }
        let mut iphone = cfg.obj("iphone");
        let tame = iphone.take("tame").map(|mut t| {
            if let Some(Value::String(l)) = t.get_mut("lut") {
                *l = join(&self.base, l).to_string_lossy().into_owned();
            }
            t
        });
        p.mix = mix_params(cfg).map_err(|e| format!("mix settings: {e}"))?;
        p.rules = Rules {
            light,
            iphone_tame: tame,
            other: Map::new(),
        };
        if !iphone.0.is_empty() {
            self.keep_foreign("iphone.", &iphone.0);
            p.rules
                .other
                .insert("foreign_iphone".into(), Value::Object(iphone.0));
        }
        for (key, style) in STYLE_KEYS {
            if let Some(mut v) = cfg.take(key) {
                if *key == "style"
                    && let Some(Value::String(d)) = v.get_mut("fonts_dir")
                {
                    *d = join(&self.base, d).to_string_lossy().into_owned();
                }
                p.styles.insert((*style).to_owned(), v);
            }
        }
        self.styles.clone_from(&p.styles);
        p.output = Self::output(cfg.get("output"));
        Ok(p)
    }

    /// `output.{preview,final}.{width,height,max_punch}` -> `project.output` (the aspect from the
    /// preview size). The rest of `output` (codecs, paths) stays foreign.
    fn output(o: Option<&Value>) -> Output {
        let mode = |k: &str| {
            let m = o.and_then(|o| o.get(k));
            let get = |f: &str| m.and_then(|m| m.get(f)).and_then(num);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let size = match (get("width"), get("height")) {
                (Some(w), Some(h)) if w >= 1.0 && h >= 1.0 => Some([w as u32, h as u32]),
                _ => None,
            };
            OutputMode {
                size,
                max_punch: get("max_punch"),
            }
        };
        let preview = mode("preview");
        let aspect = preview.size.or(mode("final").size).map(|[w, h]| {
            let ar = f64::from(w) / f64::from(h);
            [
                ("landscape", 16.0 / 9.0),
                ("reel", 9.0 / 16.0),
                ("feed", 4.0 / 5.0),
                ("square", 1.0),
            ]
            .iter()
            .find(|(_, a)| (ar - a).abs() < 0.01)
            .map_or_else(|| format!("{w}x{h}"), |(n, _)| (*n).to_owned())
        });
        Output {
            aspect,
            preview,
            master: mode("final"),
            other: Map::new(),
        }
    }

    /// `dji:0024`, `<prefix>:<file>` or a path (`render.py` `resolve_src`).
    fn resolve(&mut self, src: &str, id: &str) -> Source {
        let dir = |c: &Self, key: &str| {
            c.sources
                .get(key)
                .and_then(Value::as_str)
                .map(|d| join(&c.base, d))
        };
        let photo = |p: &Path| {
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            matches!(
                ext.as_str(),
                "heic" | "jpg" | "jpeg" | "png" | "tif" | "tiff" | "dng"
            )
        };
        if let Some(num) = src.strip_prefix("dji:") {
            let Some(d) = dir(self, "dji_dir") else {
                self.report
                    .warn(format!("shot {id}: {src} but no sources.dji_dir"));
                return Source {
                    src: src.into(),
                    path: None,
                    kind: Kind::Dji,
                };
            };
            let suffix = format!("_{num}_D.MP4");
            let mut found: Vec<PathBuf> = std::fs::read_dir(&d)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name().is_some_and(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with("DJI_") && n.ends_with(&suffix)
                    })
                })
                .collect();
            found.sort();
            if found.len() != 1 {
                self.report.warn(format!(
                    "shot {id}: {src} matches {} files in {}; media left missing",
                    found.len(),
                    d.display()
                ));
                return Source {
                    src: src.into(),
                    path: None,
                    kind: Kind::Dji,
                };
            }
            return Source {
                src: src.into(),
                path: found.pop(),
                kind: Kind::Dji,
            };
        }
        let path = match src.split_once(':') {
            Some((pre, rest)) if pre.len() > 1 => match dir(self, &format!("{pre}_dir")) {
                Some(d) => join(&d, rest),
                None => join(&self.base, src),
            },
            _ => join(&self.base, src),
        };
        if !path.exists() {
            self.report
                .warn(format!("shot {id}: missing source {}", path.display()));
        }
        let kind = if photo(&path) {
            Kind::Photo
        } else {
            Kind::Video
        };
        Source {
            src: src.into(),
            path: Some(path),
            kind,
        }
    }

    fn duration_of(&mut self, path: &Path) -> Option<f64> {
        if let Some(f) = self.opts.media_duration
            && let Some(d) = f(path)
        {
            return Some(d);
        }
        wav_duration(path)
    }

    /// `live: true` on a HEIC: the Live Photo MOV for the motion, the HEIC as the end still.
    fn live(o: &mut Obj) -> Option<String> {
        let live = o.truthy("live");
        let src = o.get("src").and_then(Value::as_str)?.to_owned();
        if !(live && src.to_ascii_lowercase().ends_with(".heic")) {
            return None;
        }
        let stem = src.rsplit_once('.').map_or(src.as_str(), |(s, _)| s);
        o.0.insert("src".into(), format!("{stem}_live.MOV").into());
        let still_dur = o.get("still_dur").and_then(num).unwrap_or(0.0);
        (still_dur > 0.0 && o.get("still").is_none()).then_some(src)
    }

    fn panel(&mut self, mut o: Obj, id: &str) -> Panel {
        let heic = Self::live(&mut o);
        let still_src = o.str("still").or(heic);
        let src = o.str("src").unwrap_or_default();
        let source = self.resolve(&src, id);
        let still = still_src.map(|s| self.resolve(&s, id));
        let mut reframe = o.take_keys(&["focus", "push", "push_center", "zoom", "zoom_center"]);
        reframe.retain(|_, v| !v.is_null());
        let p = Panel {
            source,
            in_s: o.f64("in").unwrap_or(0.0),
            out_s: o.f64("out"),
            still,
            still_dur: o.f64("still_dur").unwrap_or(0.0),
            speed: o.f64("speed").unwrap_or(1.0),
            look: o.str("look"),
            reframe,
            rest: o.0,
        };
        self.keep_foreign("shots[].panels[].", &p.rest);
        p
    }

    #[allow(clippy::too_many_lines)] // one line per field
    fn shot(&mut self, mut o: Obj, pos: usize, end_screen: Option<&Value>) -> Res<Shot> {
        let id = o.str("id").unwrap_or_else(|| pos.to_string());
        let mut layout = None;
        let mut still = None;
        let (source, in_s, mut out_s);
        if o.get("layout").is_some() {
            let params = o.take_keys(LAYOUT_KEYS);
            let panels: Vec<Panel> = match o.take("panels") {
                Some(Value::Array(a)) => a
                    .into_iter()
                    .map(|v| self.panel(Obj::from_value(Some(v)), &id))
                    .collect(),
                _ => return Err(format!("shot {id}: layout without panels")),
            };
            let ap = params
                .get("audio_panel")
                .and_then(Value::as_u64)
                .and_then(|x| usize::try_from(x).ok())
                .unwrap_or(0);
            let ap = panels
                .get(ap)
                .or(panels.first())
                .ok_or(format!("shot {id}: no panels"))?;
            source = ap.source.clone();
            in_s = ap.in_s;
            let dur = o
                .f64("dur")
                .ok_or(format!("shot {id}: layout without dur"))?;
            out_s = in_s + dur;
            o.take("src");
            o.take("in");
            o.take("out");
            layout = Some((params, panels));
        } else {
            let heic = Self::live(&mut o);
            let still_src = o.str("still").or(heic);
            let src = o.str("src").ok_or(format!("shot {id}: no src"))?;
            source = self.resolve(&src, &id);
            still = still_src.map(|s| self.resolve(&s, &id));
            in_s = o.f64("in").unwrap_or(0.0);
            out_s = match o.f64("out") {
                Some(x) => x,
                None if source.kind == Kind::Photo => 3.0,
                None => {
                    let d = source.path.clone().and_then(|p| self.duration_of(&p));
                    if let Some(d) = d {
                        d - 0.04
                    } else {
                        self.report.warn(format!(
                            "shot {id}: no `out` and no media duration; using in + 3 s"
                        ));
                        in_s + 3.0
                    }
                }
            };
        }
        if let Some(es) = end_screen
            && es.get("shot").map(text).as_deref() == Some(id.as_str())
            && let Some(secs) = es.get("seconds").and_then(num)
        {
            out_s = in_s + secs.clamp(5.0, 20.0);
        }
        let ramp = o.take("ramp");
        let speed = o.f64("speed").filter(|s| *s != 0.0).unwrap_or(1.0);
        let pts = speed_points(ramp.as_ref(), speed);
        let hold_s = o.f64("freeze").unwrap_or(0.0) + o.f64("still_dur").unwrap_or(0.0);

        let look = o.str("look");
        let grade_over = o.take("grade");
        let grade = if look.is_some() || grade_over.is_some() {
            let params = match grade_over {
                Some(Value::Object(m)) => {
                    grade_params(m).map_err(|e| format!("shot {id} grade: {e}"))?
                }
                _ => GradeParams::default(),
            };
            Some(ClipGrade { look, params })
        } else {
            None
        };
        let subs = match o.take("subs") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|c| {
                    let c = c.as_array()?;
                    Some((num(c.first()?)?, num(c.get(1)?)?, text(c.get(2)?)))
                })
                .collect(),
            _ => Vec::new(),
        };
        let sfx = match o.take("sfx") {
            Some(Value::Array(a)) => a.into_iter().map(|v| Obj::from_value(Some(v))).collect(),
            _ => Vec::new(),
        };
        let mut reframe = o.take_keys(REFRAME_KEYS);
        reframe.retain(|_, v| !v.is_null());
        let level = o.take_keys(&["gain_db", "duck_db", "nat_db"]);
        let dissolve = o.f64("dissolve");
        let (transition, dissolve_s) = match o.take("transition") {
            Some(t) => self.transition(&t, dissolve, &id),
            None => (None, dissolve.unwrap_or(0.0)),
        };
        let overlays = match o.take("overlays") {
            Some(Value::Array(a)) => a.into_iter().map(|v| Obj::from_value(Some(v))).collect(),
            _ => Vec::new(),
        };
        // render.py tests these for truthiness: 0 is off.
        let nonzero = |x: Option<f64>| x.filter(|v| *v != 0.0);
        let shot = Shot {
            id: id.clone(),
            source,
            in_s,
            out_s,
            audio: o.str("audio").unwrap_or_else(|| "music".into()),
            pts,
            hold_s,
            dissolve_s,
            still,
            label: o.str("label"),
            grade,
            blend: o.f64("blend"),
            stabilize: o
                .take("stabilize")
                .or_else(|| o.take("stabilise"))
                .filter(truthy),
            denoise: o.take("denoise").filter(truthy),
            reframe,
            layout,
            chapter: o.str("chapter"),
            card: o.str("card"),
            location: o.str("location"),
            location_at: o.f64("location_at"),
            subs,
            sfx,
            level,
            clean: o.f64("clean"),
            audio_lead: o.f64("audio_lead").unwrap_or(0.0),
            audio_tail: o.f64("audio_tail").unwrap_or(0.0),
            caption_y: o.f64("caption_y"),
            ai: o.truthy("ai"),
            no_auto_sfx: o.truthy("no_auto_sfx"),
            snap: o.take("snap").is_none_or(|v| truthy(&v)),
            vignette: nonzero(o.f64("vignette")),
            darken: nonzero(o.f64("darken")),
            fade_in: o.f64("fade_in").unwrap_or(0.0),
            fade_out: o.f64("fade_out").unwrap_or(0.0),
            flash: o.truthy("flash"),
            transition,
            overlays,
            rest: {
                o.take("dur");
                o.0
            },
            dissolve: 0,
            start: 0,
            base_n: 0,
            n: 0,
            play_n: 0,
            snapped: 0,
        };
        self.keep_foreign("shots[].", &shot.rest);
        Ok(shot)
    }

    /// `transition: kind` or `{kind, colour, direction, dur}`: the `ffcine.transition/1`
    /// metadata (none for a plain dissolve) and the duration (`dur`, else `dissolve`, else the
    /// kind's default).
    fn transition(
        &mut self,
        v: &Value,
        dissolve: Option<f64>,
        id: &str,
    ) -> (Option<Map<String, Value>>, f64) {
        let (kind, mut o) = match v {
            Value::Object(m) => {
                let mut o = Obj(m.clone());
                (o.str("kind").unwrap_or_else(|| "dissolve".into()), o)
            }
            other => (text(other), Obj::default()),
        };
        let dflt = match kind.as_str() {
            "flash" => 0.2,
            "whip" => 0.25,
            _ => 0.5,
        };
        let dur = o.f64("dur").or(dissolve).unwrap_or(dflt);
        let mut m = obj(json!({"schema": "ffcine.transition/1", "kind": kind}));
        match kind.as_str() {
            "dissolve" => {}
            "dip" | "flash" => {
                let white = kind == "flash";
                let c = match o.take("colour").or_else(|| o.take("color")) {
                    None => None,
                    Some(c) => {
                        let rgb = colour(&c);
                        if rgb.is_none() {
                            self.report
                                .warn(format!("shot {id}: transition colour {c} not understood"));
                        }
                        rgb
                    }
                };
                let c = c.unwrap_or(if white { [1.0; 3] } else { [0.0; 3] });
                m.insert("colour".into(), json!(c));
            }
            "whip" => {
                let d = o.str("direction").unwrap_or_else(|| "left".into());
                let d = if matches!(d.as_str(), "left" | "right" | "up" | "down") {
                    d
                } else {
                    self.report.warn(format!(
                        "shot {id}: whip direction {d:?} is not left/right/up/down; left"
                    ));
                    "left".into()
                };
                m.insert("direction".into(), d.into());
            }
            other => self.report.warn(format!(
                "shot {id}: transition {other:?} rendered as a dissolve (kind kept)"
            )),
        }
        if !o.0.is_empty() {
            self.keep_foreign("shots[].transition.", &o.0);
            m.insert("foreign".into(), json!({ APP: Value::Object(o.0) }));
        }
        (
            (kind != "dissolve" || m.contains_key("foreign")).then_some(m),
            dur,
        )
    }

    /// `music.cues`, in `render.py`'s order (by shot, then as listed).
    fn cues(&mut self, cfg: &mut Obj, by_id: &BTreeMap<String, usize>) -> Vec<Cue> {
        let Some(Value::Array(list)) = cfg.take("cues") else {
            return Vec::new();
        };
        let mut cues: Vec<Cue> = list
            .into_iter()
            .filter_map(|c| {
                let mut o = Obj::from_value(Some(c));
                let sid = o.str("shot")?;
                let Some(&shot) = by_id.get(&sid) else {
                    self.report.warn(format!("music cue on unknown shot {sid}"));
                    return None;
                };
                let in_v = match o.take("in") {
                    Some(Value::String(s)) if s == "end" => None,
                    Some(v) => Some(num(&v).unwrap_or(0.0)),
                    None => Some(0.0),
                };
                Some(Cue {
                    shot,
                    offset: o.f64("offset").unwrap_or(0.0),
                    track: o.str("track").unwrap_or_default(),
                    in_v,
                    snap_in: o.truthy("snap_in"),
                    start: None,
                    in_s: None,
                    o,
                })
            })
            .collect();
        cues.sort_by_key(|c| c.shot);
        cues
    }

    /// Beat times of the cued music tracks, through `ImportOptions::beats`.
    fn load_beats(&mut self, tracks: &Map<String, Value>, cues: &[Cue]) {
        let Some(f) = self.opts.beats else {
            return;
        };
        for c in cues {
            if self.beats.contains_key(&c.track) {
                continue;
            }
            let Some(file) = tracks.get(&c.track).and_then(|t| t.get("file")) else {
                continue;
            };
            if let Some(b) = f(&join(&self.base, &text(file))) {
                self.beats.insert(c.track.clone(), b);
            }
        }
    }

    /// Beats of the cue playing at frame `f`, as timeline frames (`render.py` `beats_near`).
    #[allow(clippy::cast_precision_loss)]
    fn beats_near(&self, cues: &[Cue], f: i64) -> Vec<f64> {
        let Some(c) = cues.iter().rfind(|c| c.start.is_some_and(|s| s <= f - 2)) else {
            return Vec::new();
        };
        let (Some(start), Some(in_s), Some(beats)) = (c.start, c.in_s, self.beats.get(&c.track))
        else {
            return Vec::new();
        };
        beats
            .iter()
            .filter(|b| **b >= in_s)
            .map(|b| start as f64 + (b - in_s) * self.rate)
            .collect()
    }

    /// `render.py` `snap_time`: `f` onto the nearest beat within 0.25 s.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn snap_time(&self, cues: &[Cue], f: i64) -> i64 {
        match nearest(&self.beats_near(cues, f), f as f64) {
            Some(x) if (x - f as f64).abs() <= 0.25 * self.rate => x.round() as i64,
            _ => f,
        }
    }

    /// `render.py` `Timeline.__init__`: shot starts and lengths with dissolves, ramps, holds
    /// and cuts snapped to the music's beats (`snap_max` s, `None`: off); cue starts and `in`
    /// points. Returns the total in frames.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn time_shots(
        &mut self,
        shots: &mut [Shot],
        cues: &mut [Cue],
        tracks: &Map<String, Value>,
        snap_max: Option<f64>,
    ) -> i64 {
        let mut t = 0;
        let count = shots.len();
        for (i, s) in shots.iter_mut().enumerate() {
            let d = if i > 0 { self.s2f(s.dissolve_s) } else { 0 };
            s.dissolve = d;
            s.start = t - d;
            for c in cues.iter_mut().filter(|c| c.shot == i) {
                c.start = Some(s.start + self.s2f(c.offset));
                if let Some(mut cin) = c.in_v {
                    if c.snap_in {
                        match self.beats.get(&c.track).and_then(|b| nearest(b, cin)) {
                            Some(b) => cin = (b - 0.02).max(0.0),
                            None => self.report.warn(
                                "music cues with snap_in start at their unsnapped `in` (no beats)",
                            ),
                        }
                    }
                    c.in_s = Some(cin);
                }
            }
            let base_n = self.s2f(ramp_out_len(&s.pts, s.out_s - s.in_s));
            let hold_n = self.s2f(s.hold_s);
            let mut n = base_n + hold_n;
            s.play_n = base_n;
            s.snapped = 0;
            if let Some(snap_max) = snap_max
                && matches!(s.audio.as_str(), "music" | "nat")
                && s.snap
                && i + 1 < count
            {
                let end = s.start + n;
                if let Some(b) = nearest(&self.beats_near(cues, end), end as f64) {
                    let nn = (b - s.start as f64).round() as i64;
                    let last_speed = s.pts.last().map_or(1.0, |p| p.1);
                    let (min_n, max_n) =
                        if hold_n > 0 || s.source.kind == Kind::Photo || s.layout.is_some() {
                            (if hold_n > 0 { base_n + 6 } else { 24 }, i64::MAX)
                        } else {
                            let dur = s.source.path.clone().and_then(|p| self.duration_of(&p));
                            let max_n = dur.map_or(base_n, |d| {
                                base_n
                                    + ((d - 0.05 - s.out_s) / last_speed * self.rate).floor() as i64
                            });
                            if dur.is_none() && nn > max_n {
                                self.report.warn(format!(
                                "shot {}: cut not moved later onto the beat (no media duration)",
                                s.id
                            ));
                            }
                            (24, max_n)
                        };
                    if (b - end as f64).abs() <= snap_max * self.rate
                        && (min_n..=max_n).contains(&nn)
                    {
                        s.snapped = nn - n;
                        n = nn;
                    }
                }
            }
            // Snapping moves the hold's end, or the end of a shot without one.
            s.base_n = if hold_n > 0 { base_n.min(n) } else { n };
            s.n = n;
            t = s.start + n;
        }
        for c in cues.iter_mut().filter(|c| c.in_v.is_none()) {
            let file = tracks.get(&c.track).and_then(|t| t.get("file")).map(text);
            let d = file.and_then(|f| self.duration_of(&join(&self.base, &f)));
            c.in_s = Some(if let Some(d) = d {
                (d - self.f2s(t - c.start.unwrap_or(0)) - 0.25).max(0.0)
            } else {
                self.report.warn(format!(
                    "music cue {} `in: end` needs the track's duration (no media_duration); \
                     starting at 0",
                    c.track
                ));
                0.0
            });
        }
        t
    }

    fn grade_effect(g: &ClipGrade) -> Effect {
        let params = match serde_json::to_value(g) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
        Effect::Effect(GenericEffect::ffcine(
            ffcine_timeline::project::GRADE_SCHEMA,
            params,
        ))
    }

    fn media(s: &Source) -> MediaReference {
        match &s.path {
            Some(p) => MediaReference::External(ExternalReference::from_path(p, None)),
            None => MediaReference::Missing(MissingReference {
                name: s.src.clone(),
                ..MissingReference::default()
            }),
        }
    }

    /// A clip of `source` from `in_s` for `n` timeline frames at `speed`.
    fn clip(&self, name: &str, source: &Source, in_s: f64, n: i64, speed: f64) -> Clip {
        let range = TimeRange::new(self.secs(in_s), self.frames(n));
        let mut c = Clip::new(name, Self::media(source), Some(range));
        c.ffcine_mut().insert(
            "source".into(),
            json!({"src": source.src, "kind": source.kind.name()}),
        );
        if (speed - 1.0).abs() > 1e-9 {
            c.effects
                .push(Effect::LinearTimeWarp(LinearTimeWarp::new(speed)));
        }
        c
    }

    fn video_track(&mut self, shots: &[Shot]) -> Track {
        let mut track = Track::new("V1", track_kind::VIDEO);
        for s in shots {
            let src_len = s.out_s - s.in_s;
            let mut items = Vec::new();
            let avg = if s.play_n > 0 {
                src_len / self.f2s(s.play_n)
            } else {
                1.0
            };
            if let Some((params, panels)) = &s.layout {
                items.push(self.layout_stack(s, params, panels));
            } else {
                let speed = if is_const(&s.pts) { s.speed() } else { avg };
                let mut c = self.clip(&s.id, &s.source, s.in_s, s.base_n, speed);
                if !is_const(&s.pts) {
                    let pts: Vec<Value> = s.pts.iter().map(|(u, v)| json!([u, v])).collect();
                    let mut p = Map::new();
                    p.insert("points".into(), Value::Array(pts));
                    p.insert("source_duration".into(), json!(src_len));
                    c.effects.insert(
                        0,
                        Effect::Effect(GenericEffect::ffcine("ffcine.time.ramp/1", p)),
                    );
                }
                Self::shot_effects(s, &mut c);
                c.effects
                    .extend(Self::finish_effects(s, true, s.n > s.base_n));
                items.push(Item::Clip(c));
            }
            let hold_n = s.n - s.base_n;
            if hold_n > 0 {
                items.push(Item::Clip(self.hold_clip(s, hold_n)));
            }
            if s.dissolve > 0 {
                let mut tr = Transition::dissolve(self.frames(0), self.frames(s.dissolve));
                if let Some(m) = &s.transition {
                    tr.ffcine_mut().extend(m.clone());
                }
                let trimmed = track
                    .children
                    .iter_mut()
                    .rev()
                    .find(|i| !matches!(i, Item::Transition(_)))
                    .is_some_and(|prev| trim_end(prev, s.dissolve));
                if trimmed {
                    track.children.push(Item::Transition(tr));
                } else {
                    self.report.warn(format!(
                        "shot {}: dissolve longer than the previous shot; dropped",
                        s.id
                    ));
                }
            }
            if let Some(title) = s.chapter.clone().or_else(|| {
                s.card.as_ref().map(|c| {
                    let mut cs = c.chars();
                    cs.next()
                        .map(|f| f.to_uppercase().chain(cs).collect())
                        .unwrap_or_default()
                })
            }) {
                let at = if track.markers.is_empty() {
                    0
                } else {
                    s.start + s.dissolve / 2
                };
                let mut m = Marker::new(
                    title,
                    TimeRange::new(self.frames(at), self.frames(0)),
                    "GREEN",
                );
                m.ffcine_mut().insert("chapter".into(), true.into());
                track.markers.push(m);
            }
            track.children.extend(items);
        }
        track
    }

    /// Grade, stabilize, reframe, blend and the shot's bookkeeping, on its main clip.
    fn shot_effects(s: &Shot, c: &mut Clip) {
        if let Some(b) = s.blend
            && b > 1.0
        {
            c.effects.push(Effect::Effect(GenericEffect::ffcine(
                "ffcine.time.blend/1",
                obj(json!({"frames": b})),
            )));
        }
        if let Some(st) = &s.stabilize {
            // render.py stab_params
            let mut p = obj(json!({"smoothing": 20, "shakiness": 6, "accuracy": 15,
                "zoomspeed": 0.25}));
            match st {
                Value::Object(m) => p.extend(m.clone()),
                Value::Bool(_) => {}
                other => {
                    p.insert(
                        "smoothing".into(),
                        num(other).map_or(json!(20), |x| json!(x.trunc())),
                    );
                }
            }
            c.effects.push(Effect::Effect(GenericEffect::ffcine(
                "ffcine.fx.stabilize/1",
                p,
            )));
        }
        if let Some(d) = &s.denoise {
            // A number is the temporal strength (NVOF, radius 2).
            let p = match d {
                Value::Object(m) => m.clone(),
                other => obj(json!({"temporal": {"strength": num(other).unwrap_or(4.0)}})),
            };
            c.effects.push(Effect::Effect(GenericEffect::ffcine(
                "ffcine.fx.denoise/1",
                p,
            )));
        }
        if !s.reframe.is_empty() {
            c.effects.push(Effect::Effect(GenericEffect::ffcine(
                "ffcine.geo.reframe/1",
                s.reframe.clone(),
            )));
        }
        if let Some(g) = &s.grade {
            c.effects.push(Self::grade_effect(g));
        }
        Self::shot_meta(s, c);
    }

    /// `darken`, `vignette` and the fades, which `render.py` runs over the whole shot (moving
    /// part and hold): fade in on the main clip, fade out on the last of the two.
    fn finish_effects(s: &Shot, main: bool, has_hold: bool) -> Vec<Effect> {
        let mut out = Vec::new();
        if let Some(k) = s.darken {
            out.push(Effect::Effect(GenericEffect::ffcine(
                "ffcine.fx.darken/1",
                obj(json!({"amount": k})),
            )));
        }
        if let Some(a) = s.vignette {
            out.push(Effect::Effect(GenericEffect::ffcine(
                "ffcine.fx.vignette/1",
                obj(json!({"amount": a})),
            )));
        }
        let fin = if main { s.fade_in } else { 0.0 };
        let fout = if main && has_hold { 0.0 } else { s.fade_out };
        if fin > 0.0 || fout > 0.0 {
            let mut p = Map::new();
            if fin > 0.0 {
                p.insert("in".into(), fin.into());
            }
            if fout > 0.0 {
                p.insert("out".into(), fout.into());
            }
            p.insert("colour".into(), json!([0.0, 0.0, 0.0]));
            out.push(Effect::Effect(GenericEffect::ffcine("ffcine.fx.fade/1", p)));
        }
        out
    }

    fn shot_meta(s: &Shot, c: &mut impl HasMetadata) {
        c.set_ffcine_id(s.id.clone());
        let ns = c.ffcine_mut();
        if s.snapped != 0 {
            ns.insert("snap".into(), json!({"moved": s.snapped}));
        }
        if let Some(l) = &s.label {
            ns.insert("label".into(), l.clone().into());
        }
        if s.ai {
            ns.insert("ai".into(), true.into());
        }
        if !s.rest.is_empty() {
            c.set_foreign(APP, Value::Object(s.rest.clone()));
        }
    }

    /// The freeze (or Live Photo still) after a shot.
    fn hold_clip(&self, s: &Shot, hold_n: i64) -> Clip {
        let (src, speed) = match &s.layout {
            Some((_, panels)) => (&panels[0].source, 1.0),
            None => (&s.source, s.pts.last().map_or(1.0, |p| p.1)),
        };
        let mut c = if let Some(still) = &s.still {
            self.clip(&format!("{} still", s.id), still, 0.0, hold_n, 1.0)
        } else {
            let last = (s.out_s - self.f2s(1) * speed).max(s.in_s);
            let mut c = Clip::new(
                format!("{} freeze", s.id),
                Self::media(src),
                Some(TimeRange::new(self.secs(last), self.frames(hold_n))),
            );
            c.effects
                .push(Effect::FreezeFrame(LinearTimeWarp::freeze_frame()));
            c
        };
        if let Some(g) = &s.grade {
            c.effects.push(Self::grade_effect(g));
        }
        // render.py's shutter flash: colorlevels romin per frame from the hold's start.
        let levels = if s.still.is_some() {
            Some([0.55, 0.55, 0.22])
        } else {
            s.flash.then_some([0.5, 0.5, 0.2])
        };
        if let Some(l) = levels {
            c.effects.push(Effect::Effect(GenericEffect::ffcine(
                "ffcine.fx.flash/1",
                obj(json!({"at": 0, "levels": l})),
            )));
        }
        c.effects.extend(Self::finish_effects(s, false, true));
        c.set_ffcine_id(format!("{}/hold", s.id));
        c.ffcine_mut().insert("hold_of".into(), s.id.clone().into());
        c
    }

    fn layout_stack(&mut self, s: &Shot, params: &Map<String, Value>, panels: &[Panel]) -> Item {
        let mut stack = Stack {
            name: s.id.clone(),
            source_range: Some(TimeRange::new(self.frames(0), self.frames(s.base_n))),
            ..Stack::default()
        };
        for (k, p) in panels.iter().enumerate() {
            let mut track = Track::new(format!("panel {}", k + 1), track_kind::VIDEO);
            let still_n = self.s2f(p.still_dur).min(s.base_n);
            let mut play_n = s.base_n - still_n;
            if let Some(out) = p.out_s {
                play_n = play_n.min(self.s2f((out - p.in_s) / p.speed));
            }
            let mut c = self.clip(
                &format!("{} panel {}", s.id, k + 1),
                &p.source,
                p.in_s,
                play_n,
                p.speed,
            );
            c.set_ffcine_id(format!("{}/p{}", s.id, k + 1));
            if !p.reframe.is_empty() {
                c.effects.push(Effect::Effect(GenericEffect::ffcine(
                    "ffcine.geo.reframe/1",
                    p.reframe.clone(),
                )));
            }
            let grade = p.look.clone().map(|look| ClipGrade {
                look: Some(look),
                params: GradeParams::default(),
            });
            if let Some(g) = grade.as_ref().or(s.grade.as_ref()) {
                c.effects.push(Self::grade_effect(g));
            }
            if !p.rest.is_empty() {
                c.set_foreign(APP, Value::Object(p.rest.clone()));
            }
            track.children.push(Item::Clip(c));
            let fill = s.base_n - play_n - still_n;
            if fill > 0 {
                // render.py: panels hold their last frame.
                let last =
                    p.out_s.unwrap_or(p.in_s + self.f2s(play_n) * p.speed) - self.f2s(1) * p.speed;
                let mut f = Clip::new(
                    format!("{} panel {} hold", s.id, k + 1),
                    Self::media(&p.source),
                    Some(TimeRange::new(
                        self.secs(last.max(p.in_s)),
                        self.frames(fill),
                    )),
                );
                f.effects
                    .push(Effect::FreezeFrame(LinearTimeWarp::freeze_frame()));
                track.children.push(Item::Clip(f));
            }
            if still_n > 0 {
                if let Some(still) = &p.still {
                    let mut c = self.clip(
                        &format!("{} panel {} still", s.id, k + 1),
                        still,
                        0.0,
                        still_n,
                        1.0,
                    );
                    c.set_ffcine_id(format!("{}/p{}/still", s.id, k + 1));
                    track.children.push(Item::Clip(c));
                } else {
                    track
                        .children
                        .push(Item::Gap(Gap::new(self.frames(still_n))));
                }
            }
            stack.children.push(Item::Track(track));
        }
        stack.effects.push(Effect::Effect(GenericEffect::ffcine(
            "ffcine.comp.layout/1",
            params.clone(),
        )));
        if let Some(g) = &s.grade {
            stack.effects.push(Self::grade_effect(g));
        }
        stack
            .effects
            .extend(Self::finish_effects(s, true, s.n > s.base_n));
        let mut item = Item::Stack(stack);
        if let Item::Stack(st) = &mut item {
            Self::shot_meta(s, st);
        }
        item
    }

    /// Top-level `grain: {preview, final}` (ffmpeg `noise` uniform amplitude, chroma 0.4x) ->
    /// `ffcine.fx.grain/1` (sigma = amplitude / sqrt 3) with per-profile strengths.
    fn grain(&mut self, v: Option<Value>, foreign: &mut Map<String, Value>) -> Option<Effect> {
        let mut g = Obj::from_value(v);
        let r3 = |x: f64| (x / 3f64.sqrt() * 1000.0).round() / 1000.0;
        let (pv, fv) = (g.f64("preview"), g.f64("final"));
        if !g.0.is_empty() {
            self.keep_foreign("grain.", &g.0);
            foreign.insert("grain".into(), Value::Object(g.0));
        }
        let base = pv.or(fv)?;
        if pv.unwrap_or(0.0) <= 0.0 && fv.unwrap_or(0.0) <= 0.0 {
            return None;
        }
        let mut profiles = Map::new();
        for (name, x) in [("preview", pv), ("master", fv)] {
            if let Some(x) = x {
                profiles.insert(name.into(), json!({"strength": r3(x.max(0.0))}));
            }
        }
        Some(Effect::Effect(GenericEffect::ffcine(
            "ffcine.fx.grain/1",
            obj(json!({"strength": r3(base.max(0.0)), "chroma": 0.4, "profiles": profiles})),
        )))
    }

    /// Per-shot `overlays` (stickers, logos, picture-in-picture) -> clips for the `PiP` tracks.
    fn pips(&mut self, shots: &[Shot]) -> Vec<Placed> {
        let mut out = Vec::new();
        for s in shots {
            for (k, o) in s.overlays.iter().enumerate() {
                if let Some(p) = self.pip(s, k + 1, o.clone()) {
                    out.push(p);
                }
            }
        }
        out
    }

    #[allow(clippy::many_single_char_names)]
    fn pip(&mut self, s: &Shot, k: usize, mut o: Obj) -> Option<Placed> {
        let id = format!("{}/ov{k}", s.id);
        let (src, image) = match (o.str("image"), o.str("src")) {
            (Some(i), None) => (i, true),
            (None, Some(v)) => (v, false),
            _ => {
                self.report.warn(format!(
                    "{id}: an overlay needs one of `image` or `src`; dropped"
                ));
                return None;
            }
        };
        let source = self.resolve(&src, &s.id);
        let at = self.s2f(o.f64("at").unwrap_or(0.0));
        let n = o.f64("dur").map_or(s.n - at, |d| self.s2f(d)).max(1);
        let in_s = if image {
            0.0
        } else {
            o.f64("in").unwrap_or(0.0)
        };
        let mut c = Clip::new(
            format!("{} overlay {k}", s.id),
            Self::media(&source),
            Some(TimeRange::new(self.secs(in_s), self.frames(n))),
        );
        c.ffcine_mut().insert(
            "source".into(),
            json!({"src": src, "kind": if image { "image" } else { "video" }}),
        );
        if let Some(look) = o.str("look") {
            c.effects.push(Self::grade_effect(&ClipGrade {
                look: Some(look),
                params: GradeParams::default(),
            }));
        }
        let dur = self.f2s(n);
        let key = |c: &Self, t: f64, v: Value, interp: &str| json!({"t": c.secs(in_s + t), "v": v, "interp": interp});
        let anim = |c: &mut Self, name: &str, v: Option<Value>, dflt: Value| -> Anim {
            match v {
                None => Anim::Still(dflt),
                Some(v) => Anim::parse(&v).unwrap_or_else(|| {
                    c.report
                        .warn(format!("{id}: overlay {name} {v} not understood; default"));
                    Anim::Still(dflt)
                }),
            }
        };
        let position = o.take("position");
        let position = anim(self, "position", position, json!([0.0, 0.0]));
        let scale = o.take("scale");
        let scale = anim(self, "scale", scale, json!(1.0));
        let opacity = o.take("opacity");
        let mut opacity = anim(self, "opacity", opacity, json!(1.0));
        let (fin, fout) = (
            o.f64("fade_in").unwrap_or(0.0),
            o.f64("fade_out").unwrap_or(0.0),
        );
        if fin > 0.0 || fout > 0.0 {
            opacity = opacity.faded(fin, fout, dur);
        }
        let comp = |a: &Anim, i: Option<usize>| -> Value {
            let pick = |v: &Value| match i {
                Some(i) => v.get(i).cloned().unwrap_or(json!(0.0)),
                None => v.clone(),
            };
            match a {
                Anim::Still(v) => pick(v),
                Anim::Keys(ks) => Value::Array(
                    ks.iter()
                        .map(|(t, v, interp)| key(self, *t, pick(v), interp))
                        .collect(),
                ),
            }
        };
        c.effects.push(Effect::Effect(GenericEffect::ffcine(
            "ffcine.geo.transform/1",
            obj(json!({
                "units": "frame", "y": "down",
                "translate_x": comp(&position, Some(0)), "translate_y": comp(&position, Some(1)),
                "scale_x": comp(&scale, None), "scale_y": comp(&scale, None),
            })),
        )));
        c.effects.push(Effect::Effect(GenericEffect::ffcine(
            "ffcine.comp.blend/1",
            obj(json!({
                "opacity": comp(&opacity, None), "mode": "normal",
                "radius": o.f64("radius").unwrap_or(0.0),
            })),
        )));
        c.set_ffcine_id(id);
        if !o.0.is_empty() {
            self.keep_foreign("shots[].overlays[].", &o.0);
            c.set_foreign(APP, Value::Object(o.0));
        }
        Some(Placed {
            start: s.start + at,
            clip: c,
        })
    }

    /// Camera audio of `orig` / `nat` shots (and `music` shots with `nat_db`), with J/L cuts.
    fn dialogue(&mut self, shots: &[Shot]) -> Vec<Placed> {
        let mut out = Vec::new();
        for s in shots {
            let keep = matches!(s.audio.as_str(), "orig" | "nat") || s.level.contains_key("nat_db");
            if !keep || s.source.kind == Kind::Photo {
                continue;
            }
            let lead = self.s2f(s.audio_lead);
            let tail = self.s2f(s.audio_tail);
            let speed = if is_const(&s.pts) {
                s.speed()
            } else {
                (s.out_s - s.in_s) / self.f2s(s.base_n.max(1))
            };
            let in_s = s.in_s - self.f2s(lead) * speed;
            let mut c = self.clip(&s.id, &s.source, in_s, s.base_n + lead + tail, speed);
            c.set_ffcine_id(format!("{}/audio", s.id));
            let mut level = obj(json!({"mode": s.audio}));
            level.extend(s.level.clone());
            c.effects.push(Effect::Effect(GenericEffect::ffcine(
                "ffcine.audio.level/1",
                level,
            )));
            if let Some(k) = s.clean {
                c.effects.push(Effect::Effect(GenericEffect::ffcine(
                    "ffcine.audio.clean/1",
                    obj(json!({"strength": k})),
                )));
            }
            out.push(Placed {
                start: s.start - lead,
                clip: c,
            });
        }
        out
    }

    /// A `GeneratorReference(ffcine.text)` clip.
    fn text_clip(&self, name: &str, params: Map<String, Value>, n: i64) -> Clip {
        let g = GeneratorReference::new("ffcine.text", params);
        Clip::new(
            name,
            MediaReference::Generator(g),
            Some(TimeRange::new(self.frames(0), self.frames(n))),
        )
    }

    /// `subs` in source time -> caption clips (`render.py` `build_captions`, before its
    /// min-duration and overlap fixes, which are the caption style's job).
    fn captions(&mut self, shots: &[Shot]) -> Vec<Placed> {
        let mut out = Vec::new();
        for s in shots {
            if s.subs.is_empty() {
                continue;
            }
            if !is_const(&s.pts) {
                self.report.warn(format!(
                    "shot {}: captions on a speed ramp dropped (as render.py does)",
                    s.id
                ));
                continue;
            }
            let start_s = self.f2s(s.start);
            let lo = start_s - s.audio_lead;
            let hi = self.f2s(s.start + s.n) + s.audio_tail;
            for (k, (a, b, txt)) in s.subs.iter().enumerate() {
                let t0 = (start_s + (a - s.in_s) / s.speed()).max(lo);
                let t1 = (start_s + (b - s.in_s) / s.speed()).min(hi);
                if t1 - t0 < 0.25 {
                    continue;
                }
                let (f0, f1) = (self.s2f(t0), self.s2f(t1));
                let mut p = obj(json!({"text": txt.trim(), "style": "caption"}));
                if let Some(y) = s.caption_y {
                    p.insert("y".into(), y.into());
                }
                let mut c = self.text_clip(&format!("{} sub {}", s.id, k + 1), p, f1 - f0);
                c.set_ffcine_id(format!("{}/sub{}", s.id, k + 1));
                out.push(Placed { start: f0, clip: c });
            }
        }
        out
    }

    /// Title, chapter cards, location stamps, end card, end screen. Also returns the card and
    /// location sound effects (`(frame, event, shot id, default gain)`).
    fn overlays(
        &mut self,
        shots: &[Shot],
        by_id: &BTreeMap<String, usize>,
        cues: &[Cue],
    ) -> (Vec<Placed>, Vec<AutoSfx>) {
        let mut out = Vec::new();
        let mut sfx = Vec::new();
        out.extend(self.title_overlays(shots, by_id, cues));
        let (cards, locs) = (self.style_cfg("card"), self.style_cfg("location"));
        let dur = |c: &Obj, d: (f64, f64, f64)| {
            let g = |k: &str, dflt: f64| c.get(k).and_then(num).unwrap_or(dflt);
            g("fade_in", d.0) + g("hold", d.1) + g("fade_out", d.2) + 0.05
        };
        for s in shots {
            let dis = self.f2s(s.dissolve) + 0.3;
            if let Some(text) = &s.card {
                let delay = cards.get("delay").and_then(num).unwrap_or(0.45).max(dis);
                let start = self.snap_time(cues, s.start + self.s2f(delay));
                let n = self.s2f(dur(&cards, (0.7, 2.5, 0.6)));
                let mut c = self.text_clip(
                    &format!("{} card", s.id),
                    obj(json!({"text": text, "style": "card"})),
                    n,
                );
                c.set_ffcine_id(format!("{}/card", s.id));
                out.push(Placed { start, clip: c });
                if let Some(e) = cards.get("sfx").cloned()
                    && !s.no_auto_sfx
                {
                    sfx.push((start, Obj::from_value(Some(e)), s.id.clone(), -8.0));
                }
            }
            if let Some(text) = &s.location
                && locs.get("enabled").is_none_or(truthy)
            {
                let delay = s
                    .location_at
                    .or_else(|| locs.get("delay").and_then(num))
                    .unwrap_or(0.5)
                    .max(dis);
                let start = self.snap_time(cues, s.start + self.s2f(delay));
                let n = self.s2f(dur(&locs, (0.6, 2.6, 0.6)));
                let mut c = self.text_clip(
                    &format!("{} location", s.id),
                    obj(json!({"text": text, "style": "location"})),
                    n,
                );
                c.set_ffcine_id(format!("{}/location", s.id));
                out.push(Placed { start, clip: c });
                if let Some(e) = locs.get("sfx").cloned()
                    && !s.no_auto_sfx
                {
                    sfx.push((start, Obj::from_value(Some(e)), s.id.clone(), -8.0));
                }
            }
        }
        (out, sfx)
    }

    /// `project.styles.<name>` as read from the file (kept on the converter by `project`).
    fn style_cfg(&self, name: &str) -> Obj {
        Obj::from_value(self.styles.get(name).cloned())
    }

    #[allow(clippy::many_single_char_names)]
    fn title_overlays(
        &mut self,
        shots: &[Shot],
        by_id: &BTreeMap<String, usize>,
        cues: &[Cue],
    ) -> Vec<Placed> {
        let mut out = Vec::new();
        let ti = self.style_cfg("title");
        if let Some(sid) = ti.get("shot").map(text) {
            if let Some(&i) = by_id.get(&sid) {
                let s = &shots[i];
                let txt = ti.get("title").map(text).unwrap_or_default();
                let mut p = obj(json!({"text": txt, "style": "title"}));
                // render.py snaps each `*_at` onto a beat; the snapped ones override the style.
                if ti.get("snap_to_beats").is_none_or(truthy) {
                    for k in [
                        "title_at",
                        "rule_at",
                        "subline1_at",
                        "subline2_at",
                        "out_at",
                    ] {
                        let Some(at) = ti.get(k).and_then(num) else {
                            continue;
                        };
                        let f = self.snap_time(cues, s.start + self.s2f(at)) - s.start;
                        if f != self.s2f(at) {
                            p.insert(k.into(), self.f2s(f).into());
                        }
                    }
                }
                let g = |k: &str| p.get(k).or_else(|| ti.get(k)).and_then(num).unwrap_or(0.0);
                let n = self.s2f(g("out_at") + g("out_duration") + 0.1);
                let mut c = self.text_clip("title", p, n);
                c.set_ffcine_id("title");
                out.push(Placed {
                    start: s.start,
                    clip: c,
                });
            } else {
                self.report.warn(format!("title.shot {sid} is not a shot"));
            }
        }
        for (style, name) in [("endcard", "endcard"), ("endscreen", "endscreen")] {
            let cfg = self.style_cfg(style);
            let Some(sid) = cfg.get("shot").map(text) else {
                continue;
            };
            let Some(&i) = by_id.get(&sid) else {
                self.report
                    .warn(format!("{style}.shot {sid} is not a shot"));
                continue;
            };
            let s = &shots[i];
            let mut p = obj(json!({"style": name}));
            for k in ["line1", "line2"] {
                if let Some(v) = cfg.get(k) {
                    p.insert(k.into(), v.clone());
                }
            }
            let mut c = self.text_clip(name, p, s.n);
            c.set_ffcine_id(name);
            out.push(Placed {
                start: s.start,
                clip: c,
            });
        }
        out
    }

    /// Per-shot `sfx` events plus the card/location ones (`render.py` `build_sfx`).
    fn sfx(&mut self, shots: &[Shot], auto: Vec<AutoSfx>, cfg: &mut Obj) -> Vec<Placed> {
        let dir = join(&self.base, &cfg.str("dir").unwrap_or_else(|| ".".into()));
        let default_gain = cfg.f64("default_gain").unwrap_or(-6.0);
        let duck_db = cfg.f64("duck_db").unwrap_or(-3.5);
        let duck_files = cfg.str("duck_files").unwrap_or_else(|| DUCK_FILES.into());
        let public_map = cfg.obj("public_map").0;
        let mut events: Vec<AutoSfx> = Vec::new();
        for s in shots {
            for e in &s.sfx {
                let at = e.get("at").and_then(num).unwrap_or(0.0);
                events.push((
                    s.start + self.s2f(at),
                    e.clone(),
                    s.id.clone(),
                    default_gain,
                ));
            }
        }
        events.extend(auto);
        events.sort_by_key(|e| e.0);
        let mut out = Vec::new();
        for (k, (start, mut e, shot, dflt_gain)) in events.into_iter().enumerate() {
            let Some(file) = e.str("file") else {
                self.report.warn(format!("shot {shot}: sfx without file"));
                continue;
            };
            e.take("at");
            let path = join(&dir, &file);
            let secs = self.duration_of(&path).unwrap_or_else(|| {
                self.report.warn(format!(
                    "{}: no duration (not a readable WAV, no media_duration); using 1 s",
                    path.display()
                ));
                1.0
            });
            let n = self.s2f(secs).max(1);
            let source = Source {
                src: file.clone(),
                path: Some(path),
                kind: Kind::Video,
            };
            let mut c = self.clip(&file, &source, 0.0, n, 1.0);
            c.ffcine_mut().shift_remove("source");
            let gain = e.f64("gain").unwrap_or(dflt_gain);
            let duck = e.f64("duck").unwrap_or_else(|| {
                if duck_files
                    .split('|')
                    .any(|p| !p.is_empty() && file.contains(p))
                {
                    duck_db
                } else {
                    0.0
                }
            });
            c.effects.push(Effect::Effect(GenericEffect::ffcine(
                "ffcine.audio.level/1",
                obj(json!({"gain_db": gain, "duck_db": duck})),
            )));
            let private = e.truthy("private") || file.starts_with("meme/");
            let public = e.take("public").or_else(|| public_map.get(&file).cloned());
            c.set_ffcine_id(format!("{shot}/sfx{}", k + 1));
            if private {
                let alt = public
                    .filter(|v| !v.is_null())
                    .map(|v| Value::String(join(&dir, &text(&v)).to_string_lossy().into_owned()));
                let ns = c.ffcine_mut();
                ns.insert("private".into(), true.into());
                ns.insert("public_alt".into(), alt.unwrap_or(Value::Null));
            }
            if !e.0.is_empty() {
                self.keep_foreign("shots[].sfx[].", &e.0);
                c.set_foreign(APP, Value::Object(e.0));
            }
            out.push(Placed { start, clip: c });
        }
        out
    }

    /// `music.cues` -> music clips (`render.py` cue timing, from [`Self::time_shots`]).
    fn music(
        &mut self,
        cfg: &mut Obj,
        tracks: &Map<String, Value>,
        mut cues: Vec<Cue>,
        total: i64,
    ) -> Vec<Placed> {
        let level = cfg.f64("level_lufs");
        let mut out = Vec::new();
        for k in 0..cues.len() {
            let start = cues[k].start.unwrap_or(0);
            let next = cues.get(k + 1).map(|c| {
                (
                    c.start.unwrap_or(0),
                    c.o.get("fade_out_prev").and_then(num).unwrap_or(1.0),
                )
            });
            let in_s = cues[k].in_s.unwrap_or(0.0);
            let name = cues[k].track.clone();
            let c = &mut cues[k].o;
            let fade_out = next.map_or_else(|| c.f64("fade_out").unwrap_or(0.0), |n| n.1);
            let end = next.map_or(total, |(s, f)| s + self.s2f(f));
            let mut track = Obj::from_value(tracks.get(&name).cloned());
            let Some(file) = track.str("file") else {
                self.report.warn(format!("music cue: unknown track {name}"));
                continue;
            };
            let path = join(&self.base, &file);
            c.take("fade_out_prev");
            let source = Source {
                src: file.clone(),
                path: Some(path),
                kind: Kind::Video,
            };
            let mut clip = self.clip(&name, &source, in_s, end - start, 1.0);
            clip.ffcine_mut().shift_remove("source");
            let mut lv = Map::new();
            if let Some(l) = level {
                lv.insert("lufs".into(), l.into());
            }
            for (key, val) in [
                ("gain_db", c.f64("gain_db")),
                ("fade_in", c.f64("fade_in")),
                ("fade_out", Some(fade_out)),
            ] {
                if let Some(v) = val {
                    lv.insert(key.into(), v.into());
                }
            }
            clip.effects.push(Effect::Effect(GenericEffect::ffcine(
                "ffcine.audio.level/1",
                lv,
            )));
            clip.set_ffcine_id(format!("music{}", k + 1));
            let ns = clip.ffcine_mut();
            ns.insert("music".into(), name.clone().into());
            if let Some(a) = track.take("attribution") {
                ns.insert("attribution".into(), a);
            }
            let mut rest = std::mem::take(&mut c.0);
            rest.extend(track.0.into_iter().map(|(k, v)| (format!("track.{k}"), v)));
            if !rest.is_empty() {
                self.keep_foreign("music.cues[].", &rest);
                clip.set_foreign(APP, Value::Object(rest));
            }
            out.push(Placed { start, clip });
        }
        out
    }

    /// `ambience` beds: a looped source range under a run of shots.
    fn ambience(
        &mut self,
        beds: Option<Value>,
        shots: &[Shot],
        by_id: &BTreeMap<String, usize>,
    ) -> Vec<Placed> {
        let Some(Value::Array(beds)) = beds else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (k, b) in beds.into_iter().enumerate() {
            let mut b = Obj::from_value(Some(b));
            let (Some(from), Some(to), Some(src)) = (b.str("from"), b.str("to"), b.str("src"))
            else {
                self.report
                    .warn(format!("ambience[{k}]: needs from, to and src"));
                continue;
            };
            let (Some(&i0), Some(&i1)) = (by_id.get(&from), by_id.get(&to)) else {
                self.report
                    .warn(format!("ambience[{k}]: unknown shot {from} or {to}"));
                continue;
            };
            // The bed runs a little past its shots at both ends, within the edit.
            let total = shots.last().map_or(0, |s| s.start + s.n);
            let pre = self.s2f(b.f64("pre").unwrap_or(0.3));
            let post = self.s2f(b.f64("post").unwrap_or(0.3));
            let start = (shots[i0].start - pre).max(0);
            let end = (shots[i1].start + shots[i1].n + post).min(total);
            let source = self.resolve(&src, &format!("ambience[{k}]"));
            let in_s = b.f64("in").unwrap_or(0.0);
            let out_s = b.f64("out");
            let mut c = self.clip(
                &format!("ambience {from}-{to}"),
                &source,
                in_s,
                end - start,
                1.0,
            );
            c.set_ffcine_id(format!("ambience{}", k + 1));
            let mut lv = Map::new();
            if let Some(l) = b.f64("level_lufs") {
                lv.insert("lufs".into(), l.into());
            }
            if let Some(f) = b.f64("fade") {
                lv.insert("fade_in".into(), f.into());
                lv.insert("fade_out".into(), f.into());
            }
            c.effects.push(Effect::Effect(GenericEffect::ffcine(
                "ffcine.audio.level/1",
                lv,
            )));
            let (highpass, lowpass) = (b.f64("highpass"), b.f64("lowpass"));
            if highpass.is_some() || lowpass.is_some() {
                c.effects.push(Effect::Effect(GenericEffect::ffcine(
                    "ffcine.audio.clean/1",
                    obj(json!({
                        "amount": 0.0,
                        "highpass": highpass.unwrap_or(60.0),
                        "lowpass": lowpass.unwrap_or(14000.0),
                    })),
                )));
            }
            let mut lp = obj(json!({"in": in_s, "crossfade": 1.0}));
            if let Some(o) = out_s {
                lp.insert("out".into(), o.into());
            }
            c.effects.push(Effect::Effect(GenericEffect::ffcine(
                "ffcine.audio.loop/1",
                lp,
            )));
            if !b.0.is_empty() {
                self.keep_foreign("ambience[].", &b.0);
                c.set_foreign(APP, Value::Object(b.0));
            }
            out.push(Placed { start, clip: c });
        }
        out
    }

    /// Lays clips out on as few tracks as fit without overlaps, with gaps between.
    fn lay_out(&mut self, prefix: &str, kind: &str, mut placed: Vec<Placed>) -> Vec<Track> {
        placed.sort_by_key(|p| p.start);
        let mut tracks: Vec<(Track, i64)> = Vec::new();
        for Placed {
            mut start,
            mut clip,
        } in placed
        {
            if start < 0 {
                let id = clip.ffcine_id().unwrap_or_default().to_owned();
                if !trim_front(&mut clip, -start) {
                    self.report
                        .warn(format!("{id}: entirely before the timeline start; dropped"));
                    continue;
                }
                self.report.warn(format!(
                    "{id}: starts {:.2} s before the timeline; trimmed",
                    self.f2s(-start)
                ));
                start = 0;
            }
            let n = clip_frames(&clip, self.rate);
            let slot = tracks.iter().position(|(_, end)| *end <= start);
            let i = slot.unwrap_or_else(|| {
                let k = tracks.len() + 1;
                tracks.push((Track::new(format!("{prefix} {k}"), kind), 0));
                k - 1
            });
            let (track, end) = &mut tracks[i];
            if start > *end {
                track
                    .children
                    .push(Item::Gap(Gap::new(self.frames(start - *end))));
            }
            track.children.push(Item::Clip(clip));
            *end = start + n;
        }
        tracks.into_iter().map(|(t, _)| t).collect()
    }
}

/// An overlay parameter: a value, or keyframes `[(seconds from the overlay's start, value,
/// interp)]`.
#[derive(Debug, Clone)]
enum Anim {
    Still(Value),
    Keys(Vec<(f64, Value, String)>),
}

impl Anim {
    /// A plain value, or keys as `[[t, v], ...]` or `[{t, v, interp}, ...]`.
    fn parse(v: &Value) -> Option<Self> {
        let Some(a) = v.as_array() else {
            return num(v).map(|x| Anim::Still(json!(x)));
        };
        if a.iter().all(Value::is_number) {
            return Some(Anim::Still(v.clone()));
        }
        let mut keys = Vec::new();
        for k in a {
            keys.push(match k {
                Value::Array(p) if p.len() == 2 => (num(&p[0])?, p[1].clone(), "linear".into()),
                Value::Object(m) => (
                    num(m.get("t")?)?,
                    m.get("v")?.clone(),
                    m.get("interp")
                        .and_then(Value::as_str)
                        .unwrap_or("linear")
                        .to_owned(),
                ),
                _ => return None,
            });
        }
        keys.sort_by(|a, b| a.0.total_cmp(&b.0));
        (!keys.is_empty()).then_some(Anim::Keys(keys))
    }

    /// A scalar's value at `t` (linear, ease or hold segments).
    #[allow(clippy::many_single_char_names)]
    fn at(&self, t: f64) -> f64 {
        let ks = match self {
            Anim::Still(v) => return num(v).unwrap_or(0.0),
            Anim::Keys(ks) => ks,
        };
        let val = |v: &Value| num(v).unwrap_or(0.0);
        let Some(i) = ks.iter().rposition(|k| k.0 <= t) else {
            return val(&ks[0].1);
        };
        let Some(next) = ks.get(i + 1) else {
            return val(&ks[i].1);
        };
        let (a, b) = (val(&ks[i].1), val(&next.1));
        let u = ((t - ks[i].0) / (next.0 - ks[i].0)).clamp(0.0, 1.0);
        let u = match ks[i].2.as_str() {
            "hold" => 0.0,
            "ease" => 0.5 - 0.5 * (std::f64::consts::PI * u).cos(),
            _ => u,
        };
        a + (b - a) * u
    }

    /// A scalar faded up from 0 over `fin` s and down to 0 over the last `fout` of `dur` s.
    fn faded(&self, fin: f64, fout: f64, dur: f64) -> Self {
        let (lo, hi) = (fin.min(dur), (dur - fout).max(fin.min(dur)));
        let interp_at = |t: f64| match self {
            Anim::Keys(ks) => ks
                .iter()
                .rev()
                .find(|k| k.0 <= t)
                .map_or_else(|| "linear".to_owned(), |k| k.2.clone()),
            Anim::Still(_) => "linear".to_owned(),
        };
        let lin = || "linear".to_owned();
        let mut keys = Vec::new();
        if fin > 0.0 {
            keys.push((0.0, json!(0.0), lin()));
        }
        keys.push((lo, json!(self.at(lo)), interp_at(lo)));
        if let Anim::Keys(ks) = self {
            keys.extend(ks.iter().filter(|k| k.0 > lo && k.0 < hi).cloned());
        }
        if hi > lo {
            keys.push((hi, json!(self.at(hi)), lin()));
        }
        if fout > 0.0 {
            keys.push((dur, json!(0.0), lin()));
        }
        Anim::Keys(keys)
    }
}

/// `white`, `black` or `[r, g, b]` (0..1).
fn colour(v: &Value) -> Option<[f64; 3]> {
    match v {
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "white" => Some([1.0; 3]),
            "black" => Some([0.0; 3]),
            _ => None,
        },
        Value::Array(a) if a.len() == 3 => {
            let c: Vec<f64> = a.iter().filter_map(num).collect();
            (c.len() == 3).then(|| [c[0], c[1], c[2]])
        }
        _ => None,
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// `GradeParams` from a YAML grade mapping; unknown keys land in `other`.
fn grade_params(m: Map<String, Value>) -> std::result::Result<GradeParams, String> {
    let mut m = m;
    // YAML `extra: []` and numbers written as ints are fine; `lut: null` means unset.
    m.retain(|_, v| !v.is_null());
    serde_json::from_value(Value::Object(m)).map_err(|e| e.to_string())
}

#[allow(clippy::cast_possible_truncation)]
fn clip_frames(c: &Clip, rate: f64) -> i64 {
    c.source_range
        .as_ref()
        .map_or(0, |r| r.duration.rescaled_to(rate).value.round() as i64)
}

fn warp_scalar(c: &Clip) -> f64 {
    c.effects
        .iter()
        .find_map(|e| match e {
            Effect::LinearTimeWarp(w) => Some(w.time_scalar),
            _ => None,
        })
        .unwrap_or(1.0)
}

/// Drops `k` frames from the start of a clip. False if nothing would be left.
#[allow(clippy::cast_precision_loss)]
fn trim_front(c: &mut Clip, k: i64) -> bool {
    let speed = warp_scalar(c);
    let Some(r) = c.source_range.as_mut() else {
        return false;
    };
    if r.duration.value <= k as f64 {
        return false;
    }
    let rate = r.duration.rate;
    r.start_time.value += (k as f64) * speed * r.start_time.rate / rate;
    r.duration.value -= k as f64;
    true
}

/// Drops `k` frames from the end of a track item. False if it isn't long enough.
#[allow(clippy::cast_precision_loss)]
fn trim_end(item: &mut Item, k: i64) -> bool {
    let range = match item {
        Item::Clip(c) => c.source_range.as_mut(),
        Item::Stack(s) => s.source_range.as_mut(),
        Item::Gap(g) => g.source_range.as_mut(),
        _ => None,
    };
    match range {
        Some(r) if r.duration.value > k as f64 => {
            r.duration.value -= k as f64;
            true
        }
        _ => false,
    }
}

/// Duration of a PCM WAV file from its header.
fn wav_duration(path: &Path) -> Option<f64> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    if ext != "wav" {
        return None;
    }
    let mut f = File::open(path).ok()?;
    let mut hdr = [0u8; 12];
    f.read_exact(&mut hdr).ok()?;
    if &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
        return None;
    }
    let mut byte_rate = None;
    loop {
        let mut ch = [0u8; 8];
        f.read_exact(&mut ch).ok()?;
        let size = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
        match &ch[0..4] {
            b"fmt " => {
                let mut fmt = vec![0u8; size as usize];
                f.read_exact(&mut fmt).ok()?;
                byte_rate = fmt
                    .get(8..12)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                if size % 2 == 1 {
                    f.seek(SeekFrom::Current(1)).ok()?;
                }
            }
            b"data" => {
                let br = byte_rate.filter(|b| *b > 0)?;
                return Some(f64::from(size) / f64::from(br));
            }
            _ => {
                f.seek(SeekFrom::Current(i64::from(size) + i64::from(size % 2)))
                    .ok()?;
            }
        }
    }
}

//! The footers of the long `--help`: examples per command, and the file formats that otherwise
//! live only in `docs/`.

/// `Examples:` and one indented line per example (`# ...` lines are comments).
macro_rules! examples {
    ($($line:literal),+ $(,)?) => {
        concat!("\n\nExamples:\n", $("\n      ", $line),+)
    };
}

/// A page under `docs/` in the repository.
macro_rules! docs {
    ($page:literal) => {
        concat!(
            "https://github.com/talaagent/ffcine/blob/master/docs/",
            $page
        )
    };
}

/// The `docs/` pages to read next.
macro_rules! see {
    ($($page:literal),+) => {
        concat!("\n\nDocs:\n", $("\n      ", docs!($page)),+)
    };
}

/// A reference section: a title, then its lines as written (indented, unwrapped).
macro_rules! reference {
    ($title:literal, $($line:literal),+ $(,)?) => {
        concat!("\n\n", $title, "\n", $("\n      ", $line),+)
    };
}

pub const TOP: &str = concat!(
    examples!(
        "ffcine doctor                                   # is the GPU path complete?",
        "ffcine ingest --to ~/projects/trip /media/CARD  # verified offload + catalog",
        "ffcine triage ~/projects/trip                   # strips, transcripts, clip page",
        "ffcine grade DJI_0001_D.MP4 graded.mkv         # one graded shot (D-Log auto)",
        "ffcine import edit.fcpxml -o timeline.otio      # another editor's cut",
        "ffcine render --profile master -o master.mp4 timeline.otio",
    ),
    see!("")
);

pub const PROBE: &str = examples!(
    "ffcine probe MVI_2884.MP4                       # Canon EOS R10",
    "ffcine probe DJI_0001_D.MP4                     # DJI Osmo Pocket 4",
    "ffcine probe photo.heic",
);

pub const GRADE: &str = concat!(
    examples!(
        "# Osmo Pocket 4 D-Log: camera LUT from the probed profile (DJI's after fetch-luts, else analytic)",
        "ffcine grade DJI_0001_D.MP4 graded.mkv",
        "# EOS R10 HDR PQ clip: tone-mapped to Rec.709 from the probe, 4K, light sharpening",
        "ffcine grade --size 3840x2160 --sharpen 0.6 MVI_3001.MP4 graded.mkv",
        "# Pocket 4 D-Log through a baked show LUT (camera transform included)",
        "ffcine grade --lut show_dlog.cube DJI_0001_D.MP4 graded.mkv",
        "# 5 s from 12.5 s at 23.976 fps, as a small 8-bit preview",
        "ffcine grade --ss 12.5 --frames 120 --preview MVI_2884.MP4 preview.mp4",
        "# Handheld Pocket 4 night shot: stabilized, night denoise, grain, reframed to a 9:16 reel",
        "ffcine grade --stabilize --denoise-preset night --grain 2 --render-profile reel DJI_0001_D.MP4 reel.mkv",
        "# Phone delivery with the timeline's titles over it",
        "ffcine grade --deliver phone --text timeline.otio --text-at 30 MVI_2884.MP4 phone.mp4",
        "# HDR10 master of an R10 HDR PQ clip (BT.2020 PQ, 1000 cd/m² mastering metadata)",
        "ffcine grade --hdr pq MVI_3001.MP4 hdr10.mkv",
        "# HLG master of a Pocket 4 D-Log clip, through the analytic HDR camera transform",
        "ffcine grade --hdr hlg DJI_0001_D.MP4 hlg.mkv",
    ),
    see!(
        "color.md",
        "denoise.md",
        "stabilize.md",
        "reframe.md",
        "hooks.md",
        "delivery.md#render-profiles"
    )
);

pub const BATCH: &str = concat!(
    reference!(
        "Batch file:",
        "One shot per line, written as `ffcine grade` arguments (flags, then INPUT OUTPUT).",
        "Words split on whitespace; '...' and \"...\" quote; # starts a comment; blank lines are skipped.",
        "Every line is checked before the first shot renders; errors name FILE:LINE.",
        "--cache-dir and --no-cache apply to the whole batch, from the command line.",
    ),
    examples!(
        "ffcine batch shots.txt",
        "# shots.txt:",
        "--lut show_dlog.cube --size 3840x2160 DJI_0001_D.MP4 DJI_0001_graded.mkv",
        "--ss 4 --frames 96 'card 2/MVI_2891.MP4' MVI_2891_graded.mkv   # a 4 s pickup",
        "--preview --camera dlog-vivid DJI_0007_D.MP4 DJI_0007_preview.mp4",
    ),
);

pub const STABILIZE: &str = concat!(
    examples!(
        "# 10 s at 23.976 fps: the warps as a shader for ffmpeg's libplacebo filter",
        "ffcine stabilize --frames 240 --hook warp.hook MVI_2884.MP4",
        "# Stabilize while grading instead",
        "ffcine grade --stabilize --frames 240 MVI_2884.MP4 stable.mkv",
        "# Pocket 4 walking shot: stronger smoothing, zoom capped at 1.1, warps as text",
        "ffcine stabilize --start 30 --frames 480 --smoothing 40 --max-zoom 1.1 --transforms warps.txt DJI_0001_D.MP4",
    ),
    see!("stabilize.md")
);

pub const JITTER: &str = examples!(
    "ffcine jitter --frames 240 stable.mkv",
    "# Skip the pair across a cut at frame 120",
    "ffcine jitter --frames 240 --exclude 120 --rate 30000/1001 stable.mp4",
);

pub const FETCH_LUTS: &str = examples!(
    "ffcine fetch-luts",
    "ffcine fetch-luts --dir ~/luts --force",
);

pub const BAKE: &str = concat!(
    examples!(
        "# Osmo Pocket 4 D-Log -> Rec.709 with a show LUT and a warmer look, as one 65^3 LUT",
        "ffcine bake --camera dlog --lut show.cube --warmth 0.03 --saturation 1.1 baked.cube",
        "# Pocket 4 D-Log -> Rec.709 with half a stop more exposure, a 33^3 LUT for a monitor",
        "ffcine bake --camera dlog --exposure 0.5 --size 33 dlog_709.cube",
        "# The analytic Pocket 4 D-Log -> BT.2020 PQ transform, mastered to 1000 cd/m²",
        "ffcine bake --camera dlog --hdr-peak 1000 dlog_pq.cube",
    ),
    see!("color.md#lut-baker")
);

pub const IMPORT: &str = concat!(
    examples!(
        "ffcine import edit.fcpxml -o timeline.otio",
        "ffcine import --format resolve --size 3840x2160 resolve_export.otio -o timeline.otio",
        "ffcine import --rate 24000/1001 --strict cut.edl -o timeline.otio",
        "ffcine import ~/CapCut/draft/0601 -o timeline.otio   # a CapCut draft folder",
    ),
    see!("import.md")
);

pub const TRIAGE: &str = concat!(
    examples!(
        "ffcine triage ~/projects/trip",
        "ffcine triage -j 8 --language en --hotwords 'gelato, piazza' ~/projects/trip",
        "ffcine triage --no-transcribe -o /tmp/triage /media/CARD",
        "ffcine triage --immich http://photos.example/api --camera-tz +02:00 ~/projects/trip",
    ),
    see!("triage.md")
);

pub const BREAKDOWN: &str = concat!(
    reference!(
        "Writes, in DIR:",
        "breakdown.json      every shot: in/out, frames, keyframes, motion, colour, beat, speech, text",
        "shotlist.md         # | Time | Dur | Motion | Notes | Keyframe (♪: cut on the beat)",
        "contact-sheet.jpg   the keyframes in a grid, numbered, with each shot's length (contact-sheet-2.jpg... past 12 rows)",
        "keyframes/NNN.jpg   each shot's sharpest middle frame; NNN-first.jpg, NNN-last.jpg",
    ),
    examples!(
        "ffcine breakdown reel.mp4",
        "ffcine breakdown -o /tmp/ref --no-transcribe trailer.mkv",
        "# Fast cutting: shots down to 3 frames, a lower cut threshold",
        "ffcine breakdown --min-shot 0.1 --threshold 0.22 reel.mp4",
        "# ffcine reads local files only: download first",
        "yt-dlp -o ref.mp4 URL && ffcine breakdown ref.mp4",
    ),
    see!("breakdown.md")
);

pub const SEARCH: &str = concat!(
    examples!(
        "ffcine search sunset",
        "ffcine search --paths 'over there'",
        "ffcine search --index ~/projects/trip/triage/index.json gelato",
    ),
    see!("triage.md")
);

pub const SNAP: &str = concat!(
    examples!(
        "# Move a rough 2.7 s - 7.3 s cut onto the nearest word edges",
        "ffcine snap MVI_2884 2.7 7.3",
        "ffcine snap --keep --pad 0.2 DJI_0001_D 1:02.5 1:09",
    ),
    see!("triage.md")
);

pub const CHAPTERS: &str = concat!(
    examples!(
        "ffcine chapters timeline.otio",
        "ffcine chapters --intro Welcome -o chapters.txt timeline.otio",
    ),
    see!("delivery.md#youtube-chapters")
);

pub const TAG: &str = concat!(
    examples!(
        "ffcine tag --timeline timeline.otio --cover 0:42 master.mp4",
        "ffcine tag --cover cover.jpg master.mp4",
        "ffcine tag --clear --timeline timeline.otio master.mp4",
    ),
    see!("delivery.md")
);

pub const CREDITS: &str = concat!(
    examples!(
        "ffcine credits timeline.otio",
        "ffcine credits --public -o credits.txt timeline.otio",
    ),
    see!("delivery.md#music-credits")
);

pub const STILL: &str = concat!(
    examples!(
        "ffcine still --at 1:23 timeline.otio",
        "ffcine still --at 95.5 --size 1280x720 -o thumb.png timeline.otio",
        "ffcine still --at 0:42 --no-text -o frame.png timeline.otio",
        "ffcine still --at 2.6 -o f.png story.comp.json   # a comp: every layer and text",
    ),
    see!("delivery.md#stills")
);

pub const COMP: &str = concat!(
    examples!(
        "ffcine comp check story.comp.json",
        "ffcine render -o story.mp4 story.comp.json",
        "ffcine still --at 2.6 -o f.png story.comp.json",
    ),
    see!("compositing.md")
);

pub const COMP_CHECK: &str = concat!(
    reference!(
        "Comp file (ffcine.comp/1, JSON or YAML; unknown keys are errors, x_* keys are ignored):",
        "schema    \"ffcine.comp/1\"",
        "canvas    size [w, h] px, fps, duration s, background (colour), fade (1 = picture),",
        "          fade_colour",
        "vars      {name: track}, readable by every expression",
        "project   an ffcine.project/1 object or a timeline file whose project grades go",
        "          through; with one, every video, still and comp layer is graded",
        "comps     {name: {canvas, vars, layers}}, nested comps used by name",
        "layers    bottom first: id, source, in, out, source_in, size [w, h], fit (cover,",
        "          contain, fill, none), content {zoom, zoom_center, crop_x, crop_y, focus, punch,",
        "          push, push_center, pan} (the picture inside the box), crop {left, right, top,",
        "          bottom, feather} (fractions of the source), transform {position, anchor, scale,",
        "          rotation, opacity}, radius, mask {shape: rect|ellipse, rect, feather, invert},",
        "          blend, fade, fade_colour, grade, time, filter, vars",
        "time      video layers: {speed: track} (source s per layer s, integrated from",
        "          source_in; negative runs back), {source: track} (source s), {freeze: s},",
        "          {reverse: true} (alone or with speed); frames picked as setpts + fps do",
        "grade     video, still and comp layers, as a clip's ffcine.color.grade/1: look,",
        "          camera (auto probes the source; video only), lut, lut_strength, rgb_gain,",
        "          sharpen, exposure, warmth, tint, contrast, gamma, lift, gain, saturation",
        "          (tracks), filters [\"eq=contrast=1.1\" or {eq: {option: track}}] (eq, hue,",
        "          colorbalance, colorlevels (layers only, not timeline extra), curves,",
        "          unsharp, vignette; others are errors: use filter),",
        "          shaders [{shader: NAME or PATH, params: {name: track}}]",
        "filter    video layers: {graph, hw: cpu|cuda|vulkan, clock: layer|comp, preroll,",
        "          postroll, whole_clip, timing: free}, a raw libavfilter graph run in the prep",
        "          (lossless intermediate); t is the layer's (or comp's) time in every piece;",
        "          stateful filters get rolls or the whole clip from a known table; n is an",
        "          error unless whole_clip; several chains name their ends [in] and [out];",
        "          the graph keeps the layer's size and frame count; only allowlisted",
        "          filters run (docs/compositing.md): pixel filters, plus readers (lut3d, lut1d,",
        "          subtitles, ass, drawtext, ...) whose files resolve against the comp's dir",
        "          and are keyed by contents; writers, sources, plugin loaders and URLs are",
        "          refused",
        "source    {video: PATH}, {still: PATH} (PNG, JPEG, WebP, HEIC; colour tags converted,",
        "          untagged taken as the output's space), {solid: COLOUR}, {text: {text, ...}}",
        "          (ffcine.text parameters), {comp: NAME or {canvas, vars, layers}}",
        "Every number is a track: a constant, keys [{t, v, interp}] (t: s from the layer's in),",
        "an expression string (t, lt, n, W, H, w, h, in, out, dur, T, vars; ffmpeg's functions),",
        "or {keys, expr}. Colours: [r, g, b(, a)] sRGB 0..1, black, white, #rrggbb[aa].",
        "Limits: canvas ≤ 16384 px a side, ≤ 8192x8192 px, aspect ≤ 4:1; fps 1..240;",
        "duration, in, out, source_in 0..3600 s; ≤ 64 layers (≤ 16 video), nested comps'",
        "included; comps nest ≤ 4 deep, at the parent's fps; unique string ids;",
        "layer size ≤ 65536 px; a layer's texture at its largest scale ≤ 16384 px a side;",
        "rotation within ±1e6 degrees; time speed within ±100, time source 0..3600 s.",
        "The canvas fade covers the composite, every blend mode.",
    ),
    examples!(
        "ffcine comp check story.comp.json",
        "ffcine comp check --profile master story.comp.json",
    ),
    see!("compositing.md")
);

pub const LOOKS: &str = concat!(
    reference!(
        "Variants (-v [NAME:]SPEC):",
        "SPEC is a project look name, a .cube file or LUT alias, `cut` (the clip's own grade),",
        "or comma-separated look=NAME, lut=ALIAS|FILE|none and grade fields:",
        "exposure, warmth, tint, contrast, gamma, lift, gain, saturation, lut_strength, sharpen.",
        "Each variant layers like a clip grade:",
        "project grade <- look <- source-kind rule <- the clip's override <- the variant's fields.",
        "rgb_gain and extra filters apply; denoise and extra filters with no GPU equivalent are",
        "skipped with a warning.",
    ),
    examples!(
        "# The cut against every project look, at two times",
        "ffcine looks --at 0:12 --at 1:40 timeline.otio",
        "# Named variants on 6 shots spread over the timeline",
        "ffcine looks --sample 6 -v warm:look=day,warmth=0.04 -v neutral:lut=none -v film.cube timeline.otio",
        "# A clip outside the timeline",
        "ffcine looks --clip MVI_2884.MP4@3.5 -v warm.cube -v cut -o looks/r10",
    ),
    see!("delivery.md#look-comparisons", "timeline.md#gradeparams")
);

pub const SUBS: &str = concat!(
    examples!(
        "ffcine subs --into timeline.otio captions.ass",
        "ffcine subs --offset=-1.5 --into timeline.otio -o captioned.otio captions.ass",
    ),
    see!("text.md#subtitles-from-ass")
);

pub const TITLE: &str = concat!(
    reference!(
        "--set keys (ffcine.title parameters, values as JSON):",
        "title_style, subtitle_style, lines_style   style overrides per piece (font, face, size,",
        "                                           colour, highlight, shadow, ...)",
        "rule, background, stagger, duration, fade_out, position, alignment, gap",
    ),
    examples!(
        "ffcine title --into edit.otio \"The Long Road\" --subtitle \"A short film\"",
        "ffcine title --into edit.otio --end \"Thanks for watching\" --line \"Music: Example Artist (CC BY 4.0)\" --line \"Made with ffcine\"",
        "ffcine title --into edit.otio \"Day one\" --at 12.5 --duration 4 --no-rule --set 'title_style={\"font\":\"DejaVu Serif\",\"face\":\"Light\"}' -o titled.otio",
        "ffcine still --at 2 -o title.png titled.otio",
    ),
    see!("text.md#titles-and-end-screens", "timeline.md#ffcinetitle")
);

pub const RENDER: &str = concat!(
    reference!(
        "Timeline keys (OTIO JSON, under metadata.ffcine; docs/timeline.md):",
        "project      ffcine.project/1 on the timeline: rate, grade, default_look, looks, luts,",
        "             shaders, rules, styles, output, mix",
        "output       aspect (landscape, reel, feed, square, WxH, NAME=WxH);",
        "             preview / master: size [w, h], max_punch",
        "mix          lufs, true_peak, headroom, normalize, fade_out, dialogue_lufs, nat_lufs,",
        "             gain_range, xfade, duck_db, nat_duck_db, duck_attack, duck_release,",
        "             sidechain, duck_threshold, clean, clean_nat, highpass, buses (docs/audio.md)",
        "grade        camera, lut, lut_strength, rgb_gain, warmth, tint, exposure, contrast,",
        "             gamma, lift, gain, saturation, denoise, sharpen, extra, extra_light",
        "clips        id, label, source, ai, hold_of, music, attribution, private, public_alt",
        "markers      chapter",
        "Grades layer: project.grade <- look <- light rule <- clip override.",
    ),
    reference!(
        "Effects (schema + parameters on a clip or stack):",
        "ffcine.color.grade/1   look + any grade field (the clip override)",
        "ffcine.color.cdl/1     ASC CDL slope, offset, power, saturation",
        "ffcine.fx.denoise/1    preset (night, snow), luma, chroma, protect, temporal {radius,",
        "                       motion (nvof, blocks), strength, chroma} (docs/denoise.md)",
        "ffcine.fx.grain/1      strength, size, chroma, profiles",
        "ffcine.fx.stabilize/1  smoothing, zoom (auto or a factor), max_zoom (docs/stabilize.md)",
        "ffcine.fx.shader/1     shader, params: an mpv/libplacebo hook (docs/hooks.md)",
        "ffcine.fx.fade/1, ffcine.fx.flash/1, ffcine.fx.vignette/1, ffcine.fx.darken/1",
        "ffcine.geo.reframe/1   zoom, punch, push, pan, fit, crop_x, crop_y, focus, profiles",
        "                       (docs/reframe.md)",
        "ffcine.geo.crop/1, ffcine.geo.transform/1   picture-in-picture: crop, anchor,",
        "                       translate, scale, rotate_deg",
        "ffcine.comp.blend/1    opacity, radius, mode: normal, add, subtract, difference, multiply,",
        "                       screen, overlay, hardlight, softlight, ... (Resolve's 27 modes)",
        "ffcine.comp.layout/1   split screens: layout (split, row, column, 1+2), gap, margin, ...",
        "ffcine.time.ramp/1     speed ramp points [[u, speed]]; ffcine.time.blend/1 frames",
        "ffcine.audio.level/1, .clean/1, .pan/1, .loop/1   per-clip audio (docs/audio.md)",
        "Transitions: dissolve, cross_fade, wipe, dip, flash, whip.",
        "Numbers may be keyframes [{t, v, interp}] (t: source time) or an expression over",
        "st, e.g. \"1 + 0.05*sin(st*4)\" (ffmpeg's functions; docs/compositing.md).",
    ),
    reference!(
        "Generators (text and colour clips; docs/text.md):",
        "ffcine.text    text, style, font, face, size, colour, position, alignment, profiles,",
        "               animation {in, out}: fade, slide, pop, typewriter, wipe, none;",
        "               keyframes {position, scale, opacity, colour}: [{t, v, interp}], or",
        "               expressions over lt and dur",
        "ffcine.title   title sequence or end screen (`ffcine title`)",
        "ffcine.solid   a colour fill",
        "HDR output is a `grade --hdr` option; render writes BT.709 SDR (docs/color.md).",
    ),
    examples!(
        "ffcine render -o preview.mp4 timeline.otio",
        "ffcine render --profile master -o master.mp4 timeline.otio",
        "ffcine render --render-profile reel -o reel.mp4 timeline.otio",
        "ffcine render --dry-run -o master.mp4 timeline.otio   # what would render",
        "ffcine render --profile review --cover 0:42 -o review.mp4 timeline.otio",
        "ffcine render --lufs=-16 --sidechain --json -o master.mp4 timeline.otio",
    ),
    see!("timeline.md", "assembly.md", "text.md", "color.md")
);

pub const DIFF: &str = concat!(
    examples!(
        "ffcine diff timeline.v1.otio timeline.v2.otio",
        "ffcine diff --profile master --render-profile reel --all v1.otio v2.otio",
    ),
    see!("assembly.md")
);

pub const PUBLISH: &str = examples!(
    "ffcine publish --immich 'Review' preview.mp4",
    "ffcine publish --immich 'Trip cuts' --config immich.json reel.mp4 master.mp4",
);

pub const INGEST: &str = concat!(
    examples!(
        "ffcine ingest --to ~/projects/trip /media/CARD",
        "ffcine ingest --to ~/projects/trip --backup /media/backup/trip /media/CARD",
        "# The R10's clock ran 2 min fast: fix it while copying",
        "ffcine ingest --dry-run --to ~/projects/trip --camera r10 --clock r10=-2m /media/CARD/DCIM",
    ),
    see!("ingest.md")
);

pub const CATALOG: &str = concat!(
    examples!(
        "ffcine catalog ~/projects/trip",
        "ffcine catalog --full --histogram always ~/projects/trip",
        "ffcine catalog --zone +02:00 --clock pocket=+1h ~/projects/trip",
    ),
    see!("ingest.md")
);

pub const DOCTOR: &str = examples!("ffcine doctor");

pub const SYNC: &str = concat!(
    examples!(
        "ffcine sync --audio recorder/ --video footage/ -o synced.otio",
        "ffcine sync --audio rec.wav --video MVI_2884.MP4 --timeline timeline.otio --wind -o timeline.otio",
        "ffcine sync --audio recorder/ --video footage/ --lead 0.5 --tail 1 --min-confidence 0.3 -o synced.otio",
    ),
    see!("audio.md")
);

pub const AUDIO: &str = concat!(
    examples!(
        "ffcine audio -o mix.wav timeline.otio",
        "ffcine audio --stems stems/ --lufs=-16 --true-peak=-1.5 -o mix.wav timeline.otio",
    ),
    see!("audio.md")
);

pub const BEATS: &str = examples!("ffcine beats song.mp3", "ffcine beats --json song.flac");

#[cfg(test)]
mod tests {
    /// The `--help` of `ffcine ARGS` as markdown (unwrapped), full or brief (`-h`).
    fn help(args: &[&str], full: bool) -> String {
        let mut words = args.to_vec();
        words.extend(["--help", "--help"]);
        match crate::cmd().run_inner(&words[..]) {
            Err(bpaf::ParseFailure::Stdout(doc, _)) => doc.render_markdown(full),
            other => panic!("ffcine {args:?} --help: {other:?}"),
        }
    }

    /// Commands and their one-line descriptions from the top-level help.
    fn commands() -> Vec<(String, String)> {
        let md = help(&[], true);
        let mut lines = md.lines().skip_while(|l| *l != "**Available commands:**");
        let mut out = Vec::new();
        while let Some(l) = lines.next() {
            if let Some(name) = l.strip_prefix("- **`").and_then(|l| l.split('`').next()) {
                out.push((
                    name.to_owned(),
                    lines.next().unwrap_or("").trim().to_owned(),
                ));
            }
        }
        out
    }

    #[test]
    fn long_help_flag() {
        let words = |v: &[&str]| {
            v.iter()
                .map(Into::into)
                .collect::<Vec<std::ffi::OsString>>()
        };
        assert_eq!(
            crate::long_help(words(&["grade", "--help"])),
            words(&["grade", "--help", "--help"])
        );
        assert_eq!(crate::long_help(words(&["-h"])), words(&["-h"]));
        assert_eq!(
            crate::long_help(words(&["publish", "--", "--help"])),
            words(&["publish", "--", "--help"])
        );
    }

    #[test]
    fn command_list_fits_one_line_each() {
        let cmds = commands();
        assert!(cmds.len() > 10, "{cmds:?}");
        // bpaf lists only the first line of a description: it must be a sentence that fits.
        let long: Vec<_> = cmds
            .iter()
            .filter(|(_, d)| !d.ends_with('.') || d.len() > 80)
            .collect();
        assert!(
            long.is_empty(),
            "first doc lines should be one short sentence: {long:#?}"
        );
    }

    /// Every command: every flag and positional has a help line, there are examples, and the full
    /// help says more than the brief one.
    #[test]
    fn every_command_is_documented() {
        let mut problems = Vec::new();
        for (name, _) in commands() {
            let md = help(&[&name], true);
            let lines: Vec<&str> = md.lines().collect();
            for (i, l) in lines.iter().enumerate() {
                if l.starts_with("- **") || l.starts_with("- _") {
                    let next = lines.get(i + 1).copied().unwrap_or_default();
                    let text = next.trim();
                    if !next.starts_with("  ") || text.is_empty() || text.starts_with("[default") {
                        problems.push(format!("{name}: no help for {l}"));
                    }
                }
            }
            let usage = lines
                .iter()
                .find(|l| l.starts_with("**Usage**"))
                .unwrap_or(&"");
            for word in usage.split_whitespace() {
                let word = word.trim_start_matches("\\[");
                if let Some(meta) = word.strip_prefix("_`").and_then(|w| w.split('`').next()) {
                    // A command group (`comp check`) lists its commands instead.
                    let group = meta == "COMMAND" && md.contains("Available commands");
                    if !group && !md.contains(&format!("- _`{meta}`_")) {
                        problems.push(format!("{name}: no help for positional {meta}"));
                    }
                }
            }
            let examples = md.split_once("Examples:").map_or("", |(_, e)| e);
            if !examples.contains(&format!("ffcine {name}")) {
                problems.push(format!("{name}: no examples"));
            }
            if help(&[&name], false).len() >= md.len() {
                problems.push(format!("{name}: --help says no more than -h"));
            }
        }
        assert!(problems.is_empty(), "\n{}", problems.join("\n"));
    }

    /// GitHub's anchor for a markdown heading.
    fn slug(heading: &str) -> String {
        heading
            .trim_start_matches('#')
            .trim()
            .to_lowercase()
            .chars()
            .filter_map(|c| match c {
                ' ' => Some('-'),
                c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
                _ => None,
            })
            .collect()
    }

    /// Every `docs/PAGE.md[#anchor]` the help points to exists, and no help says something is
    /// not done yet.
    #[test]
    fn help_links_resolve() {
        let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
        // A packaged source (the nix build, crates.io) has no docs/; the checkout's run checks links.
        let has_docs = docs.is_dir();
        let mut pages = vec![help(&[], true)];
        pages.extend(commands().iter().map(|(name, _)| help(&[name], true)));
        let mut problems = Vec::new();
        for md in &pages {
            for (i, _) in md.match_indices("docs/").filter(|_| has_docs) {
                let link: String = md[i + 5..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || "-_.#".contains(*c))
                    .collect();
                let link = link.trim_end_matches('.');
                let (page, anchor) = link.split_once('#').unwrap_or((link, ""));
                if page.is_empty() {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(docs.join(page)) else {
                    problems.push(format!("no docs/{page}"));
                    continue;
                };
                if !anchor.is_empty()
                    && !text
                        .lines()
                        .any(|l| l.starts_with('#') && slug(l) == anchor)
                {
                    problems.push(format!("no #{anchor} in docs/{page}"));
                }
            }
            for stale in ["not applied yet", "not implemented", "TODO"] {
                if md.contains(stale) {
                    problems.push(format!("help says {stale:?}"));
                }
            }
        }
        problems.sort();
        problems.dedup();
        assert!(problems.is_empty(), "\n{}", problems.join("\n"));
    }
}

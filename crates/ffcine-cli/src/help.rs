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
            "https://github.com/macalinao/ffcine/blob/master/docs/",
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
        "ffcine grade --lut look.cube clip.mov out.mkv   # one graded shot",
        "ffcine import edit.fcpxml -o timeline.otio      # another editor's cut",
        "ffcine render --profile master -o master.mp4 timeline.otio",
    ),
    see!("")
);

pub const PROBE: &str = examples!("ffcine probe clip.mov", "ffcine probe photo.heic");

pub const GRADE: &str = concat!(
    examples!(
        "# Log clip: camera LUT from the probed profile, 10-bit HEVC master",
        "ffcine grade clip.mov graded.mkv",
        "# A baked grade LUT, 4K out, light sharpening",
        "ffcine grade --lut look.cube --size 3840x2160 --sharpen 0.6 clip.mov graded.mkv",
        "# 5 s from 12.5 s, as a small 8-bit preview",
        "ffcine grade --ss 12.5 --frames 120 --preview clip.mov preview.mp4",
        "# Stabilized, denoised and with grain, reframed to a 9:16 reel",
        "ffcine grade --stabilize --temporal-radius 2 --grain 2 --render-profile reel clip.mov reel.mkv",
        "# Phone delivery with the timeline's titles over it",
        "ffcine grade --deliver phone --text timeline.otio --text-at 30 clip.mov phone.mp4",
        "# HDR10 master (BT.2020 PQ, 1000 cd/m² mastering metadata); --hdr hlg for HLG",
        "ffcine grade --hdr pq clip.mov hdr10.mkv",
    ),
    see!("color.md", "denoise.md", "delivery.md")
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
        "--lut look.cube --size 3840x2160 a001.mov a001_graded.mkv",
        "--ss 4 --frames 96 'card 2/b014.mov' b014_graded.mkv   # a 4 s pickup",
        "--preview --camera dlogm c007.mp4 c007_preview.mp4",
    ),
);

pub const STABILIZE: &str = concat!(
    examples!(
        "# 10 s at 23.976 fps: the warps as a shader for ffmpeg's libplacebo filter",
        "ffcine stabilize --frames 240 --hook warp.hook clip.mov",
        "# Stabilize while grading instead",
        "ffcine grade --stabilize --frames 240 clip.mov stable.mkv",
        "# Stronger smoothing, zoom capped at 1.1, warps as text",
        "ffcine stabilize --start 30 --frames 480 --smoothing 40 --max-zoom 1.1 --transforms warps.txt clip.mov",
    ),
    see!("stabilize.md")
);

pub const JITTER: &str = examples!(
    "ffcine jitter --frames 240 stable.mkv",
    "# Skip the pair across a cut at frame 120",
    "ffcine jitter --frames 240 --exclude 120 --rate 30 stable.mp4",
);

pub const FETCH_LUTS: &str = examples!(
    "ffcine fetch-luts",
    "ffcine fetch-luts --dir ~/luts --force",
);

pub const BAKE: &str = concat!(
    examples!(
        "# DJI D-Log -> Rec.709 with a show LUT and a warmer look, as one 65^3 LUT",
        "ffcine bake --camera dlog --lut show.cube --warmth 0.03 --saturation 1.1 baked.cube",
        "# The analytic Canon Log 3 -> BT.2020 PQ transform, mastered to 1000 cd/m²",
        "ffcine bake --camera clog3 --hdr-peak 1000 clog3_pq.cube",
    ),
    see!("color.md")
);

pub const IMPORT: &str = concat!(
    examples!(
        "ffcine import edit.fcpxml -o timeline.otio",
        "ffcine import --format resolve --size 3840x2160 resolve_export.otio -o timeline.otio",
        "ffcine import --rate 24000/1001 --strict cut.edl -o timeline.otio",
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

pub const SEARCH: &str = examples!(
    "ffcine search sunset",
    "ffcine search --paths 'over there'",
    "ffcine search --index ~/projects/trip/triage/index.json gelato",
);

pub const SNAP: &str = examples!(
    "# Move a rough 2.7 s - 7.3 s cut onto the nearest word edges",
    "ffcine snap A001_C003 2.7 7.3",
    "ffcine snap --keep --pad 0.2 A001_C003 1:02.5 1:09",
);

pub const CHAPTERS: &str = examples!(
    "ffcine chapters timeline.otio",
    "ffcine chapters --intro Welcome -o chapters.txt timeline.otio",
);

pub const TAG: &str = concat!(
    examples!(
        "ffcine tag --timeline timeline.otio --cover 0:42 master.mp4",
        "ffcine tag --cover cover.jpg master.mp4",
        "ffcine tag --clear --timeline timeline.otio master.mp4",
    ),
    see!("delivery.md")
);

pub const CREDITS: &str = examples!(
    "ffcine credits timeline.otio",
    "ffcine credits --public -o credits.txt timeline.otio",
);

pub const STILL: &str = examples!(
    "ffcine still --at 1:23 timeline.otio",
    "ffcine still --at 95.5 --size 1280x720 -o thumb.png timeline.otio",
    "ffcine still --at 0:42 --no-text -o frame.png timeline.otio",
);

pub const LOOKS: &str = concat!(
    reference!(
        "Variants (-v [NAME:]SPEC):",
        "SPEC is a project look name, a .cube file or LUT alias, `cut` (the clip's own grade),",
        "or comma-separated look=NAME, lut=ALIAS|FILE|none and grade fields:",
        "warmth, tint, contrast, gamma, lift, gain, saturation, lut_strength, sharpen.",
        "Each variant layers like a clip grade:",
        "project grade <- look <- source-kind rule <- the clip's override <- the variant's fields.",
        "exposure, rgb_gain, denoise and extra are not applied yet (warned).",
    ),
    examples!(
        "# The cut against every project look, at two times",
        "ffcine looks --at 0:12 --at 1:40 timeline.otio",
        "# Named variants on 6 shots spread over the timeline",
        "ffcine looks --sample 6 -v warm:look=day,warmth=0.04 -v neutral:lut=none -v film.cube timeline.otio",
        "# A clip outside the timeline",
        "ffcine looks --clip clip.mov@3.5 -v look.cube -v cut -o looks/clip",
    ),
    see!("delivery.md", "timeline.md#gradeparams")
);

pub const SUBS: &str = examples!(
    "ffcine subs --into timeline.otio captions.ass",
    "ffcine subs --offset=-1.5 --into timeline.otio -o captioned.otio captions.ass",
);

pub const RENDER: &str = concat!(
    reference!(
        "Timeline keys (OTIO JSON, under metadata.ffcine):",
        "project      ffcine.project/1 on the timeline: rate, grade, default_look, looks, luts,",
        "             rules, styles, output, mix",
        "output       aspect (landscape, reel, feed, square, WxH, NAME=WxH);",
        "             preview / master: size [w, h], max_punch",
        "mix          lufs, true_peak, headroom, normalize, fade_out, dialogue_lufs, nat_lufs,",
        "             duck_db, sidechain, clean, highpass, ...",
        "grade        camera, lut, lut_strength, rgb_gain, warmth, tint, exposure, contrast,",
        "             gamma, lift, gain, saturation, denoise, sharpen, extra",
        "clips        id, label, source, music, attribution, private, public_alt",
        "markers      chapter",
        "effects      ffcine.<domain>.<name> with their parameters, e.g. ffcine.geo.reframe/1,",
        "             ffcine.fx.denoise/1, ffcine.audio.level/1; ffcine.text generators",
        "Grades layer: project.grade <- look <- light rule <- clip override.",
    ),
    examples!(
        "ffcine render -o preview.mp4 timeline.otio",
        "ffcine render --profile master -o master.mp4 timeline.otio",
        "ffcine render --render-profile reel -o reel.mp4 timeline.otio",
        "ffcine render --dry-run -o master.mp4 timeline.otio   # what would render",
        "ffcine render --profile review --cover 0:42 -o review.mp4 timeline.otio",
        "ffcine render --lufs=-16 --sidechain --json -o master.mp4 timeline.otio",
    ),
    see!("timeline.md", "assembly.md")
);

pub const DIFF: &str = examples!(
    "ffcine diff timeline.v1.otio timeline.v2.otio",
    "ffcine diff --profile master --render-profile reel --all v1.otio v2.otio",
);

pub const PUBLISH: &str = examples!(
    "ffcine publish --immich 'Review' preview.mp4",
    "ffcine publish --immich 'Trip cuts' --config immich.json reel.mp4 master.mp4",
);

pub const INGEST: &str = concat!(
    examples!(
        "ffcine ingest --to ~/projects/trip /media/CARD",
        "ffcine ingest --to ~/projects/trip --backup /media/backup/trip /media/CARD",
        "ffcine ingest --dry-run --to ~/projects/trip --camera cam2 --clock cam2=-2m /media/CARD/DCIM",
    ),
    see!("ingest.md")
);

pub const CATALOG: &str = concat!(
    examples!(
        "ffcine catalog ~/projects/trip",
        "ffcine catalog --full --histogram always ~/projects/trip",
        "ffcine catalog --zone +02:00 --clock cam1=+1h ~/projects/trip",
    ),
    see!("ingest.md")
);

pub const DOCTOR: &str = examples!("ffcine doctor");

pub const SYNC: &str = concat!(
    examples!(
        "ffcine sync --audio recorder/ --video footage/ -o synced.otio",
        "ffcine sync --audio rec.wav --video clip.mov --timeline timeline.otio --wind -o timeline.otio",
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
                    if !md.contains(&format!("- _`{meta}`_")) {
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
}

//! `shotlist.md`: the shots as a Markdown table with an empty notes column to fill in.

use std::fmt::Write as _;

use crate::{Breakdown, clock};

/// Pipes and newlines would break the table.
fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

pub fn render(b: &Breakdown) -> String {
    let name = b.source.file_name().map_or_else(
        || b.source.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let mut s = format!("# {}\n\n", cell(&name));
    let p = &b.pacing;
    let _ = write!(
        s,
        "{} · {}x{} · {:.3} fps · {} shots, mean {:.2} s, median {:.2} s, {:.1} cuts/min",
        clock(b.duration),
        b.width,
        b.height,
        b.fps,
        p.shots,
        p.mean,
        p.median,
        p.cuts_per_minute
    );
    if let Some(m) = &b.music {
        let _ = write!(
            s,
            " · {:.0} BPM, {}/{} cuts on the beat (chance {:.0} %)",
            m.tempo,
            m.on_beat,
            m.cuts,
            m.chance * 100.0
        );
    }
    s.push_str("\n\n");
    let words = b
        .shots
        .iter()
        .any(|x| !x.speech.is_empty() || !x.text.is_empty());
    s.push_str("| # | Time | Dur | Motion |");
    if words {
        s.push_str(" Speech / text |");
    }
    s.push_str(" Notes | Keyframe |\n|---|---|---|---|");
    if words {
        s.push_str("---|");
    }
    s.push_str("---|---|\n");
    for x in &b.shots {
        let beat = match x.cut_on_beat {
            Some(true) => " ♪",
            _ => "",
        };
        let _ = write!(
            s,
            "| {} | {}{beat} | {:.2} s | {} |",
            x.n,
            clock(x.start),
            x.duration,
            x.motion.label()
        );
        if words {
            let mut w = Vec::new();
            if !x.speech.is_empty() {
                w.push(format!("“{}”", x.speech));
            }
            if !x.text.is_empty() {
                w.push(format!("[{}]", x.text));
            }
            let _ = write!(s, " {} |", cell(&w.join(" ")));
        }
        let _ = writeln!(s, "  | [{0}]({0}) |", x.keyframes.key);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::{Motion, MotionKind};
    use crate::{Keyframes, Pacing, Shot, colour};

    #[test]
    fn table() {
        let shot = Shot {
            n: 1,
            start: 0.0,
            end: 1.5,
            duration: 1.5,
            frames: [0, 45],
            keyframes: Keyframes {
                key: "keyframes/001.jpg".into(),
                key_time: 0.7,
                first: "keyframes/001-first.jpg".into(),
                last: "keyframes/001-last.jpg".into(),
            },
            motion: Motion {
                kind: MotionKind::Pan,
                direction: Some("left".into()),
                handheld: true,
                pan: -0.3,
                tilt: 0.0,
                zoom: 1.0,
                shake: 0.4,
                speed: 0.2,
            },
            colour: colour::average(&[]),
            cut_on_beat: None,
            beat_offset: None,
            beats: None,
            speech: "a | b".into(),
            text: String::new(),
        };
        let b = Breakdown {
            version: 1,
            source: "dir/reel.mp4".into(),
            duration: 1.5,
            fps: 30.0,
            width: 1080,
            height: 1920,
            frames: 45,
            decoder: "cpu".into(),
            pacing: Pacing {
                shots: 1,
                mean: 1.5,
                median: 1.5,
                shortest: 1.5,
                longest: 1.5,
                cuts_per_minute: 0.0,
            },
            cuts: vec![],
            shots: vec![shot],
            music: None,
            transcript: None,
            contact_sheets: vec![],
            shotlist: "shotlist.md".into(),
        };
        let md = render(&b);
        assert!(md.starts_with("# reel.mp4\n"));
        assert!(md.contains(
            "| 1 | 0:00.00 | 1.50 s | pan left (handheld) | “a \\| b” |  | [keyframes/001.jpg](keyframes/001.jpg) |"
        ));
    }
}

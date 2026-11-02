//! `ffcine title`: adds a title sequence or end screen (`ffcine.title` clip) to a timeline.

use std::path::PathBuf;

use anyhow::Context as _;
use bpaf::Bpaf;
use ffcine::timeline::Timeline;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Bpaf)]
#[allow(clippy::struct_excessive_bools)]
pub struct TitleArgs {
    /// Timeline to add it to.
    #[bpaf(argument("TIMELINE"))]
    into: PathBuf,
    /// Output timeline (default: overwrite --into).
    #[bpaf(short('o'), argument("FILE"))]
    output: Option<PathBuf>,
    /// An end screen: a dimmed backdrop, the title fading in word by word, credit lines after
    /// it, placed at the end of the timeline.
    end: bool,
    /// Start, timeline seconds (default: 0, or the end minus --duration with --end).
    #[bpaf(argument("SECONDS"))]
    at: Option<f64>,
    /// Length in seconds (default: 5, 8 with --end).
    #[bpaf(argument("SECONDS"))]
    duration: Option<f64>,
    /// Smaller line under the rule, in capitals.
    #[bpaf(argument("TEXT"))]
    subtitle: Option<String>,
    /// A credit line under the rest. Repeatable.
    #[bpaf(argument("TEXT"), many)]
    line: Vec<String>,
    /// Project style for every piece's font and colour (`project.styles.<NAME>`).
    #[bpaf(argument("NAME"))]
    style: Option<String>,
    /// Seconds between the title's words (default: 0.12, 0.15 with --end).
    #[bpaf(argument("SECONDS"))]
    stagger: Option<f64>,
    /// No rule between the title and the rest.
    no_rule: bool,
    /// Backdrop colour R,G,B[,A] in 0..1 (default: none; 0,0,0,0.6 with --end).
    #[bpaf(argument("R,G,B,A"))]
    background: Option<String>,
    /// No backdrop, also with --end.
    no_background: bool,
    /// Any other parameter as KEY=JSON, e.g. `title_style={"size":0.1}`. Repeatable.
    #[bpaf(argument("KEY=JSON"), many)]
    set: Vec<String>,
    /// The title (main line; `\n` breaks it, `*marks*` highlight with a `highlight` colour).
    #[bpaf(positional("TITLE"))]
    title: Option<String>,
}

/// The clip's parameters from the flags.
fn params(a: &TitleArgs) -> anyhow::Result<Map<String, Value>> {
    let mut p = Map::new();
    if a.end {
        p.insert("kind".into(), "end".into());
    }
    if let Some(t) = &a.title {
        p.insert("title".into(), t.replace("\\n", "\n").into());
    }
    if let Some(s) = &a.subtitle {
        p.insert("subtitle".into(), s.clone().into());
    }
    if !a.line.is_empty() {
        p.insert("lines".into(), a.line.clone().into());
    }
    if let Some(s) = &a.style {
        p.insert("style".into(), s.clone().into());
    }
    if let Some(s) = a.stagger {
        p.insert("stagger".into(), s.into());
    }
    if a.no_rule {
        p.insert("rule".into(), false.into());
    }
    if a.no_background {
        p.insert("background".into(), false.into());
    } else if let Some(b) = &a.background {
        let c: Vec<f64> = b
            .split(',')
            .map(|v| v.trim().parse::<f64>())
            .collect::<Result<_, _>>()
            .with_context(|| format!("--background {b}: expected R,G,B[,A]"))?;
        anyhow::ensure!(
            matches!(c.len(), 3 | 4),
            "--background {b}: expected R,G,B[,A]"
        );
        p.insert("background".into(), c.into());
    }
    for kv in &a.set {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("--set {kv}: expected KEY=JSON"))?;
        // Bare words are strings.
        let v = serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_owned()));
        p.insert(k.to_owned(), v);
    }
    anyhow::ensure!(
        ["title", "subtitle", "lines"]
            .iter()
            .any(|k| p.contains_key(*k)),
        "nothing to show: give a TITLE, --subtitle or --line"
    );
    Ok(p)
}

pub fn title(a: &TitleArgs) -> anyhow::Result<()> {
    let p = params(a)?;
    let mut t =
        Timeline::from_file(&a.into).with_context(|| format!("reading {}", a.into.display()))?;
    let duration = a.duration.unwrap_or(if a.end { 8.0 } else { 5.0 });
    anyhow::ensure!(duration > 0.0, "--duration must be positive");
    let at = a.at.unwrap_or(if a.end {
        (t.duration() - duration).max(0.0)
    } else {
        0.0
    });
    let track = ffcine::text::title::insert(&mut t, p, at, duration)?;
    let out = a.output.as_ref().unwrap_or(&a.into);
    t.to_file(out)
        .with_context(|| format!("writing {}", out.display()))?;
    eprintln!(
        "{}: {} at {at:.2}s for {duration:.2}s on track {track}",
        out.display(),
        if a.end { "end screen" } else { "title" },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bpaf::Parser as _;

    #[test]
    fn flags_to_params() {
        let a = title_args()
            .to_options()
            .run_inner(bpaf::Args::from(&[
                "--into",
                "edit.otio",
                "--end",
                "--line",
                "One",
                "--line",
                "Two",
                "--background",
                "0,0,0,1",
                "--set",
                "title_style={\"size\":0.1}",
                "--set",
                "alignment=left",
                "Thanks\\nfor watching",
            ]))
            .expect("parse");
        let p = params(&a).expect("params");
        assert_eq!(p["kind"], "end");
        assert_eq!(p["title"], "Thanks\nfor watching");
        assert_eq!(p["lines"], serde_json::json!(["One", "Two"]));
        assert_eq!(p["background"], serde_json::json!([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(p["title_style"]["size"], 0.1);
        assert_eq!(p["alignment"], "left");
        let none = title_args()
            .to_options()
            .run_inner(bpaf::Args::from(&["--into", "edit.otio"]))
            .expect("parse");
        assert!(params(&none).is_err());
    }
}

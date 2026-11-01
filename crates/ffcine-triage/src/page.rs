//! The clip page: one static `index.html` (strips beside it) that the editor can open from a share or
//! an Immich/Syncthing folder, search, and mark clips Use/Skip before anything is rendered. Picks
//! live in the browser and leave as `approvals.json` (Export), which [`Approvals`] reads.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::index::Index;

const TEMPLATE: &str = include_str!("page.html");

#[derive(Serialize)]
struct PageClip<'a> {
    id: &'a str,
    source: String,
    duration: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    recorded: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    strip: Option<&'a str>,
    times: &'a [f64],
    flags: Vec<PageFlag<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lang: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<&'a str>,
    /// (start, text) per transcript segment.
    segments: Vec<(f64, &'a str)>,
}

#[derive(Serialize)]
struct PageFlag<'a> {
    kind: crate::index::FlagKind,
    detail: &'a str,
}

/// The page HTML for an index whose strips are relative to the page.
pub fn render(index: &Index, title: &str) -> Result<String> {
    let clips: Vec<PageClip<'_>> = index
        .clips
        .iter()
        .map(|c| PageClip {
            id: &c.id,
            source: c
                .source
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            duration: c.duration,
            recorded: c.recorded.as_deref(),
            strip: c.strip.as_deref(),
            times: &c.strip_times,
            flags: c
                .flags
                .iter()
                .map(|f| PageFlag {
                    kind: f.kind,
                    detail: &f.detail,
                })
                .collect(),
            lang: c
                .transcript
                .as_ref()
                .map(|t| t.language.as_str())
                .filter(|l| !l.is_empty()),
            location: c.location.as_ref().and_then(|l| l.name.as_deref()),
            segments: c
                .transcript
                .iter()
                .flat_map(|t| &t.segments)
                .map(|s| (s.start, s.text.trim()))
                .collect(),
        })
        .collect();
    let data = serde_json::json!({ "root": index.root, "clips": clips });
    // `</` can't appear inside a <script> block.
    let data = serde_json::to_string(&data)?.replace("</", "<\\/");
    let title = title.replace('&', "&amp;").replace('<', "&lt;");
    Ok(TEMPLATE
        .replace("__TITLE__", &title)
        .replace("__DATA__", &data))
}

/// `approvals.json` as exported from the page.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Approvals {
    pub root: PathBuf,
    pub clips: Vec<Approval>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    #[serde(default)]
    pub source: String,
    /// `use`, `skip`, or none (a note only).
    #[serde(default)]
    pub pick: Option<String>,
    #[serde(default)]
    pub note: String,
}

impl Approvals {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }

    /// Ids marked Use.
    pub fn used(&self) -> impl Iterator<Item = &str> {
        self.clips
            .iter()
            .filter(|c| c.pick.as_deref() == Some("use"))
            .map(|c| c.id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Clip, Segment, Transcript};

    #[test]
    fn page_embeds_clips_safely() {
        let ix = Index {
            root: "/footage".into(),
            clips: vec![Clip {
                id: "c1".into(),
                source: "/footage/c1.MP4".into(),
                transcript: Some(Transcript {
                    segments: vec![Segment {
                        start: 1.0,
                        text: " </script> konbini".into(),
                        ..Segment::default()
                    }],
                    ..Transcript::default()
                }),
                ..Clip::default()
            }],
            ..Index::default()
        };
        let html = render(&ix, "Day 1").expect("render");
        assert!(html.contains("<title>Day 1</title>"));
        assert!(html.contains("<\\/script> konbini"));
        assert_eq!(html.matches("</script>").count(), 2);
    }

    #[test]
    fn reads_exported_approvals() {
        let a: Approvals = serde_json::from_str(
            r#"{"root":"/f","clips":[{"id":"a","source":"a.MP4","pick":"use","note":""},
               {"id":"b","source":"b.MP4","pick":null,"note":"gifu stalls"}]}"#,
        )
        .expect("parse");
        assert_eq!(a.used().collect::<Vec<_>>(), ["a"]);
    }
}

//! The ffcine timeline: plain OTIO JSON (`Timeline.1`, core schemas only), see `docs/design.md`.
//!
//! [`schema`] has serde types for the core OTIO objects, [`metadata`] the `ffcine` metadata
//! namespace. Reading and writing a file keeps everything, including fields and schemas the
//! model doesn't know (see [`schema`] for the one upgrade OTIO itself applies).
//!
//! ```
//! use ffcine_timeline::{Clip, ExternalReference, HasMetadata, Item, MediaReference, TimeRange,
//!     Timeline, Track, track_kind};
//!
//! let mut track = Track::new("V1", track_kind::VIDEO);
//! let media = MediaReference::External(ExternalReference::new("file:///a.mov", None));
//! let mut clip = Clip::new("a", media, Some(TimeRange::from_frames(10.0, 58.0, 24.0)));
//! clip.set_ffcine_id("a");
//! track.children.push(Item::Clip(clip));
//! let mut t = Timeline::default();
//! t.tracks.children.push(Item::Track(track));
//! let json = t.to_json()?;
//! assert_eq!(Timeline::from_json(&json)?, t);
//! # Ok::<(), ffcine_core::Error>(())
//! ```

pub mod metadata;
pub mod place;
pub mod project;
pub mod schema;
pub mod time;

use std::path::Path;

pub use metadata::{FFCINE, FFCINE_VERSION, HasMetadata};
pub use place::{Placed, PlacedMarker};
pub use project::{ClipGrade, GradeParams, MixParams, Output, OutputMode, Project, SourceKind};
pub use schema::{
    Clip, DEFAULT_MEDIA_KEY, Effect, ExternalReference, Gap, GeneratorReference, GenericEffect,
    Item, LinearTimeWarp, Marker, MediaReference, MissingReference, Stack, Timeline, Track,
    Transition, track_kind, transition_type,
};
pub use time::{RationalTime, TimeRange};

use ffcine_core::{Error, Result};

impl Timeline {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }

    /// Pretty-printed with 4-space indents, like OTIO.
    pub fn to_json(&self) -> Result<String> {
        let mut out = Vec::new();
        let fmt = serde_json::ser::PrettyFormatter::with_indent(b"    ");
        let mut ser = serde_json::Serializer::with_formatter(&mut out, fmt);
        serde::Serialize::serialize(self, &mut ser)?;
        out.push(b'\n');
        Ok(String::from_utf8(out).unwrap_or_default())
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let json = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        Self::from_json(&json)
    }

    pub fn to_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        std::fs::write(path, self.to_json()?).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })
    }

    /// `metadata.ffcine.project`; `None` if absent.
    pub fn project(&self) -> Option<Result<Project>> {
        self.ffcine_get("project")
    }

    /// Sets `metadata.ffcine.project` and the namespace version `metadata.ffcine.v`.
    pub fn set_project(&mut self, project: &Project) -> Result<()> {
        self.ffcine_mut().insert("v".into(), FFCINE_VERSION.into());
        self.ffcine_set("project", project)
    }

    /// The top-level tracks of `tracks`, bottom first.
    pub fn track_list(&self) -> impl Iterator<Item = &Track> {
        self.tracks.children.iter().filter_map(|c| match c {
            Item::Track(t) => Some(t),
            _ => None,
        })
    }

    /// Every clip, depth first, in timeline order within each track.
    pub fn clips(&self) -> Vec<&Clip> {
        let mut out = Vec::new();
        collect_clips(&self.tracks.children, &mut out);
        out
    }
}

fn collect_clips<'a>(items: &'a [Item], out: &mut Vec<&'a Clip>) {
    for item in items {
        match item {
            Item::Clip(c) => out.push(c),
            Item::Stack(s) => collect_clips(&s.children, out),
            Item::Track(t) => collect_clips(&t.children, out),
            Item::Gap(_) | Item::Transition(_) | Item::Unknown(_) => {}
        }
    }
}

impl Item {
    pub fn name(&self) -> &str {
        match self {
            Item::Clip(x) => &x.name,
            Item::Gap(x) => &x.name,
            Item::Stack(x) => &x.name,
            Item::Track(x) => &x.name,
            Item::Transition(x) => &x.name,
            Item::Unknown(m) => m
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(""),
        }
    }

    pub fn source_range(&self) -> Option<&TimeRange> {
        match self {
            Item::Clip(x) => x.source_range.as_ref(),
            Item::Gap(x) => x.source_range.as_ref(),
            Item::Stack(x) => x.source_range.as_ref(),
            Item::Track(x) => x.source_range.as_ref(),
            Item::Transition(_) | Item::Unknown(_) => None,
        }
    }
}

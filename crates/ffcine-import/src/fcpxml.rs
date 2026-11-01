//! Final Cut Pro X `.fcpxml` / `.fcpxmld` reader (FCPXML 1.8–1.11), straight from the XML: the
//! OTIO FCPXML adapter drops transitions and every filter (`docs/import.md`).
//!
//! What maps (fields and units: [`common`](super::common)):
//! - The first `project`'s sequence. The primary storyline is lane 0 (`V1`); connected clips and
//!   secondary storylines go to tracks by `lane`, audio-only items (`audio`, `srcEnable="audio"`,
//!   assets without video) to audio tracks. Compound clips (`ref-clip`) become nested `Stack`s.
//! - `asset-clip`, `clip`/`sync-clip` (media from their `video`/`audio` component), `video`,
//!   `audio` -> `Clip` + `ExternalReference` (the asset's `src` / `media-rep`); `gap` -> `Gap`;
//!   `title` -> `ffcine.text` generator.
//! - `transition` -> `Transition(SMPTE_Dissolve)` around the cut; anything other than Cross
//!   Dissolve also gets `metadata.ffcine.transition.kind`.
//! - `timeMap` -> `LinearTimeWarp` (constant) or `ffcine.time.ramp/1` + average
//!   `LinearTimeWarp`; `conform-rate` with scaling -> `LinearTimeWarp` (*unverified*).
//! - `adjust-transform`, `adjust-crop`, `adjust-blend`, `adjust-volume` (with
//!   `keyframeAnimation`), `filter-video` with `info-asc-cdl`, `marker` / `chapter-marker`.
//!
//! Units are *unverified* against FCP itself: `adjust-transform` position and crop edges are
//! taken as percent of the frame height (+y up), title `Position` as pixels (+y up).
//! Everything else is kept under `metadata.ffcine.foreign.fcpx` on the object it belongs to.

#![allow(clippy::many_single_char_names)] // maths on x, y, w, h, t, v

mod map;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::xml::Element;
use super::{ImportOptions, Imported};
use ffcine_core::{Error, Result};

/// Reads an `.fcpxml` file or `.fcpxmld` bundle.
pub fn import(input: &Path, opts: &ImportOptions) -> Result<Imported> {
    let path = document_path(input);
    let src = std::fs::read_to_string(&path).map_err(|source| Error::Io {
        path: path.clone(),
        source,
    })?;
    import_str(&src, opts)
}

/// Reads FCPXML from a string.
pub fn import_str(src: &str, opts: &ImportOptions) -> Result<Imported> {
    let doc = Doc::parse(src)?;
    map::map(&doc, opts)
}

/// `.fcpxmld` bundles are directories holding `Info.fcpxml`.
fn document_path(input: &Path) -> PathBuf {
    if input.is_dir() {
        input.join("Info.fcpxml")
    } else {
        input.to_owned()
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid {
        what: "FCPXML",
        value: msg.into(),
    }
}

/// An FCPXML time value (`"1001/30000s"`, `"5s"`, `"0s"`) in seconds.
pub(crate) fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    let s = s.strip_suffix('s').unwrap_or(s);
    match s.split_once('/') {
        Some((n, d)) => {
            let n: f64 = n.trim().parse().ok()?;
            let d: f64 = d.trim().parse().ok()?;
            (d != 0.0).then(|| n / d)
        }
        None => s.parse().ok(),
    }
}

/// Space-separated numbers (`"1 0.5 0.2"`).
pub(crate) fn parse_floats(s: &str) -> Vec<f64> {
    s.split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect()
}

/// A `format` resource.
#[derive(Debug, Clone)]
pub(crate) struct Format {
    /// Seconds per frame.
    pub frame_duration: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub el: Element,
}

/// An `asset` resource.
#[derive(Debug, Clone)]
pub(crate) struct Asset {
    pub name: String,
    /// Media URL: `src`, or the original `media-rep` (1.9+).
    pub src: Option<String>,
    pub start: f64,
    pub duration: Option<f64>,
    pub has_video: bool,
    pub has_audio: bool,
    pub el: Element,
}

/// A `project` and its `sequence`.
#[derive(Debug, Clone)]
pub(crate) struct Project {
    pub name: String,
    pub el: Element,
    pub sequence: Element,
    /// `library` location and `event` name it sits in, if any.
    pub event: Option<String>,
}

/// The parts of an FCPXML document the mapper looks things up in.
#[derive(Debug, Clone)]
pub(crate) struct Doc {
    pub version: String,
    pub formats: HashMap<String, Format>,
    pub assets: HashMap<String, Asset>,
    /// `effect` resources (filters, transitions, title templates).
    pub effects: HashMap<String, Element>,
    /// `media` resources: compound clips (`sequence`) and multicams.
    pub media: HashMap<String, Element>,
    /// `text-style-def`s by id, from anywhere in the document (titles share them).
    pub text_styles: HashMap<String, Element>,
    pub projects: Vec<Project>,
    pub library_location: Option<String>,
}

impl Doc {
    pub fn parse(src: &str) -> Result<Self> {
        let root = Element::parse(src).map_err(|e| invalid(e.to_string()))?;
        if root.name != "fcpxml" {
            return Err(invalid(format!("root element is <{}>", root.name)));
        }
        let mut doc = Doc {
            version: root.attr("version").unwrap_or_default().to_owned(),
            formats: HashMap::new(),
            assets: HashMap::new(),
            effects: HashMap::new(),
            media: HashMap::new(),
            text_styles: HashMap::new(),
            projects: Vec::new(),
            library_location: None,
        };
        if let Some(res) = root.child("resources") {
            for r in &res.children {
                let Some(id) = r.attr("id") else { continue };
                match r.name.as_str() {
                    "format" => {
                        doc.formats.insert(id.to_owned(), parse_format(r));
                    }
                    "asset" => {
                        doc.assets.insert(id.to_owned(), parse_asset(r));
                    }
                    "effect" => {
                        doc.effects.insert(id.to_owned(), r.clone());
                    }
                    "media" => {
                        doc.media.insert(id.to_owned(), r.clone());
                    }
                    _ => {}
                }
            }
        }
        collect_text_styles(&root, &mut doc.text_styles);
        doc.library_location = root
            .child("library")
            .and_then(|l| l.attr("location"))
            .map(str::to_owned);
        collect_projects(&root, None, &mut doc.projects);
        Ok(doc)
    }

    /// Seconds per frame of a format id.
    pub fn frame_duration(&self, format: Option<&str>) -> Option<f64> {
        self.formats.get(format?)?.frame_duration
    }
}

fn parse_format(r: &Element) -> Format {
    Format {
        frame_duration: r.attr("frameDuration").and_then(parse_time),
        width: r.attr("width").and_then(|v| v.parse().ok()),
        height: r.attr("height").and_then(|v| v.parse().ok()),
        el: r.clone(),
    }
}

fn parse_asset(r: &Element) -> Asset {
    let rep = r
        .children_named("media-rep")
        .find(|m| m.attr("kind") == Some("original-media"))
        .or_else(|| r.child("media-rep"));
    Asset {
        name: r.attr("name").unwrap_or_default().to_owned(),
        src: r
            .attr("src")
            .or_else(|| rep.and_then(|m| m.attr("src")))
            .map(str::to_owned),
        start: r.attr("start").and_then(parse_time).unwrap_or(0.0),
        duration: r.attr("duration").and_then(parse_time),
        has_video: r.attr("hasVideo") == Some("1"),
        has_audio: r.attr("hasAudio") == Some("1"),
        el: r.clone(),
    }
}

fn collect_text_styles(e: &Element, out: &mut HashMap<String, Element>) {
    for c in &e.children {
        if c.name == "text-style-def" {
            if let (Some(id), Some(style)) = (c.attr("id"), c.child("text-style")) {
                out.insert(id.to_owned(), style.clone());
            }
        } else {
            collect_text_styles(c, out);
        }
    }
}

fn collect_projects(e: &Element, event: Option<&str>, out: &mut Vec<Project>) {
    for c in &e.children {
        match c.name.as_str() {
            "library" => collect_projects(c, event, out),
            "event" => collect_projects(c, c.attr("name").or(event), out),
            "project" => {
                if let Some(seq) = c.child("sequence") {
                    out.push(Project {
                        name: c.attr("name").unwrap_or_default().to_owned(),
                        el: c.clone(),
                        sequence: seq.clone(),
                        event: event.map(str::to_owned),
                    });
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("0s"), Some(0.0));
        assert_eq!(parse_time("5s"), Some(5.0));
        assert_eq!(parse_time("28500/3000s"), Some(9.5));
        assert!((parse_time("1001/30000s").unwrap_or_default() - 0.033_366_7).abs() < 1e-6);
        assert_eq!(parse_time("1/0s"), None);
        assert_eq!(parse_time("x"), None);
    }

    #[test]
    fn resources_and_projects() {
        let doc = Doc::parse(
            r#"<?xml version="1.0"?><!DOCTYPE fcpxml>
            <fcpxml version="1.10">
              <resources>
                <format id="r1" frameDuration="1001/24000s" width="3840" height="2160"/>
                <asset id="r2" name="a" start="3600s" duration="10s" hasVideo="1">
                  <media-rep kind="original-media" src="file:///a.mov"/>
                </asset>
              </resources>
              <library location="file:///lib.fcpbundle/">
                <event name="E">
                  <project name="P"><sequence format="r1"><spine/></sequence></project>
                </event>
              </library>
            </fcpxml>"#,
        )
        .expect("parse");
        assert_eq!(doc.version, "1.10");
        let f = &doc.formats["r1"];
        assert_eq!((f.width, f.height), (Some(3840), Some(2160)));
        assert!((1.0 / f.frame_duration.unwrap_or(1.0) - 23.976).abs() < 1e-3);
        let a = &doc.assets["r2"];
        assert_eq!(a.src.as_deref(), Some("file:///a.mov"));
        assert!((a.start - 3600.0).abs() < 1e-9 && a.has_video && !a.has_audio);
        assert_eq!(doc.projects.len(), 1);
        assert_eq!(doc.projects[0].event.as_deref(), Some("E"));
        assert_eq!(
            doc.library_location.as_deref(),
            Some("file:///lib.fcpbundle/")
        );
    }

    #[test]
    fn rejects_other_xml() {
        assert!(Doc::parse("<mlt/>").is_err());
    }
}

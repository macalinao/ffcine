//! Importers: foreign projects -> an ffcine [`Timeline`] (`docs/import.md`).
//!
//! Each format is a module with `pub fn import(input: &Path, opts: &ImportOptions) ->
//! Result<Imported>`. Anything a module doesn't map goes to `metadata.ffcine.foreign.<app>` on
//! the object it came from and is listed in [`ImportReport::unmapped`]. Readers are lenient by
//! default: what they can't read at all (an unknown element, effect or event) is skipped and
//! listed in [`ImportReport::skipped`]; [`ImportOptions::strict`] turns any skip into an error.

pub mod capcut;
pub mod cdl;
mod common;
pub mod edit_yaml;
pub mod edl;
pub mod export;
pub mod fcp7;
pub mod fcpxml;
pub mod mlt;
pub mod resolve;
mod xml;

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use ffcine_core::{Error, Result};
use ffcine_timeline::Timeline;

/// Beat times (s) of a music file, `None` if it can't be analysed.
pub type BeatsFn = fn(&Path) -> Option<Vec<f64>>;

/// Settings the source format may lack.
#[derive(Debug, Clone, Copy, Default)]
pub struct ImportOptions {
    /// Frame size of the timeline, for formats that don't store one (Resolve OTIO).
    pub frame_size: Option<(u32, u32)>,
    /// Timeline frame rate; overrides the source's.
    pub rate: Option<f64>,
    /// Media duration in seconds, for formats that leave lengths to the media (`edit.yaml` shots
    /// without `out`, sound effects, `in: end` music cues). Without it, importers fall back to
    /// what they can read themselves (WAV headers) and warn.
    pub media_duration: Option<fn(&Path) -> Option<f64>>,
    /// Beat times (seconds into the file) of a music track, for formats that snap cuts to the
    /// music (`edit.yaml`). Without it cuts land unsnapped, with a warning.
    pub beats: Option<BeatsFn>,
    /// Fail instead of skipping what can't be read ([`ImportReport::skipped`]).
    pub strict: bool,
}

/// An imported timeline and what didn't carry over.
#[derive(Debug, Clone)]
pub struct Imported {
    pub timeline: Timeline,
    pub report: ImportReport,
}

/// What an import couldn't map exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Approximations and fallbacks, one line each.
    pub warnings: Vec<String>,
    /// Source fields kept only under `metadata.ffcine.foreign`, as paths (`shots[].snap`).
    pub unmapped: Vec<String>,
    /// Things that couldn't be read at all and were dropped (lenient mode), one line each.
    pub skipped: Vec<String>,
}

impl ImportReport {
    pub fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }

    /// Records something skipped; also a warning.
    pub fn skip(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.skipped.contains(&msg) {
            self.skipped.push(msg.clone());
        }
        self.warn(format!("skipped: {msg}"));
    }

    /// Records unmapped field paths, keeping the list sorted and unique.
    pub fn unmapped<I: IntoIterator<Item = String>>(&mut self, paths: I) {
        let mut all: BTreeSet<String> = self.unmapped.drain(..).collect();
        all.extend(paths);
        self.unmapped = all.into_iter().collect();
    }
}

impl fmt::Display for ImportReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for w in &self.warnings {
            writeln!(f, "warning: {w}")?;
        }
        if !self.unmapped.is_empty() {
            writeln!(f, "kept under ffcine.foreign: {}", self.unmapped.join(", "))?;
        }
        Ok(())
    }
}

/// Source formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    /// The Python prototype's `edit.yaml`.
    EditYaml,
    /// OTIO JSON, read as-is (a Resolve export is read as [`Format::Resolve`]).
    Otio,
    /// Final Cut Pro X `.fcpxml` / `.fcpxmld` ([`fcpxml`]).
    Fcpxml,
    /// MLT XML: Kdenlive `.kdenlive`, Shotcut / `melt` `.mlt` ([`mlt`]).
    Mlt,
    /// `DaVinci Resolve`'s native OTIO export ([`resolve`]).
    Resolve,
    /// Final Cut Pro 7 / Premiere Pro XML (`xmeml`, [`fcp7`]).
    Fcp7,
    /// CMX 3600 EDL, with ASC CDL comments ([`edl`]).
    Edl,
    /// `CapCut` / `JianYing` `draft_content.json` ([`capcut`]).
    CapCut,
}

impl Format {
    /// Guesses the format from the file name, and for `.xml` and `.json` from the content.
    pub fn detect(path: &Path) -> Option<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "yaml" | "yml" => Some(Format::EditYaml),
            "otio" => Some(Format::Otio),
            "fcpxml" | "fcpxmld" => Some(Format::Fcpxml),
            "mlt" | "kdenlive" => Some(Format::Mlt),
            "edl" => Some(Format::Edl),
            "xml" => {
                let head = sniff(path)?;
                if head.contains("<xmeml") {
                    Some(Format::Fcp7)
                } else if head.contains("<fcpxml") {
                    Some(Format::Fcpxml)
                } else if head.contains("<mlt") {
                    Some(Format::Mlt)
                } else {
                    None
                }
            }
            "json" => {
                let head = sniff(path)?;
                (head.contains("\"materials\"") || head.contains("\"canvas_config\""))
                    .then_some(Format::CapCut)
            }
            _ if path.is_dir() => capcut::draft_file(path).map(|_| Format::CapCut),
            _ => None,
        }
    }
}

impl FromStr for Format {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "edit-yaml" | "edit_yaml" | "yaml" => Ok(Format::EditYaml),
            "otio" => Ok(Format::Otio),
            "fcpxml" | "fcpxmld" | "fcpx" => Ok(Format::Fcpxml),
            "mlt" | "kdenlive" | "shotcut" => Ok(Format::Mlt),
            "resolve" => Ok(Format::Resolve),
            "fcp7" | "xmeml" | "premiere" | "fcp-xml" | "fcp_xml" => Ok(Format::Fcp7),
            "edl" | "cmx3600" | "cmx_3600" => Ok(Format::Edl),
            "capcut" | "jianying" => Ok(Format::CapCut),
            _ => Err(Error::Invalid {
                what: "import format",
                value: s.to_owned(),
            }),
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Format::EditYaml => "edit-yaml",
            Format::Otio => "otio",
            Format::Fcpxml => "fcpxml",
            Format::Mlt => "mlt",
            Format::Resolve => "resolve",
            Format::Fcp7 => "fcp7",
            Format::Edl => "edl",
            Format::CapCut => "capcut",
        })
    }
}

/// Imports `input` as `format`, or as the format its name suggests.
pub fn import(input: &Path, format: Option<Format>, opts: &ImportOptions) -> Result<Imported> {
    let format = format
        .or_else(|| Format::detect(input))
        .ok_or_else(|| Error::Invalid {
            what: "import format (pass one)",
            value: input.display().to_string(),
        })?;
    let imported = match format {
        Format::EditYaml => edit_yaml::import(input, opts),
        Format::Fcpxml => fcpxml::import(input, opts),
        Format::Mlt => mlt::import(input, opts),
        Format::Resolve => resolve::import(input, opts),
        Format::Fcp7 => fcp7::import(input, opts),
        Format::Edl => edl::import(input, opts),
        Format::CapCut => capcut::import(input, opts),
        Format::Otio if resolve::detect(input) => resolve::import(input, opts),
        Format::Otio => Ok(Imported {
            timeline: Timeline::from_file(input)?,
            report: ImportReport::default(),
        }),
    }?;
    if opts.strict && !imported.report.skipped.is_empty() {
        return Err(Error::Invalid {
            what: "input (strict import)",
            value: imported.report.skipped.join("; "),
        });
    }
    Ok(imported)
}

/// The first 4 KiB of a file, lossily as UTF-8.
fn sniff(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut buf = vec![0; 4096];
    let n = std::fs::File::open(path).ok()?.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

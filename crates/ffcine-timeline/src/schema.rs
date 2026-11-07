//! Serde types for the OTIO core schemas.
//!
//! Every object keeps the `OTIO_SCHEMA` string it was read with and an `extra` map for fields this
//! model doesn't name (newer OTIO fields such as `color`, `comment` or `enabled` on effects,
//! adapter fields), so a read/write cycle loses nothing. Objects of unknown schemas inside
//! polymorphic slots (`children`, `effects`, `media_references`) are kept verbatim as `Unknown`.
//! The only changes on read are OTIO's own: `Clip.1` -> `Clip.2`, and `Sequence.1` (the old
//! name) -> `Track.1`.

use std::collections::BTreeMap;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::time::{RationalTime, TimeRange};

/// `active_media_reference_key` of a clip with a single reference.
pub const DEFAULT_MEDIA_KEY: &str = "DEFAULT_MEDIA";

/// Defines `deserialize_with` functions that accept the `OTIO_SCHEMA` strings of some families.
macro_rules! schema_check {
    ($($fn:ident => [$($family:literal),+], max $max:literal;)*) => {$(
        pub(super) fn $fn<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
            let s = String::deserialize(d)?;
            match split_schema(&s) {
                Some((name, v)) if [$($family),+].contains(&name) && (1..=$max).contains(&v) => {
                    Ok(s)
                }
                _ => Err(de::Error::custom(format_args!(
                    "expected {} (version 1..={}), found {s:?}",
                    [$($family),+].join(" or "),
                    $max
                ))),
            }
        }
    )*};
}

schema_check! {
    rational_time => ["RationalTime"], max 1;
    time_range => ["TimeRange"], max 1;
    timeline => ["Timeline"], max 1;
    stack => ["Stack"], max 1;
    track => ["Track"], max 1;
    clip => ["Clip"], max 2;
    gap => ["Gap"], max 1;
    transition => ["Transition"], max 1;
    effect => ["Effect", "TimeEffect"], max 1;
    warp => ["LinearTimeWarp", "FreezeFrame"], max 1;
    marker => ["Marker"], max 3;
    external_reference => ["ExternalReference"], max 1;
    missing_reference => ["MissingReference"], max 1;
    generator_reference => ["GeneratorReference"], max 1;
}

/// `"Clip.2"` -> `("Clip", 2)`.
pub fn split_schema(s: &str) -> Option<(&str, u32)> {
    let (name, v) = s.rsplit_once('.')?;
    Some((name, v.parse().ok()?))
}

/// OTIO writes `"name": null` for unnamed objects in some files; read it as `""`.
fn name<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

fn yes() -> bool {
    true
}

macro_rules! schema_default {
    ($($fn:ident => $s:literal;)*) => {$(
        fn $fn() -> String {
            $s.into()
        }
    )*};
}

schema_default! {
    timeline_v => "Timeline.1";
    stack_v => "Stack.1";
    track_v => "Track.1";
    clip_v => "Clip.2";
    gap_v => "Gap.1";
    transition_v => "Transition.1";
    effect_v => "Effect.1";
    linear_time_warp_v => "LinearTimeWarp.1";
    freeze_frame_v => "FreezeFrame.1";
    marker_v => "Marker.2";
    external_reference_v => "ExternalReference.1";
    missing_reference_v => "MissingReference.1";
    generator_reference_v => "GeneratorReference.1";
}

/// `Timeline.1`: the root object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "timeline")]
    pub otio_schema: String,
    #[serde(default, deserialize_with = "name")]
    pub name: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub global_start_time: Option<RationalTime>,
    pub tracks: Stack,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            otio_schema: timeline_v(),
            name: String::new(),
            metadata: Map::new(),
            global_start_time: None,
            tracks: Stack::default(),
            extra: Map::new(),
        }
    }
}

/// `Stack.1`: children play at the same time, first at the bottom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stack {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "stack")]
    pub otio_schema: String,
    #[serde(default, deserialize_with = "name")]
    pub name: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub source_range: Option<TimeRange>,
    #[serde(default)]
    pub effects: Vec<Effect>,
    #[serde(default)]
    pub markers: Vec<Marker>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub children: Vec<Item>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Stack {
    fn default() -> Self {
        Self {
            otio_schema: stack_v(),
            name: String::new(),
            metadata: Map::new(),
            source_range: None,
            effects: Vec::new(),
            markers: Vec::new(),
            enabled: true,
            children: Vec::new(),
            extra: Map::new(),
        }
    }
}

/// `Track.kind` values.
pub mod track_kind {
    pub const VIDEO: &str = "Video";
    pub const AUDIO: &str = "Audio";
}

/// `Track.1`: children play one after another.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "track")]
    pub otio_schema: String,
    #[serde(default, deserialize_with = "name")]
    pub name: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub source_range: Option<TimeRange>,
    #[serde(default)]
    pub effects: Vec<Effect>,
    #[serde(default)]
    pub markers: Vec<Marker>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub children: Vec<Item>,
    /// [`track_kind::VIDEO`] or [`track_kind::AUDIO`].
    #[serde(default)]
    pub kind: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Track {
    pub fn new(name: impl Into<String>, kind: &str) -> Self {
        Self {
            otio_schema: track_v(),
            name: name.into(),
            metadata: Map::new(),
            source_range: None,
            effects: Vec::new(),
            markers: Vec::new(),
            enabled: true,
            children: Vec::new(),
            kind: kind.into(),
            extra: Map::new(),
        }
    }
}

/// `Clip.2`: a range of one media reference (of several alternatives).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "clip")]
    pub otio_schema: String,
    #[serde(default, deserialize_with = "name")]
    pub name: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub source_range: Option<TimeRange>,
    #[serde(default)]
    pub effects: Vec<Effect>,
    #[serde(default)]
    pub markers: Vec<Marker>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub media_references: BTreeMap<String, MediaReference>,
    #[serde(default)]
    pub active_media_reference_key: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Clip {
    pub fn new(
        name: impl Into<String>,
        media: MediaReference,
        source_range: Option<TimeRange>,
    ) -> Self {
        Self {
            otio_schema: clip_v(),
            name: name.into(),
            metadata: Map::new(),
            source_range,
            effects: Vec::new(),
            markers: Vec::new(),
            enabled: true,
            media_references: BTreeMap::from([(DEFAULT_MEDIA_KEY.to_owned(), media)]),
            active_media_reference_key: DEFAULT_MEDIA_KEY.into(),
            extra: Map::new(),
        }
    }

    pub fn media_reference(&self) -> Option<&MediaReference> {
        self.media_references.get(&self.active_media_reference_key)
    }

    pub fn media_reference_mut(&mut self) -> Option<&mut MediaReference> {
        self.media_references
            .get_mut(&self.active_media_reference_key)
    }
}

/// `Gap.1`: empty time in a track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gap {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "gap")]
    pub otio_schema: String,
    #[serde(default, deserialize_with = "name")]
    pub name: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub source_range: Option<TimeRange>,
    #[serde(default)]
    pub effects: Vec<Effect>,
    #[serde(default)]
    pub markers: Vec<Marker>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Gap {
    pub fn new(duration: RationalTime) -> Self {
        let start = RationalTime::new(0.0, duration.rate);
        Self {
            otio_schema: gap_v(),
            name: String::new(),
            metadata: Map::new(),
            source_range: Some(TimeRange::new(start, duration)),
            effects: Vec::new(),
            markers: Vec::new(),
            enabled: true,
            extra: Map::new(),
        }
    }
}

/// `Transition.transition_type` values.
pub mod transition_type {
    pub const SMPTE_DISSOLVE: &str = "SMPTE_Dissolve";
    pub const CUSTOM: &str = "Custom_Transition";
}

/// `Transition.1`: overlaps its neighbours by `in_offset` / `out_offset`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "transition")]
    pub otio_schema: String,
    #[serde(default, deserialize_with = "name")]
    pub name: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub transition_type: String,
    pub in_offset: RationalTime,
    pub out_offset: RationalTime,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Transition {
    pub fn dissolve(in_offset: RationalTime, out_offset: RationalTime) -> Self {
        Self {
            otio_schema: transition_v(),
            name: String::new(),
            metadata: Map::new(),
            transition_type: transition_type::SMPTE_DISSOLVE.into(),
            in_offset,
            out_offset,
            extra: Map::new(),
        }
    }
}

/// A child of a [`Track`] or [`Stack`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Item {
    Clip(Clip),
    Gap(Gap),
    Stack(Stack),
    Track(Track),
    Transition(Transition),
    /// Any other schema, verbatim.
    Unknown(Map<String, Value>),
}

impl<'de> Deserialize<'de> for Item {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut obj = Map::deserialize(d)?;
        let family = schema_of(&obj);
        let r = match family {
            Some(("Clip", 1)) => {
                upgrade_clip_1(&mut obj);
                serde_json::from_value(obj.into()).map(Item::Clip)
            }
            Some(("Clip", _)) => serde_json::from_value(obj.into()).map(Item::Clip),
            Some(("Gap", _)) => serde_json::from_value(obj.into()).map(Item::Gap),
            Some(("Stack", _)) => serde_json::from_value(obj.into()).map(Item::Stack),
            Some(("Track", _)) => serde_json::from_value(obj.into()).map(Item::Track),
            Some(("Sequence", 1)) => {
                obj.insert("OTIO_SCHEMA".into(), "Track.1".into());
                serde_json::from_value(obj.into()).map(Item::Track)
            }
            Some(("Transition", _)) => serde_json::from_value(obj.into()).map(Item::Transition),
            _ => Ok(Item::Unknown(obj)),
        };
        r.map_err(de::Error::custom)
    }
}

/// OTIO's upgrade: `media_reference` becomes `media_references[DEFAULT_MEDIA]`.
fn upgrade_clip_1(obj: &mut Map<String, Value>) {
    let media = match obj.shift_remove("media_reference") {
        Some(v @ Value::Object(_)) => v,
        _ => serde_json::to_value(MediaReference::Missing(MissingReference::default()))
            .unwrap_or_default(),
    };
    obj.insert("OTIO_SCHEMA".into(), "Clip.2".into());
    obj.insert(
        "media_references".into(),
        Value::Object(Map::from_iter([(DEFAULT_MEDIA_KEY.to_owned(), media)])),
    );
    obj.insert(
        "active_media_reference_key".into(),
        DEFAULT_MEDIA_KEY.into(),
    );
}

fn schema_of(obj: &Map<String, Value>) -> Option<(&str, u32)> {
    obj.get("OTIO_SCHEMA")
        .and_then(Value::as_str)
        .and_then(split_schema)
}

/// An entry of an item's `effects`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Effect {
    /// `Effect.1` (also used for `TimeEffect.1`).
    Effect(GenericEffect),
    /// `LinearTimeWarp.1`.
    LinearTimeWarp(LinearTimeWarp),
    /// `FreezeFrame.1` (a `LinearTimeWarp` with `time_scalar` 0).
    FreezeFrame(LinearTimeWarp),
    /// Any other schema, verbatim.
    Unknown(Map<String, Value>),
}

impl Effect {
    pub fn effect_name(&self) -> Option<&str> {
        match self {
            Effect::Effect(e) => Some(&e.effect_name),
            Effect::LinearTimeWarp(e) | Effect::FreezeFrame(e) => Some(&e.effect_name),
            Effect::Unknown(m) => m.get("effect_name").and_then(Value::as_str),
        }
    }
}

impl<'de> Deserialize<'de> for Effect {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let obj = Map::deserialize(d)?;
        let r = match schema_of(&obj) {
            Some(("Effect" | "TimeEffect", _)) => {
                serde_json::from_value(obj.into()).map(Effect::Effect)
            }
            Some(("LinearTimeWarp", _)) => {
                serde_json::from_value(obj.into()).map(Effect::LinearTimeWarp)
            }
            Some(("FreezeFrame", _)) => serde_json::from_value(obj.into()).map(Effect::FreezeFrame),
            _ => Ok(Effect::Unknown(obj)),
        };
        r.map_err(de::Error::custom)
    }
}

/// `Effect.1`: a named effect; ffcine's parameters live in `metadata.ffcine`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenericEffect {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "effect")]
    pub otio_schema: String,
    #[serde(default, deserialize_with = "name")]
    pub name: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub effect_name: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl GenericEffect {
    pub fn new(name: impl Into<String>, effect_name: impl Into<String>) -> Self {
        Self {
            otio_schema: effect_v(),
            name: name.into(),
            metadata: Map::new(),
            effect_name: effect_name.into(),
            extra: Map::new(),
        }
    }
}

/// `LinearTimeWarp.1` / `FreezeFrame.1`: constant speed change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinearTimeWarp {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "warp")]
    pub otio_schema: String,
    #[serde(default, deserialize_with = "name")]
    pub name: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub effect_name: String,
    #[serde(default = "one")]
    pub time_scalar: f64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn one() -> f64 {
    1.0
}

impl LinearTimeWarp {
    pub fn new(time_scalar: f64) -> Self {
        Self {
            otio_schema: linear_time_warp_v(),
            name: String::new(),
            metadata: Map::new(),
            effect_name: "LinearTimeWarp".into(),
            time_scalar,
            extra: Map::new(),
        }
    }

    pub fn freeze_frame() -> Self {
        Self {
            otio_schema: freeze_frame_v(),
            effect_name: "FreezeFrame".into(),
            ..Self::new(0.0)
        }
    }
}

/// `Marker.2` / `Marker.3`. `color` (a name in 2, an object in 3) and `comment` (3) stay in
/// `extra`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "marker")]
    pub otio_schema: String,
    #[serde(default, deserialize_with = "name")]
    pub name: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    pub marked_range: TimeRange,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Marker {
    /// A `Marker.2` (readable by OTIO 0.14+) with a colour name such as `"GREEN"`.
    pub fn new(name: impl Into<String>, marked_range: TimeRange, color: &str) -> Self {
        Self {
            otio_schema: marker_v(),
            name: name.into(),
            metadata: Map::new(),
            marked_range,
            extra: Map::from_iter([("color".to_owned(), color.into())]),
        }
    }
}

/// A clip's media.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum MediaReference {
    External(ExternalReference),
    Missing(MissingReference),
    Generator(GeneratorReference),
    /// Any other schema (`ImageSequenceReference`, ...), verbatim.
    Unknown(Map<String, Value>),
}

impl MediaReference {
    pub fn available_range(&self) -> Option<&TimeRange> {
        match self {
            MediaReference::External(r) => r.available_range.as_ref(),
            MediaReference::Missing(r) => r.available_range.as_ref(),
            MediaReference::Generator(r) => r.available_range.as_ref(),
            MediaReference::Unknown(_) => None,
        }
    }
}

impl<'de> Deserialize<'de> for MediaReference {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let obj = Map::deserialize(d)?;
        let r = match schema_of(&obj) {
            Some(("ExternalReference", _)) => {
                serde_json::from_value(obj.into()).map(MediaReference::External)
            }
            Some(("MissingReference", _)) => {
                serde_json::from_value(obj.into()).map(MediaReference::Missing)
            }
            Some(("GeneratorReference", _)) => {
                serde_json::from_value(obj.into()).map(MediaReference::Generator)
            }
            _ => Ok(MediaReference::Unknown(obj)),
        };
        r.map_err(de::Error::custom)
    }
}

/// `ExternalReference.1`: a file or URL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalReference {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "external_reference")]
    pub otio_schema: String,
    #[serde(default, deserialize_with = "name")]
    pub name: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub available_range: Option<TimeRange>,
    #[serde(default)]
    pub target_url: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ExternalReference {
    pub fn new(target_url: impl Into<String>, available_range: Option<TimeRange>) -> Self {
        Self {
            otio_schema: external_reference_v(),
            name: String::new(),
            metadata: Map::new(),
            available_range,
            target_url: target_url.into(),
            extra: Map::new(),
        }
    }
}

impl ExternalReference {
    /// A `file://` URL for an absolute path (`%`, space, `#` and `?` percent-encoded).
    pub fn from_path(path: &std::path::Path, available_range: Option<TimeRange>) -> Self {
        let mut url = String::from("file://");
        for c in path.to_string_lossy().chars() {
            match c {
                '%' => url.push_str("%25"),
                ' ' => url.push_str("%20"),
                '#' => url.push_str("%23"),
                '?' => url.push_str("%3F"),
                c => url.push(c),
            }
        }
        Self::new(url, available_range)
    }

    /// The local path of a `file://` URL (percent-decoded) or of a plain path.
    pub fn path(&self) -> Option<std::path::PathBuf> {
        let rest = match self.target_url.strip_prefix("file://") {
            Some(r) => r.strip_prefix("localhost").unwrap_or(r),
            None if self.target_url.contains("://") => return None,
            None => return Some(self.target_url.clone().into()),
        };
        let bytes = rest.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            let hex = |b: u8| char::from(b).to_digit(16);
            if bytes[i] == b'%'
                && i + 2 < bytes.len()
                && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
            {
                out.push(u8::try_from(h * 16 + l).unwrap_or(b'?'));
                i += 3;
                continue;
            }
            out.push(bytes[i]);
            i += 1;
        }
        Some(String::from_utf8_lossy(&out).into_owned().into())
    }
}

/// `MissingReference.1`: media that isn't known.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissingReference {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "missing_reference")]
    pub otio_schema: String,
    #[serde(default, deserialize_with = "name")]
    pub name: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub available_range: Option<TimeRange>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for MissingReference {
    fn default() -> Self {
        Self {
            otio_schema: missing_reference_v(),
            name: String::new(),
            metadata: Map::new(),
            available_range: None,
            extra: Map::new(),
        }
    }
}

/// `GeneratorReference.1`: synthesised media (`ffcine.text`, solids, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneratorReference {
    #[serde(rename = "OTIO_SCHEMA", deserialize_with = "generator_reference")]
    pub otio_schema: String,
    #[serde(default, deserialize_with = "name")]
    pub name: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub available_range: Option<TimeRange>,
    #[serde(default)]
    pub generator_kind: String,
    #[serde(default)]
    pub parameters: Map<String, Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl GeneratorReference {
    pub fn new(generator_kind: impl Into<String>, parameters: Map<String, Value>) -> Self {
        Self {
            otio_schema: generator_reference_v(),
            name: String::new(),
            metadata: Map::new(),
            available_range: None,
            generator_kind: generator_kind.into(),
            parameters,
            extra: Map::new(),
        }
    }
}

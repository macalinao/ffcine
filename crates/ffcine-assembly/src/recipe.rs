//! Content keys: a canonical JSON form of what a render unit is made of, hashed.
//!
//! A key covers everything that changes the pixels (media identity, source range, the resolved
//! grade and effects, the render profile, the ffcine version) and nothing about where the unit sits
//! on the timeline, so inserting or removing a clip leaves every other clip's key alone.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Bump when the render path changes its output for the same recipe (new effect applied, encoder
/// tweak, ...): every cached unit re-renders.
pub const RENDER_REVISION: u32 = 4;

/// Bump when the audio mix changes its output for the same recipe: the mix re-renders, the
/// picture doesn't.
pub const AUDIO_REVISION: u32 = 1;

/// A content key: 128 bits of SHA-256, hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Key(pub String);

impl Key {
    /// The key of a JSON value, in canonical form (object keys sorted).
    pub fn of(v: &Value) -> Self {
        let digest = Sha256::digest(canonical(v).as_bytes());
        Self(digest[..16].iter().fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        }))
    }

    /// The first 10 hex digits, for display.
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(10)]
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `v` as compact JSON with every object's keys sorted, so key order in the timeline file doesn't
/// matter.
pub fn canonical(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), sorted(&m[k])))
                        .collect::<Map<_, _>>(),
                )
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            v => v.clone(),
        }
    }
    sorted(v).to_string()
}

/// Identity of a media file: canonical path, size and modification time (no content read, so
/// fingerprinting a 15-minute 4K timeline is instant). A missing file fingerprints as missing,
/// so plans and diffs still work without the media.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_ns: Option<u128>,
}

/// Looks up file fingerprints; tests swap in a fake.
pub trait Fingerprinter {
    fn fingerprint(&self, path: &Path) -> Fingerprint;
    /// SHA-256 of a small file's bytes (LUTs, shaders), `None` if unreadable.
    fn content_hash(&self, path: &Path) -> Option<String>;
}

/// Fingerprints from the filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsFingerprinter;

impl Fingerprinter for FsFingerprinter {
    fn fingerprint(&self, path: &Path) -> Fingerprint {
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        let meta = std::fs::metadata(&canon).ok();
        Fingerprint {
            path: canon,
            size: meta.as_ref().map(std::fs::Metadata::len),
            mtime_ns: meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos()),
        }
    }

    fn content_hash(&self, path: &Path) -> Option<String> {
        let bytes = std::fs::read(path).ok()?;
        Some(Key::of(&Value::String(format!("{:x}", Sha256::digest(&bytes)))).0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_ignores_object_key_order() {
        let a = json!({"a": 1, "b": {"x": [1, 2], "y": null}});
        let b = json!({"b": {"y": null, "x": [1, 2]}, "a": 1});
        assert_eq!(Key::of(&a), Key::of(&b));
        assert_ne!(
            Key::of(&a),
            Key::of(&json!({"a": 2, "b": {"x": [1, 2], "y": null}}))
        );
        assert_eq!(Key::of(&a).0.len(), 32);
    }
}

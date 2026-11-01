//! Card offload and the clip catalog, with no GPU or libav dependency so it runs on the Mac in
//! the van: [`ingest()`] copies camera cards with BLAKE3 verification, a manifest and an
//! optional backup; [`clock`] applies per-camera clock/zone fixes and flags disagreeing clocks;
//! [`catalog`] indexes clips with format and picture profile (`docs/ingest.md`).

pub mod camera;
pub mod catalog;
pub mod clock;
pub mod ingest;
pub mod stats;
pub mod time;

use std::path::{Path, PathBuf};

pub use catalog::{Catalog, CatalogOptions, Clip, Histogram};
pub use clock::{ClockConfig, ClockIssue};
pub use ingest::{Event, IngestOptions, IngestReport, ingest};
pub use time::{Shift, WallTime, Zone};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: copy does not match the card (blake3 {want}, copy has {got})")]
    Verify {
        path: PathBuf,
        want: String,
        got: String,
    },
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Core(#[from] ffcine_core::Error),
}

impl Error {
    pub(crate) fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().to_owned(),
            source,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Now, UTC.
pub fn now_utc() -> WallTime {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    WallTime(i64::try_from(s).unwrap_or_default())
}

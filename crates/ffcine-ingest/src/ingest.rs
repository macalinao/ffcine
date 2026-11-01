//! Card offload: copy every camera file into `DEST/01_footage/<camera>/<day>/` (audio into
//! `02_audio/`), hashing the card's bytes as they stream, re-reading the copy to verify it, and
//! appending each verified file to `DEST/.ffcine/ingest.jsonl`. A second verified copy goes to
//! `--backup` (same layout) from the destination, so the card is read once.
//!
//! Guarantees:
//! - The card is only ever opened for reading; nothing on it is renamed, written or deleted.
//! - A file is only final (renamed from `.part`) after its copy re-reads to the card's hash.
//! - Re-running is idempotent: files already in the manifest (same name, size and mtime, copy
//!   still present) are skipped; an interrupted copy restarts from scratch on the next run.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::camera::{self, Kind, Origin};
use crate::clock::ClockConfig;
use crate::{Error, Result};

const CHUNK: usize = 8 << 20;
/// Manifest, relative to the destination root.
pub const MANIFEST: &str = ".ffcine/ingest.jsonl";

#[derive(Debug, Clone, Default)]
pub struct IngestOptions {
    pub card: PathBuf,
    pub dest: PathBuf,
    pub backup: Option<PathBuf>,
    pub clocks: ClockConfig,
    /// Force a camera id for every file on this card (`--camera r10`).
    pub camera: Option<String>,
    /// Plan only: print what would be copied.
    pub dry_run: bool,
    /// Re-hash copies already in the manifest instead of trusting name, size and mtime.
    pub verify_existing: bool,
}

/// One manifest line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// `copy` (card -> dest) or `backup` (dest -> backup).
    pub event: String,
    /// Card path (copy) or destination path (backup), absolute.
    pub source: String,
    pub size: u64,
    /// Card file mtime, Unix seconds.
    pub mtime: i64,
    pub blake3: String,
    /// Relative to the destination root (and to the backup root).
    pub dest: String,
    pub camera: String,
    pub kind: Kind,
    /// Unix seconds when it was verified.
    pub at: i64,
}

/// Progress events, for the CLI.
#[derive(Debug, Clone)]
pub enum Event<'a> {
    Copied { rel: &'a str, bytes: u64 },
    Skipped { rel: &'a str },
    BackedUp { rel: &'a str },
    Failed { path: &'a Path, error: &'a Error },
}

#[derive(Debug, Default)]
pub struct IngestReport {
    pub copied: usize,
    pub skipped: usize,
    pub backed_up: usize,
    pub bytes: u64,
    /// Planned copies (dry run): card path -> destination relative path.
    pub planned: Vec<(PathBuf, String)>,
    /// Card files ingest doesn't copy (system files, unknown extensions).
    pub ignored: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
}

impl IngestReport {
    pub fn ok(&self) -> bool {
        self.failed.is_empty()
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or_default())
}

fn mtime_of(m: &fs::Metadata) -> (i64, Option<SystemTime>) {
    let t = m.modified().ok();
    let s = t
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or_default());
    (s, t)
}

/// All regular files under `dir`, sorted, skipping hidden entries (`.Trashes`, `._*`, ...).
pub fn walk(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).map_err(|e| Error::io(&d, e))? {
            let e = e.map_err(|e| Error::io(&d, e))?;
            let name = e.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let ft = e.file_type().map_err(|err| Error::io(e.path(), err))?;
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                out.push(e.path());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Reads the manifest (missing = empty). A torn last line (crash mid-write) is ignored.
pub fn read_manifest(dest: &Path) -> Result<Vec<Entry>> {
    let path = dest.join(MANIFEST);
    let f = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(&path, e)),
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = line.map_err(|e| Error::io(&path, e))?;
        if let Ok(e) = serde_json::from_str::<Entry>(&line) {
            out.push(e);
        }
    }
    Ok(out)
}

struct Manifest {
    file: Mutex<File>,
    path: PathBuf,
}

impl Manifest {
    fn open(dest: &Path) -> Result<Self> {
        let path = dest.join(MANIFEST);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).map_err(|e| Error::io(p, e))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| Error::io(&path, e))?;
        Ok(Self {
            file: Mutex::new(file),
            path,
        })
    }

    fn append(&self, e: &Entry) -> Result<()> {
        let mut line = serde_json::to_string(e)?;
        line.push('\n');
        let mut f = self
            .file
            .lock()
            .map_err(|_| Error::Config("manifest lock poisoned".into()))?;
        f.write_all(line.as_bytes())
            .and_then(|()| f.sync_data())
            .map_err(|e| Error::io(&self.path, e))
    }
}

/// Streams `src` to `dst.part`, hashing what was read; fsyncs; re-reads the copy and checks
/// its hash; renames into place. Returns the hash. `expect` (backup) must match the card's.
fn copy_verified(
    src: &Path,
    dst: &Path,
    expect: Option<&str>,
    mtime: Option<SystemTime>,
) -> Result<String> {
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p).map_err(|e| Error::io(p, e))?;
    }
    let part = part_path(dst);
    let mut last = None;
    for _attempt in 0..2 {
        let mut input = File::open(src).map_err(|e| Error::io(src, e))?;
        let mut out = File::create(&part).map_err(|e| Error::io(&part, e))?;
        let mut h = blake3::Hasher::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = input.read(&mut buf).map_err(|e| Error::io(src, e))?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(|e| Error::io(&part, e))?;
        }
        out.sync_all().map_err(|e| Error::io(&part, e))?;
        if let Some(t) = mtime {
            // Keep the camera's mtime (best effort; some filesystems refuse).
            let _ = out.set_modified(t);
        }
        drop(out);
        let read = h.finalize().to_hex().to_string();
        let copy = hash_file(&part)?;
        let want = expect.unwrap_or(&read);
        if copy == read && read == want {
            fs::rename(&part, dst).map_err(|e| Error::io(dst, e))?;
            return Ok(read);
        }
        last = Some(Error::Verify {
            path: dst.to_owned(),
            want: want.to_owned(),
            got: copy,
        });
        let _ = fs::remove_file(&part);
    }
    Err(last.unwrap_or_else(|| Error::Config("copy failed".into())))
}

fn part_path(dst: &Path) -> PathBuf {
    let mut s = dst.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// BLAKE3 of a file, hex.
pub fn hash_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().to_hex().to_string())
}

/// Where a card file goes, relative to the destination root.
pub fn dest_rel(kind: Kind, camera: &str, day: &str, name: &str) -> String {
    let top = if kind == Kind::Audio {
        "02_audio"
    } else {
        "01_footage"
    };
    format!("{top}/{camera}/{day}/{name}")
}

/// `stem_<hash8>.ext`, for a name collision with different content.
fn disambiguate(rel: &str, hash: &str) -> String {
    let p = Path::new(rel);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let name = format!("{stem}_{}{ext}", &hash[..8]);
    p.with_file_name(name).to_string_lossy().into_owned()
}

/// `a` (which may not exist yet) is `b` or under it. Resolves the nearest existing ancestor, so
/// it can run before anything is created.
fn inside(a: &Path, b: &Path) -> bool {
    let Ok(b) = b.canonicalize() else {
        return false;
    };
    let mut probe = a.to_owned();
    let mut rest = Vec::new();
    loop {
        if let Ok(c) = probe.canonicalize() {
            let full = rest.iter().rev().fold(c, |p: PathBuf, n| p.join(n));
            return full.starts_with(&b);
        }
        match (probe.file_name().map(ToOwned::to_owned), probe.parent()) {
            (Some(n), Some(p)) => {
                rest.push(n);
                probe = if p.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    p.to_owned()
                };
            }
            _ => return false,
        }
    }
}

/// Identifies every card file; sidecars (`.LRF`, `.SRT`, `.THM`) take their clip's origin so
/// they land next to it.
fn plan(
    opts: &IngestOptions,
    files: &[PathBuf],
    report: &mut IngestReport,
) -> Vec<(PathBuf, Kind, Origin)> {
    let mut primary: HashMap<(PathBuf, String), Origin> = HashMap::new();
    let mut out = Vec::new();
    let stem_key = |p: &Path| {
        (
            p.parent().map(Path::to_owned).unwrap_or_default(),
            p.file_stem()
                .map(|s| s.to_string_lossy().to_ascii_uppercase())
                .unwrap_or_default(),
        )
    };
    let mut later = Vec::new();
    for f in files {
        let Some(kind) = Kind::of(f) else {
            report.ignored.push(f.clone());
            continue;
        };
        if matches!(kind, Kind::Proxy | Kind::Sidecar) {
            later.push((f.clone(), kind));
            continue;
        }
        let meta = camera::read_meta(f, kind);
        let mut o = camera::origin(f, kind, &meta);
        if let Some(c) = &opts.camera {
            o.camera.clone_from(c);
        }
        if kind == Kind::Video {
            primary.insert(stem_key(f), o.clone());
        }
        out.push((f.clone(), kind, o));
    }
    for (f, kind) in later {
        let o = primary.get(&stem_key(&f)).cloned().unwrap_or_else(|| {
            let mut o = camera::origin(&f, kind, &camera::read_meta(&f, kind));
            if let Some(c) = &opts.camera {
                o.camera.clone_from(c);
            }
            o
        });
        out.push((f, kind, o));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Offloads a card. Per-file failures are collected in the report (the rest still copy); the
/// error return is for setup problems (unreadable card, destination inside the card).
#[allow(clippy::too_many_lines)]
pub fn ingest(opts: &IngestOptions, on_event: &(dyn Fn(Event<'_>) + Sync)) -> Result<IngestReport> {
    if !opts.card.is_dir() {
        return Err(Error::Config(format!(
            "{} is not a directory",
            opts.card.display()
        )));
    }
    let card = &opts
        .card
        .canonicalize()
        .map_err(|e| Error::io(&opts.card, e))?;
    for d in std::iter::once(&opts.dest).chain(opts.backup.as_ref()) {
        if inside(d, card) {
            return Err(Error::Config(format!(
                "{} is inside the card {}; refusing to write to a card",
                d.display(),
                card.display()
            )));
        }
    }
    if !opts.dry_run {
        fs::create_dir_all(&opts.dest).map_err(|e| Error::io(&opts.dest, e))?;
    }
    if let Some(b) = &opts.backup
        && !opts.dry_run
    {
        fs::create_dir_all(b).map_err(|e| Error::io(b, e))?;
    }
    let mut report = IngestReport::default();
    let files = walk(card)?;
    let planned = plan(opts, &files, &mut report);

    let existing = read_manifest(&opts.dest)?;
    let mut copied: HashMap<(String, u64, i64), Entry> = HashMap::new();
    let mut by_rel: BTreeMap<String, Entry> = BTreeMap::new();
    let mut backed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for e in existing {
        if e.event == "backup" {
            backed.insert(e.dest.clone());
        } else {
            let name = Path::new(&e.source)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            copied.insert((name, e.size, e.mtime), e.clone());
            by_rel.insert(e.dest.clone(), e);
        }
    }

    let manifest = if opts.dry_run {
        None
    } else {
        Some(Manifest::open(&opts.dest)?)
    };
    let (tx, rx) = mpsc::channel::<Entry>();
    let backup_failed: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());
    let backed_up = std::sync::atomic::AtomicUsize::new(0);

    std::thread::scope(|scope| -> Result<()> {
        // Backup worker: dest -> backup, verified against the card's hash.
        if let (Some(broot), Some(m)) = (&opts.backup, &manifest) {
            let (dest, bf, bu) = (&opts.dest, &backup_failed, &backed_up);
            scope.spawn(move || {
                for e in rx {
                    let from = dest.join(&e.dest);
                    let target = broot.join(&e.dest);
                    let res = if target.exists() && hash_file(&target).is_ok_and(|h| h == e.blake3)
                    {
                        Ok(e.blake3.clone())
                    } else {
                        let mt = fs::metadata(&from).ok().and_then(|m| m.modified().ok());
                        copy_verified(&from, &target, Some(&e.blake3), mt)
                    };
                    match res.and_then(|h| {
                        m.append(&Entry {
                            event: "backup".into(),
                            source: from.to_string_lossy().into_owned(),
                            blake3: h,
                            at: now(),
                            ..e.clone()
                        })
                    }) {
                        Ok(()) => {
                            bu.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            on_event(Event::BackedUp { rel: &e.dest });
                        }
                        Err(err) => {
                            on_event(Event::Failed {
                                path: &target,
                                error: &err,
                            });
                            if let Ok(mut v) = bf.lock() {
                                v.push((target.clone(), err.to_string()));
                            }
                        }
                    }
                }
            });
        }

        for (path, kind, origin) in &planned {
            let res = (|| -> Result<()> {
                let md = fs::metadata(path).map_err(|e| Error::io(path, e))?;
                let (mt, mt_sys) = mtime_of(&md);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let clock = opts.clocks.apply(origin);
                let mut rel = dest_rel(*kind, &origin.camera, &clock.day(), &name);

                let mut damaged = false;
                if let Some(e) = copied.get(&(name.clone(), md.len(), mt)) {
                    let dst = opts.dest.join(&e.dest);
                    let ok = fs::metadata(&dst).is_ok_and(|m| m.len() == e.size)
                        && (!opts.verify_existing || hash_file(&dst)? == e.blake3);
                    if ok {
                        report.skipped += 1;
                        on_event(Event::Skipped { rel: &e.dest });
                        if opts.backup.is_some() && !backed.contains(&e.dest) {
                            let _ = tx.send(e.clone());
                        }
                        return Ok(());
                    }
                    // Our copy went missing or bad: copy it again, in the same place.
                    rel.clone_from(&e.dest);
                    damaged = true;
                }
                if opts.dry_run {
                    report.planned.push((path.clone(), rel));
                    return Ok(());
                }
                let m = manifest
                    .as_ref()
                    .ok_or_else(|| Error::Config("no manifest".into()))?;
                let dst = opts.dest.join(&rel);
                let hash = if damaged {
                    copy_verified(path, &dst, None, mt_sys)?
                } else if dst.exists() {
                    // Not in the manifest under this card file: an earlier run died after the
                    // rename, or a different clip with the same name (Canon counter reset).
                    let have = hash_file(&dst)?;
                    let src = hash_file(path)?;
                    if have == src {
                        src
                    } else {
                        rel = disambiguate(&rel, &src);
                        let dst = opts.dest.join(&rel);
                        if dst.exists() && hash_file(&dst)? == src {
                            src
                        } else {
                            copy_verified(path, &dst, Some(&src), mt_sys)?
                        }
                    }
                } else {
                    copy_verified(path, &dst, None, mt_sys)?
                };
                let e = Entry {
                    event: "copy".into(),
                    source: path.to_string_lossy().into_owned(),
                    size: md.len(),
                    mtime: mt,
                    blake3: hash,
                    dest: rel.clone(),
                    camera: origin.camera.clone(),
                    kind: *kind,
                    at: now(),
                };
                m.append(&e)?;
                report.copied += 1;
                report.bytes += md.len();
                on_event(Event::Copied {
                    rel: &rel,
                    bytes: md.len(),
                });
                if opts.backup.is_some() {
                    let _ = tx.send(e);
                }
                Ok(())
            })();
            if let Err(err) = res {
                on_event(Event::Failed { path, error: &err });
                report.failed.push((path.clone(), err.to_string()));
            }
        }
        drop(tx);
        Ok(())
    })?;
    report.backed_up = backed_up.into_inner();
    report
        .failed
        .extend(backup_failed.into_inner().unwrap_or_default());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_collisions() {
        assert_eq!(
            dest_rel(Kind::Video, "pocket", "2026-12-01", "DJI_1.MP4"),
            "01_footage/pocket/2026-12-01/DJI_1.MP4"
        );
        assert_eq!(
            dest_rel(Kind::Audio, "h2e", "2026-12-01", "ZOOM0001.WAV"),
            "02_audio/h2e/2026-12-01/ZOOM0001.WAV"
        );
        assert_eq!(
            disambiguate("01_footage/r10/2026-12-01/MVI_0001.MP4", "abcdef0123"),
            "01_footage/r10/2026-12-01/MVI_0001_abcdef01.MP4"
        );
    }
}

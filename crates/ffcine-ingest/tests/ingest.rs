//! End-to-end ingest on synthetic cards: layout, verification, idempotency, resume, collisions,
//! backup, and that the card is never modified.

#![allow(
    clippy::unwrap_used,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::case_sensitive_file_extension_comparisons
)]

use std::fs;
use std::path::{Path, PathBuf};

use ffcine_ingest::catalog::{self, CatalogOptions, Histogram};
use ffcine_ingest::ingest::{self, MANIFEST, hash_file, read_manifest, walk};
use ffcine_ingest::{ClockConfig, IngestOptions, Zone};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffcine-ingest-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn write(p: &Path, bytes: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

/// A 48 kHz stereo 16-bit WAV with a `bext` stamp.
fn wav(originator: &str, date: &str, time: &str) -> Vec<u8> {
    let mut fmt = Vec::new();
    for v in [1u16, 2] {
        fmt.extend_from_slice(&v.to_le_bytes());
    }
    fmt.extend_from_slice(&48_000u32.to_le_bytes());
    fmt.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
    for v in [4u16, 16] {
        fmt.extend_from_slice(&v.to_le_bytes());
    }
    let mut bext = vec![0u8; 602];
    bext[256..256 + originator.len()].copy_from_slice(originator.as_bytes());
    bext[320..330].copy_from_slice(date.as_bytes());
    bext[330..338].copy_from_slice(time.as_bytes());
    let data = vec![1u8; 48_000 * 4];
    let mut body = b"WAVE".to_vec();
    for (id, c) in [(b"fmt ", &fmt), (b"bext", &bext), (b"data", &data)] {
        body.extend_from_slice(id);
        body.extend_from_slice(&u32::try_from(c.len()).unwrap().to_le_bytes());
        body.extend_from_slice(c);
    }
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    v.extend_from_slice(&body);
    v
}

/// A card with DJI clips (+ LRF proxy), a Canon clip, an H2essential WAV and junk.
fn card(root: &Path, canon_bytes: &[u8]) {
    let dji = root.join("DCIM/DJI_001");
    write(
        &dji.join("DJI_20261201101500_0001_D.MP4"),
        &vec![7u8; 300_000],
    );
    write(&dji.join("DJI_20261201101500_0001_D.LRF"), &[8u8; 3_000]);
    write(&dji.join("DJI_20261202093000_0002_D.MP4"), &[9u8; 1000]);
    write(&root.join("DCIM/100CANON/MVI_0001.MP4"), canon_bytes);
    write(
        &root.join("STEREO/FOLDER01/ZOOM0001.WAV"),
        &wav("ZOOM H2essential", "2026-12-01", "23:59:30"),
    );
    write(&root.join("MISC/junk.bin"), b"x");
    write(&root.join(".Trashes/x"), b"x");
}

fn snapshot(dir: &Path) -> Vec<(PathBuf, String)> {
    walk(dir)
        .unwrap()
        .into_iter()
        .map(|p| {
            let h = hash_file(&p).unwrap();
            (p, h)
        })
        .collect()
}

fn opts(card: &Path, dest: &Path, backup: Option<&Path>) -> IngestOptions {
    let mut clocks = ClockConfig {
        zone: Some(Zone::UTC),
        ..ClockConfig::default()
    };
    clocks.set("h2e=+1m").unwrap();
    IngestOptions {
        card: card.to_owned(),
        dest: dest.to_owned(),
        backup: backup.map(Path::to_owned),
        clocks,
        ..IngestOptions::default()
    }
}

#[test]
fn offload_verify_resume_backup() {
    let t = tmp("e2e");
    let (c, dest, bak) = (t.join("card"), t.join("proj"), t.join("store"));
    card(&c, &[1u8; 5000]);
    let before = snapshot(&c);

    let r = ingest::ingest(&opts(&c, &dest, Some(&bak)), &|_| {}).unwrap();
    assert!(r.ok(), "{:?}", r.failed);
    assert_eq!((r.copied, r.skipped, r.backed_up), (5, 0, 5));
    assert_eq!(r.ignored.len(), 1); // MISC/junk.bin; hidden dirs are skipped
    // No metadata in the fake Canon clip: dated by mtime (now).
    let canon_rel = format!(
        "01_footage/canon/{}/MVI_0001.MP4",
        ffcine_ingest::now_utc().date()
    );
    for rel in [
        "01_footage/dji/2026-12-01/DJI_20261201101500_0001_D.MP4",
        "01_footage/dji/2026-12-01/DJI_20261201101500_0001_D.LRF",
        "01_footage/dji/2026-12-02/DJI_20261202093000_0002_D.MP4",
        &canon_rel,
        // 23:59:30 + 1 min shift: the next day.
        "02_audio/h2e/2026-12-02/ZOOM0001.WAV",
    ] {
        assert!(dest.join(rel).is_file(), "{rel}");
        assert_eq!(
            hash_file(&dest.join(rel)).unwrap(),
            hash_file(&bak.join(rel)).unwrap()
        );
    }
    assert!(
        walk(&dest)
            .unwrap()
            .iter()
            .all(|p| !p.to_string_lossy().ends_with(".part"))
    );
    let m = read_manifest(&dest).unwrap();
    assert_eq!(m.iter().filter(|e| e.event == "copy").count(), 5);
    assert!(m.iter().all(|e| Path::new(&e.source).is_absolute()));

    // Idempotent.
    let r = ingest::ingest(&opts(&c, &dest, Some(&bak)), &|_| {}).unwrap();
    assert_eq!((r.copied, r.skipped, r.backed_up), (0, 5, 0));

    // A lost copy and a same-size corrupted one: re-copied in place (the latter only when
    // asked to re-verify), never renamed.
    let lost = dest.join("01_footage/dji/2026-12-02/DJI_20261202093000_0002_D.MP4");
    fs::remove_file(&lost).unwrap();
    let bad = dest.join("01_footage/dji/2026-12-01/DJI_20261201101500_0001_D.MP4");
    fs::write(&bad, vec![0u8; 300_000]).unwrap();
    // A torn manifest line from a crash.
    let mut text = fs::read_to_string(dest.join(MANIFEST)).unwrap();
    text.push_str("{\"event\":\"co");
    fs::write(dest.join(MANIFEST), text).unwrap();
    let r = ingest::ingest(&opts(&c, &dest, None), &|_| {}).unwrap();
    assert_eq!((r.copied, r.skipped), (1, 4));
    let mut o = opts(&c, &dest, None);
    o.verify_existing = true;
    let r = ingest::ingest(&o, &|_| {}).unwrap();
    assert_eq!((r.copied, r.skipped), (1, 4));
    assert_eq!(
        hash_file(&bad).unwrap(),
        hash_file(&c.join("DCIM/DJI_001/DJI_20261201101500_0001_D.MP4")).unwrap()
    );
    assert!(
        !dest
            .join("01_footage/dji/2026-12-01")
            .read_dir()
            .unwrap()
            .any(|e| {
                e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("_0001_D_")
            })
    );

    // A second card whose Canon counter reset: same name, different clip -> kept side by side.
    let c2 = t.join("card2");
    card(&c2, &[2u8; 6000]);
    let r = ingest::ingest(&opts(&c2, &dest, None), &|_| {}).unwrap();
    assert!(r.ok());
    let canon: Vec<_> = fs::read_dir(dest.join(&canon_rel).parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(canon.len(), 2, "{canon:?}");

    // The cards were never touched.
    assert_eq!(snapshot(&c), before);

    // Catalog over the project: WAV from bext, proxy linked, h2e shift applied.
    let cat = catalog::build(
        &dest,
        &CatalogOptions {
            clocks: opts(&c, &dest, None).clocks,
            histogram: Histogram::Never,
            full: false,
        },
        ffcine_ingest::now_utc(),
    )
    .unwrap();
    let wav = cat
        .clips
        .iter()
        .find(|c| c.path.ends_with("ZOOM0001.WAV"))
        .unwrap();
    assert_eq!(wav.camera, "h2e");
    assert_eq!(wav.clock.local.unwrap().to_string(), "2026-12-02T00:00:30");
    assert!((wav.duration.unwrap() - 1.0).abs() < 1e-9);
    let dji = cat
        .clips
        .iter()
        .find(|c| c.path.ends_with("0001_D.MP4"))
        .unwrap();
    assert!(dji.proxy.as_deref().unwrap().ends_with("0001_D.LRF"));
    assert!(!cat.clips.iter().any(|c| c.path.ends_with(".LRF")));
    let p = catalog::write(&dest, &cat).unwrap();
    assert_eq!(catalog::read(&dest).unwrap().clips.len(), cat.clips.len());
    assert!(p.ends_with("catalog.json"));
    fs::remove_dir_all(&t).unwrap();
}

#[test]
fn refuses_to_write_into_the_card() {
    let t = tmp("inside");
    let c = t.join("card");
    card(&c, b"canon");
    let err = ingest::ingest(&opts(&c, &c.join("proj"), None), &|_| {}).unwrap_err();
    assert!(err.to_string().contains("inside the card"), "{err}");
    assert!(!c.join("proj").exists(), "created a directory on the card");
    fs::remove_dir_all(&t).unwrap();
}

#[test]
fn dry_run_copies_nothing() {
    let t = tmp("dry");
    let (c, dest) = (t.join("card"), t.join("proj"));
    card(&c, b"canon");
    let mut o = opts(&c, &dest, None);
    o.dry_run = true;
    let r = ingest::ingest(&o, &|_| {}).unwrap();
    assert_eq!((r.copied, r.planned.len()), (0, 5));
    assert!(!dest.exists());
    fs::remove_dir_all(&t).unwrap();
}

/// `FFCINE_SAMPLE_DIR=/media/footage/trip cargo test -p ffcine-ingest
/// --test ingest real_dir -- --nocapture`: catalogs real footage (read-only) and prints a summary.
#[test]
fn real_dir_catalog() {
    let Some(dir) = std::env::var_os("FFCINE_SAMPLE_DIR") else {
        return;
    };
    let cat = catalog::build(
        Path::new(&dir),
        &CatalogOptions {
            histogram: Histogram::Never,
            full: true,
            ..CatalogOptions::default()
        },
        ffcine_ingest::now_utc(),
    )
    .unwrap();
    println!("{}", serde_json::to_string_pretty(&cat.cameras).unwrap());
    for w in cat.warnings.iter().take(10) {
        println!("warning: {w}");
    }
    for i in &cat.clock_issues {
        println!("clock: {} {}", i.camera, i.message);
    }
    let unprobed = cat
        .clips
        .iter()
        .filter(|c| c.kind == ffcine_ingest::camera::Kind::Video && c.video.is_none())
        .count();
    assert_eq!(unprobed, 0, "video files without a parsed video track");
}

//! `edit.yaml` -> `edit.otio` -> render plan: every setting the render reads (mix included)
//! lives in the timeline, so the keys survive the file round trip and a re-import.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use ffcine_assembly::{FsFingerprinter, Plan, Profile, plan};
use ffcine_import::{self as import, ImportOptions};
use ffcine_timeline::Timeline;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../ffcine-import/tests/data/edit_yaml/edit.yaml")
}

fn import_fixture() -> Timeline {
    import::import(&fixture(), None, &ImportOptions::default())
        .expect("import")
        .timeline
}

fn plan_of(t: &Timeline, dir: &Path) -> Plan {
    plan(t, dir, &Profile::preview(), &FsFingerprinter, None).expect("plan")
}

fn keys(p: &Plan) -> (Vec<String>, Option<String>, String) {
    (
        p.segments.iter().map(|s| s.key.0.clone()).collect(),
        p.audio.as_ref().map(|a| a.key.0.clone()),
        p.master_key().0,
    )
}

#[test]
fn edit_yaml_to_otio_to_render_keeps_keys() {
    let dir = fixture().parent().unwrap().to_owned();
    let t = import_fixture();
    let mix = &t.project().unwrap().unwrap().mix;
    assert_eq!(
        (mix.lufs, mix.true_peak, mix.duck_db, mix.duck_release),
        (Some(-14.0), Some(-1.0), Some(-12.0), Some(0.5))
    );
    assert_eq!((mix.clean, mix.xfade), (Some(0.5), Some(0.125)));

    let direct = plan_of(&t, &dir);
    let audio = direct.audio.as_ref().expect("the fixture has audio");
    assert_eq!(audio.options.defaults.duck_db, -12.0);
    assert_eq!(audio.options.defaults.clean, 0.5);
    assert!((audio.options.duck_release - 0.5).abs() < 1e-12);

    let otio = std::env::temp_dir().join(format!("ffcine-keys-{}", std::process::id()));
    std::fs::create_dir_all(&otio).unwrap();
    let path = otio.join("edit.otio");
    t.to_file(&path).unwrap();
    let back = Timeline::from_file(&path).unwrap();
    assert_eq!(keys(&direct), keys(&plan_of(&back, &otio)));
    assert_eq!(keys(&direct), keys(&plan_of(&import_fixture(), &dir)));
    let _ = std::fs::remove_dir_all(otio);
}

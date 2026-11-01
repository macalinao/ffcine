//! The timeline rule: everything a render needs lives in the OTIO. `edit.yaml`
//! imported, written to OTIO JSON and read back plans exactly the same segments, keys and
//! layers as the imported timeline itself.

use std::path::Path;

use ffcine::assembly::{self, FsFingerprinter, Profile, Unit};
use ffcine::import::{self, ImportOptions};
use ffcine::timeline::Timeline;

#[allow(clippy::unnecessary_wraps)] // the importer hook's signature
fn beats(_: &Path) -> Option<Vec<f64>> {
    Some((0..400).map(|i| f64::from(i) * 0.5).collect())
}

#[test]
fn edit_yaml_round_trips_to_the_same_keys() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ffcine-import/tests/data/edit_yaml");
    for file in ["effects.yaml", "edit.yaml"] {
        let opts = ImportOptions {
            beats: Some(beats),
            ..ImportOptions::default()
        };
        let imported = import::import(&dir.join(file), None, &opts).expect("import");
        let json = imported.timeline.to_json().expect("to json");
        let read = Timeline::from_json(&json).expect("from json");
        for profile in [Profile::preview(), Profile::master()] {
            let a = assembly::plan(&imported.timeline, &dir, &profile, &FsFingerprinter, None)
                .expect("plan");
            let b = assembly::plan(&read, &dir, &profile, &FsFingerprinter, None).expect("plan");
            assert_eq!(a.master_key(), b.master_key(), "{file}");
            assert_eq!(a.segments, b.segments, "{file}");
            assert_eq!(a.grain, b.grain, "{file}");
            if file == "effects.yaml" {
                // The fixture's features all reach the plan.
                assert!(a.segments.iter().any(|s| matches!(s.unit, Unit::Split(_))));
                assert!(a.segments.iter().any(|s| !s.layers.is_empty()));
                assert!(a.segments.iter().any(|s| matches!(
                    &s.unit,
                    Unit::Transition { kind, .. } if *kind != assembly::comp::TransitionKind::Dissolve
                )));
                assert!(a.grain.is_some());
            }
        }
    }
}

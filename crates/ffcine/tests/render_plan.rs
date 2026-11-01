//! `edit.yaml` -> OTIO -> `ffcine render` plan: every per-clip filter and the output aspect come
//! through the OTIO timeline, and the timeline read back from its JSON plans the same cache keys.

use std::path::Path;

use ffcine::assembly::{self, Profile, Unit};
use ffcine::import::{self, ImportOptions};
use ffcine::timeline::{Timeline, project::Project};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../ffcine-import/tests/data/edit_yaml/filters.yaml"
);

fn plan(t: &Timeline, aspect: Option<&str>) -> assembly::Plan {
    let output = t
        .project()
        .transpose()
        .expect("ok")
        .unwrap_or_default()
        .output;
    let profile = Profile::resolve("preview", &output, aspect).expect("ok");
    assembly::plan(
        t,
        Path::new(FIXTURE).parent().expect("ok"),
        &profile,
        &assembly::FsFingerprinter,
        None,
    )
    .expect("ok")
}

fn shot<'a>(p: &'a assembly::Plan, name: &str) -> &'a assembly::Shot {
    p.segments
        .iter()
        .find_map(|s| match &s.unit {
            Unit::Shot(x) if x.name == name => Some(x),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no shot {name}"))
}

#[test]
fn edit_yaml_to_otio_to_render_keeps_keys() {
    let t = import::import(Path::new(FIXTURE), None, &ImportOptions::default())
        .expect("import")
        .timeline;
    let back = Timeline::from_json(&t.to_json().expect("ok")).expect("ok");
    let (a, b) = (plan(&t, None), plan(&back, None));
    let keys = |p: &assembly::Plan| p.segments.iter().map(|s| s.key.clone()).collect::<Vec<_>>();
    assert_eq!(keys(&a), keys(&b));
    assert_eq!(a.master_key(), b.master_key());

    // The aspect and punch cap live in project.output.
    let project: Project = t.project().expect("ok").expect("ok");
    assert_eq!(project.output.aspect.as_deref(), Some("reel"));
    assert_eq!((a.profile.size.width, a.profile.size.height), (1080, 1920));
    assert!((a.profile.aspect.max_punch - 1.12).abs() < 1e-9);

    // Every filter reached the plan.
    assert_eq!(
        shot(&a, "shaky").filters.stabilize.expect("ok").smoothing,
        30
    );
    assert_eq!(
        shot(&a, "walk").filters.stabilize.expect("ok").smoothing,
        20
    );
    assert!(
        shot(&a, "walk")
            .filters
            .reframe
            .as_ref()
            .is_some_and(|r| r.punch.len() == 1)
    );
    assert_eq!(shot(&a, "dark").grade.exposure, Some(0.5));
    assert_eq!(
        shot(&a, "dark")
            .filters
            .denoise
            .and_then(|d| d.temporal)
            .map(|t| t.strength),
        Some(3.0)
    );
    let ramp = shot(&a, "ramp");
    assert!(ramp.filters.ramp.is_some());
    assert_eq!(
        ramp.filters
            .denoise
            .and_then(|d| d.temporal)
            .map(|t| t.radius),
        Some(3)
    );
    assert_eq!(shot(&a, "still").filters, assembly::Filters::default());

    // The CLI aspect overrides the timeline's: every key changes, nothing else does.
    let l = plan(&t, Some("landscape"));
    assert_eq!((l.profile.size.width, l.profile.size.height), (1920, 1080));
    assert_eq!(assembly::Diff::new(&a, &l).render_count(), a.segments.len());
}

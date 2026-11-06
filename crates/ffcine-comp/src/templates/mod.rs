//! Templates (`docs/compositing.md#templates`): functions from a few parameters to an ordinary
//! comp document, run when a comp file is read. A comp file names one as `"template": {"story":
//! {...}}` or `"template": {"layout": {...}, "panels": [...], ...}`; [`expand`] writes it out
//! (`ffcine comp lower` prints the result), so templates add no render path and the plan keys
//! only see the lowered layers.

pub mod layout;
pub mod story;

use std::collections::HashSet;

use ffcine_core::Result;
use serde_json::{Map, Value};

use crate::model::err;

/// The template names, for errors and `--help`.
pub const NAMES: &[&str] = &["layout", "story"];

/// A comp document with its `template` written out: the template's canvas, vars, comps and
/// layers, with the document's `schema` and `project` kept. A document without `template` is
/// returned as is.
///
/// # Errors
/// An unknown template or bad parameters, or a document mixing `template` with `canvas`,
/// `vars`, `comps` or `layers`.
pub fn expand(doc: &Value) -> Result<Value> {
    let Some(m) = doc.as_object() else {
        return Ok(doc.clone());
    };
    let Some(t) = m.get("template") else {
        return Ok(doc.clone());
    };
    for k in ["canvas", "vars", "comps", "layers"] {
        if m.contains_key(k) {
            return Err(err(
                k,
                "a comp with a `template` gets its canvas and layers from it",
            ));
        }
    }
    let t = t
        .as_object()
        .ok_or_else(|| err("template", format!("expected an object, got {t}")))?;
    let mut out = if let Some(s) = t.get("story") {
        if t.len() != 1 {
            return Err(err("template", "`story` takes its parameters inside it"));
        }
        let s = s
            .as_object()
            .ok_or_else(|| err("template.story", "expected an object"))?;
        story::expand(s)?
    } else if t.contains_key("layout") {
        layout::expand(t)?
    } else {
        return Err(err(
            "template",
            format!("expected one of {}", NAMES.join(", ")),
        ));
    };
    let Some(o) = out.as_object_mut() else {
        return Err(err("template", "expanded to a non-object"));
    };
    let mut doc = Map::new();
    doc.insert(
        "schema".into(),
        m.get("schema")
            .cloned()
            .unwrap_or_else(|| format!("{}/1", crate::SCHEMA).into()),
    );
    if let Some(p) = m.get("project") {
        doc.insert("project".into(), p.clone());
    }
    doc.append(o);
    for (k, v) in m {
        if k.starts_with("x_") {
            doc.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Object(doc))
}

/// A number as an expression literal: the shortest form that reads back the same (`1`, `0.4`).
pub(crate) fn num(x: f64) -> String {
    format!("{x}")
}

/// A number as JSON, whole numbers as integers (`24`, not `24.0`).
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn jnum(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        Value::from(x as i64)
    } else {
        Value::from(x)
    }
}

/// `[w, h]` in px.
pub(crate) fn size(v: Option<&Value>, path: &str) -> Result<[u32; 2]> {
    let a = v
        .and_then(Value::as_array)
        .filter(|a| a.len() == 2)
        .ok_or_else(|| err(path, "expected [width, height] in px"))?;
    let px = |v: &Value| {
        v.as_u64()
            .and_then(|x| u32::try_from(x).ok())
            .filter(|x| *x > 0)
            .ok_or_else(|| err(path, format!("{v}: expected a positive whole number")))
    };
    Ok([px(&a[0])?, px(&a[1])?])
}

/// An item as a layer: `{"video": PATH, ...}` (or `still`, `solid`, `comp`, `text`) moves the
/// source key into `source`; an object with `source` is a layer already.
pub(crate) fn item_layer(v: &Value, path: &str) -> Result<Map<String, Value>> {
    let m = v
        .as_object()
        .ok_or_else(|| err(path, format!("expected an object, got {v}")))?;
    if m.contains_key("source") {
        return Ok(m.clone());
    }
    let kinds = ["video", "still", "solid", "comp", "text"];
    let found: Vec<&str> = kinds.into_iter().filter(|k| m.contains_key(*k)).collect();
    let [k] = found[..] else {
        return Err(err(
            path,
            "expected one of `video`, `still`, `solid`, `comp`, `text` (or a `source`)",
        ));
    };
    let mut out = Map::new();
    for (key, x) in m {
        if key == k {
            let mut s = Map::new();
            s.insert(k.into(), x.clone());
            out.insert("source".into(), Value::Object(s));
        } else {
            out.insert(key.clone(), x.clone());
        }
    }
    Ok(out)
}

/// Layer ids, unique: a wanted id, or a fallback, with `_2`, `_3`, ... on repeats.
#[derive(Default)]
pub(crate) struct Ids(HashSet<String>);

impl Ids {
    pub(crate) fn take(&mut self, want: Option<&str>, fallback: &str) -> String {
        let base = want.filter(|s| !s.is_empty()).unwrap_or(fallback);
        let mut id = base.to_owned();
        let mut j = 2;
        while !self.0.insert(id.clone()) {
            id = format!("{base}_{j}");
            j += 1;
        }
        id
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::{Value, json};

    use super::expand;
    use crate::Comp;

    const STORY: &str = include_str!("../../tests/data/story.comp.json");
    const LOWERED: &str = include_str!("../../tests/data/story.lowered.json");

    /// Every number as an `f64`, so `1` and `1.0` compare equal.
    fn norm(v: &Value) -> Value {
        match v {
            Value::Number(n) => json!(n.as_f64().expect("finite")),
            Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
            Value::Object(m) => {
                Value::Object(m.iter().map(|(k, v)| (k.clone(), norm(v))).collect())
            }
            v => v.clone(),
        }
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).expect("json")
    }

    /// The first JSON code block after `heading` in `doc`.
    fn doc_block(doc: &str, heading: &str) -> Value {
        let at = doc.find(heading).expect("heading");
        let a = doc[at..].find("```json\n").expect("block") + at + 8;
        let b = doc[a..].find("```").expect("end") + a;
        parse(&doc[a..b])
    }

    #[test]
    fn story_lowers_to_the_worked_example() {
        let got = expand(&parse(STORY)).expect("expand");
        assert_eq!(norm(&got), norm(&parse(LOWERED)), "{got:#}");
    }

    #[test]
    fn golden_files_are_the_docs_examples() {
        // Read at run time: the Nix build's source has the crates but not `docs/`.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/compositing.md");
        let Ok(doc) = std::fs::read_to_string(&path) else {
            eprintln!("skipped: no {}", path.display());
            return;
        };
        assert_eq!(doc_block(&doc, "## Templates"), parse(STORY));
        assert_eq!(
            doc_block(&doc, "### Worked example: the story"),
            parse(LOWERED)
        );
    }

    #[test]
    fn story_and_its_lowering_read_as_the_same_comp() {
        let dir = Path::new("media");
        let a = Comp::from_value(&parse(STORY), dir).expect("template");
        let b = Comp::from_value(&parse(LOWERED), dir).expect("lowered");
        // `Vars::visible` is a hash map: its Debug order varies, and it follows from `entries`.
        let shown = |c: &Comp| {
            let s = format!("{c:?}");
            let mut out = String::new();
            let mut rest = s.as_str();
            while let Some(i) = rest.find("visible: {") {
                out.push_str(&rest[..i]);
                rest = &rest[i..];
                rest = &rest[rest.find('}').map_or(rest.len(), |j| j + 1)..];
            }
            out + rest
        };
        assert_eq!(shown(&a), shown(&b));
    }

    #[test]
    fn templates_reject_mixing_and_unknowns() {
        let e = |v: Value| expand(&v).expect_err("error").to_string();
        assert!(e(json!({"template": {"story": {}}, "layers": []})).contains("layers"));
        assert!(e(json!({"template": {"grid": {}}})).contains("layout, story"));
        let base = json!({"size": [1080, 1920], "duration": 2.0,
                          "panels": [[{"video": "a.mov"}, {"still": "b.png"}, {"still": "c.png"}]]});
        let mut s = base.clone();
        assert!(e(json!({"template": {"story": s.clone()}})).contains("one or two"));
        s["panels"] = json!([[{"video": "a.mov"}, {"still": "b.png"}]]);
        assert!(e(json!({"template": {"story": s.clone()}})).contains("swap"));
        s["bogus"] = json!(1);
        assert!(e(json!({"template": {"story": s}})).contains("template.story.bogus"));
    }

    #[test]
    fn story_without_swap_or_exit_holds_still() {
        let v = expand(&json!({"template": {"story": {
            "size": [1080, 1920], "duration": 3.0, "tiles": 2, "gutter": 10,
            "panels": [[{"still": "a.png"}], [{"video": "b.mov"}]]
        }}}))
        .expect("expand");
        assert!(v.get("vars").is_none());
        assert_eq!(v["canvas"]["background"], "black");
        assert_eq!(v["layers"][1]["transform"]["position"], json!([0, 965]));
        assert_eq!(v["layers"][1]["size"], json!([1080, 955]));
        let tile = &v["comps"]["tile0"]["layers"][0];
        assert_eq!(tile["transform"]["position"], json!([0, 0]));
        assert!(tile.get("content").is_none());
        Comp::from_value(&v, Path::new(".")).expect("reads");
    }

    #[test]
    fn starter_deals_media_round_the_tiles() {
        let media: Vec<String> = ["a.mp4", "b.JPG", "c.mov", "d.heic"]
            .map(String::from)
            .to_vec();
        let doc = super::story::starter(&media, 3, 6.0).expect("starter");
        let s = &doc["template"]["story"];
        assert_eq!(
            s["panels"],
            json!([[{"video": "a.mp4"}, {"still": "d.heic"}], [{"still": "b.JPG"}], [{"video": "c.mov"}]])
        );
        assert_eq!(s["swap"]["at"], json!(2.4));
        assert_eq!(s["exit"]["at"], json!(5.2));
        Comp::from_value(&doc, Path::new("media")).expect("a valid comp");
        // No second round, no swap; more than two rounds is an error.
        let two = super::story::starter(&media[..2], 2, 6.0).expect("starter");
        assert!(two["template"]["story"].get("swap").is_none());
        assert!(super::story::starter(&media, 1, 6.0).is_err());
        assert!(super::story::starter(&[], 3, 6.0).is_err());
    }

    #[test]
    fn exit_with_its_own_timing_gets_its_own_vars() {
        let v = expand(&json!({"template": {"story": {
            "size": [1080, 1920], "duration": 4.0,
            "swap": {"at": 1.0, "stagger": 0.1, "slide": 0.3},
            "exit": {"at": 3.0, "stagger": 0.2, "slide": 0.3, "direction": "right"},
            "panels": [[{"still": "a.png", "punch": {"at": 0.5, "zoom": 1.2}}, {"still": "b.png"}]],
            "ken_burns": [1.0, 1.1]
        }}}))
        .expect("expand");
        assert_eq!(v["vars"]["exit_step"], json!(0.2));
        assert!(v["vars"].get("exit_slide").is_none());
        assert_eq!(
            v["layers"][0]["transform"]["position"][0],
            "if(lt(t, exit + i * exit_step), 0, W * ease_in((t - exit - i * exit_step) / slide))"
        );
        let zoom = &v["comps"]["tile0"]["layers"][0]["content"]["zoom"];
        assert_eq!(zoom["expr"], "value * lerp(1, 1.1, lt / dur)");
        Comp::from_value(&v, Path::new(".")).expect("reads");
    }
}

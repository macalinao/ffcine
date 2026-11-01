//! Shared pieces of the FCPXML and MLT readers: an owned XML element tree (so unmapped elements
//! can be kept verbatim), the report collector and stable clip ids.

#![allow(clippy::many_single_char_names)] // maths on x, y, w, h, t, v

use std::collections::HashSet;

use serde_json::{Map, Value};

use super::ImportReport;

/// An XML element, owned. Namespaces are dropped (neither format uses them).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    /// Direct text content, concatenated and trimmed.
    pub text: String,
}

impl Element {
    /// Parses a whole document and returns its root element. DTDs are allowed (FCPXML files
    /// start with `<!DOCTYPE fcpxml>`).
    pub fn parse(src: &str) -> Result<Self, roxmltree::Error> {
        let opts = roxmltree::ParsingOptions {
            allow_dtd: true,
            ..roxmltree::ParsingOptions::default()
        };
        let doc = roxmltree::Document::parse_with_options(src, opts)?;
        Ok(Self::from_node(doc.root_element()))
    }

    fn from_node(node: roxmltree::Node<'_, '_>) -> Self {
        let mut text = String::new();
        let mut children = Vec::new();
        for c in node.children() {
            if c.is_element() {
                children.push(Self::from_node(c));
            } else if c.is_text() {
                text.push_str(c.text().unwrap_or_default());
            }
        }
        Self {
            name: node.tag_name().name().to_owned(),
            attrs: node
                .attributes()
                .map(|a| (a.name().to_owned(), a.value().to_owned()))
                .collect(),
            children,
            text: text.trim().to_owned(),
        }
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// JSON form kept under `ffcine.foreign.<app>`:
    /// `{"name", "attrs": {..}, "children": [..], "text"}`, empty parts left out.
    pub fn to_json(&self) -> Value {
        self.to_json_except(&[])
    }

    /// [`Self::to_json`] without the attributes in `skip` (the ones that were mapped).
    pub fn to_json_except(&self, skip: &[&str]) -> Value {
        let mut o = Map::new();
        o.insert("name".into(), self.name.clone().into());
        let attrs: Map<String, Value> = self
            .attrs
            .iter()
            .filter(|(k, _)| !skip.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone().into()))
            .collect();
        if !attrs.is_empty() {
            o.insert("attrs".into(), attrs.into());
        }
        if !self.children.is_empty() {
            let c: Vec<Value> = self.children.iter().map(Element::to_json).collect();
            o.insert("children".into(), c.into());
        }
        if !self.text.is_empty() {
            o.insert("text".into(), self.text.clone().into());
        }
        Value::Object(o)
    }

    /// The attributes not in `skip`, as a JSON object (`None` if there are none).
    pub fn attrs_except(&self, skip: &[&str]) -> Option<Value> {
        let m: Map<String, Value> = self
            .attrs
            .iter()
            .filter(|(k, _)| !skip.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone().into()))
            .collect();
        (!m.is_empty()).then_some(Value::Object(m))
    }
}

/// Collects warnings (deduplicated, first-seen order) and unmapped names.
#[derive(Debug, Default)]
pub(crate) struct Report {
    warnings: Vec<String>,
    unmapped: Vec<String>,
    seen: HashSet<String>,
}

impl Report {
    pub fn warn(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if self.seen.insert(format!("w:{msg}")) {
            self.warnings.push(msg);
        }
    }

    /// Records something that was kept under `ffcine.foreign` instead of mapped.
    pub fn unmapped(&mut self, what: impl Into<String>) {
        let what = what.into();
        if self.seen.insert(format!("u:{what}")) {
            self.unmapped.push(what);
        }
    }

    /// The report, `unmapped` sorted like [`ImportReport::unmapped`] keeps it.
    pub fn finish(self) -> ImportReport {
        let mut r = ImportReport {
            warnings: self.warnings,
            ..ImportReport::default()
        };
        r.unmapped(self.unmapped);
        r
    }
}

/// Stable ids: a hash of what identifies the object in its source (media, position, track), so
/// re-importing the same file gives the same ids and unrelated edits don't shift them.
#[derive(Debug, Default)]
pub(crate) struct Ids {
    used: HashSet<String>,
}

impl Ids {
    pub fn make(&mut self, prefix: &str, key: &str) -> String {
        let base = format!("{prefix}-{:012x}", fnv1a(key.as_bytes()) & 0xffff_ffff_ffff);
        let mut id = base.clone();
        let mut n = 2;
        while !self.used.insert(id.clone()) {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// `x` with float noise from rational -> frame conversion removed.
pub(crate) fn clean(x: f64) -> f64 {
    let r = x.round();
    if (x - r).abs() < 1e-6 { r } else { x }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_with_doctype_and_keeps_text() {
        let e = Element::parse(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE fcpxml>\n<a x=\"1\"><b>hi <c/> there</b></a>",
        )
        .expect("parse");
        assert_eq!(e.name, "a");
        assert_eq!(e.attr("x"), Some("1"));
        let b = e.child("b").expect("b");
        assert_eq!(b.text, "hi  there");
        assert_eq!(
            e.to_json(),
            json!({"name": "a", "attrs": {"x": "1"},
                   "children": [{"name": "b", "children": [{"name": "c"}], "text": "hi  there"}]})
        );
        assert_eq!(
            e.to_json_except(&["x"]),
            json!({"name": "a", "children": b_json(b)})
        );
    }

    fn b_json(b: &Element) -> Value {
        json!([b.to_json()])
    }

    #[test]
    fn ids_are_stable_and_unique() {
        let mut a = Ids::default();
        let mut b = Ids::default();
        let x = a.make("fcpx", "r1@0");
        assert_eq!(x, b.make("fcpx", "r1@0"));
        let y = a.make("fcpx", "r1@0");
        assert_eq!(y, format!("{x}-2"));
    }

    #[test]
    fn report_dedupes() {
        let mut r = Report::default();
        r.warn("a");
        r.warn("a");
        r.unmapped("x");
        r.unmapped("x");
        r.unmapped("y");
        let r = r.finish();
        assert_eq!(r.warnings, ["a"]);
        assert_eq!(r.unmapped, ["x", "y"]);
    }
}

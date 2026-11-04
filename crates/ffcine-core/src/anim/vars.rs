//! `vars` (`docs/compositing.md#expressions`): named tracks an expression can read, on a comp
//! or a layer. Levels are pushed outermost first (the root comp, nested comps, the layer); a
//! name in an inner level shadows the outer one. Within a level the vars are ordered so each is
//! compiled after the ones it reads, and a cycle is an error naming it.

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::{Builtin, Clock, Frame, Scope, Track, expr};
use crate::{Error, Result};

/// Resolved vars: slot `i` is the `i`th var pushed, read through the latest name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vars {
    entries: Vec<(String, Track<f64>, Clock)>,
    visible: HashMap<String, usize>,
}

fn err(path: &str, msg: String) -> Error {
    Error::Track {
        path: path.to_owned(),
        msg,
    }
}

impl Vars {
    pub fn new() -> Self {
        Self::default()
    }

    /// The slot of the var `name` sees, if any.
    pub fn slot(&self, name: &str) -> Option<usize> {
        self.visible.get(name).copied()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Adds a level of `{name: track}` (keys on `clock`); `path` prefixes errors
    /// (`layers[2].vars`).
    pub fn push_level(&mut self, m: &Map<String, Value>, clock: Clock, path: &str) -> Result<()> {
        for name in m.keys() {
            if expr::is_builtin_name(name) {
                return Err(err(
                    &format!("{path}.{name}"),
                    format!("`{name}` is a built-in name and can't be a var"),
                ));
            }
            if !name.starts_with(|c: char| c.is_alphabetic() || c == '_')
                || !name.chars().all(|c| c.is_alphanumeric() || c == '_')
            {
                return Err(err(
                    &format!("{path}.{name}"),
                    "not a valid var name".into(),
                ));
            }
        }
        // What each var reads among this level's names.
        let mut deps: HashMap<&str, Vec<String>> = HashMap::new();
        for (name, v) in m {
            let mut names = Vec::new();
            for src in sources(v) {
                if let Ok(node) = expr::parse(src) {
                    node.names(&mut names);
                }
            }
            names.retain(|n| m.contains_key(n));
            names.dedup();
            deps.insert(name, names);
        }
        let mut order: Vec<&str> = Vec::new();
        let mut state: HashMap<&str, u8> = HashMap::new();
        let mut stack: Vec<&str> = Vec::new();
        for name in m.keys() {
            visit(name, &deps, &mut state, &mut stack, &mut order)
                .map_err(|c| err(path, format!("vars cycle: {c}")))?;
        }
        // Compile in order; the level's names become visible as they are added.
        for name in order {
            let what = format!("{path}.{name}");
            let track = Track::parse_in(&m[name], &Scope::with_vars(clock, self), &what)?;
            self.entries.push((name.to_owned(), track, clock));
            self.visible.insert(name.to_owned(), self.entries.len() - 1);
        }
        Ok(())
    }

    /// Every var's value on the frame with built-ins `b`, by slot.
    pub fn eval(&self, b: &[f64; Builtin::COUNT]) -> Result<Vec<f64>> {
        let mut out = Vec::with_capacity(self.entries.len());
        for (name, track, clock) in &self.entries {
            let v = track
                .try_eval(*clock, &Frame { b: *b, vars: &out })
                .map_err(|e| err(name, e.to_string()))?
                .unwrap_or(0.0);
            out.push(v);
        }
        Ok(out)
    }
}

/// Depth-first topological order; a grey node met again closes a cycle.
fn visit<'a>(
    n: &'a str,
    deps: &'a HashMap<&str, Vec<String>>,
    state: &mut HashMap<&'a str, u8>,
    stack: &mut Vec<&'a str>,
    order: &mut Vec<&'a str>,
) -> std::result::Result<(), String> {
    match state.get(n) {
        Some(2) => return Ok(()),
        Some(1) => {
            let at = stack.iter().position(|s| *s == n).unwrap_or(0);
            let mut cyc: Vec<&str> = stack[at..].to_vec();
            cyc.push(n);
            return Err(cyc.join(" -> "));
        }
        _ => {}
    }
    state.insert(n, 1);
    stack.push(n);
    for d in &deps[n] {
        visit(d, deps, state, stack, order)?;
    }
    stack.pop();
    state.insert(n, 2);
    order.push(n);
    Ok(())
}

/// The expression strings in a track's JSON.
fn sources(v: &Value) -> Vec<&str> {
    match v {
        Value::String(s) => vec![s],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        Value::Object(o) => o.get("expr").map_or_else(Vec::new, sources),
        _ => Vec::new(),
    }
}

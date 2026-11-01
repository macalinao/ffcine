//! The `ffcine` metadata namespace.
//!
//! ```json
//! "metadata": {
//!   "ffcine": {
//!     "v": 1,
//!     "id": "payday",
//!     "foreign": { "edit_yaml": { ... }, "resolve": { ... } }
//!   },
//!   "cdl": { ... }
//! }
//! ```
//!
//! - `v`: the namespace version, on the timeline only ([`FFCINE_VERSION`]).
//! - `id`: a stable id per clip so editorial changes made elsewhere merge back by id.
//! - `foreign.<app>`: everything an importer didn't map, verbatim (`docs/import.md`).
//! - `ffcine` effects (`Effect.1` with `effect_name = "ffcine.<domain>.<name>"`) carry their
//!   parameters here too, tagged `"schema": "ffcine.<domain>.<name>/<major>"`.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::schema::{
    Clip, Effect, ExternalReference, Gap, GeneratorReference, GenericEffect, Item, LinearTimeWarp,
    Marker, MediaReference, MissingReference, Stack, Timeline, Track, Transition,
};
use ffcine_core::{Error, Result};

/// The metadata key ffcine owns.
pub const FFCINE: &str = "ffcine";
/// Version of the `ffcine` namespace layout, written as `metadata.ffcine.v` on the timeline.
pub const FFCINE_VERSION: u64 = 1;

/// Access to an OTIO object's `metadata`, and the `ffcine` namespace inside it.
pub trait HasMetadata {
    fn metadata(&self) -> &Map<String, Value>;
    fn metadata_mut(&mut self) -> &mut Map<String, Value>;

    /// `metadata.ffcine`, if it is an object.
    fn ffcine(&self) -> Option<&Map<String, Value>> {
        self.metadata().get(FFCINE).and_then(Value::as_object)
    }

    /// `metadata.ffcine`, created if missing (a non-object value there is replaced).
    fn ffcine_mut(&mut self) -> &mut Map<String, Value> {
        object_entry(self.metadata_mut(), FFCINE)
    }

    /// `metadata.ffcine.<key>`.
    fn ffcine_value(&self, key: &str) -> Option<&Value> {
        self.ffcine()?.get(key)
    }

    /// `metadata.ffcine.<key>` decoded as `T`; `None` if absent.
    fn ffcine_get<T: DeserializeOwned>(&self, key: &str) -> Option<Result<T>> {
        self.ffcine_value(key)
            .map(|v| T::deserialize(v).map_err(Error::from))
    }

    /// Sets `metadata.ffcine.<key>`.
    fn ffcine_set<T: Serialize + ?Sized>(&mut self, key: &str, value: &T) -> Result<()> {
        let v = serde_json::to_value(value)?;
        self.ffcine_mut().insert(key.to_owned(), v);
        Ok(())
    }

    /// `metadata.ffcine.id`.
    fn ffcine_id(&self) -> Option<&str> {
        self.ffcine_value("id").and_then(Value::as_str)
    }

    fn set_ffcine_id(&mut self, id: impl Into<String>) {
        self.ffcine_mut()
            .insert("id".into(), Value::String(id.into()));
    }

    /// `metadata.ffcine.foreign.<app>`.
    fn foreign(&self, app: &str) -> Option<&Value> {
        self.ffcine_value("foreign")?.as_object()?.get(app)
    }

    /// Sets `metadata.ffcine.foreign.<app>`.
    fn set_foreign(&mut self, app: &str, value: Value) {
        object_entry(self.ffcine_mut(), "foreign").insert(app.to_owned(), value);
    }

    /// Inserts `key: value` into the object at `metadata.ffcine.foreign.<app>`.
    fn insert_foreign(&mut self, app: &str, key: &str, value: Value) {
        let foreign = object_entry(self.ffcine_mut(), "foreign");
        object_entry(foreign, app).insert(key.to_owned(), value);
    }

    /// Moves an OTIO adapter's namespace (`metadata.<key>`, e.g. `Resolve_OTIO`) to
    /// `metadata.ffcine.foreign.<app>`. Returns whether there was one.
    fn move_to_foreign(&mut self, key: &str, app: &str) -> bool {
        match self.metadata_mut().shift_remove(key) {
            Some(v) => {
                self.set_foreign(app, v);
                true
            }
            None => false,
        }
    }
}

/// `map[key]` as an object, created (or replaced, if not an object) on demand.
fn object_entry<'a>(map: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let slot = map.entry(key).or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    match slot {
        Value::Object(m) => m,
        _ => unreachable!("just made an object"),
    }
}

macro_rules! has_metadata {
    ($($t:ty),*) => {$(
        impl HasMetadata for $t {
            fn metadata(&self) -> &Map<String, Value> {
                &self.metadata
            }
            fn metadata_mut(&mut self) -> &mut Map<String, Value> {
                &mut self.metadata
            }
        }
    )*};
}

has_metadata!(
    Timeline,
    Stack,
    Track,
    Clip,
    Gap,
    Transition,
    GenericEffect,
    LinearTimeWarp,
    Marker,
    ExternalReference,
    MissingReference,
    GeneratorReference
);

/// Empty map handed out for `Unknown` objects without metadata.
static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);

/// Unknown objects expose their `metadata` if it is an object (created on mutable access).
macro_rules! has_metadata_enum {
    ($t:ident { $($v:ident),* }) => {
        impl HasMetadata for $t {
            fn metadata(&self) -> &Map<String, Value> {
                match self {
                    $($t::$v(x) => &x.metadata,)*
                    $t::Unknown(m) => m.get("metadata").and_then(Value::as_object).unwrap_or(&EMPTY),
                }
            }
            fn metadata_mut(&mut self) -> &mut Map<String, Value> {
                match self {
                    $($t::$v(x) => &mut x.metadata,)*
                    $t::Unknown(m) => object_entry(m, "metadata"),
                }
            }
        }
    };
}

has_metadata_enum!(Item {
    Clip,
    Gap,
    Stack,
    Track,
    Transition
});
has_metadata_enum!(Effect {
    Effect,
    LinearTimeWarp,
    FreezeFrame
});
has_metadata_enum!(MediaReference {
    External,
    Missing,
    Generator
});

/// `"ffcine.color.grade/1"` -> `("ffcine.color.grade", 1)`.
pub fn split_ffcine_schema(s: &str) -> Option<(&str, u32)> {
    let (name, major) = s.rsplit_once('/')?;
    Some((name, major.parse().ok()?))
}

impl GenericEffect {
    /// An ffcine effect: `effect_name` is the schema name, `params` go to `metadata.ffcine`
    /// with `"schema": schema`. `schema` is `"ffcine.<domain>.<name>/<major>"`.
    pub fn ffcine(schema: &str, params: Map<String, Value>) -> Self {
        let effect_name = split_ffcine_schema(schema).map_or(schema, |(n, _)| n);
        let short = effect_name.rsplit('.').next().unwrap_or(effect_name);
        let mut e = GenericEffect::new(short, effect_name);
        let ns = e.ffcine_mut();
        ns.insert("schema".into(), schema.into());
        ns.extend(params);
        e
    }

    /// `metadata.ffcine.schema` split into name and major version.
    pub fn ffcine_schema(&self) -> Option<(&str, u32)> {
        self.ffcine_value("schema")
            .and_then(Value::as_str)
            .and_then(split_ffcine_schema)
    }

    /// `metadata.ffcine` without `schema`, decoded as `T`, if this effect is `name` at `major`.
    pub fn ffcine_params<T: DeserializeOwned>(&self, name: &str, major: u32) -> Option<Result<T>> {
        if self.ffcine_schema() != Some((name, major)) {
            return None;
        }
        let mut params = self.ffcine()?.clone();
        params.shift_remove("schema");
        Some(T::deserialize(Value::Object(params)).map_err(Error::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn foreign_and_id() {
        let mut t = Timeline::default();
        t.metadata.insert(
            "Resolve_OTIO".into(),
            json!({"Resolve OTIO Meta Version": "1.0"}),
        );
        assert!(t.move_to_foreign("Resolve_OTIO", "resolve"));
        t.set_ffcine_id("a");
        t.insert_foreign("edit_yaml", "audio", json!("nat"));
        assert_eq!(
            Value::Object(t.metadata.clone()),
            json!({"ffcine": {
                "foreign": {
                    "resolve": {"Resolve OTIO Meta Version": "1.0"},
                    "edit_yaml": {"audio": "nat"},
                },
                "id": "a",
            }})
        );
        assert_eq!(t.ffcine_id(), Some("a"));
        assert_eq!(t.foreign("edit_yaml"), Some(&json!({"audio": "nat"})));
    }

    #[test]
    fn ffcine_effect() {
        let params = json!({"look": "day", "gamma": 1.2});
        let Value::Object(params) = params else {
            unreachable!()
        };
        let e = GenericEffect::ffcine("ffcine.color.grade/1", params);
        assert_eq!(e.name, "grade");
        assert_eq!(e.effect_name, "ffcine.color.grade");
        assert_eq!(e.ffcine_schema(), Some(("ffcine.color.grade", 1)));
        let p: Value = e
            .ffcine_params("ffcine.color.grade", 1)
            .and_then(Result::ok)
            .unwrap_or_default();
        assert_eq!(p, json!({"look": "day", "gamma": 1.2}));
        assert!(e.ffcine_params::<Value>("ffcine.color.grade", 2).is_none());
    }
}

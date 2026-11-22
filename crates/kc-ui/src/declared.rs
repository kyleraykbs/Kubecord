//! Settings declared by Nix, layered over the ones set in the app.
//!
//! The NixOS module writes `/etc/kubecord/settings.json` and the Home
//! Manager module `~/.config/kubecord/nix-settings.json`; both hold the same
//! keys as `settings.json`, any subset of them. Kubecord merges them over
//! the app's own `settings.json`, key by key (Home Manager over NixOS, both
//! over the app). A declared value can still be changed in the app, but only
//! until the next start: declared keys are never written to `settings.json`,
//! which keeps what the app had (or nothing). Every key Nix leaves alone is
//! saved as usual.
//!
//! A declared key the settings do not accept (a wrong type) is left out with
//! a warning rather than making every setting fall back to its default.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value};

/// Where a declared setting comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The NixOS module (`/etc/kubecord/settings.json`).
    System,
    /// The Home Manager module (`~/.config/kubecord/nix-settings.json`).
    User,
}

impl Source {
    /// "your NixOS configuration", for sentences.
    pub fn describe(self) -> &'static str {
        match self {
            Self::System => "your NixOS configuration",
            Self::User => "your Home Manager configuration",
        }
    }
}

/// The file the NixOS module writes (`KUBECORD_SYSTEM_SETTINGS` overrides it,
/// for testing).
pub fn system_path() -> PathBuf {
    std::env::var_os("KUBECORD_SYSTEM_SETTINGS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/kubecord/settings.json"))
}

/// The file the Home Manager module writes.
pub fn user_path() -> Option<PathBuf> {
    kc_core::paths::config_dir()
        .ok()
        .map(|dir| dir.join("nix-settings.json"))
}

/// A declared settings file as a JSON object; empty when missing. A file
/// that is not a JSON object is ignored, with a warning.
pub fn read(path: &Path) -> Value {
    let Ok(bytes) = std::fs::read(path) else {
        return Value::Object(Map::new());
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value @ Value::Object(_)) => value,
        Ok(_) => {
            tracing::warn!(path = %path.display(), "declared settings are not a JSON object; ignored");
            Value::Object(Map::new())
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "declared settings are not valid JSON; ignored");
            Value::Object(Map::new())
        }
    }
}

/// The settings with the declared layers merged in.
pub struct Layers {
    /// What the app runs with.
    pub effective: Value,
    /// Everything declared (both layers merged), as accepted.
    pub declared: Value,
    /// Each declared value's dotted path (`logger.viewed.text`), and where
    /// it comes from.
    pub sources: BTreeMap<String, Source>,
    /// Top-level keys left out because the settings would not accept them.
    pub rejected: Vec<(String, Source)>,
}

/// Merges `system` then `user` over `imperative`, one top-level key at a
/// time; a key that would make the settings fail `valid` is rejected.
pub fn layer(imperative: &Value, system: &Value, user: &Value, valid: impl Fn(&Value) -> bool) -> Layers {
    let mut effective = match imperative {
        Value::Object(_) if valid(imperative) => imperative.clone(),
        _ => Value::Object(Map::new()),
    };
    let mut declared = Value::Object(Map::new());
    let mut sources = BTreeMap::new();
    let mut rejected = Vec::new();
    for (source, layer) in [(Source::System, system), (Source::User, user)] {
        let Value::Object(keys) = layer else { continue };
        for (key, value) in keys {
            let mut candidate = effective.clone();
            merge(entry(&mut candidate, key), value);
            if !valid(&candidate) {
                rejected.push((key.clone(), source));
                continue;
            }
            effective = candidate;
            merge(entry(&mut declared, key), value);
            let mut paths = Vec::new();
            leaves(value, key, &mut paths);
            for path in paths {
                sources.insert(path, source);
            }
        }
    }
    Layers {
        effective,
        declared,
        sources,
        rejected,
    }
}

/// Merges `over` into `base`: objects key by key, anything else replaced.
pub fn merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            for (key, value) in over {
                merge(base.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (base, over) => *base = over.clone(),
    }
}

/// `object[key]`, made (null) if missing; `object` becomes an object.
fn entry<'a>(object: &'a mut Value, key: &str) -> &'a mut Value {
    if !object.is_object() {
        *object = Value::Object(Map::new());
    }
    object
        .as_object_mut()
        .expect("just made an object")
        .entry(key.to_owned())
        .or_insert(Value::Null)
}

/// The dotted paths of `value`'s leaves under `prefix` (an object's own
/// keys, recursively; anything else, an empty object included, is a leaf).
pub fn leaves(value: &Value, prefix: &str, out: &mut Vec<String>) {
    match value {
        Value::Object(keys) if !keys.is_empty() => {
            for (key, value) in keys {
                leaves(value, &format!("{prefix}.{key}"), out);
            }
        }
        _ => out.push(prefix.to_owned()),
    }
}

/// The value at a dotted path.
pub fn get<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |value, key| value.get(key))
}

/// Sets (or, with `None`, removes) the value at a dotted path.
pub fn set(value: &mut Value, path: &str, new: Option<Value>) {
    let mut keys: Vec<&str> = path.split('.').collect();
    let Some(last) = keys.pop() else { return };
    let mut target = value;
    for key in keys {
        if new.is_none() && target.get(key).is_none() {
            return;
        }
        target = entry(target, key);
    }
    match new {
        Some(new) => *entry(target, last) = new,
        None => {
            if let Some(object) = target.as_object_mut() {
                object.remove(last);
            }
        }
    }
}

/// What was declared, and the app's own settings as last read or written.
struct State {
    declared: Value,
    sources: BTreeMap<String, Source>,
    imperative: Value,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Remembers what was declared and the app's own settings file, for saving
/// and for the settings window.
pub fn install(layers: &Layers, imperative: Value) {
    if let Ok(mut state) = STATE.lock() {
        *state = Some(State {
            declared: layers.declared.clone(),
            sources: layers.sources.clone(),
            imperative,
        });
    }
}

/// What to write to `settings.json` for the settings now in effect: each
/// declared value put back to what the file had (or left out), so a change
/// to it in the app lasts only until the next start.
pub fn for_saving(mut current: Value) -> Value {
    let Ok(mut state) = STATE.lock() else {
        return current;
    };
    let Some(state) = state.as_mut() else {
        return current;
    };
    for path in state.sources.keys() {
        set(&mut current, path, get(&state.imperative, path).cloned());
    }
    state.imperative = current.clone();
    current
}

/// Where `key` (a top-level setting, or a dotted path) is declared, if it
/// is; for an object, where any of its values is (Home Manager first).
pub fn source_of(key: &str) -> Option<Source> {
    let state = STATE.lock().ok()?;
    let state = state.as_ref()?;
    let nested = format!("{key}.");
    let mut found = None;
    for (path, source) in &state.sources {
        if path == key || path.starts_with(&nested) {
            if *source == Source::User {
                return Some(Source::User);
            }
            found = Some(*source);
        }
    }
    found
}

/// Whether any declared value under `key` differs from what `current` has
/// (changed in the app, until the next start).
pub fn changed(current: &Value, key: &str) -> bool {
    let Ok(state) = STATE.lock() else { return false };
    let Some(state) = state.as_ref() else { return false };
    let nested = format!("{key}.");
    state
        .sources
        .keys()
        .filter(|path| *path == key || path.starts_with(&nested))
        .any(|path| get(current, path) != get(&state.declared, path))
}

/// Every declared value's dotted path and where it comes from.
pub fn all() -> Vec<(String, Source)> {
    STATE
        .lock()
        .ok()
        .and_then(|state| {
            state
                .as_ref()
                .map(|state| state.sources.iter().map(|(path, source)| (path.clone(), *source)).collect())
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn numbers_only(value: &Value) -> bool {
        value
            .as_object()
            .is_some_and(|keys| keys.values().all(|value| value.is_number() || value.is_object()))
    }

    #[test]
    fn home_manager_wins_over_nixos_and_both_over_the_app() {
        let imperative = json!({"a": 1, "b": 2, "c": 3});
        let system = json!({"b": 20, "c": 30});
        let user = json!({"c": 300});
        let layers = layer(&imperative, &system, &user, numbers_only);
        assert_eq!(layers.effective, json!({"a": 1, "b": 20, "c": 300}));
        assert_eq!(layers.sources.get("b"), Some(&Source::System));
        assert_eq!(layers.sources.get("c"), Some(&Source::User));
        assert!(!layers.sources.contains_key("a"));
    }

    #[test]
    fn objects_merge_key_by_key() {
        let imperative = json!({"logger": {"viewed": {"text": 1, "media": 2}, "cap": 5}});
        let user = json!({"logger": {"viewed": {"media": 9}}});
        let layers = layer(&imperative, &json!({}), &user, numbers_only);
        assert_eq!(layers.effective, json!({"logger": {"viewed": {"text": 1, "media": 9}, "cap": 5}}));
        assert_eq!(layers.sources.keys().collect::<Vec<_>>(), ["logger.viewed.media"]);
    }

    #[test]
    fn a_value_the_settings_reject_is_left_out() {
        let layers = layer(&json!({"a": 1}), &json!({"a": "nope", "b": 2}), &json!({}), numbers_only);
        assert_eq!(layers.effective, json!({"a": 1, "b": 2}));
        assert_eq!(layers.rejected, [("a".to_owned(), Source::System)]);
    }

    #[test]
    fn declared_values_are_never_saved_and_the_rest_are() {
        let imperative = json!({"a": 1, "logger": {"x": 1, "y": 2}});
        let layers = layer(&imperative, &json!({"b": 5}), &json!({"logger": {"x": 7}}), numbers_only);
        install(&layers, imperative);
        // Changed in the app: a (not declared), b and logger.x (declared).
        let current = json!({"a": 10, "b": 50, "logger": {"x": 70, "y": 20}});
        assert!(changed(&current, "b"));
        assert!(changed(&current, "logger"));
        assert!(!changed(&json!({"b": 5}), "b"));
        let saved = for_saving(current);
        assert_eq!(saved, json!({"a": 10, "logger": {"x": 1, "y": 20}}));
        assert_eq!(source_of("logger"), Some(Source::User));
        assert_eq!(source_of("a"), None);
        *STATE.lock().unwrap() = None;
    }

    #[test]
    fn dotted_paths_are_read_set_and_removed() {
        let mut value = json!({"a": {"b": 1}});
        assert_eq!(get(&value, "a.b"), Some(&json!(1)));
        set(&mut value, "a.c.d", Some(json!(2)));
        assert_eq!(value, json!({"a": {"b": 1, "c": {"d": 2}}}));
        set(&mut value, "a.b", None);
        set(&mut value, "x.y", None);
        assert_eq!(value, json!({"a": {"c": {"d": 2}}}));
    }
}

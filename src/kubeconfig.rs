//! Reading the main kubeconfig and reading/writing per-shell overlays.
//!
//! kk never writes to the main kubeconfig. It only needs the context stanzas from
//! it, so it parses just those and leaves merging to kubectl and friends.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_norway::{Mapping, Value};

use crate::paths::write_atomic;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedContext {
    pub name: String,
    pub context: ContextFields,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContextFields {
    #[serde(default)]
    pub cluster: String,
    #[serde(default)]
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// The subset of a kubeconfig file that kk cares about.
#[derive(Debug, Default, Deserialize)]
pub struct KubeConfig {
    #[serde(default, rename = "current-context")]
    pub current_context: Option<String>,
    #[serde(default)]
    pub contexts: Vec<NamedContext>,
}

impl KubeConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_norway::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn context(&self, name: &str) -> Option<&NamedContext> {
        self.contexts.iter().find(|c| c.name == name)
    }

    /// The context selected by `current-context`, if it exists.
    pub fn current(&self) -> Option<&NamedContext> {
        self.current_context
            .as_deref()
            .and_then(|name| self.context(name))
    }
}

/// Writes the overlay for a shell, selecting `shadow` as the current context.
///
/// Anything else found in an existing overlay (clusters, users or extra contexts,
/// typically added by a tool such as `aws eks update-kubeconfig`) is carried over
/// rather than silently dropped. The returned warnings describe what was kept.
pub fn write_overlay(path: &Path, shadow: &NamedContext) -> Result<Vec<String>> {
    let foreign = match read_overlay_mapping(path)? {
        Some(existing) => Foreign::split(existing),
        None => Foreign::default(),
    };
    let warnings = foreign
        .describe()
        .into_iter()
        .map(|entry| {
            format!(
                "kept {entry} found in the overlay; it was probably added by another tool and belongs in the main kubeconfig"
            )
        })
        .collect();

    let mut contexts = vec![serde_norway::to_value(shadow)?];
    contexts.extend(
        foreign
            .contexts
            .into_iter()
            .filter(|ctx| ctx.get("name").and_then(Value::as_str) != Some(&shadow.name)),
    );

    let mut doc = Mapping::new();
    doc.insert("apiVersion".into(), "v1".into());
    doc.insert("kind".into(), "Config".into());
    doc.insert("current-context".into(), shadow.name.clone().into());
    doc.insert("contexts".into(), Value::Sequence(contexts));
    doc.extend(foreign.other);

    let text = serde_norway::to_string(&Value::Mapping(doc))?;
    write_atomic(path, text.as_bytes(), 0o600)?;
    Ok(warnings)
}

/// Describes the entries in an overlay that kk didn't write, such as a cluster
/// added by `aws eks update-kubeconfig`. Deleting an overlay with any of these
/// would lose them. A missing overlay has none.
pub fn foreign_entries(path: &Path) -> Result<Vec<String>> {
    Ok(match read_overlay_mapping(path)? {
        Some(mapping) => Foreign::split(mapping).describe(),
        None => Vec::new(),
    })
}

/// The parts of an overlay that kk didn't write.
#[derive(Default)]
struct Foreign {
    /// Contexts other than the shadow context (the one `current-context` selects).
    contexts: Vec<Value>,
    /// Non-empty top-level keys other than the ones kk writes, e.g. `clusters`.
    other: Mapping,
}

impl Foreign {
    fn split(overlay: Mapping) -> Self {
        let shadow = overlay
            .get("current-context")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut foreign = Foreign::default();
        for (key, value) in overlay {
            match key.as_str() {
                Some("apiVersion" | "kind" | "current-context" | "preferences") => {}
                Some("contexts") => {
                    foreign.contexts.extend(
                        value
                            .as_sequence()
                            .into_iter()
                            .flatten()
                            .filter(|ctx| {
                                ctx.get("name").and_then(Value::as_str) != shadow.as_deref()
                            })
                            .cloned(),
                    );
                }
                _ if is_empty(&value) => {}
                _ => {
                    foreign.other.insert(key, value);
                }
            }
        }
        foreign
    }

    /// Human-readable descriptions, e.g. `context 'new'` or `cluster 'new'`.
    fn describe(&self) -> Vec<String> {
        let mut entries: Vec<String> = self
            .contexts
            .iter()
            .map(|ctx| format!("context '{}'", entry_name(ctx)))
            .collect();
        for (key, value) in &self.other {
            let key = key.as_str().unwrap_or("?");
            match (key, value.as_sequence()) {
                ("clusters" | "users", Some(items)) => {
                    let kind = key.trim_end_matches('s');
                    entries.extend(
                        items
                            .iter()
                            .map(|item| format!("{kind} '{}'", entry_name(item))),
                    );
                }
                _ => entries.push(format!("'{key}' entries")),
            }
        }
        entries
    }
}

fn entry_name(entry: &Value) -> &str {
    entry.get("name").and_then(Value::as_str).unwrap_or("?")
}

/// Returns the context an overlay currently selects, if the overlay exists and is valid.
pub fn read_overlay_current(path: &Path) -> Result<Option<NamedContext>> {
    if !path.exists() {
        return Ok(None);
    }
    Ok(KubeConfig::load(path)?.current().cloned())
}

fn read_overlay_mapping(path: &Path) -> Result<Option<Mapping>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    match serde_norway::from_str(&text) {
        Ok(Value::Mapping(m)) => Ok(Some(m)),
        Ok(_) => bail!("{} is not a kubeconfig mapping", path.display()),
        Err(e) => Err(e).with_context(|| format!("parsing {}", path.display())),
    }
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Sequence(s) => s.is_empty(),
        Value::Mapping(m) => m.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shadow(name: &str, ns: &str) -> NamedContext {
        NamedContext {
            name: name.into(),
            context: ContextFields {
                cluster: format!("{name}-cluster"),
                user: format!("{name}-user"),
                namespace: Some(ns.into()),
            },
        }
    }

    #[test]
    fn writes_minimal_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay");
        let warnings = write_overlay(&path, &shadow("prod", "team-a")).unwrap();
        assert!(warnings.is_empty());

        let current = read_overlay_current(&path).unwrap().unwrap();
        assert_eq!(current, shadow("prod", "team-a"));
        assert!(foreign_entries(&path).unwrap().is_empty());
        assert!(
            foreign_entries(&dir.path().join("missing"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn replaces_previous_shadow_and_keeps_foreign_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlay");
        write_overlay(&path, &shadow("prod", "team-a")).unwrap();

        // Simulate a tool adding a new cluster/user/context to the overlay.
        let mut text = fs::read_to_string(&path).unwrap();
        text = text.replace(
            "contexts:\n",
            "contexts:\n- name: new\n  context:\n    cluster: new\n    user: new\n",
        );
        text.push_str("clusters:\n- name: new\n  cluster:\n    server: https://new\n");
        fs::write(&path, text).unwrap();

        assert_eq!(
            foreign_entries(&path).unwrap(),
            ["context 'new'", "cluster 'new'"]
        );

        let warnings = write_overlay(&path, &shadow("staging", "team-b")).unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");

        let config = KubeConfig::load(&path).unwrap();
        let names: Vec<_> = config.contexts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["staging", "new"]);
        assert_eq!(config.current_context.as_deref(), Some("staging"));
        assert!(fs::read_to_string(&path).unwrap().contains("https://new"));
        // Still foreign after the switch, so the overlay must not be deleted.
        assert_eq!(
            foreign_entries(&path).unwrap(),
            ["context 'new'", "cluster 'new'"]
        );
    }
}

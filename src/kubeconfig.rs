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
    let existing = read_overlay_mapping(path)?;
    let mut warnings = Vec::new();

    let previous_shadow = existing
        .as_ref()
        .and_then(|m| m.get("current-context"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let mut contexts = vec![serde_norway::to_value(shadow)?];
    let mut carried = Mapping::new();
    if let Some(existing) = existing {
        for (key, value) in existing {
            match key.as_str() {
                Some("apiVersion" | "kind" | "current-context" | "preferences") => {}
                Some("contexts") => {
                    for ctx in value.as_sequence().into_iter().flatten() {
                        let name = ctx.get("name").and_then(Value::as_str);
                        if name.is_some()
                            && name != previous_shadow.as_deref()
                            && name != Some(&shadow.name)
                        {
                            warnings.push(format!(
                                "kept context '{}' found in the overlay; it was probably added by another tool and belongs in the main kubeconfig",
                                name.unwrap_or_default()
                            ));
                            contexts.push(ctx.clone());
                        }
                    }
                }
                _ => {
                    if !is_empty(&value) {
                        warnings.push(format!(
                            "kept '{}' entries found in the overlay; they were probably added by another tool and belong in the main kubeconfig",
                            key.as_str().unwrap_or("?")
                        ));
                        carried.insert(key, value);
                    }
                }
            }
        }
    }

    let mut doc = Mapping::new();
    doc.insert("apiVersion".into(), "v1".into());
    doc.insert("kind".into(), "Config".into());
    doc.insert("current-context".into(), shadow.name.clone().into());
    doc.insert("contexts".into(), Value::Sequence(contexts));
    doc.extend(carried);

    let text = serde_norway::to_string(&Value::Mapping(doc))?;
    write_atomic(path, text.as_bytes(), 0o600)?;
    Ok(warnings)
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

        let warnings = write_overlay(&path, &shadow("staging", "team-b")).unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");

        let config = KubeConfig::load(&path).unwrap();
        let names: Vec<_> = config.contexts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["staging", "new"]);
        assert_eq!(config.current_context.as_deref(), Some("staging"));
        assert!(fs::read_to_string(&path).unwrap().contains("https://new"));
    }
}

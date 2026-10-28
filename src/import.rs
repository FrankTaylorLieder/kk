//! Moving entries that other tools wrote into an overlay over to the main kubeconfig.
//!
//! Tools such as `aws eks update-kubeconfig` write new clusters, users and contexts
//! into the first `KUBECONFIG` file, which is the shell's overlay. Importing moves
//! them where the tool would have put them without kk, so every shell can use them.
//!
//! The main kubeconfig is written under kubectl's own lock file (`<file>.lock`, as
//! client-go uses), after taking a backup, and replaced atomically. Unknown fields
//! and ordering are preserved; YAML comments are not.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_norway::{Mapping, Value};

use crate::kubeconfig::{Foreign, entry_name, is_empty, read_overlay_mapping};
use crate::paths::write_atomic;

const SECTIONS: [(&str, &str); 3] = [
    ("clusters", "cluster"),
    ("users", "user"),
    ("contexts", "context"),
];
const LOCK_TIMEOUT: Duration = Duration::from_secs(3);

/// An entry found in the overlay that doesn't belong to kk.
#[derive(Debug, Clone)]
pub struct Entry {
    /// `clusters`, `users` or `contexts`.
    section: &'static str,
    kind: &'static str,
    pub name: String,
    value: Value,
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} '{}'", self.kind, self.name)
    }
}

/// An overlay entry whose name exists in the main config with different contents.
#[derive(Debug)]
pub struct Conflict {
    pub entry: Entry,
    /// What differs, e.g. `cluster.server: https://a → https://b`. Values that may be
    /// secret are not shown.
    pub differences: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ImportReport {
    /// Added to the main config.
    pub moved: Vec<Entry>,
    /// Replaced in the main config after a conflict was resolved.
    pub replaced: Vec<Entry>,
    /// Already in the main config with identical contents; dropped from the overlay.
    pub duplicates: Vec<Entry>,
    /// Left in the overlay because they differ from the main config.
    pub conflicts: Vec<Conflict>,
    /// Top-level keys kk can't merge by name (e.g. `extensions`); left in the overlay.
    pub unmergeable: Vec<String>,
    /// The overlay's `current-context` when another tool switched it away from kk's
    /// context.
    pub switched_to: Option<String>,
    /// Where the main config was backed up, if it was changed.
    pub backup: Option<PathBuf>,
}

impl ImportReport {
    pub fn changed_main(&self) -> bool {
        !self.moved.is_empty() || !self.replaced.is_empty()
    }
}

/// Imports foreign entries from `overlay` into `main`.
///
/// `shadow` names the context kk wrote into the overlay (never imported). With
/// `dry_run`, nothing is written and conflicts are only reported. Otherwise
/// `resolve` is asked about each conflict and returns whether to replace the main
/// config's version.
pub fn import(
    overlay: &Path,
    main: &Path,
    shadow: Option<&str>,
    dry_run: bool,
    resolve: &mut dyn FnMut(&Conflict) -> Result<bool>,
) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    let Some(overlay_doc) = read_overlay_mapping(overlay)? else {
        return Ok(report);
    };
    let current = overlay_doc
        .get("current-context")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let shadow = shadow.map(str::to_owned).or_else(|| current.clone());
    report.switched_to = current.filter(|c| Some(c) != shadow.as_ref());

    let entries = foreign_entries(
        Foreign::split(overlay_doc.clone(), shadow.as_deref()),
        &mut report,
    );
    if entries.is_empty() {
        return Ok(report);
    }

    let _lock = if dry_run {
        None
    } else {
        Some(KubeconfigLock::acquire(main)?)
    };
    let mut main_doc = read_main(main)?;

    let mut resolved: Vec<&Entry> = Vec::new();
    for entry in &entries {
        match find(&main_doc, entry) {
            None => {
                if !dry_run {
                    section_mut(&mut main_doc, entry.section).push(entry.value.clone());
                }
                report.moved.push(entry.clone());
                resolved.push(entry);
            }
            Some(existing) if *existing == entry.value => {
                report.duplicates.push(entry.clone());
                resolved.push(entry);
            }
            Some(existing) => {
                let conflict = Conflict {
                    entry: entry.clone(),
                    differences: differences(entry.kind, existing, &entry.value),
                };
                if !dry_run && resolve(&conflict)? {
                    replace(&mut main_doc, entry);
                    report.replaced.push(entry.clone());
                    resolved.push(entry);
                } else {
                    report.conflicts.push(conflict);
                }
            }
        }
    }

    if dry_run {
        return Ok(report);
    }
    if report.changed_main() {
        report.backup = Some(write_main(main, &main_doc)?);
    }
    if !resolved.is_empty() {
        remove_from_overlay(overlay, overlay_doc, shadow.as_deref(), &resolved)?;
    }
    Ok(report)
}

fn foreign_entries(foreign: Foreign, report: &mut ImportReport) -> Vec<Entry> {
    let mut entries = Vec::new();
    for (key, value) in foreign.other {
        let key = key.as_str().unwrap_or("?").to_owned();
        match SECTIONS.iter().find(|(section, _)| *section == key) {
            Some((section, kind)) if value.is_sequence() => {
                for item in value.as_sequence().into_iter().flatten() {
                    entries.push(Entry {
                        section,
                        kind,
                        name: entry_name(item).to_owned(),
                        value: item.clone(),
                    });
                }
            }
            _ => report.unmergeable.push(key),
        }
    }
    for item in foreign.contexts {
        entries.push(Entry {
            section: "contexts",
            kind: "context",
            name: entry_name(&item).to_owned(),
            value: item,
        });
    }
    entries
}

fn read_main(path: &Path) -> Result<Mapping> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if text.trim().is_empty() {
        let mut doc = Mapping::new();
        doc.insert("apiVersion".into(), "v1".into());
        doc.insert("kind".into(), "Config".into());
        return Ok(doc);
    }
    match serde_norway::from_str(&text) {
        Ok(Value::Mapping(m)) => Ok(m),
        Ok(_) => bail!("{} is not a kubeconfig mapping", path.display()),
        Err(e) => Err(e).with_context(|| format!("parsing {}", path.display())),
    }
}

/// Backs up and atomically replaces the main config, keeping its permissions.
/// Returns the backup's path.
fn write_main(path: &Path, doc: &Mapping) -> Result<PathBuf> {
    // Write through a symlinked ~/.kube/config rather than replacing the link.
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let backup = backup_path(path);
    let mode = match fs::metadata(&target) {
        Ok(meta) => {
            fs::copy(&target, &backup).with_context(|| {
                format!("backing up {} to {}", target.display(), backup.display())
            })?;
            meta.permissions().mode() & 0o777
        }
        Err(_) => 0o600,
    };
    let text = serde_norway::to_string(&Value::Mapping(doc.clone()))?;
    write_atomic(&target, text.as_bytes(), mode)?;
    Ok(backup)
}

pub fn backup_path(main: &Path) -> PathBuf {
    let mut name = main.as_os_str().to_owned();
    name.push(".kk-backup");
    name.into()
}

fn remove_from_overlay(
    path: &Path,
    mut doc: Mapping,
    shadow: Option<&str>,
    resolved: &[&Entry],
) -> Result<()> {
    for (section, _) in SECTIONS {
        if let Some(Value::Sequence(items)) = doc.get_mut(section) {
            items.retain(|item| {
                let name = entry_name(item);
                // Never remove kk's own context, even if a foreign one shares its name.
                (section == "contexts" && Some(name) == shadow)
                    || !resolved
                        .iter()
                        .any(|e| e.section == section && e.name == name)
            });
        }
        if doc.get(section).is_some_and(is_empty) && section != "contexts" {
            doc.remove(section);
        }
    }
    let text = serde_norway::to_string(&Value::Mapping(doc))?;
    write_atomic(path, text.as_bytes(), 0o600)
}

fn find<'a>(doc: &'a Mapping, entry: &Entry) -> Option<&'a Value> {
    doc.get(entry.section)?
        .as_sequence()?
        .iter()
        .find(|item| entry_name(item) == entry.name)
}

fn section_mut<'a>(doc: &'a mut Mapping, section: &str) -> &'a mut Vec<Value> {
    if !doc.get(section).is_some_and(Value::is_sequence) {
        doc.insert(section.into(), Value::Sequence(Vec::new()));
    }
    match doc.get_mut(section) {
        Some(Value::Sequence(items)) => items,
        _ => unreachable!("just inserted a sequence"),
    }
}

fn replace(doc: &mut Mapping, entry: &Entry) {
    let items = section_mut(doc, entry.section);
    if let Some(item) = items.iter_mut().find(|item| entry_name(item) == entry.name) {
        *item = entry.value.clone();
    }
}

/// Lists the paths that differ between two versions of an entry. Values are shown
/// only for fields that are never secret.
fn differences(kind: &str, old: &Value, new: &Value) -> Vec<String> {
    let mut out = Vec::new();
    diff(kind, "", old, new, &mut out);
    out
}

fn diff(kind: &str, path: &str, old: &Value, new: &Value, out: &mut Vec<String>) {
    if old == new {
        return;
    }
    if let (Value::Mapping(a), Value::Mapping(b)) = (old, new) {
        let mut keys: Vec<&Value> = a.keys().collect();
        keys.extend(b.keys().filter(|k| !a.contains_key(*k)));
        for key in keys {
            let name = key.as_str().unwrap_or("?");
            let child = if path.is_empty() {
                name.to_owned()
            } else {
                format!("{path}.{name}")
            };
            diff(
                kind,
                &child,
                a.get(key).unwrap_or(&Value::Null),
                b.get(key).unwrap_or(&Value::Null),
                out,
            );
        }
        return;
    }
    let showable = kind == "context" || path.ends_with("server");
    if showable {
        out.push(format!("{path}: {} → {}", scalar(old), scalar(new)));
    } else {
        out.push(format!("{path} changed"));
    }
}

fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "(none)".to_owned(),
        Value::String(s) => s.clone(),
        other => serde_norway::to_string(other)
            .map(|s| s.trim().to_owned())
            .unwrap_or_default(),
    }
}

/// kubectl's lock on a kubeconfig file: `<file>.lock`, created exclusively and
/// removed when done.
struct KubeconfigLock(PathBuf);

impl KubeconfigLock {
    fn acquire(main: &Path) -> Result<Self> {
        let mut name = main.as_os_str().to_owned();
        name.push(".lock");
        let path = PathBuf::from(name);
        let start = Instant::now();
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(KubeconfigLock(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if start.elapsed() > LOCK_TIMEOUT {
                        bail!(
                            "{} exists: another kubectl may be writing the config. If not, remove the stale lock file",
                            path.display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("creating {}", path.display()));
                }
            }
        }
    }
}

impl Drop for KubeconfigLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubeconfig::{KubeConfig, foreign_entries as describe_foreign};

    const MAIN: &str = "\
apiVersion: v1
kind: Config
current-context: prod
clusters:
- name: prod
  cluster:
    server: https://prod
users:
- name: prod
  user:
    token: secret
contexts:
- name: prod
  context:
    cluster: prod
    user: prod
";

    /// An overlay where a tool added cluster/user/context `new`, made it current,
    /// and also wrote a copy of the existing `prod` cluster.
    const OVERLAY: &str = "\
apiVersion: v1
kind: Config
current-context: new
contexts:
- name: prod
  context:
    cluster: prod
    user: prod
    namespace: team-a
- name: new
  context:
    cluster: new
    user: new
clusters:
- name: new
  cluster:
    server: https://new
- name: prod
  cluster:
    server: PROD_SERVER
users:
- name: new
  user:
    token: other-secret
";

    struct Setup {
        _dir: tempfile::TempDir,
        main: PathBuf,
        overlay: PathBuf,
    }

    fn setup(prod_server_in_overlay: &str) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("config");
        let overlay = dir.path().join("overlay");
        fs::write(&main, MAIN).unwrap();
        fs::set_permissions(&main, fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(
            &overlay,
            OVERLAY.replace("PROD_SERVER", prod_server_in_overlay),
        )
        .unwrap();
        Setup {
            _dir: dir,
            main,
            overlay,
        }
    }

    fn names(entries: &[Entry]) -> Vec<String> {
        entries.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn moves_new_entries_and_drops_duplicates() {
        let s = setup("https://prod");
        let report = import(&s.overlay, &s.main, Some("prod"), false, &mut |_| {
            panic!("no conflicts expected")
        })
        .unwrap();

        assert_eq!(
            names(&report.moved),
            ["cluster 'new'", "user 'new'", "context 'new'"]
        );
        assert_eq!(names(&report.duplicates), ["cluster 'prod'"]);
        assert_eq!(report.switched_to.as_deref(), Some("new"));

        let main = KubeConfig::load(&s.main).unwrap();
        assert!(main.context("new").is_some());
        assert_eq!(main.current_context.as_deref(), Some("prod"));
        let text = fs::read_to_string(&s.main).unwrap();
        assert!(text.contains("https://new") && text.contains("other-secret"));
        assert_eq!(
            fs::metadata(&s.main).unwrap().permissions().mode() & 0o777,
            0o640
        );

        assert_eq!(
            fs::read_to_string(report.backup.unwrap()).unwrap(),
            MAIN,
            "backup holds the original"
        );
        assert!(!s.main.with_extension("lock").exists());

        // Only kk's own context is left in the overlay.
        assert!(
            describe_foreign(&s.overlay, Some("prod"))
                .unwrap()
                .is_empty()
        );
        let overlay = KubeConfig::load(&s.overlay).unwrap();
        assert_eq!(overlay.contexts.len(), 1);
        assert_eq!(overlay.contexts[0].name, "prod");
    }

    #[test]
    fn conflicts_are_left_unless_resolved() {
        let s = setup("https://prod-moved");
        let mut asked = Vec::new();
        let report = import(&s.overlay, &s.main, Some("prod"), false, &mut |c| {
            asked.push(c.differences.clone());
            Ok(false)
        })
        .unwrap();
        assert_eq!(
            asked,
            [["cluster.server: https://prod → https://prod-moved"]]
        );
        assert_eq!(
            names(
                &report
                    .conflicts
                    .iter()
                    .map(|c| c.entry.clone())
                    .collect::<Vec<_>>()
            ),
            ["cluster 'prod'"]
        );
        assert!(
            fs::read_to_string(&s.main)
                .unwrap()
                .contains("https://prod\n")
        );
        assert_eq!(
            describe_foreign(&s.overlay, Some("prod")).unwrap(),
            ["cluster 'prod'"]
        );

        // Resolving it replaces the main config's version.
        let report = import(&s.overlay, &s.main, Some("prod"), false, &mut |_| Ok(true)).unwrap();
        assert_eq!(names(&report.replaced), ["cluster 'prod'"]);
        assert!(
            fs::read_to_string(&s.main)
                .unwrap()
                .contains("https://prod-moved")
        );
        assert!(
            describe_foreign(&s.overlay, Some("prod"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn secrets_are_not_shown_in_differences() {
        let old: Value = serde_norway::from_str("user: {token: a}").unwrap();
        let new: Value = serde_norway::from_str("user: {token: b}").unwrap();
        assert_eq!(differences("user", &old, &new), ["user.token changed"]);
    }

    #[test]
    fn dry_run_changes_nothing() {
        let s = setup("https://prod-moved");
        let overlay_before = fs::read_to_string(&s.overlay).unwrap();
        let report = import(&s.overlay, &s.main, Some("prod"), true, &mut |_| {
            panic!("dry run doesn't resolve")
        })
        .unwrap();
        assert_eq!(names(&report.moved).len(), 3);
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(fs::read_to_string(&s.main).unwrap(), MAIN);
        assert_eq!(fs::read_to_string(&s.overlay).unwrap(), overlay_before);
        assert!(report.backup.is_none());
    }

    #[test]
    fn waits_for_kubectl_lock() {
        let s = setup("https://prod");
        let lock = PathBuf::from(format!("{}.lock", s.main.display()));
        fs::write(&lock, "").unwrap();
        let err = import(&s.overlay, &s.main, Some("prod"), false, &mut |_| Ok(false)).unwrap_err();
        assert!(err.to_string().contains("another kubectl"), "{err}");
        assert_eq!(fs::read_to_string(&s.main).unwrap(), MAIN);
    }

    #[test]
    fn context_switch_alone_is_reported() {
        let s = setup("https://prod");
        fs::write(
            &s.overlay,
            "current-context: other\ncontexts:\n- name: prod\n  context: {cluster: prod, user: prod}\n",
        )
        .unwrap();
        let report = import(&s.overlay, &s.main, Some("prod"), false, &mut |_| Ok(false)).unwrap();
        assert_eq!(report.switched_to.as_deref(), Some("other"));
        assert!(!report.changed_main());
        assert_eq!(fs::read_to_string(&s.main).unwrap(), MAIN);
    }
}

use std::fs;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};

use crate::abbrev::abbreviate;
use crate::catalogue::{Catalogue, Key, Profile, Store};
use crate::kubeconfig::{
    ContextFields, KubeConfig, NamedContext, foreign_entries, read_overlay_current, write_overlay,
};
use crate::namespaces;
use crate::paths::Paths;
use crate::picker;
use crate::shell::{Shell, ShellKind, ShellState, init_script, validate_env};

/// Returned when the user cancels a picker; exits quietly with status 130.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Returned to exit with status 1 without printing an error.
#[derive(Debug)]
pub struct Silent;

impl std::fmt::Display for Silent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("failed")
    }
}

impl std::error::Error for Silent {}

pub struct App {
    paths: Paths,
    store: Store,
    now: DateTime<Utc>,
}

impl App {
    pub fn new() -> Result<Self> {
        let paths = Paths::from_env()?;
        let store = Store::new(paths.profiles_file());
        Ok(App {
            paths,
            store,
            now: Utc::now(),
        })
    }

    pub fn profile(&self, target: Option<String>) -> Result<()> {
        let mut shell = Shell::require()?;
        let key = match target.as_deref() {
            Some("-") => ShellState::load(&self.paths.state_file(shell.pid))?
                .previous
                .ok_or_else(|| anyhow!("no previous profile in this shell"))?,
            Some(name) => self
                .store
                .load()?
                .resolve(name)
                .map(Profile::key)
                .ok_or_else(|| anyhow!("no profile named '{name}' (see `kk profile list`)"))?,
            None => self.pick_profile(shell.pid)?,
        };
        self.switch(&mut shell, key)?;
        shell.finish()
    }

    pub fn profile_name(&self, name: &str) -> Result<()> {
        let shell = Shell::detect()?;
        let main = self.main_config()?;
        let key = self
            .current_key(shell.as_ref().map(|s| s.pid), &main)?
            .ok_or_else(|| anyhow!("no current context to name; switch with `kk context` first"))?;
        let profile = self.store.update(|c| c.set_name(&key, name))?;
        eprintln!("kk: named {} '{name}'", key.derived_name());

        if let Some(mut shell) = shell {
            if self.overlay_key(shell.pid, &main)?.as_ref() == Some(&key) {
                shell.export("KK_PROFILE", &profile.display_name());
            }
            shell.finish()?;
        }
        Ok(())
    }

    pub fn profile_list(&self) -> Result<()> {
        let catalogue = self.store.load()?;
        if catalogue.profiles.is_empty() {
            eprintln!(
                "kk: no profiles yet; switch with `kk context` or `kk namespace` to record some"
            );
            return Ok(());
        }
        let main = self.main_config()?;
        let current = self.current_key(Shell::detect()?.map(|s| s.pid), &main)?;

        let rows: Vec<[String; 6]> = catalogue
            .ordered(self.now)
            .into_iter()
            .map(|p| {
                let marker = if current.as_ref() == Some(&p.key()) {
                    "*"
                } else {
                    " "
                };
                let mut namespace = p.namespace.clone();
                if let Some(user) = &p.user {
                    namespace.push_str(&format!(" (as {user})"));
                }
                let mut context = p.context.clone();
                if main.context(&p.context).is_none() {
                    context.push_str(" [stale]");
                }
                [
                    marker.to_owned(),
                    p.display_name(),
                    context,
                    namespace,
                    p.use_count.to_string(),
                    p.last_used
                        .map_or("never".to_owned(), |t| humanize_age(self.now, t)),
                ]
            })
            .collect();

        let header = ["", "NAME", "CONTEXT", "NAMESPACE", "USES", "LAST USED"].map(str::to_owned);
        let mut widths = [0usize; 6];
        for row in std::iter::once(&header).chain(&rows) {
            for (w, cell) in widths.iter_mut().zip(row) {
                *w = (*w).max(cell.chars().count());
            }
        }
        for row in std::iter::once(&header).chain(&rows) {
            let line: Vec<String> = row
                .iter()
                .zip(widths)
                .map(|(cell, w)| format!("{cell:<w$}"))
                .collect();
            println!("{}", line.join("  ").trim_end());
        }
        Ok(())
    }

    /// Prints the current profile's name, or its derived `<context>/<namespace>` name.
    /// Cheap and quiet so it can run in a prompt.
    /// With `short`, an unnamed profile's context and namespace are abbreviated to
    /// stay unique among the main config's contexts and the namespaces profiles use
    /// with that context. Names given with `kk profile name` are shown as they are.
    pub fn profile_current(&self, short: bool) -> Result<()> {
        let main = self.main_config()?;
        // Find the overlay from KUBECONFIG, so this works when run as `command kk`
        // from a prompt, without the shell function.
        let Some(key) = self.current_key(self.active_overlay_pid(), &main)? else {
            return Err(Silent.into());
        };
        let catalogue = self.store.load()?;
        let name = match catalogue.profile(&key) {
            Some(Profile {
                name: Some(name), ..
            }) => name.clone(),
            _ if short => {
                let contexts = main.contexts.iter().map(|c| c.name.as_str());
                let namespaces = catalogue
                    .profiles
                    .iter()
                    .filter(|p| p.context == key.context)
                    .map(|p| p.namespace.as_str());
                let short_key = Key {
                    context: abbreviate(&key.context, contexts),
                    namespace: abbreviate(&key.namespace, namespaces),
                    user: key.user.clone(),
                };
                short_key.derived_name()
            }
            _ => key.derived_name(),
        };
        println!("{name}");
        Ok(())
    }

    pub fn profile_rm(&self, name: &str) -> Result<()> {
        let removed = self.store.update(|c| c.remove(name))?;
        eprintln!("kk: removed profile {}", removed.display_name());
        Ok(())
    }

    pub fn context(&self, target: Option<String>) -> Result<()> {
        let mut shell = Shell::require()?;
        let main = self.main_config()?;
        if main.contexts.is_empty() {
            bail!("no contexts in {}", self.paths.main_kubeconfig.display());
        }
        let catalogue = self.store.load()?;

        let context = match target {
            Some(name) => main.context(&name).ok_or_else(|| {
                anyhow!(
                    "no context '{name}' in {}",
                    self.paths.main_kubeconfig.display()
                )
            })?,
            None => {
                let ordered = self.ordered_contexts(&main, &catalogue);
                let items: Vec<String> = ordered.iter().map(|c| c.name.clone()).collect();
                let index = picker::pick("context", &items)?.ok_or(Cancelled)?;
                ordered[index]
            }
        };

        // Prefer the namespace last used with this context, then the context's own.
        let namespace = catalogue
            .profiles
            .iter()
            .filter(|p| p.context == context.name && p.user.is_none())
            .max_by_key(|p| p.last_used)
            .map(|p| p.namespace.clone())
            .or_else(|| context.context.namespace.clone())
            .unwrap_or_else(|| "default".to_owned());

        let key = Key {
            context: context.name.clone(),
            namespace,
            user: None,
        };
        self.switch(&mut shell, key)?;
        shell.finish()
    }

    pub fn namespace(&self, target: Option<String>, refresh: bool) -> Result<()> {
        let mut shell = Shell::require()?;
        let main = self.main_config()?;
        let current = self
            .current_key(Some(shell.pid), &main)?
            .ok_or_else(|| anyhow!("no current context; pick one with `kk context` first"))?;

        let namespace = match target {
            Some(ns) => ns,
            None => {
                let items = self.namespace_candidates(&current, refresh)?;
                let index = picker::pick("namespace", &items)?.ok_or(Cancelled)?;
                items[index].clone()
            }
        };

        let key = Key {
            namespace,
            ..current
        };
        self.switch(&mut shell, key)?;
        shell.finish()
    }

    pub fn clean(&self, force: bool) -> Result<()> {
        let cleanup = self.remove_dead_shell_files(force)?;
        eprintln!("kk: removed {} stale file(s)", cleanup.removed);
        for (overlay, entries) in &cleanup.kept {
            eprintln!(
                "kk: warning: kept {} because it holds {}",
                overlay.display(),
                entries.join(", ")
            );
        }
        if !cleanup.kept.is_empty() {
            eprintln!(
                "kk: move those entries into {} (inspect with `kubectl --kubeconfig <overlay> config view --raw`), or run `kk clean --force` to discard them",
                self.paths.main_kubeconfig.display()
            );
        }
        Ok(())
    }

    pub fn exit(&self, force: bool) -> Result<()> {
        let mut shell = Shell::require()?;
        let overlay = self.paths.overlay_file(shell.pid);
        if !force {
            let entries = foreign_entries(&overlay)?;
            if !entries.is_empty() {
                bail!(
                    "not exiting: this shell's overlay holds {}, probably added by another tool, which would be lost.\n\
                     Move them into {} (inspect with `kubectl --kubeconfig {} config view --raw`), or run `kk exit --force` to discard them",
                    entries.join(", "),
                    self.paths.main_kubeconfig.display(),
                    overlay.display()
                );
            }
        }
        let state_file = self.paths.state_file(shell.pid);
        let state = ShellState::load(&state_file)?;
        for name in &state.env_keys {
            shell.unset(name);
        }
        shell.unset("KUBECONFIG");
        shell.unset("KK_PROFILE");
        remove_if_exists(&overlay)?;
        remove_if_exists(&state_file)?;
        eprintln!("kk: back to {}", self.paths.main_kubeconfig.display());
        shell.finish()
    }

    pub fn init(&self, kind: ShellKind, aliases: bool) -> Result<()> {
        print!("{}", init_script(kind, aliases));
        Ok(())
    }

    pub fn refresh_namespaces(&self, context: &str) -> Result<()> {
        namespaces::fetch_and_cache(&self.paths, context).map(|_| ())
    }

    /// Points the shell at `key`: writes its overlay, records the use, and exports
    /// `KUBECONFIG`, `KK_PROFILE` and the profile's `env`.
    fn switch(&self, shell: &mut Shell, key: Key) -> Result<()> {
        let main = self.main_config()?;
        let Some(context) = main.context(&key.context) else {
            bail!(
                "context '{}' is not in {} (the profile may be stale)",
                key.context,
                self.paths.main_kubeconfig.display()
            );
        };

        self.paths.ensure_runtime_dir()?;
        let overlay = self.paths.overlay_file(shell.pid);
        let state_file = self.paths.state_file(shell.pid);
        let mut state = ShellState::load(&state_file)?;
        let previous = self.overlay_key(shell.pid, &main)?;
        let unchanged = previous.as_ref() == Some(&key);

        let profile = self.store.update(|c| {
            Ok(if unchanged {
                c.ensure(&key)
            } else {
                c.record_use(&key, self.now)
            })
        })?;
        validate_env(&profile.env)?;

        let shadow = NamedContext {
            name: context.name.clone(),
            context: ContextFields {
                cluster: context.context.cluster.clone(),
                user: key
                    .user
                    .clone()
                    .unwrap_or_else(|| context.context.user.clone()),
                namespace: Some(key.namespace.clone()),
            },
        };
        for warning in write_overlay(&overlay, &shadow)? {
            eprintln!("kk: warning: {warning}");
        }

        if !unchanged && let Some(previous) = previous {
            state.previous = Some(previous);
        }
        for name in &state.env_keys {
            if !profile.env.contains_key(name) {
                shell.unset(name);
            }
        }
        for (name, value) in &profile.env {
            shell.export(name, value);
        }
        state.env_keys = profile.env.keys().cloned().collect();
        state.save(&state_file)?;

        let kubeconfig =
            std::env::join_paths([overlay.as_path(), self.paths.main_kubeconfig.as_path()])?;
        shell.export("KUBECONFIG", &kubeconfig.to_string_lossy());
        shell.export("KK_PROFILE", &profile.display_name());

        match &profile.name {
            Some(name) => eprintln!("kk: switched to {name} ({})", key.derived_name()),
            None => eprintln!("kk: switched to {}", key.derived_name()),
        }

        // Opportunistic cleanup; failures here shouldn't fail the switch.
        let _ = self.remove_dead_shell_files(false);
        Ok(())
    }

    fn main_config(&self) -> Result<KubeConfig> {
        KubeConfig::load(&self.paths.main_kubeconfig)
    }

    /// The profile selected by this shell's overlay, if the shell is using it.
    fn overlay_key(&self, pid: u32, main: &KubeConfig) -> Result<Option<Key>> {
        let overlay = self.paths.overlay_file(pid);
        let active = std::env::var_os("KUBECONFIG")
            .and_then(|v| std::env::split_paths(&v).next())
            .is_some_and(|first| first == overlay);
        if !active {
            return Ok(None);
        }
        Ok(read_overlay_current(&overlay)?.map(|shadow| {
            let main_user = main.context(&shadow.name).map(|c| c.context.user.as_str());
            Key {
                user: (main_user != Some(shadow.context.user.as_str()))
                    .then_some(shadow.context.user),
                namespace: shadow
                    .context
                    .namespace
                    .unwrap_or_else(|| "default".to_owned()),
                context: shadow.name,
            }
        }))
    }

    /// The PID of the overlay that `KUBECONFIG` currently starts with, if any.
    fn active_overlay_pid(&self) -> Option<u32> {
        let kubeconfig = std::env::var_os("KUBECONFIG")?;
        let first = std::env::split_paths(&kubeconfig).next()?;
        if first.parent() != Some(self.paths.runtime_dir.as_path()) {
            return None;
        }
        first
            .file_name()?
            .to_str()?
            .strip_prefix("overlay.")?
            .parse()
            .ok()
    }

    /// The shell's current profile: its overlay if active, else the main config's
    /// current context.
    fn current_key(&self, pid: Option<u32>, main: &KubeConfig) -> Result<Option<Key>> {
        if let Some(pid) = pid
            && let Some(key) = self.overlay_key(pid, main)?
        {
            return Ok(Some(key));
        }
        Ok(main.current().map(|c| Key {
            context: c.name.clone(),
            namespace: c
                .context
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_owned()),
            user: None,
        }))
    }

    fn pick_profile(&self, pid: u32) -> Result<Key> {
        let catalogue = self.store.load()?;
        if catalogue.profiles.is_empty() {
            bail!("no profiles yet; switch with `kk context` or `kk namespace` to record some");
        }
        let main = self.main_config()?;
        let current = self.current_key(Some(pid), &main)?;
        let ordered = catalogue.ordered(self.now);

        let width = ordered
            .iter()
            .map(|p| p.display_name().chars().count())
            .max()
            .unwrap_or(0);
        let items: Vec<String> = ordered
            .iter()
            .map(|p| {
                let marker = if current.as_ref() == Some(&p.key()) {
                    '*'
                } else {
                    ' '
                };
                let mut line = format!("{marker} {:<width$}", p.display_name());
                if p.name.is_some() {
                    line.push_str(&format!("  {}", p.key().derived_name()));
                }
                if main.context(&p.context).is_none() {
                    line.push_str("  [stale]");
                }
                line.trim_end().to_owned()
            })
            .collect();

        let index = picker::pick("profile", &items)?.ok_or(Cancelled)?;
        Ok(ordered[index].key())
    }

    /// Contexts ordered by how they rank in the catalogue, then the rest in file order.
    fn ordered_contexts<'a>(
        &self,
        main: &'a KubeConfig,
        catalogue: &Catalogue,
    ) -> Vec<&'a NamedContext> {
        let mut ordered: Vec<&NamedContext> = Vec::new();
        for profile in catalogue.ordered(self.now) {
            if let Some(ctx) = main.context(&profile.context)
                && !ordered.iter().any(|c| c.name == ctx.name)
            {
                ordered.push(ctx);
            }
        }
        for ctx in &main.contexts {
            if !ordered.iter().any(|c| c.name == ctx.name) {
                ordered.push(ctx);
            }
        }
        ordered
    }

    /// Namespaces already used with `context`, in frecency order.
    fn used_namespaces(&self, context: &str) -> Result<Vec<String>> {
        let catalogue = self.store.load()?;
        let mut items: Vec<String> = Vec::new();
        for profile in catalogue.ordered(self.now) {
            if profile.context == context && !items.contains(&profile.namespace) {
                items.push(profile.namespace.clone());
            }
        }
        Ok(items)
    }

    /// Namespaces for the picker: those already used with this context (in frecency
    /// order), then the rest of the cluster's namespaces.
    fn namespace_candidates(&self, current: &Key, refresh: bool) -> Result<Vec<String>> {
        let mut items = self.used_namespaces(&current.context)?;

        match namespaces::list(&self.paths, &current.context, refresh) {
            Ok(listed) => {
                for ns in listed {
                    if !items.contains(&ns) {
                        items.push(ns);
                    }
                }
            }
            Err(e) if !items.is_empty() => eprintln!("kk: warning: {e:#}"),
            Err(e) => return Err(e.context("pass the namespace as an argument instead")),
        }
        Ok(items)
    }

    /// Profile completions: `(value, description)` in picker order. Named profiles
    /// complete by name, described by their `<context>/<namespace>`.
    pub fn complete_profiles(&self) -> Result<Vec<(String, Option<String>)>> {
        let catalogue = self.store.load()?;
        Ok(catalogue
            .ordered(self.now)
            .into_iter()
            .map(|p| match &p.name {
                Some(name) => (name.clone(), Some(p.key().derived_name())),
                None => (p.key().derived_name(), None),
            })
            .collect())
    }

    /// Context completions, most used first.
    pub fn complete_contexts(&self) -> Result<Vec<String>> {
        let main = self.main_config()?;
        let catalogue = self.store.load()?;
        Ok(self
            .ordered_contexts(&main, &catalogue)
            .into_iter()
            .map(|c| c.name.clone())
            .collect())
    }

    /// Namespace completions for the shell's current context: namespaces already used
    /// with it, then the cached list. Never waits on kubectl.
    pub fn complete_namespaces(&self) -> Result<Vec<String>> {
        let main = self.main_config()?;
        let Some(current) = self.current_key(self.active_overlay_pid(), &main)? else {
            return Ok(Vec::new());
        };
        let mut items = self.used_namespaces(&current.context)?;
        for ns in namespaces::cached(&self.paths, &current.context) {
            if !items.contains(&ns) {
                items.push(ns);
            }
        }
        Ok(items)
    }

    /// Removes overlays and state files belonging to shells that no longer exist.
    /// Unless `force` is set, overlays holding entries kk didn't write are kept,
    /// since deleting them would lose those entries.
    fn remove_dead_shell_files(&self, force: bool) -> Result<Cleanup> {
        let mut cleanup = Cleanup::default();
        let entries = match fs::read_dir(&self.paths.runtime_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(cleanup),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(pid) = shell_file_pid(&name.to_string_lossy()) else {
                continue;
            };
            if process_alive(pid) {
                continue;
            }
            let path = entry.path();
            if !force && name.to_string_lossy().starts_with("overlay.") {
                let foreign = foreign_entries(&path).unwrap_or_else(|e| {
                    // Unreadable: keep it rather than risk losing something.
                    vec![format!("content kk can't parse ({e:#})")]
                });
                if !foreign.is_empty() {
                    cleanup.kept.push((path, foreign));
                    continue;
                }
            }
            remove_if_exists(&path)?;
            cleanup.removed += 1;
        }
        Ok(cleanup)
    }
}

#[derive(Default)]
struct Cleanup {
    removed: usize,
    /// Overlays kept because they hold entries kk didn't write, with descriptions.
    kept: Vec<(std::path::PathBuf, Vec<String>)>,
}

/// Extracts the PID from `overlay.<pid>` or `state.<pid>.json`.
fn shell_file_pid(file_name: &str) -> Option<u32> {
    file_name
        .strip_prefix("overlay.")
        .or_else(|| file_name.strip_prefix("state.")?.strip_suffix(".json"))?
        .parse()
        .ok()
}

fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // Signal 0 checks for existence without sending anything. EPERM means the process
    // exists but belongs to someone else.
    let exists = unsafe { libc::kill(pid, 0) } == 0;
    exists || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn remove_if_exists(path: &std::path::Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

fn humanize_age(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let age = now - then;
    if age.num_minutes() < 1 {
        "just now".to_owned()
    } else if age.num_hours() < 1 {
        format!("{}m ago", age.num_minutes())
    } else if age.num_days() < 1 {
        format!("{}h ago", age.num_hours())
    } else {
        format!("{}d ago", age.num_days())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shell_file_names() {
        assert_eq!(shell_file_pid("overlay.123"), Some(123));
        assert_eq!(shell_file_pid("state.123.json"), Some(123));
        assert_eq!(shell_file_pid("state.123"), None);
        assert_eq!(shell_file_pid(".overlay.123.tmp.5"), None);
        assert_eq!(shell_file_pid("profiles.yaml"), None);
    }
}

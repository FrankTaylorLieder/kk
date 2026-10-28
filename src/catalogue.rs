//! The global catalogue of profiles (`profiles.yaml`) and its frecency ordering.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::paths::write_atomic;

/// Profiles used within this window form the "recent" tier, ordered purely by recency.
pub const RECENT_WINDOW: Duration = Duration::hours(1);
/// When the sum of all use counts exceeds this, every count is scaled down.
pub const AGING_THRESHOLD: u64 = 1000;
const AGING_FACTOR: f64 = 0.9;

/// Words that can't be profile names because they are `kk profile` subcommands.
const RESERVED_NAMES: &[&str] = &["name", "list", "ls", "rm", "current"];

/// What identifies a profile: at most one catalogue entry exists per key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub context: String,
    pub namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl Key {
    /// The name used for a profile that hasn't been named: `<context>/<namespace>`,
    /// plus `@<user>` when the profile overrides the context's user.
    pub fn derived_name(&self) -> String {
        match &self.user {
            Some(user) => format!("{}/{}@{}", self.context, self.namespace, user),
            None => format!("{}/{}", self.context, self.namespace),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub context: String,
    pub namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<DateTime<Utc>>,
    #[serde(default)]
    pub use_count: u64,
}

impl Profile {
    pub fn new(key: Key) -> Self {
        Profile {
            name: None,
            context: key.context,
            namespace: key.namespace,
            user: key.user,
            env: BTreeMap::new(),
            last_used: None,
            use_count: 0,
        }
    }

    pub fn key(&self) -> Key {
        Key {
            context: self.context.clone(),
            namespace: self.namespace.clone(),
            user: self.user.clone(),
        }
    }

    pub fn display_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.key().derived_name())
    }

    fn is_key(&self, key: &Key) -> bool {
        self.context == key.context && self.namespace == key.namespace && self.user == key.user
    }

    /// The frecency score used for profiles outside the recent tier.
    pub fn score(&self, now: DateTime<Utc>) -> f64 {
        let weight = match self.last_used {
            None => 0.1,
            Some(t) => {
                let age = now - t;
                if age < Duration::days(1) {
                    1.0
                } else if age < Duration::weeks(1) {
                    0.5
                } else if age < Duration::days(30) {
                    0.25
                } else {
                    0.1
                }
            }
        };
        self.use_count as f64 * weight
    }

    fn is_recent(&self, now: DateTime<Utc>) -> bool {
        self.last_used.is_some_and(|t| now - t < RECENT_WINDOW)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Catalogue {
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

impl Catalogue {
    pub fn profile(&self, key: &Key) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.is_key(key))
    }

    fn get_or_insert(&mut self, key: &Key) -> &mut Profile {
        let index = match self.profiles.iter().position(|p| p.is_key(key)) {
            Some(i) => i,
            None => {
                self.profiles.push(Profile::new(key.clone()));
                self.profiles.len() - 1
            }
        };
        &mut self.profiles[index]
    }

    /// Finds a profile by its name, or by its derived `<context>/<namespace>` name.
    pub fn resolve(&self, name: &str) -> Option<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.name.as_deref() == Some(name))
            .or_else(|| {
                self.profiles
                    .iter()
                    .find(|p| p.key().derived_name() == name)
            })
    }

    /// Records a switch to `key`: upserts it, bumps its count and timestamp, then ages
    /// all counts if needed. Returns the (possibly new) profile.
    pub fn record_use(&mut self, key: &Key, now: DateTime<Utc>) -> Profile {
        let profile = self.get_or_insert(key);
        profile.last_used = Some(now);
        profile.use_count += 1;
        let result = profile.clone();
        self.age();
        result
    }

    /// Like `record_use`, but doesn't count as a use. Returns the profile for `key`.
    pub fn ensure(&mut self, key: &Key) -> Profile {
        self.get_or_insert(key).clone()
    }

    /// Scales counts down once their total exceeds the threshold, so old habits fade.
    /// Unnamed profiles whose count reaches zero are dropped; named ones are kept.
    pub fn age(&mut self) {
        let total: u64 = self.profiles.iter().map(|p| p.use_count).sum();
        if total <= AGING_THRESHOLD {
            return;
        }
        for p in &mut self.profiles {
            p.use_count = (p.use_count as f64 * AGING_FACTOR).floor() as u64;
            if p.name.is_some() {
                p.use_count = p.use_count.max(1);
            }
        }
        self.profiles
            .retain(|p| p.name.is_some() || p.use_count > 0);
    }

    pub fn set_name(&mut self, key: &Key, name: &str) -> Result<Profile> {
        validate_name(name)?;
        if let Some(other) = self
            .profiles
            .iter()
            .find(|p| p.name.as_deref() == Some(name))
            && !other.is_key(key)
        {
            bail!(
                "a profile named '{name}' already exists ({})",
                other.key().derived_name()
            );
        }
        let profile = self.get_or_insert(key);
        profile.name = Some(name.to_owned());
        Ok(profile.clone())
    }

    pub fn remove(&mut self, name: &str) -> Result<Profile> {
        let Some(key) = self.resolve(name).map(Profile::key) else {
            bail!("no profile named '{name}'");
        };
        let index = self.profiles.iter().position(|p| p.is_key(&key)).unwrap();
        Ok(self.profiles.remove(index))
    }

    /// Profiles in picker order: the recent tier first (newest first), then the rest by
    /// frecency score, ties broken by recency.
    pub fn ordered(&self, now: DateTime<Utc>) -> Vec<&Profile> {
        let mut profiles: Vec<&Profile> = self.profiles.iter().collect();
        profiles.sort_by(|a, b| compare(a, b, now));
        profiles
    }
}

fn compare(a: &Profile, b: &Profile, now: DateTime<Utc>) -> Ordering {
    let by_recency = || b.last_used.cmp(&a.last_used);
    match (a.is_recent(now), b.is_recent(now)) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (true, true) => by_recency(),
        (false, false) => b.score(now).total_cmp(&a.score(now)).then_with(by_recency),
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("profile names can't be empty");
    }
    if name.contains('/') {
        bail!("profile names can't contain '/' (reserved for <context>/<namespace> names)");
    }
    if name.starts_with('-') {
        bail!("profile names can't start with '-'");
    }
    if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("profile names can't contain whitespace");
    }
    if RESERVED_NAMES.contains(&name) {
        bail!("'{name}' is reserved for a `kk profile` subcommand");
    }
    Ok(())
}

/// The catalogue file, guarded by an advisory lock because many shells share it.
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Store { path }
    }

    fn lock_file(&self, exclusive: bool) -> Result<File> {
        let lock_path = self.path.with_extension("lock");
        if let Some(dir) = lock_path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        if exclusive {
            file.lock()?;
        } else {
            file.lock_shared()?;
        }
        Ok(file)
    }

    fn read(&self) -> Result<Catalogue> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Catalogue::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        if text.trim().is_empty() {
            return Ok(Catalogue::default());
        }
        serde_norway::from_str(&text).with_context(|| format!("parsing {}", self.path.display()))
    }

    pub fn load(&self) -> Result<Catalogue> {
        let _lock = self.lock_file(false)?;
        self.read()
    }

    /// Loads, modifies and saves the catalogue while holding an exclusive lock.
    pub fn update<T>(&self, f: impl FnOnce(&mut Catalogue) -> Result<T>) -> Result<T> {
        let _lock = self.lock_file(true)?;
        let mut catalogue = self.read()?;
        let result = f(&mut catalogue)?;
        let text = serde_norway::to_string(&catalogue)?;
        write_atomic(&self.path, text.as_bytes(), 0o600)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ctx: &str, ns: &str) -> Key {
        Key {
            context: ctx.into(),
            namespace: ns.into(),
            user: None,
        }
    }

    fn profile(ctx: &str, ns: &str, count: u64, age: Duration, now: DateTime<Utc>) -> Profile {
        Profile {
            use_count: count,
            last_used: Some(now - age),
            ..Profile::new(key(ctx, ns))
        }
    }

    fn order(c: &Catalogue, now: DateTime<Utc>) -> Vec<String> {
        c.ordered(now).iter().map(|p| p.display_name()).collect()
    }

    #[test]
    fn recent_tier_is_ordered_by_recency_regardless_of_count() {
        let now = Utc::now();
        let c = Catalogue {
            profiles: vec![
                profile("c", "busy", 500, Duration::hours(2), now),
                profile("c", "older", 50, Duration::minutes(30), now),
                profile("c", "newest", 1, Duration::minutes(5), now),
            ],
        };
        assert_eq!(order(&c, now), ["c/newest", "c/older", "c/busy"]);
    }

    #[test]
    fn frequency_wins_as_profiles_age() {
        let now = Utc::now();
        let c = Catalogue {
            profiles: vec![
                profile("c", "once-yesterday", 1, Duration::hours(20), now),
                profile("c", "often-last-week", 20, Duration::days(3), now),
                profile("c", "ancient-but-huge", 200, Duration::days(90), now),
                profile("c", "ancient-rare", 2, Duration::days(90), now),
            ],
        };
        assert_eq!(
            order(&c, now),
            [
                "c/ancient-but-huge",
                "c/often-last-week",
                "c/once-yesterday",
                "c/ancient-rare"
            ]
        );
    }

    #[test]
    fn score_ties_break_by_recency() {
        let now = Utc::now();
        let c = Catalogue {
            profiles: vec![
                profile("c", "a", 4, Duration::hours(10), now),
                profile("c", "b", 4, Duration::hours(5), now),
            ],
        };
        assert_eq!(order(&c, now), ["c/b", "c/a"]);
    }

    #[test]
    fn record_use_upserts_and_counts() {
        let now = Utc::now();
        let mut c = Catalogue::default();
        c.record_use(&key("c", "a"), now);
        let p = c.record_use(&key("c", "a"), now);
        assert_eq!(p.use_count, 2);
        assert_eq!(c.profiles.len(), 1);
    }

    #[test]
    fn aging_scales_counts_and_prunes_unnamed() {
        let now = Utc::now();
        let mut c = Catalogue {
            profiles: vec![
                profile("c", "big", 1000, Duration::days(1), now),
                profile("c", "unnamed-one", 1, Duration::days(1), now),
                Profile {
                    name: Some("keep".into()),
                    ..profile("c", "named-one", 1, Duration::days(1), now)
                },
            ],
        };
        c.age();
        let counts: Vec<_> = c
            .profiles
            .iter()
            .map(|p| (p.display_name(), p.use_count))
            .collect();
        assert_eq!(
            counts,
            [("c/big".to_string(), 900), ("keep".to_string(), 1)]
        );
    }

    #[test]
    fn resolve_by_name_or_derived_name() {
        let now = Utc::now();
        let mut c = Catalogue::default();
        c.record_use(&key("prod", "team-a"), now);
        c.set_name(&key("prod", "team-a"), "pa").unwrap();
        assert_eq!(c.resolve("pa").unwrap().namespace, "team-a");
        assert_eq!(
            c.resolve("prod/team-a").unwrap().name.as_deref(),
            Some("pa")
        );
        assert!(c.resolve("prod/team-b").is_none());
    }

    #[test]
    fn names_are_validated_and_unique() {
        let mut c = Catalogue::default();
        assert!(c.set_name(&key("c", "a"), "has/slash").is_err());
        assert!(c.set_name(&key("c", "a"), "list").is_err());
        assert!(c.set_name(&key("c", "a"), "-x").is_err());
        c.set_name(&key("c", "a"), "mine").unwrap();
        assert!(c.set_name(&key("c", "b"), "mine").is_err());
        // Renaming the same profile is fine.
        c.set_name(&key("c", "a"), "mine").unwrap();
    }

    #[test]
    fn store_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("profiles.yaml"));
        store
            .update(|c| {
                c.record_use(&key("c", "a"), Utc::now());
                Ok(())
            })
            .unwrap();
        assert_eq!(store.load().unwrap().profiles.len(), 1);
    }
}

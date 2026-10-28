//! Listing a cluster's namespaces, with a per-context cache so pickers open instantly.
//!
//! A cached list is always used straight away. If it is older than `STALE_AFTER`, a
//! detached `kk refresh-namespaces` process updates it in the background for next time.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::paths::{Paths, write_atomic};

const STALE_AFTER: Duration = Duration::minutes(5);

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    fetched_at: DateTime<Utc>,
    namespaces: Vec<String>,
}

/// Returns the namespaces of `context`'s cluster, from the cache when possible.
pub fn list(paths: &Paths, context: &str, refresh: bool) -> Result<Vec<String>> {
    let cache_file = cache_file(paths, context);
    if !refresh && let Some(entry) = read_cache(&cache_file) {
        if Utc::now() - entry.fetched_at > STALE_AFTER {
            spawn_background_refresh(context);
        }
        return Ok(entry.namespaces);
    }
    fetch_and_cache(paths, context)
}

/// Fetches namespaces with kubectl and updates the cache.
pub fn fetch_and_cache(paths: &Paths, context: &str) -> Result<Vec<String>> {
    let namespaces = fetch(&paths.main_kubeconfig, context)?;
    let entry = CacheEntry {
        fetched_at: Utc::now(),
        namespaces,
    };
    write_atomic(
        &cache_file(paths, context),
        serde_json::to_string(&entry)?.as_bytes(),
        0o600,
    )?;
    Ok(entry.namespaces)
}

fn fetch(main_kubeconfig: &Path, context: &str) -> Result<Vec<String>> {
    // The context's cluster and credentials live in the main config, so query through
    // it directly rather than through any overlay.
    let output = Command::new("kubectl")
        .env("KUBECONFIG", main_kubeconfig)
        .args(["--context", context, "--request-timeout=10s"])
        .args(["get", "namespaces", "-o", "name"])
        .stdin(Stdio::null())
        .output()
        .context("running kubectl")?;
    if !output.status.success() {
        // kubectl logs every retry in klog format ("E1028 15:42:14.97 ..."); prefer
        // its plain summary line.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
        let reason = lines
            .iter()
            .rev()
            .find(|l| !is_klog_line(l))
            .or(lines.last())
            .map_or("kubectl failed", |l| l.trim());
        bail!("listing namespaces for context '{context}' failed: {reason}");
    }
    let mut namespaces: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().trim_start_matches("namespace/").to_owned())
        .filter(|ns| !ns.is_empty())
        .collect();
    namespaces.sort();
    Ok(namespaces)
}

fn is_klog_line(line: &str) -> bool {
    let mut chars = line.chars();
    matches!(chars.next(), Some('I' | 'W' | 'E' | 'F'))
        && chars.take(4).filter(char::is_ascii_digit).count() == 4
}

fn spawn_background_refresh(context: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // Best effort: the child outlives us and nobody waits for it.
    let _ = Command::new(exe)
        .args(["refresh-namespaces", context])
        .env_remove("KK_EVAL_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn read_cache(path: &Path) -> Option<CacheEntry> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn cache_file(paths: &Paths, context: &str) -> PathBuf {
    paths
        .cache_dir
        .join("namespaces")
        .join(format!("{}.json", encode_file_name(context)))
}

/// Context names can contain `/`, `:` and so on (e.g. EKS ARNs), so percent-encode
/// anything that isn't safe in a file name.
fn encode_file_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_awkward_context_names() {
        assert_eq!(
            encode_file_name("arn:aws:eks:eu-west-1:123:cluster/prod"),
            "arn%3Aaws%3Aeks%3Aeu-west-1%3A123%3Acluster%2Fprod"
        );
        assert_eq!(encode_file_name("plain-name_1.x"), "plain-name_1.x");
    }
}

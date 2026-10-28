use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

/// Filesystem locations used by kk. Each can be overridden with an environment
/// variable, which is mainly useful for tests.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The single source of truth: `~/.kube/config`.
    pub main_kubeconfig: PathBuf,
    /// Holds `profiles.yaml`.
    pub config_dir: PathBuf,
    /// Holds per-shell overlays and state. Private to the user.
    pub runtime_dir: PathBuf,
    /// Holds the per-context namespace cache.
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;

        let main_kubeconfig =
            env_path("KK_MAIN_KUBECONFIG").unwrap_or_else(|| home.join(".kube").join("config"));
        let config_dir = env_path("KK_CONFIG_DIR")
            .or_else(|| dirs::config_dir().map(|d| d.join("kk")))
            .unwrap_or_else(|| home.join(".config").join("kk"));
        let runtime_dir = env_path("KK_RUNTIME_DIR").unwrap_or_else(default_runtime_dir);
        let cache_dir = env_path("KK_CACHE_DIR")
            .or_else(|| dirs::cache_dir().map(|d| d.join("kk")))
            .unwrap_or_else(|| home.join(".cache").join("kk"));

        Ok(Paths {
            main_kubeconfig,
            config_dir,
            runtime_dir,
            cache_dir,
        })
    }

    pub fn profiles_file(&self) -> PathBuf {
        self.config_dir.join("profiles.yaml")
    }

    pub fn overlay_file(&self, pid: u32) -> PathBuf {
        self.runtime_dir.join(format!("overlay.{pid}"))
    }

    pub fn state_file(&self, pid: u32) -> PathBuf {
        self.runtime_dir.join(format!("state.{pid}.json"))
    }

    /// Creates the runtime directory if needed and makes sure only the user can read it.
    pub fn ensure_runtime_dir(&self) -> Result<()> {
        ensure_private_dir(&self.runtime_dir)
    }
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn default_runtime_dir() -> PathBuf {
    match dirs::runtime_dir() {
        Some(dir) => dir.join("kk"),
        // No XDG_RUNTIME_DIR (e.g. macOS): fall back to a per-user directory in the temp dir.
        None => {
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("kk-{uid}"))
        }
    }
}

pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("setting permissions on {}", dir.display()))?;
    Ok(())
}

/// Writes `contents` to `path` atomically (temp file + rename) with the given mode.
pub fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;

    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

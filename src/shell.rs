//! Shell integration.
//!
//! A child process can't change its parent shell's environment, so the `kk` shell
//! function (printed by `kk init`) runs the binary with `KK_SHELL_PID` and
//! `KK_EVAL_FILE` set, then sources whatever shell code the binary wrote to the eval
//! file. Normal output (lists, pickers, messages) is unaffected.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::catalogue::Key;
use crate::paths::write_atomic;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ShellKind {
    Bash,
    Zsh,
}

/// The calling shell, available when kk is run through the shell function.
pub struct Shell {
    pub pid: u32,
    eval_file: PathBuf,
    lines: Vec<String>,
}

impl Shell {
    /// Returns the calling shell, or an error explaining how to set up the integration.
    pub fn require() -> Result<Self> {
        Self::detect()?.ok_or_else(|| {
            anyhow!(
                "this command changes your shell's environment, so it needs kk's shell integration.\n\
                 Add this to your ~/.zshrc or ~/.bashrc:\n\n    eval \"$(command kk init zsh)\"   # or bash"
            )
        })
    }

    pub fn detect() -> Result<Option<Self>> {
        let (Some(pid), Some(eval_file)) = (
            std::env::var("KK_SHELL_PID").ok().filter(|v| !v.is_empty()),
            std::env::var_os("KK_EVAL_FILE").filter(|v| !v.is_empty()),
        ) else {
            return Ok(None);
        };
        let pid = pid
            .parse()
            .with_context(|| format!("invalid KK_SHELL_PID '{pid}'"))?;
        Ok(Some(Shell {
            pid,
            eval_file: eval_file.into(),
            lines: Vec::new(),
        }))
    }

    pub fn export(&mut self, name: &str, value: &str) {
        self.lines.push(format!("export {name}={}", quote(value)));
    }

    pub fn unset(&mut self, name: &str) {
        self.lines.push(format!("unset {name}"));
    }

    /// Writes the collected shell code for the wrapper function to source.
    pub fn finish(self) -> Result<()> {
        if self.lines.is_empty() {
            return Ok(());
        }
        let mut text = self.lines.join("\n");
        text.push('\n');
        fs::write(&self.eval_file, text)
            .with_context(|| format!("writing {}", self.eval_file.display()))
    }
}

/// Quotes a value for POSIX shells.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

pub fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Per-shell state kept next to the overlay.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ShellState {
    /// The profile this shell was on before the current one, for `kk profile -`.
    #[serde(default)]
    pub previous: Option<Key>,
    /// Environment variables set from the current profile's `env`, so they can be
    /// unset when switching away.
    #[serde(default)]
    pub env_keys: BTreeSet<String>,
}

impl ShellState {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        write_atomic(path, serde_json::to_string(self)?.as_bytes(), 0o600)
    }
}

/// The snippet `kk init` prints for `eval` in a shell rc file.
pub fn init_script(kind: ShellKind, aliases: bool) -> String {
    // The same function works in bash and zsh.
    let mut script = String::from(
        r#"kk() {
  local __kk_eval __kk_rc
  __kk_eval=$(mktemp "${TMPDIR:-/tmp}/kk-eval.XXXXXX") || return
  KK_SHELL_PID=$$ KK_EVAL_FILE="$__kk_eval" command kk "$@"
  __kk_rc=$?
  if [ -s "$__kk_eval" ]; then
    . "$__kk_eval"
  fi
  rm -f "$__kk_eval"
  return $__kk_rc
}
"#,
    );
    // Tab completion calls back into the binary (see `completion.rs`). Registering it
    // for the name `kk` covers the function too, and zsh also completes the aliases.
    script.push_str(match kind {
        ShellKind::Bash => "eval \"$(KK_COMPLETE=bash command kk)\"\n",
        ShellKind::Zsh => {
            r#"if (( $+functions[compdef] )); then
  eval "$(KK_COMPLETE=zsh command kk)"
else
  print -u2 "kk: tab completion not enabled: run 'autoload -Uz compinit && compinit' before kk init"
fi
"#
        }
    });
    if aliases {
        script
            .push_str("alias kp='kk profile'\nalias kctx='kk context'\nalias kns='kk namespace'\n");
    }
    script
}

pub fn validate_env(env: &std::collections::BTreeMap<String, String>) -> Result<()> {
    for name in env.keys() {
        if !is_valid_env_name(name) {
            bail!("invalid environment variable name '{name}' in profile");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_single_quotes() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), r#"'it'\''s'"#);
        assert_eq!(quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn env_names() {
        assert!(is_valid_env_name("AWS_PROFILE"));
        assert!(is_valid_env_name("_x1"));
        assert!(!is_valid_env_name("1X"));
        assert!(!is_valid_env_name("A-B"));
        assert!(!is_valid_env_name("A;rm"));
        assert!(!is_valid_env_name(""));
    }
}

//! Interactive pickers: `fzf` when installed, a built-in fuzzy select otherwise.
//!
//! `KK_PICKER` overrides the choice: `builtin` forces the built-in picker, anything
//! else is run with `sh -c` as a filter that reads candidates on stdin and prints the
//! selection on stdout (e.g. `KK_PICKER=sk` or `KK_PICKER="fzf --height 40%"`).
//!
//! Pickers must draw on the terminal, not stdout: `fzf` uses `/dev/tty` and the
//! built-in picker uses stderr.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

enum Picker {
    Builtin,
    Fzf(PathBuf),
    Custom(String),
}

/// Lets the user pick one of `items`, which are shown in the given order. Returns the
/// index of the chosen item, or `None` if the picker was cancelled.
pub fn pick(prompt: &str, items: &[String]) -> Result<Option<usize>> {
    if items.is_empty() {
        bail!("nothing to pick from");
    }
    match choose_picker() {
        Picker::Builtin => pick_builtin(prompt, items),
        Picker::Fzf(fzf) => {
            let mut cmd = Command::new(fzf);
            // Keep kk's ordering for equally good matches; the user's FZF_DEFAULT_OPTS
            // still applies for everything else.
            cmd.arg(format!("--prompt={prompt}> "))
                .arg("--tiebreak=index");
            pick_external(cmd, items)
        }
        Picker::Custom(command) => {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(command);
            pick_external(cmd, items)
        }
    }
}

fn choose_picker() -> Picker {
    match std::env::var("KK_PICKER") {
        Ok(p) if p == "builtin" => Picker::Builtin,
        Ok(p) if !p.trim().is_empty() => Picker::Custom(p),
        _ => match find_in_path("fzf") {
            Some(fzf) => Picker::Fzf(fzf),
            None => Picker::Builtin,
        },
    }
}

fn pick_external(mut cmd: Command, items: &[String]) -> Result<Option<usize>> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("starting picker")?;

    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        let input = items.join("\n") + "\n";
        // The picker may exit before reading everything (e.g. cancelled straight away).
        let _ = stdin.write_all(input.as_bytes());
    }

    let output = child.wait_with_output().context("running picker")?;
    if !output.status.success() {
        // fzf exits 130 when cancelled and 1 when nothing matched.
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(selected) = stdout.lines().next() else {
        return Ok(None);
    };
    Ok(items.iter().position(|item| item == selected))
}

fn pick_builtin(prompt: &str, items: &[String]) -> Result<Option<usize>> {
    let term = dialoguer::console::Term::stderr();
    if !term.is_term() {
        bail!("no terminal available for the picker; pass the value as an argument instead");
    }
    dialoguer::FuzzySelect::new()
        .with_prompt(prompt)
        .items(items)
        .default(0)
        .interact_on_opt(&term)
        .context("running built-in picker")
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

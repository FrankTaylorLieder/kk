//! Dynamic tab completion for context, namespace and profile arguments.
//!
//! The shell calls back into `kk` (with `KK_COMPLETE` set) on each TAB, and clap
//! routes value completion to these functions. They must be fast and quiet, so any
//! error simply yields no candidates.

use clap_complete::CompletionCandidate;

use crate::commands::App;

pub fn profiles() -> Vec<CompletionCandidate> {
    let Ok(profiles) = App::new().and_then(|app| app.complete_profiles()) else {
        return Vec::new();
    };
    profiles
        .into_iter()
        .map(|(value, help)| CompletionCandidate::new(value).help(help.map(Into::into)))
        .collect()
}

pub fn contexts() -> Vec<CompletionCandidate> {
    candidates(App::new().and_then(|app| app.complete_contexts()))
}

pub fn namespaces() -> Vec<CompletionCandidate> {
    candidates(App::new().and_then(|app| app.complete_namespaces()))
}

fn candidates(values: anyhow::Result<Vec<String>>) -> Vec<CompletionCandidate> {
    values
        .unwrap_or_default()
        .into_iter()
        .map(CompletionCandidate::new)
        .collect()
}

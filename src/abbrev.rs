//! Shortening names for display, e.g. `public-local-se-sto-management` → `p-l-s-s-m`.
//!
//! Names are split into segments at `-`, `_`, `.`, `:` and `/`. Working left to right,
//! each segment is cut to the shortest prefix that still identifies the name uniquely
//! among the known names, in the style of powerlevel10k's `truncate_to_unique`.
//!
//! An abbreviation matches a known name when that name has the same separators and
//! each of its segments starts with the corresponding shortened segment. Segments
//! still at full length must match exactly, so the result is always unique.

const SEPARATORS: &[char] = &['-', '_', '.', ':', '/'];

/// Abbreviates `name` so that it can't be confused with any of `known`.
pub fn abbreviate<'a>(name: &str, known: impl IntoIterator<Item = &'a str>) -> String {
    let (segments, separators) = split(name);
    let others: Vec<Vec<&str>> = known
        .into_iter()
        .filter(|other| *other != name)
        .map(split)
        .filter(|(_, seps)| *seps == separators)
        .map(|(segs, _)| segs)
        .collect();

    let full: Vec<usize> = segments.iter().map(|s| s.chars().count()).collect();
    let mut lengths = full.clone();
    for i in 0..segments.len() {
        for len in 1..full[i] {
            lengths[i] = len;
            if is_unique(&segments, &lengths, &others) {
                break;
            }
            lengths[i] = full[i];
        }
    }

    let mut out = String::new();
    for (i, segment) in segments.iter().enumerate() {
        out.extend(segment.chars().take(lengths[i]));
        if let Some(sep) = separators.get(i) {
            out.push(*sep);
        }
    }
    out
}

fn split(name: &str) -> (Vec<&str>, Vec<char>) {
    let separators = name.chars().filter(|c| SEPARATORS.contains(c)).collect();
    (name.split(SEPARATORS).collect(), separators)
}

fn is_unique(segments: &[&str], lengths: &[usize], others: &[Vec<&str>]) -> bool {
    !others.iter().any(|other| {
        segments
            .iter()
            .zip(lengths)
            .zip(other)
            .all(|((segment, &len), candidate)| {
                if len == segment.chars().count() {
                    candidate == segment
                } else {
                    let prefix: String = segment.chars().take(len).collect();
                    candidate.starts_with(&prefix)
                }
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTEXTS: &[&str] = &[
        "public-local-management-se-sto",
        "public-local-se-sto-az1-workload",
        "public-local-se-sto-management",
        "public-local-workload-se-sto-az1",
    ];

    #[test]
    fn shortens_every_segment_when_unambiguous() {
        let short = |name| abbreviate(name, CONTEXTS.iter().copied());
        assert_eq!(short("public-local-se-sto-management"), "p-l-s-s-m");
        assert_eq!(short("public-local-management-se-sto"), "p-l-m-s-s");
        assert_eq!(short("public-local-se-sto-az1-workload"), "p-l-s-s-a-w");
    }

    #[test]
    fn keeps_enough_to_stay_unique() {
        let namespaces = [
            "evroc-operator-system",
            "evroc-operator-webhook",
            "evroc-system",
        ];
        assert_eq!(abbreviate("evroc-operator-system", namespaces), "e-o-s");
        assert_eq!(abbreviate("evroc-system", namespaces), "e-s");

        let names = ["prod-eu", "prod-us", "preprod-eu"];
        assert_eq!(abbreviate("prod-eu", names), "pro-e");
        assert_eq!(abbreviate("preprod-eu", names), "pre-e");
    }

    #[test]
    fn single_segment_names_get_shortest_unique_prefix() {
        let names = ["prod", "staging", "stage2"];
        assert_eq!(abbreviate("prod", names), "p");
        assert_eq!(abbreviate("staging", names), "stagi");
    }

    #[test]
    fn handles_other_separators_and_no_known_names() {
        assert_eq!(
            abbreviate("arn:aws:eks:eu-west-1:123:cluster/prod", []),
            "a:a:e:e-w-1:1:c/p"
        );
        assert_eq!(abbreviate("default", []), "d");
        assert_eq!(abbreviate("a--b", []), "a--b");
    }
}

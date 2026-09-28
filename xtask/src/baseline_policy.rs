//! Accepting chosen public-surface changes into a baseline, and nothing else.
//!
//! `cargo xtask public-api --bless` rewrites a crate's whole baseline from its current surface: every
//! pending change is accepted at once, whether or not anyone looked at it. `--accept` is the
//! reviewed alternative — it reconciles only the baseline lines a fragment names, so a change that
//! was reviewed can be accepted while the ones that were not stay reported.

/// A baseline after the named items were reconciled, and what changed.
#[derive(Debug, PartialEq, Eq)]
pub struct Accepted {
    /// The new baseline: the current surface's order for what it holds, then any line the surface
    /// no longer has and no fragment named, which stays reported as a removal.
    pub baseline: Vec<String>,
    /// Current items that were not in the baseline and now are.
    pub added: Vec<String>,
    /// Baseline items the current surface no longer has, taken out of the baseline.
    pub removed: Vec<String>,
}

/// Reconciles the lines of `baseline` that any of `fragments` names with `current`.
///
/// A line is named when it contains a fragment. A named current item missing from the baseline is
/// added; a named baseline item missing from the current surface is removed — that is how a changed
/// signature is accepted. Everything unnamed is left exactly as it was, so its difference is still
/// reported.
#[must_use]
pub fn accept(baseline: &[String], current: &[String], fragments: &[&str]) -> Accepted {
    let named = |item: &str| fragments.iter().any(|fragment| item.contains(fragment));
    let added: Vec<String> = current
        .iter()
        .filter(|item| named(item) && !baseline.contains(item))
        .cloned()
        .collect();
    let removed: Vec<String> = baseline
        .iter()
        .filter(|item| named(item) && !current.contains(item))
        .cloned()
        .collect();
    let mut kept: Vec<String> = current
        .iter()
        .filter(|item| baseline.contains(item) || added.contains(item))
        .cloned()
        .collect();
    kept.extend(
        baseline
            .iter()
            .filter(|item| !current.contains(item) && !removed.contains(item))
            .cloned(),
    );
    Accepted {
        baseline: kept,
        added,
        removed,
    }
}

/// The fragments that named nothing to reconcile in any crate.
///
/// A fragment that changes nothing is almost always a typo, and accepting nothing while reporting
/// success is the silent pass this command exists to avoid.
#[must_use]
pub fn unused_fragments<'a>(fragments: &[&'a str], changed: &[String]) -> Vec<&'a str> {
    fragments
        .iter()
        .copied()
        .filter(|fragment| !changed.iter().any(|item| item.contains(fragment)))
        .collect()
}

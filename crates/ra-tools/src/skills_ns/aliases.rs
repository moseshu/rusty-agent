// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Root aliases shared by catalog presentation and reads.

use std::collections::HashSet;

#[derive(Clone, Debug)]
struct AliasRoot {
    name: String,
    value: String,
}

/// Stable first-seen root aliases used by skill catalogs and package reads.
#[derive(Clone, Debug)]
pub(crate) struct AliasPlan {
    roots: Vec<AliasRoot>,
}

impl AliasPlan {
    /// Assigns aliases to unique roots in first-seen order.
    #[must_use]
    pub(crate) fn build(prefix: &str, candidates: &[&str]) -> Option<Self> {
        let mut roots = Vec::new();
        let mut seen = HashSet::new();

        for &root in candidates {
            if !seen.insert(root) {
                continue;
            }

            let index = roots.len();
            roots.push(AliasRoot {
                name: format!("{prefix}{index}"),
                value: root.to_string(),
            });
        }

        (!roots.is_empty()).then_some(Self { roots })
    }

    /// Shortens a locator using its longest matching root.
    #[must_use]
    pub(crate) fn shorten(&self, locator: &str) -> Option<String> {
        self.roots
            .iter()
            .filter_map(|root| {
                let suffix = locator
                    .strip_prefix(root.value.trim_end_matches('/'))?
                    .strip_prefix('/')?;
                Some((root, suffix))
            })
            .max_by_key(|(root, _)| root.value.len())
            .map(|(root, suffix)| format!("{}/{suffix}", root.name))
    }

    /// Renders the alias table using the upstream line format.
    #[must_use]
    pub(crate) fn root_lines(&self) -> Vec<String> {
        self.roots
            .iter()
            .map(|root| format!("- `{}` = `{}`", root.name, root.value))
            .collect()
    }
}

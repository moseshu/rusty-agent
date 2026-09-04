//! Prompt inspection, doctor, and dump reporting.

use std::fmt::{self, Write as _};

use ra_core::error::{Error, Result};
use ra_core::prompt::{CachePlan, MIN_CACHEABLE_PREFIX_TOKENS};
use serde::{Deserialize, Serialize};

use crate::assembler::StablePrefix;

/// Horizontal rule of the text report, as wide as the widest row its table can print.
const RULE: &str = "------------------------------------------------------------\
                    --------------------------------------------\n";

/// Opening banner, on [`RULE`]'s width.
const BANNER_TOP: &str = "========================================== PROMPT DUMP REPORT \
                          ==========================================\n";

/// Closing banner, on [`RULE`]'s width.
const BANNER_BOTTOM: &str = "============================================================\
                             ============================================\n";

/// Column width of the `Source` cell.
///
/// Wide enough for the longest provenance a built-in can print — `capability(apply_patch)` — so a
/// capability's row lines up with the product's own rather than pushing every column after it to
/// the right. The committed dump is read as a diff, and a table that reflows when a section changes
/// owner reports the reflow instead of the change.
const SOURCE_WIDTH: usize = 24;

/// Inspection summary of a single prompt section.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptDumpSection {
    name: String,
    purpose: String,
    source: String,
    stability: String,
    position: String,
    content_hash: String,
    token_estimate: usize,
    // Defaulted for the same reason the section's own wire field is: a dump recorded before
    // budgets existed carries no allowance, and `--baseline` has to keep reading it.
    #[serde(default)]
    token_budget: Option<usize>,
    bytes: usize,
}

impl PromptDumpSection {
    /// Creates a new inspection summary for a prompt section.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        purpose: impl Into<String>,
        source: impl Into<String>,
        stability: impl Into<String>,
        position: impl Into<String>,
        content_hash: impl Into<String>,
        token_estimate: usize,
        bytes: usize,
    ) -> Self {
        Self {
            name: name.into(),
            purpose: purpose.into(),
            source: source.into(),
            stability: stability.into(),
            position: position.into(),
            content_hash: content_hash.into(),
            token_estimate,
            token_budget: None,
            bytes,
        }
    }

    /// Records the cached-prefix allowance the section declared.
    #[must_use]
    pub const fn with_token_budget(mut self, budget: usize) -> Self {
        self.token_budget = Some(budget);
        self
    }

    /// Section name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Stated purpose.
    #[must_use]
    pub fn purpose(&self) -> &str {
        &self.purpose
    }

    /// Provenance source.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Stability classification (`stable` / `volatile`).
    #[must_use]
    pub fn stability(&self) -> &str {
        &self.stability
    }

    /// Placement position (`prefix` / `tail_message`).
    #[must_use]
    pub fn position(&self) -> &str {
        &self.position
    }

    /// SHA-256 content hash.
    #[must_use]
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    /// Estimated token count.
    #[must_use]
    pub const fn token_estimate(&self) -> usize {
        self.token_estimate
    }

    /// Declared cached-prefix allowance, if the section states one.
    #[must_use]
    pub const fn token_budget(&self) -> Option<usize> {
        self.token_budget
    }

    /// Size in bytes.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Comprehensive dump of an assembled prompt configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptDump {
    provider: Option<String>,
    model: Option<String>,
    prefix_hash: String,
    total_prefix_tokens: usize,
    sections: Vec<PromptDumpSection>,
    cache_plan: Option<CachePlan>,
}

impl PromptDump {
    /// Constructs a prompt dump from an assembled [`StablePrefix`].
    #[must_use]
    pub fn from_assembled(
        prefix: &StablePrefix,
        cache_plan: Option<CachePlan>,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Self {
        let sections = prefix
            .sections()
            .iter()
            .map(|s| PromptDumpSection {
                name: s.name().as_str().to_string(),
                purpose: s.purpose().to_string(),
                source: s.source().to_string(),
                stability: s.stability().to_string(),
                position: s.position().to_string(),
                content_hash: s.content_hash().to_string(),
                token_estimate: s.token_estimate(),
                token_budget: s.token_budget(),
                bytes: s.content().len(),
            })
            .collect();

        Self {
            provider: provider.map(str::to_owned),
            model: model.map(str::to_owned),
            prefix_hash: prefix.prefix_hash().to_string(),
            total_prefix_tokens: prefix.token_estimate(),
            sections,
            cache_plan,
        }
    }

    /// Target provider name if known.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    /// Target model name if known.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Stable prefix hash.
    #[must_use]
    pub fn prefix_hash(&self) -> &str {
        &self.prefix_hash
    }

    /// Total prefix tokens.
    ///
    /// This is the sum of the section rows rather than an independent estimate of the joined text.
    /// A report whose total does not reconcile with its own rows cannot be used to review a change,
    /// which is the only thing this report is for.
    #[must_use]
    pub const fn total_prefix_tokens(&self) -> usize {
        self.total_prefix_tokens
    }

    /// Section breakdown.
    #[must_use]
    pub fn sections(&self) -> &[PromptDumpSection] {
        &self.sections
    }

    /// Provider cache plan if generated.
    #[must_use]
    pub fn cache_plan(&self) -> Option<&CachePlan> {
        self.cache_plan.as_ref()
    }

    /// Formats the prompt dump as a structured, human-readable text report.
    #[must_use]
    pub fn render_text(&self) -> String {
        // Writing into a `String` cannot fail, so the `fmt::Result` values below are discarded
        // rather than propagated.
        let mut out = String::new();
        out.push_str(BANNER_TOP);
        if let Some(p) = &self.provider {
            let _ = writeln!(out, "Provider: {p}");
        }
        if let Some(m) = &self.model {
            let _ = writeln!(out, "Model:    {m}");
        }
        let _ = writeln!(out, "Stable Prefix Hash:   {}", self.prefix_hash);
        let _ = writeln!(out, "Total Prefix Tokens:  ~{}", self.total_prefix_tokens);
        out.push_str(RULE);
        // `Source` is here because a section's provenance is the one thing a reader cannot recover
        // from the text: it is what says this wording is the product's own rather than material
        // carried in from somewhere else. `Budget` is here because a row's token count only means
        // something next to the allowance it was written against.
        let _ = writeln!(
            out,
            "{:<22} {:<SOURCE_WIDTH$} {:<9} {:<9} {:<7} {:<7} {:<8} Hash Prefix",
            "Section Name", "Source", "Stability", "Position", "Tokens", "Budget", "Bytes"
        );
        out.push_str(RULE);
        for sec in &self.sections {
            let hash_short = if sec.content_hash.len() >= 12 {
                &sec.content_hash[..12]
            } else {
                &sec.content_hash
            };
            // A section that declares no allowance prints as one, rather than borrowing the look of
            // a section that declared a large one.
            let budget = sec
                .token_budget
                .map_or_else(|| "-".to_owned(), |budget| budget.to_string());
            let _ = writeln!(
                out,
                "{:<22} {:<SOURCE_WIDTH$} {:<9} {:<9} {:<7} {budget:<7} {:<8} {hash_short}...",
                sec.name, sec.source, sec.stability, sec.position, sec.token_estimate, sec.bytes
            );
        }
        out.push_str(RULE);
        // The plan states intent, not wire form: which bytes are stable and which calls should
        // share an entry. Breakpoints and cache-key fields are the adapter's lowering, and a dump
        // that printed them would be reporting a decision this layer does not make.
        if let Some(plan) = &self.cache_plan {
            let _ = writeln!(out, "Cache Prefix Hash:    {}", plan.prefix_hash());
            let _ = writeln!(
                out,
                "Cache Scope:          {}",
                plan.cache_scope().unwrap_or("<none>")
            );
        }
        // Whether the request is cached is deliberately *not* claimed here. The floor applies to
        // the instructions plus the whole tool table, and this report only ever sees the prompt.
        // Stating the prompt-side half is still worth doing — a prompt this far below the floor
        // will not carry the request on its own — but stating it as a verdict would be the same
        // under-measurement this layer is not in a position to correct.
        let verdict = if self.total_prefix_tokens >= MIN_CACHEABLE_PREFIX_TOKENS {
            "yes"
        } else {
            "no"
        };
        let _ = writeln!(
            out,
            "Prompt Clears Floor:  {verdict} (~{} vs {MIN_CACHEABLE_PREFIX_TOKENS}; the request's \
             tool table also counts)",
            self.total_prefix_tokens
        );
        out.push_str(BANNER_BOTTOM);
        out
    }

    /// Serializes the prompt dump into JSON format.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| Error::config(format!("failed to serialize prompt dump to json: {e}")))
    }
}

impl fmt::Display for PromptDump {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.render_text())
    }
}

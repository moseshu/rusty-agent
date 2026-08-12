//! Cross-version compatibility: schema versions and unknown-field retention (extension-safety rule 6).
//!
//! # The problem
//!
//! [`RunState`](crate::state::RunState) and rollout lines are read and written across versions
//! today, and a future cross-run task state will join them when it lands. Its mount point,
//! [`WorkStateHandle`](crate::state::WorkStateHandle), is deliberately **not** one of them: it
//! reaches task state that lives outside the run, so nothing here ever serializes it.
//!
//! **Downgrade reads are the hard half**: a record written by a newer build reaches an older one
//! carrying fields it does not know. serde's default is to **drop them silently**, which turns
//! "new writes, old reads, old writes again" into **silent data deletion** — the user only sees
//! that some state vanished after a resume, with no way to trace it.
//!
//! Three policies were therefore fixed from the start:
//!
//! | # | Policy | Carrier |
//! | ---: | --- | --- |
//! | 1 | Every serializable struct carries `schema_version` | [`SchemaVersion`] |
//! | 2 | Every added field is `#[serde(default)]` | a serde attribute, no type support needed |
//! | 3 | **Unknown fields are kept and written back verbatim, never rejected** | [`Unknown`] |
//!
//! # Usage
//!
//! ```ignore
//! #[derive(Serialize, Deserialize)]
//! struct Checkpoint {
//!     schema_version: SchemaVersion,
//!     #[serde(default)]
//!     max_turns: u32,
//!     /// Must come last: flatten claims every key the preceding fields did not.
//!     #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
//!     unknown: Unknown,
//! }
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Schema version of a serializable struct.
///
/// Serialized as a bare integer (`"schema_version": 3`) rather than an object: any reader in any
/// language has to understand it at a glance, including hosts that are not written in Rust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaVersion(u32);

impl SchemaVersion {
    /// Creates a version.
    #[must_use]
    pub const fn new(version: u32) -> Self {
        Self(version)
    }

    /// The version number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Compatibility of the record that was read relative to **the local code**.
    #[must_use]
    pub const fn compatibility(self, local: Self) -> Compatibility {
        if self.0 == local.0 {
            Compatibility::Same
        } else if self.0 < local.0 {
            Compatibility::Older
        } else {
            Compatibility::Newer
        }
    }
}

impl core::fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// How the schema version that was read relates to the local one.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Compatibility {
    /// Same version; read it directly.
    Same,
    /// The record is older than local code: fields added locally fill in via `#[serde(default)]`.
    Older,
    /// The record is newer than local code: **read it anyway**. The extra fields go into
    /// [`Unknown`] and are written back verbatim.
    ///
    /// Not failing on this tier is deliberate: failing would mean "an older client cannot open
    /// the session at all", while fields-only-grow plus unknown-field retention already make a
    /// downgrade read safe.
    Newer,
}

impl Compatibility {
    /// Stable machine-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::Older => "older",
            Self::Newer => "newer",
        }
    }

    /// Whether a migration is required before the record can be used safely (the entry point
    /// for the future migration chain).
    #[must_use]
    pub const fn needs_migration(self) -> bool {
        matches!(self, Self::Older)
    }
}

impl core::fmt::Display for Compatibility {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}

/// Key-value pairs that no known field claimed.
///
/// Used with `#[serde(flatten)]`: it catches every surplus key on the way in and writes them back
/// unchanged on the way out, so "new writes, old reads, old writes again" loses nothing.
///
/// Backed by [`BTreeMap`] rather than `HashMap`: **the write-back order has to be deterministic**,
/// or the same data serializes to different bytes every time and diffs, snapshot tests, and
/// content addressing all stop working.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Unknown(BTreeMap<String, Value>);

impl Unknown {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether there are no unknown fields. Used as `skip_serializing_if` so a clean record does
    /// not serialize an empty object.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of unknown fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns one unknown field.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// Iterates in lexical key order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }

    /// Folds another layer's unknown fields in, with the incoming layer winning on conflict.
    ///
    /// Crate-internal on purpose: unknown fields are produced by deserialization, not by callers.
    /// Layered resolution is the one place that legitimately combines two of these sets, and a
    /// merged value that silently carried fewer fields than the layer it came from would defeat
    /// the whole point of retaining them.
    pub(crate) fn extend_from(&mut self, other: &Self) {
        self.0.extend(
            other
                .0
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
}

impl<'a> IntoIterator for &'a Unknown {
    type Item = (&'a String, &'a Value);
    type IntoIter = std::collections::btree_map::Iter<'a, String, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Stable fingerprints for a raw TOML value, independent of key insertion order.

use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use toml::Value as TomlValue;

/// Hashes recursively sorted JSON bytes using the upstream sha256 version encoding.
#[must_use]
pub fn version_for_toml(value: &TomlValue) -> String {
    let mut json = serde_json::to_value(value).unwrap_or(JsonValue::Null);
    json.sort_all_objects();
    let serialized = serde_json::to_vec(&json).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized);
    let hash = hasher.finalize();
    format!("sha256:{hash:x}")
}

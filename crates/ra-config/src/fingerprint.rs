// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Stable fingerprints for a raw TOML value, independent of key insertion order.

use crate::{ConfigLayerMetadata, merge::is_structured_feature_path};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
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

pub(super) fn record_origins(
    value: &TomlValue,
    meta: &ConfigLayerMetadata,
    path: &mut Vec<String>,
    origins: &mut HashMap<String, ConfigLayerMetadata>,
    include: &impl Fn(&[String]) -> bool,
) {
    match value {
        TomlValue::Table(table) => {
            for (key, val) in table {
                path.push(key.clone());
                record_origins(val, meta, path, origins, include);
                path.pop();
            }
        }
        TomlValue::Array(items) => {
            for (idx, item) in (0_i32..).zip(items.iter()) {
                path.push(idx.to_string());
                record_origins(item, meta, path, origins, include);
                path.pop();
            }
        }
        _ => {
            if !path.is_empty() {
                if !include(path) {
                    return;
                }
                if matches!(value, TomlValue::Boolean(_)) && is_structured_feature_path(path) {
                    if path
                        .last()
                        .is_some_and(|feature| feature == "network_proxy")
                    {
                        origins.insert(path.join("."), meta.clone());
                    }
                    path.push("enabled".to_string());
                    origins.insert(path.join("."), meta.clone());
                    path.pop();
                    return;
                }
                origins.insert(path.join("."), meta.clone());
            }
        }
    }
}

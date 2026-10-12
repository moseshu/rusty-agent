//! The best-effort size policy applied after a schema is sanitized and pruned: Codex's
//! `json_schema/compaction.rs`. The order of the passes and how bytes are counted — the compact
//! JSON of the schema as read into the subset — are Codex's.

use serde_json::{Value, json};

use super::{
    DEFINITION_TABLE_KEYS, JsonSchema, SCHEMA_CHILD_KEYS, has_composition_keyword,
    parse_local_definition_ref,
    traversal::{DefinitionTraversal, for_each_schema_child_mut},
};

/// The size, in UTF-8 bytes of compact normalized JSON, above which a schema is compacted.
pub(super) const DEFAULT_COMPACT_TOOL_SCHEMA_BYTES: usize = 5_000;
const MAX_COMPACT_TOOL_SCHEMA_DEPTH: usize = 3;

/// Shrinks an unusually large schema while keeping its top-level arguments, applying ever lossier
/// passes while it stays over `max_bytes`. Not a hard cap.
pub(super) fn compact_large_tool_schema(value: &mut Value, max_bytes: usize) {
    for pass in LARGE_SCHEMA_COMPACTION_PASSES {
        if compact_schema_fits_budget(value, max_bytes) {
            break;
        }
        pass(value);
    }
}

type LargeSchemaCompactionPass = fn(&mut Value);

const LARGE_SCHEMA_COMPACTION_PASSES: &[LargeSchemaCompactionPass] = &[
    strip_schema_descriptions,
    drop_schema_definitions,
    collapse_deep_schema_objects_from_root,
    prune_schema_compositions,
];

fn collapse_deep_schema_objects_from_root(value: &mut Value) {
    collapse_deep_schema_objects(value, 0);
}

fn compact_schema_fits_budget(value: &Value, max_bytes: usize) -> bool {
    compact_normalized_schema_len(value) <= max_bytes
}

fn compact_normalized_schema_len(value: &Value) -> usize {
    serde_json::from_value::<JsonSchema>(value.clone())
        .and_then(|schema| serde_json::to_vec(&schema))
        .map_or(0, |json| json.len())
}

fn strip_schema_descriptions(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                strip_schema_descriptions(value);
            }
        }
        Value::Object(map) => {
            map.remove("description");
            for_each_schema_child_mut(map, DefinitionTraversal::Include, &mut |value| {
                strip_schema_descriptions(value);
            });
        }
        _ => {}
    }
}

/// Replaces local definition refs with empty schemas before dropping the root definition tables,
/// so what follows does not depend on how a reader treats refs to missing definitions.
fn drop_schema_definitions(value: &mut Value) {
    rewrite_definition_refs_to_empty_schemas(value);

    let Value::Object(map) = value else {
        return;
    };

    for key in DEFINITION_TABLE_KEYS {
        map.remove(key);
    }
}

fn rewrite_definition_refs_to_empty_schemas(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                rewrite_definition_refs_to_empty_schemas(value);
            }
        }
        Value::Object(map) => {
            if map
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(parse_local_definition_ref)
                .is_some()
            {
                *value = json!({});
                return;
            }

            for_each_schema_child_mut(map, DefinitionTraversal::Skip, &mut |value| {
                rewrite_definition_refs_to_empty_schemas(value);
            });
        }
        _ => {}
    }
}

fn collapse_deep_schema_objects(value: &mut Value, depth: usize) {
    match value {
        Value::Array(values) => {
            for value in values {
                collapse_deep_schema_objects(value, depth);
            }
        }
        Value::Object(map) => {
            if depth >= MAX_COMPACT_TOOL_SCHEMA_DEPTH && is_complex_schema_object(map) {
                *value = json!({});
                return;
            }

            for_each_schema_child_mut(map, DefinitionTraversal::Skip, &mut |value| {
                collapse_deep_schema_objects(value, depth + 1);
            });
        }
        _ => {}
    }
}

fn prune_schema_compositions(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                prune_schema_compositions(value);
            }
        }
        Value::Object(map) => {
            if has_composition_keyword(map) {
                *value = json!({});
                return;
            }

            for_each_schema_child_mut(map, DefinitionTraversal::Skip, &mut |value| {
                prune_schema_compositions(value);
            });
        }
        _ => {}
    }
}

fn is_complex_schema_object(map: &serde_json::Map<String, Value>) -> bool {
    SCHEMA_CHILD_KEYS.iter().any(|key| map.contains_key(*key))
        || map.contains_key("properties")
        || map.contains_key("additionalProperties")
        || map.contains_key("$ref")
}

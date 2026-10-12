//! Visits schema-valued children while leaving enum values and other instance data untouched:
//! Codex's `json_schema/traversal.rs`.

use serde_json::{Map, Value};

use super::{DEFINITION_TABLE_KEYS, SCHEMA_CHILD_KEYS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DefinitionTraversal {
    Include,
    Skip,
}

pub(super) fn for_each_schema_child(
    map: &Map<String, Value>,
    definition_traversal: DefinitionTraversal,
    visitor: &mut impl FnMut(&Value),
) {
    if let Some(properties) = map.get("properties").and_then(Value::as_object) {
        for value in properties.values() {
            visitor(value);
        }
    }

    for key in SCHEMA_CHILD_KEYS {
        if let Some(value) = map.get(key) {
            visitor(value);
        }
    }

    if let Some(additional_properties) = map.get("additionalProperties")
        && !matches!(additional_properties, Value::Bool(_))
    {
        visitor(additional_properties);
    }

    if definition_traversal == DefinitionTraversal::Include {
        for key in DEFINITION_TABLE_KEYS {
            if let Some(definitions) = map.get(key).and_then(Value::as_object) {
                for value in definitions.values() {
                    visitor(value);
                }
            }
        }
    }
}

pub(super) fn for_each_schema_child_mut(
    map: &mut Map<String, Value>,
    definition_traversal: DefinitionTraversal,
    visitor: &mut impl FnMut(&mut Value),
) {
    if let Some(properties) = map.get_mut("properties").and_then(Value::as_object_mut) {
        for value in properties.values_mut() {
            visitor(value);
        }
    }

    for key in SCHEMA_CHILD_KEYS {
        if let Some(value) = map.get_mut(key) {
            visitor(value);
        }
    }

    if let Some(additional_properties) = map.get_mut("additionalProperties")
        && !matches!(additional_properties, Value::Bool(_))
    {
        visitor(additional_properties);
    }

    if definition_traversal == DefinitionTraversal::Include {
        for key in DEFINITION_TABLE_KEYS {
            if let Some(definitions) = map.get_mut(key).and_then(Value::as_object_mut) {
                for value in definitions.values_mut() {
                    visitor(value);
                }
            }
        }
    }
}

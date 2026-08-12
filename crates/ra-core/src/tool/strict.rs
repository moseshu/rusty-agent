//! Strict JSON-schema normalization shared by derived tool inputs.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::error::{Error, Result};

const MAX_SCHEMA_NODES: usize = 100_000;
const DEFINITION_KEYS: [&str; 2] = ["$defs", "definitions"];

/// Traversal budget plus the reference path currently being inlined.
///
/// The node budget alone does not stop a self-referential type: each expansion consumes only a
/// handful of nodes but a whole stack frame, so the stack overflows long before the budget does.
/// The active-reference set turns that abort into an ordinary error.
struct NormalizeState {
    remaining: usize,
    active_refs: BTreeSet<String>,
}

pub(super) fn ensure_strict_json_schema(schema: &mut Value) -> Result<()> {
    if schema.as_object().is_some_and(Map::is_empty) {
        *schema = serde_json::json!({
            "additionalProperties": false,
            "type": "object",
            "properties": {},
            "required": []
        });
        return Ok(());
    }

    let root = schema.clone();
    let mut state = NormalizeState {
        remaining: MAX_SCHEMA_NODES,
        active_refs: BTreeSet::new(),
    };
    normalize_node(schema, &root, &mut state)?;
    prune_unreferenced_definitions(schema);

    let object = schema
        .as_object()
        .ok_or_else(|| Error::config("the root tool input schema must be a JSON object"))?;
    if object.contains_key("anyOf") {
        return Err(Error::config(
            "the root of a strict tool input schema must not use anyOf",
        ));
    }
    match object.get("type") {
        Some(Value::String(kind)) if kind == "object" => Ok(()),
        _ => Err(Error::config(
            "the root of a strict tool input schema must be a non-nullable object",
        )),
    }
}

fn normalize_node(node: &mut Value, root: &Value, state: &mut NormalizeState) -> Result<()> {
    state.remaining = state.remaining.checked_sub(1).ok_or_else(|| {
        Error::config("JSON schema is too large to convert safely to strict mode")
    })?;

    let object = node
        .as_object_mut()
        .ok_or_else(|| Error::config("every JSON schema node must be an object"))?;

    normalize_definitions(object, "$defs", root, state)?;
    normalize_definitions(object, "definitions", root, state)?;
    normalize_all_of(object, root, state)?;

    let has_properties = object.get("properties").is_some_and(Value::is_object);
    if !object.contains_key("type") && has_properties {
        object.insert("type".to_owned(), Value::String("object".to_owned()));
    }

    let is_object = match object.get("type") {
        Some(Value::String(kind)) => kind == "object",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "object"),
        _ => false,
    };
    if is_object {
        match object.get("additionalProperties") {
            None => {
                object.insert("additionalProperties".to_owned(), Value::Bool(false));
            }
            Some(Value::Bool(false)) => {}
            // An open map (`BTreeMap<String, _>`, `serde_json::Map`) is the usual way to reach
            // this: strict mode has no way to express a value-typed, open-keyed object.
            Some(_) => {
                return Err(Error::config(
                    "additionalProperties must be false for a strict object schema; \
                     an open key-value map cannot be described in strict mode",
                ));
            }
        }
        // A tool that takes no arguments still has to say so explicitly. Without this the
        // normalized schema would fail the strict verification it is supposed to satisfy.
        if !object.contains_key("properties") {
            object.insert("properties".to_owned(), Value::Object(Map::new()));
        }
    } else if object
        .get("additionalProperties")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        return Err(Error::config(
            "additionalProperties is only accepted as false on object schemas",
        ));
    }

    if let Some(mut required) = object
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect::<Vec<_>>())
    {
        required.sort_unstable();
        object.insert(
            "required".to_owned(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
    }
    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        for property in properties.values_mut() {
            normalize_node(property, root, state)?;
        }
    }

    // `items` is a single schema for homogeneous sequences and an array for tuples; draft
    // 2020-12 spells the tuple form `prefixItems`. Both carry ordinary schema nodes.
    for key in ["items", "prefixItems"] {
        match object.get_mut(key) {
            Some(Value::Array(entries)) => {
                for entry in entries {
                    normalize_node(entry, root, state)?;
                }
            }
            Some(node) => normalize_node(node, root, state)?,
            None => {}
        }
    }
    normalize_variants(object, "anyOf", root, state)?;

    if let Some(Value::Array(mut variants)) = object.remove("oneOf") {
        for variant in &mut variants {
            normalize_node(variant, root, state)?;
        }
        match object.entry("anyOf".to_owned()) {
            serde_json::map::Entry::Vacant(entry) => {
                entry.insert(Value::Array(variants));
            }
            serde_json::map::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut().as_array_mut().ok_or_else(|| {
                    Error::config("anyOf must be an array when oneOf is also present")
                })?;
                existing.extend(variants);
            }
        }
    }

    if object.get("default") == Some(&Value::Null) {
        object.remove("default");
    }

    expand_ref_with_siblings(node, root, state)
}

fn normalize_definitions(
    object: &mut Map<String, Value>,
    key: &str,
    root: &Value,
    state: &mut NormalizeState,
) -> Result<()> {
    if let Some(definitions) = object.get_mut(key).and_then(Value::as_object_mut) {
        for definition in definitions.values_mut() {
            normalize_node(definition, root, state)?;
        }
    }
    Ok(())
}

fn normalize_variants(
    object: &mut Map<String, Value>,
    key: &str,
    root: &Value,
    state: &mut NormalizeState,
) -> Result<()> {
    if let Some(variants) = object.get_mut(key).and_then(Value::as_array_mut) {
        for variant in variants {
            normalize_node(variant, root, state)?;
        }
    }
    Ok(())
}

fn normalize_all_of(
    object: &mut Map<String, Value>,
    root: &Value,
    state: &mut NormalizeState,
) -> Result<()> {
    let Some(all_of) = object.remove("allOf") else {
        return Ok(());
    };
    let Value::Array(mut variants) = all_of else {
        return Err(Error::config("allOf must be an array"));
    };
    if variants.len() == 1 {
        let mut base = variants
            .pop()
            .ok_or_else(|| Error::config("missing allOf entry"))?;
        normalize_node(&mut base, root, state)?;
        let base = base
            .as_object()
            .ok_or_else(|| Error::config("allOf entries must be schema objects"))?;
        for (key, value) in base {
            object.entry(key.clone()).or_insert_with(|| value.clone());
        }
    } else {
        for variant in &mut variants {
            normalize_node(variant, root, state)?;
        }
        object.insert("allOf".to_owned(), Value::Array(variants));
    }
    Ok(())
}

/// Inlines a `$ref` that carries sibling keywords, which cannot survive a plain reference.
///
/// A reference already being inlined higher up the stack means the type is self-referential.
/// Inlining it again never terminates, so the cycle is reported instead of expanded.
fn expand_ref_with_siblings(
    node: &mut Value,
    root: &Value,
    state: &mut NormalizeState,
) -> Result<()> {
    let Some(object) = node.as_object() else {
        return Ok(());
    };
    let Some(reference) = object.get("$ref").and_then(Value::as_str) else {
        return Ok(());
    };
    if object.len() == 1 {
        return Ok(());
    }
    let reference = reference.to_owned();
    if !state.active_refs.insert(reference.clone()) {
        return Err(Error::config(format!(
            "tool input schema is self-referential through `{reference}`; \
             a recursive type cannot be expressed as a strict JSON schema"
        )));
    }

    let mut resolved = resolve_local_ref(root, &reference)?.clone();
    let resolved_object = resolved
        .as_object_mut()
        .ok_or_else(|| Error::config("a local JSON schema reference must resolve to an object"))?;
    for (key, value) in object {
        if key != "$ref" {
            resolved_object.insert(key.clone(), value.clone());
        }
    }
    *node = resolved;
    let result = normalize_node(node, root, state);
    state.active_refs.remove(&reference);
    result
}

/// Drops definitions that no surviving `$ref` points at.
///
/// Sibling expansion copies a definition into the node that referenced it. Leaving the original
/// behind ships the same type twice in every request, which is pure waste against the schema
/// token budget and the cached prompt prefix.
fn prune_unreferenced_definitions(schema: &mut Value) {
    // Mark from the schema body outwards: a reference that only exists inside an unreachable
    // definition must not keep that definition — or a self-referential one — alive.
    let mut reachable = BTreeSet::new();
    let mut pending = collect_refs(schema);
    while let Some(reference) = pending.pop_first() {
        if !reachable.insert(reference.clone()) {
            continue;
        }
        if let Ok(definition) = resolve_local_ref(schema, &reference) {
            pending.extend(collect_refs(definition));
        }
    }

    let Some(object) = schema.as_object_mut() else {
        return;
    };
    for key in DEFINITION_KEYS {
        let Some(definitions) = object.get_mut(key).and_then(Value::as_object_mut) else {
            continue;
        };
        definitions.retain(|name, _| reachable.contains(&format!("#/{key}/{name}")));
        if definitions.is_empty() {
            object.remove(key);
        }
    }
}

/// Collects every `$ref` outside the definition maps of `value`.
fn collect_refs(value: &Value) -> BTreeSet<String> {
    fn walk(value: &Value, found: &mut BTreeSet<String>) {
        match value {
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                    found.insert(reference.to_owned());
                }
                for (key, child) in object {
                    if !DEFINITION_KEYS.contains(&key.as_str()) {
                        walk(child, found);
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    walk(value, found);
                }
            }
            _ => {}
        }
    }

    let mut found = BTreeSet::new();
    walk(value, &mut found);
    found
}

/// Checks the strict-mode invariants without rewriting the schema.
///
/// Hand-written and MCP-sourced schemas never pass through [`ensure_strict_json_schema`], so a
/// schema that merely *claims* to be strict would otherwise reach the provider and be rejected
/// there. Unlike normalization this accepts any `required` order and does not inline references.
pub(super) fn verify_strict_json_schema(schema: &Value) -> Result<()> {
    let object = schema
        .as_object()
        .ok_or_else(|| Error::config("the root tool input schema must be a JSON object"))?;
    if object.contains_key("anyOf") {
        return Err(Error::config(
            "the root of a strict tool input schema must not use anyOf",
        ));
    }
    if !matches!(object.get("type"), Some(Value::String(kind)) if kind == "object") {
        return Err(Error::config(
            "the root of a strict tool input schema must be a non-nullable object",
        ));
    }
    verify_node(schema, "$")
}

fn verify_node(node: &Value, path: &str) -> Result<()> {
    let Some(object) = node.as_object() else {
        return Ok(());
    };

    // Every object node needs the closed-world marker, including one that declares no properties
    // at all: the provider checks the keyword, not whether the object happens to be empty.
    let properties = object.get("properties").and_then(Value::as_object);
    let is_object = properties.is_some()
        || matches!(object.get("type"), Some(Value::String(kind)) if kind == "object")
        || object
            .get("type")
            .and_then(Value::as_array)
            .is_some_and(|kinds| kinds.iter().any(|kind| kind == "object"));
    if is_object {
        if object.get("additionalProperties") != Some(&Value::Bool(false)) {
            return Err(Error::config(format!(
                "strict tool input schema requires `additionalProperties: false` at `{path}`"
            )));
        }
        let properties = properties.ok_or_else(|| {
            Error::config(format!(
                "strict tool input schema requires an object `properties` map at `{path}`"
            ))
        })?;
        let required_values = object
            .get("required")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                Error::config(format!(
                    "strict tool input schema requires a `required` string array at `{path}`"
                ))
            })?;
        let mut required = BTreeSet::new();
        for name in required_values {
            let name = name.as_str().ok_or_else(|| {
                Error::config(format!(
                    "strict tool input schema requires only strings in `required` at `{path}`"
                ))
            })?;
            if !required.insert(name) {
                return Err(Error::config(format!(
                    "strict tool input schema has duplicate `required` entry `{name}` at `{path}`"
                )));
            }
        }
        let declared = properties
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if let Some(missing) = declared.difference(&required).next() {
            return Err(Error::config(format!(
                "strict tool input schema requires every property in `required`; \
                 `{path}.{missing}` is missing"
            )));
        }
        if let Some(unexpected) = required.difference(&declared).next() {
            return Err(Error::config(format!(
                "strict tool input schema has undeclared `required` entry \
                 `{path}.{unexpected}`"
            )));
        }
        for (name, property) in properties {
            verify_node(property, &format!("{path}.{name}"))?;
        }
    }

    for key in ["items", "prefixItems"] {
        match object.get(key) {
            Some(Value::Array(entries)) => {
                for (index, entry) in entries.iter().enumerate() {
                    verify_node(entry, &format!("{path}[{index}]"))?;
                }
            }
            Some(node) => verify_node(node, &format!("{path}[]"))?,
            None => {}
        }
    }
    for key in ["anyOf", "allOf", "oneOf"] {
        if let Some(variants) = object.get(key).and_then(Value::as_array) {
            for variant in variants {
                verify_node(variant, path)?;
            }
        }
    }
    for key in DEFINITION_KEYS {
        if let Some(definitions) = object.get(key).and_then(Value::as_object) {
            for (name, definition) in definitions {
                verify_node(definition, &format!("#/{key}/{name}"))?;
            }
        }
    }
    Ok(())
}

fn resolve_local_ref<'a>(root: &'a Value, reference: &str) -> Result<&'a Value> {
    let path = reference.strip_prefix("#/").ok_or_else(|| {
        Error::config(format!(
            "unsupported non-local schema reference `{reference}`"
        ))
    })?;
    let mut current = root;
    for segment in path.split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        current = current.get(&segment).ok_or_else(|| {
            Error::config(format!("schema reference `{reference}` cannot be resolved"))
        })?;
    }
    Ok(current)
}

pub(super) fn canonicalize_json(value: &mut Value) {
    match value {
        Value::Object(object) => {
            let mut sorted = BTreeMap::new();
            for (key, mut value) in core::mem::take(object) {
                canonicalize_json(&mut value);
                sorted.insert(key, value);
            }
            object.extend(sorted);
        }
        Value::Array(values) => {
            for value in values {
                canonicalize_json(value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

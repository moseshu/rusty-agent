//! Scalar coercion guided by the original loose type schema.

use serde_json::Value;

pub(super) fn coerce(value: &mut Value, schema: &Value) {
    walk(value, schema, schema, 0);
}

fn walk(value: &mut Value, schema: &Value, root: &Value, depth: usize) {
    if depth > 128 {
        return;
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str)
        && let Some(target) = reference
            .strip_prefix('#')
            .and_then(|path| root.pointer(path))
    {
        walk(value, target, root, depth + 1);
        return;
    }
    if let Some(variants) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)
    {
        // Retain an already matching JSON type, but still visit its nested fields.
        if let Some(branch) = variants
            .iter()
            .find(|branch| matches_type(value, branch_type(branch, root)))
        {
            walk(value, branch, root, depth + 1);
            return;
        }
        for branch in variants {
            let mut candidate = value.clone();
            walk(&mut candidate, branch, root, depth + 1);
            if matches_type(&candidate, branch_type(branch, root)) {
                *value = candidate;
                return;
            }
        }
    }
    if let Some(Value::Array(kinds)) = schema.get("type") {
        if !kinds.iter().any(|kind| matches_type(value, Some(kind))) {
            for kind in kinds {
                let mut candidate = value.clone();
                scalar(&mut candidate, kind.as_str().unwrap_or_default());
                if matches_type(&candidate, Some(kind)) {
                    *value = candidate;
                    break;
                }
            }
        }
    } else {
        scalar(
            value,
            schema
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
    }
    if let Some(object) = value.as_object_mut() {
        for (name, child) in object {
            if let Some(property) = schema
                .get("properties")
                .and_then(|properties| properties.get(name))
                .or_else(|| {
                    schema
                        .get("additionalProperties")
                        .filter(|schema| schema.is_object())
                })
            {
                walk(child, property, root, depth + 1);
            }
        }
    }
    if let Some(items) = value.as_array_mut()
        && let Some(item_schema) = schema.get("items")
    {
        for (index, item) in items.iter_mut().enumerate() {
            let schema = item_schema
                .as_array()
                .map_or(Some(item_schema), |tuple| tuple.get(index));
            if let Some(schema) = schema {
                walk(item, schema, root, depth + 1);
            }
        }
    }
}

fn scalar(value: &mut Value, kind: &str) {
    let converted = match kind {
        "integer" => value
            .as_str()
            .and_then(|text| {
                text.trim()
                    .parse::<i64>()
                    .ok()
                    .map(Value::from)
                    .or_else(|| text.trim().parse::<u64>().ok().map(Value::from))
            })
            .or_else(|| {
                value
                    .as_bool()
                    .map(|boolean| Value::from(i64::from(boolean)))
            }),
        "number" => value
            .as_str()
            .and_then(|text| text.trim().parse::<f64>().ok())
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .or_else(|| {
                value
                    .as_bool()
                    .map(|boolean| Value::from(i64::from(boolean)))
            }),
        "boolean" => match value {
            Value::String(text) => match text.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "y" | "on" | "t" => Some(Value::Bool(true)),
                "false" | "0" | "no" | "n" | "off" | "f" => Some(Value::Bool(false)),
                _ => None,
            },
            Value::Number(number) if number.as_i64() == Some(1) => Some(Value::Bool(true)),
            Value::Number(number) if number.as_i64() == Some(0) => Some(Value::Bool(false)),
            _ => None,
        },
        _ => None,
    };
    if let Some(converted) = converted {
        *value = converted;
    }
}

fn matches_type(value: &Value, kind: Option<&Value>) -> bool {
    match kind.and_then(Value::as_str) {
        Some("string") => value.is_string(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some("boolean") => value.is_boolean(),
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("null") => value.is_null(),
        _ => false,
    }
}

fn branch_type<'a>(branch: &'a Value, root: &'a Value) -> Option<&'a Value> {
    branch.get("type").or_else(|| {
        branch
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix('#'))
            .and_then(|path| root.pointer(path))
            .and_then(|target| target.get("type"))
    })
}

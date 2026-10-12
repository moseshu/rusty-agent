//! The tools a client provides to a thread: how their specs are read, including the legacy shape,
//! how their input schemas are read into the schema subset, and the checks they pass before a
//! thread starts with them.
//!
//! The schema cases port Codex's `codex-tools` `json_schema_tests.rs` and
//! `json_schema/{types,compaction}_tests.rs`; Codex compares its `JsonSchema` values, and these
//! compare the same values as the JSON they serialize to, built by helpers named after Codex's
//! constructors. The validation cases port the `validate_dynamic_tools` tests of Codex's
//! `thread_processor_tests.rs`, and the normalization cases Codex's
//! `session_meta_normalizes_legacy_dynamic_tools` and the item conversion of its v2 tests.

use ra_protocol::server::{
    protocol::{
        DynamicToolCallItem, DynamicToolCallOutputContentItem, DynamicToolCallStatus,
        DynamicToolFunctionSpec, DynamicToolNamespaceSpec, DynamicToolNamespaceTool,
        DynamicToolSpec, ThreadItem, normalize_dynamic_tool_specs,
    },
    test_api::{
        parse_tool_input_schema, parse_tool_input_schema_with_max_bytes,
        parse_tool_input_schema_without_compaction, validate_dynamic_tools,
    },
};
use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------------------------
// Codex's `JsonSchema` constructors, as the JSON their values serialize to
// ---------------------------------------------------------------------------------------------

fn empty() -> Value {
    json!({})
}

fn typed(name: &str) -> Value {
    json!({ "type": name })
}

fn string() -> Value {
    typed("string")
}

fn number() -> Value {
    typed("number")
}

fn integer() -> Value {
    typed("integer")
}

fn null() -> Value {
    typed("null")
}

fn string_enum(values: Vec<Value>) -> Value {
    json!({ "type": "string", "enum": values })
}

fn array(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}

fn object(
    properties: Vec<(&str, Value)>,
    required: Option<Vec<&str>>,
    additional: Option<Value>,
) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": properties
            .into_iter()
            .map(|(name, schema)| (name.to_owned(), schema))
            .collect::<Map<_, _>>(),
    });
    if let Some(required) = required {
        schema["required"] = json!(required);
    }
    if let Some(additional) = additional {
        schema["additionalProperties"] = additional;
    }
    schema
}

fn composition(keyword: &str, variants: Vec<Value>) -> Value {
    json!({ keyword: variants })
}

fn schema_ref(target: &str) -> Value {
    json!({ "$ref": target })
}

fn parse(input: Value) -> Value {
    parse_tool_input_schema(&input).expect("parse schema")
}

// ---------------------------------------------------------------------------------------------
// Normalization of malformed schemas
// ---------------------------------------------------------------------------------------------

/// A boolean schema becomes an accept-all string: the subset cannot say what `true` means.
#[test]
fn parse_tool_input_schema_coerces_boolean_schemas() {
    assert_eq!(parse(json!(true)), string());
}

/// `properties` implies an object; a property with no recognized keyword becomes `{}`.
#[test]
fn parse_tool_input_schema_infers_object_shape_and_defaults_properties() {
    let schema = parse(json!({
        "properties": {
            "query": {"description": "search query"}
        }
    }));

    assert_eq!(schema, object(vec![("query", empty())], None, None));
}

/// A schema with no recognized keyword is malformed and becomes `{}`.
#[test]
fn parse_tool_input_schema_coerces_unrecognized_object_schema_to_empty_schema() {
    let schema = parse(json!({
        "description": "Ticket identifier",
        "title": "Ticket ID"
    }));

    assert_eq!(schema, empty());
}

/// `integer` stays distinct from `number`; an array without `items` gets string items.
#[test]
fn parse_tool_input_schema_preserves_integer_and_defaults_array_items() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "page": {"type": "integer"},
            "tags": {"type": "array"}
        }
    }));

    assert_eq!(
        schema,
        object(
            vec![("page", integer()), ("tags", array(string()))],
            None,
            None
        )
    );
}

/// A schema-valued `additionalProperties` is sanitized too.
#[test]
fn parse_tool_input_schema_sanitizes_additional_properties_schema() {
    let schema = parse(json!({
        "type": "object",
        "additionalProperties": {
            "required": ["value"],
            "properties": {
                "value": {"anyOf": [{"type": "string"}, {"type": "number"}]}
            }
        }
    }));

    assert_eq!(
        schema,
        object(
            vec![],
            None,
            Some(object(
                vec![("value", composition("anyOf", vec![string(), number()]))],
                Some(vec!["value"]),
                None,
            ))
        )
    );
}

/// `additionalProperties` alone implies an object, and a boolean one is kept.
#[test]
fn parse_tool_input_schema_infers_object_shape_from_boolean_additional_properties_only() {
    let schema = parse(json!({
        "additionalProperties": false
    }));

    assert_eq!(schema, object(vec![], None, Some(json!(false))));
}

/// Numeric constraints imply a number.
#[test]
fn parse_tool_input_schema_infers_number_from_numeric_keywords() {
    assert_eq!(parse(json!({ "minimum": 1 })), number());
}

/// `multipleOf` implies a number as `minimum` does.
#[test]
fn parse_tool_input_schema_infers_number_from_multiple_of() {
    assert_eq!(parse(json!({ "multipleOf": 5 })), number());
}

/// `enum` and `const` become string enums; `format` alone a plain string.
#[test]
fn parse_tool_input_schema_infers_string_from_enum_const_and_format_keywords() {
    assert_eq!(
        parse(json!({ "enum": ["fast", "safe"] })),
        string_enum(vec![json!("fast"), json!("safe")])
    );
    assert_eq!(
        parse(json!({ "const": "file" })),
        string_enum(vec![json!("file")])
    );
    assert_eq!(parse(json!({ "format": "date-time" })), string());
}

/// `{}` already accepts anything and stays empty.
#[test]
fn parse_tool_input_schema_preserves_empty_schema() {
    assert_eq!(parse(json!({})), empty());
}

/// Nested properties are sanitized, and a nested `{}` stays empty.
#[test]
fn parse_tool_input_schema_preserves_nested_empty_schema() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "metadata": {
                "properties": {
                    "extra": {}
                }
            }
        }
    }));

    assert_eq!(
        schema,
        object(
            vec![("metadata", object(vec![("extra", empty())], None, None))],
            None,
            None
        )
    );
}

/// `prefixItems` implies an array, read as one of string items.
#[test]
fn parse_tool_input_schema_infers_array_from_prefix_items() {
    let schema = parse(json!({
        "prefixItems": [
            {"type": "string"}
        ]
    }));

    assert_eq!(schema, array(string()));
}

/// A nested object inferred from `additionalProperties: true` keeps it.
#[test]
fn parse_tool_input_schema_preserves_boolean_additional_properties_on_inferred_object() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "metadata": {
                "additionalProperties": true
            }
        }
    }));

    assert_eq!(
        schema,
        object(
            vec![("metadata", object(vec![], None, Some(json!(true))))],
            None,
            None
        )
    );
}

/// A schema-valued `additionalProperties` alone implies an object and is kept.
#[test]
fn parse_tool_input_schema_infers_object_shape_from_schema_additional_properties_only() {
    let schema = parse(json!({
        "additionalProperties": {
            "type": "string"
        }
    }));

    assert_eq!(schema, object(vec![], None, Some(string())));
}

/// `const` becomes a single-value enum.
#[test]
fn parse_tool_input_schema_rewrites_const_to_single_value_enum() {
    assert_eq!(
        parse(json!({ "const": "tagged" })),
        string_enum(vec![json!("tagged")])
    );
}

/// A schema that is only `null` is refused.
#[test]
fn parse_tool_input_schema_rejects_singleton_null_type() {
    let err = parse_tool_input_schema(&json!({ "type": "null" }))
        .expect_err("singleton null should be rejected");

    assert!(
        err.contains("tool input schema must not be a singleton null type"),
        "unexpected error: {err}"
    );
}

/// A nullable object union is kept whole, and its object member gets default `properties`.
#[test]
fn parse_tool_input_schema_fills_default_properties_for_nullable_object_union() {
    assert_eq!(
        parse(json!({ "type": ["object", "null"] })),
        json!({ "type": ["object", "null"], "properties": {} })
    );
}

/// A nullable array union is kept whole, and its array member gets default `items`.
#[test]
fn parse_tool_input_schema_fills_default_items_for_nullable_array_union() {
    assert_eq!(
        parse(json!({ "type": ["array", "null"] })),
        json!({ "type": ["array", "null"], "items": { "type": "string" } })
    );
}

// ---------------------------------------------------------------------------------------------
// Shapes kept for the Responses API rather than rewritten
// ---------------------------------------------------------------------------------------------

/// Nested nullable `anyOf` shapes are kept all the way down.
#[test]
fn parse_tool_input_schema_preserves_nested_nullable_any_of_shape() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "open": {
                "anyOf": [
                    {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "ref_id": {"type": "string"},
                                "lineno": {"anyOf": [{"type": "integer"}, {"type": "null"}]}
                            },
                            "required": ["ref_id"],
                            "additionalProperties": false
                        }
                    },
                    {"type": "null"}
                ]
            }
        }
    }));

    assert_eq!(
        schema,
        object(
            vec![(
                "open",
                composition(
                    "anyOf",
                    vec![
                        array(object(
                            vec![
                                ("lineno", composition("anyOf", vec![integer(), null()])),
                                ("ref_id", string()),
                            ],
                            Some(vec!["ref_id"]),
                            Some(json!(false)),
                        )),
                        null(),
                    ],
                ),
            )],
            None,
            None
        )
    );
}

/// A nested `["string", "null"]` union is kept, with the object's `required` and
/// `additionalProperties`.
#[test]
fn parse_tool_input_schema_preserves_nested_nullable_type_union() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "nickname": {
                "type": ["string", "null"],
                "description": "Optional nickname"
            }
        },
        "required": ["nickname"],
        "additionalProperties": false
    }));

    assert_eq!(
        schema,
        object(
            vec![(
                "nickname",
                json!({ "type": ["string", "null"], "description": "Optional nickname" }),
            )],
            Some(vec!["nickname"]),
            Some(json!(false)),
        )
    );
}

/// A nested `anyOf` is kept rather than flattened.
#[test]
fn parse_tool_input_schema_preserves_nested_any_of_property() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "query": {
                "anyOf": [
                    { "type": "string" },
                    { "type": "number" }
                ]
            }
        }
    }));

    assert_eq!(
        schema,
        object(
            vec![("query", composition("anyOf", vec![string(), number()]))],
            None,
            None
        )
    );
}

/// A nested `oneOf` is kept, its variants sanitized.
#[test]
fn parse_tool_input_schema_preserves_nested_one_of_property() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "query": {
                "oneOf": [
                    { "const": "exact" },
                    { "type": "number" }
                ]
            }
        }
    }));

    assert_eq!(
        schema,
        object(
            vec![(
                "query",
                composition("oneOf", vec![string_enum(vec![json!("exact")]), number()]),
            )],
            None,
            None
        )
    );
}

/// A nested `allOf` is kept, its variants sanitized.
#[test]
fn parse_tool_input_schema_preserves_nested_all_of_property() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "query": {
                "allOf": [
                    { "type": "string" },
                    { "description": "unrecognized by itself" }
                ]
            }
        }
    }));

    assert_eq!(
        schema,
        object(
            vec![("query", composition("allOf", vec![string(), empty()]))],
            None,
            None
        )
    );
}

/// A type union stays a union rather than becoming an `anyOf`.
#[test]
fn parse_tool_input_schema_preserves_type_unions_without_rewriting_to_any_of() {
    let schema = parse(json!({
        "type": ["string", "null"],
        "description": "optional string"
    }));

    assert_eq!(
        schema,
        json!({ "type": ["string", "null"], "description": "optional string" })
    );
}

/// A string/null union is kept alongside its enum.
#[test]
fn parse_tool_input_schema_preserves_explicit_enum_type_union() {
    let schema = parse(json!({
        "type": ["string", "null"],
        "enum": ["short", "medium", "long"],
        "description": "optional response length"
    }));

    assert_eq!(
        schema,
        json!({
            "type": ["string", "null"],
            "description": "optional response length",
            "enum": ["short", "medium", "long"]
        })
    );
}

// ---------------------------------------------------------------------------------------------
// Compaction of large schemas
// ---------------------------------------------------------------------------------------------

fn many_string_properties(count: usize) -> Map<String, Value> {
    (0..count)
        .map(|index| (format!("field_{index:03}"), json!({ "type": "string" })))
        .collect()
}

/// A large schema loses its descriptions on the default path and keeps them without compaction.
#[test]
fn parse_large_tool_input_schema_compacts_descriptions_only_on_default_path() {
    let input_schema = json!({
        "type": "object",
        "description": "x".repeat(5_500),
        "properties": {
            "metadata": {
                "$ref": "#/$defs/metadata"
            }
        },
        "$defs": {
            "metadata": {
                "type": "string",
                "description": "Metadata value"
            }
        }
    });

    assert_eq!(
        parse(input_schema.clone()),
        json!({
            "type": "object",
            "properties": {
                "metadata": {
                    "$ref": "#/$defs/metadata"
                }
            },
            "$defs": {
                "metadata": {
                    "type": "string"
                }
            }
        })
    );
    assert_eq!(
        parse_tool_input_schema_without_compaction(&input_schema).expect("parse schema"),
        json!({
            "type": "object",
            "description": "x".repeat(5_500),
            "properties": {
                "metadata": {
                    "$ref": "#/$defs/metadata"
                }
            },
            "$defs": {
                "metadata": {
                    "type": "string",
                    "description": "Metadata value"
                }
            }
        })
    );
}

/// Keywords outside the subset do not count toward the size, so they trigger no compaction.
#[test]
fn parse_large_tool_input_schema_ignores_dropped_metadata_for_budget() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "event": {
                "type": "object",
                "title": "Calendar event",
                "properties": {
                    "recurrence": {
                        "type": "object",
                        "examples": [
                            {
                                "payload": "x".repeat(5_500)
                            }
                        ],
                        "properties": {
                            "pattern": {
                                "type": "string",
                                "title": "Recurrence pattern"
                            }
                        }
                    }
                }
            }
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "event": {
                    "type": "object",
                    "properties": {
                        "recurrence": {
                            "type": "object",
                            "properties": {
                                "pattern": {
                                    "type": "string"
                                }
                            }
                        }
                    }
                }
            }
        })
    );
}

/// Compaction stops once dropping the root definitions brings the schema under the size.
#[test]
fn parse_large_tool_input_schema_stops_after_dropping_root_definitions_when_under_budget() {
    let schema = parse(json!({
        "type": "object",
        "description": "x".repeat(5_500),
        "properties": {
            "event": {
                "type": "object",
                "description": "Calendar event",
                "properties": {
                    "recurrence": {
                        "type": "object",
                        "description": "Recurrence settings",
                        "properties": {
                            "pattern": {
                                "type": "string",
                                "description": "Recurrence pattern"
                            }
                        }
                    }
                }
            },
            "metadata": {
                "$ref": "#/$defs/metadata"
            }
        },
        "$defs": {
            "metadata": {
                "type": "object",
                "description": "metadata object",
                "properties": many_string_properties(300)
            }
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "event": {
                    "type": "object",
                    "properties": {
                        "recurrence": {
                            "type": "object",
                            "properties": {
                                "pattern": {
                                    "type": "string"
                                }
                            }
                        }
                    }
                },
                "metadata": {}
            }
        })
    );
}

/// Stripping descriptions keeps a property named `description`.
#[test]
fn parse_large_tool_input_schema_strips_descriptions_without_removing_description_property() {
    let schema = parse(json!({
        "type": "object",
        "description": "x".repeat(5_500),
        "properties": {
            "description": {
                "type": "string",
                "description": "User-facing description value"
            },
            "metadata": {
                "type": "object",
                "description": "Metadata object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Metadata label"
                    }
                }
            },
            "tags": {
                "type": "array",
                "description": "Tag list",
                "items": {
                    "type": "string",
                    "description": "Tag value"
                }
            },
            "extras": {
                "type": "object",
                "additionalProperties": {
                    "type": "string",
                    "description": "Extra value"
                }
            },
            "choice": {
                "description": "Choice value",
                "anyOf": [
                    {
                        "type": "string",
                        "description": "String choice"
                    },
                    {
                        "type": "number",
                        "description": "Number choice"
                    }
                ]
            }
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "choice": {
                    "anyOf": [
                        {
                            "type": "string"
                        },
                        {
                            "type": "number"
                        }
                    ]
                },
                "description": {
                    "type": "string"
                },
                "extras": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": {
                        "type": "string"
                    }
                },
                "metadata": {
                    "type": "object",
                    "properties": {
                        "label": {
                            "type": "string"
                        }
                    }
                },
                "tags": {
                    "type": "array",
                    "items": {
                        "type": "string"
                    }
                }
            }
        })
    );
}

/// Compositions are pruned last, under each of the three keywords.
#[test]
fn parse_large_tool_input_schema_prunes_compositions_as_last_resort() {
    for composition_key in ["anyOf", "oneOf", "allOf"] {
        let variants = vec![
            json!({
                "type": "string",
                "enum": ["first ".repeat(400)]
            }),
            json!({
                "type": "string",
                "enum": ["second ".repeat(400)]
            }),
            json!({
                "type": "string",
                "enum": ["third ".repeat(400)]
            }),
        ];

        let schema = parse(json!({
            "type": "object",
            "properties": {
                "choice": composition(composition_key, variants)
            }
        }));

        assert_eq!(
            schema,
            json!({
                "type": "object",
                "properties": {
                    "choice": {}
                }
            })
        );
    }
}

/// A single composition variant is pruned as well when the schema is still too large.
#[test]
fn parse_large_tool_input_schema_prunes_single_composition_variant_if_still_over_budget() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "choice": {
                "anyOf": [
                    {
                        "type": "string",
                        "enum": ["x".repeat(5_500)]
                    }
                ]
            }
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "choice": {}
            }
        })
    );
}

/// Stripping descriptions leaves enum values, which are data, alone.
#[test]
fn parse_large_tool_input_schema_preserves_object_enum_literal_descriptions() {
    let schema = parse(json!({
        "type": "object",
        "description": "x".repeat(5_500),
        "properties": {
            "choice": {
                "enum": [
                    {
                        "description": "first literal",
                        "id": 1
                    },
                    {
                        "description": "second literal",
                        "id": 2
                    }
                ]
            }
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "choice": {
                    "type": "string",
                    "enum": [
                        {
                            "description": "first literal",
                            "id": 1
                        },
                        {
                            "description": "second literal",
                            "id": 2
                        }
                    ]
                }
            }
        })
    );
}

/// Codex's `collapse_deep_schema_objects_traverses_schema_children`, through the size policy:
/// with the size set to that of the collapsed schema, compaction stops after collapsing, so deep
/// objects under properties, items, `additionalProperties` and `anyOf` all become `{}` while the
/// composition itself is kept. The input is sanitized first, which gives the object holding only
/// `additionalProperties` its default `properties`.
#[test]
fn collapse_deep_schema_objects_traverses_schema_children() {
    let deep = || {
        json!({
            "type": "object",
            "properties": {
                "nested": {
                    "type": "object",
                    "properties": {
                        "leaf": { "type": "string" }
                    }
                }
            }
        })
    };
    let collapsed = || json!({ "type": "object", "properties": { "nested": {} } });
    let input = json!({
        "type": "object",
        "properties": {
            "object_parent": {
                "type": "object",
                "properties": {
                    "complex": deep(),
                    "scalar": { "type": "string" }
                }
            },
            "array_parent": { "type": "array", "items": deep() },
            "map_parent": { "type": "object", "additionalProperties": deep() },
            "union_parent": { "anyOf": [deep(), { "type": "string" }] }
        }
    });
    let expected = json!({
        "type": "object",
        "properties": {
            "object_parent": {
                "type": "object",
                "properties": {
                    "complex": collapsed(),
                    "scalar": { "type": "string" }
                }
            },
            "array_parent": { "type": "array", "items": collapsed() },
            "map_parent": {
                "type": "object",
                "properties": {},
                "additionalProperties": collapsed()
            },
            "union_parent": { "anyOf": [collapsed(), { "type": "string" }] }
        }
    });
    let collapsed_size = serde_json::to_vec(&expected).unwrap().len();

    assert_eq!(
        parse_tool_input_schema_with_max_bytes(&input, collapsed_size).expect("parse schema"),
        expected
    );
}

/// Codex's `json_schema_serializes_encrypted_marker`: the Responses marker for encrypted
/// parameters is kept.
#[test]
fn json_schema_serializes_encrypted_marker() {
    let schema = json!({
        "type": "string",
        "description": "Secret value",
        "encrypted": true,
    });

    assert_eq!(parse(schema.clone()), schema);
}

// ---------------------------------------------------------------------------------------------
// Enums and references
// ---------------------------------------------------------------------------------------------

/// The legacy `type: "enum"` and `type: "const"` become string enums.
#[test]
fn parse_tool_input_schema_preserves_string_enum_constraints() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "response_length": {
                "type": "enum",
                "enum": ["short", "medium", "long"]
            },
            "kind": {
                "type": "const",
                "const": "tagged"
            },
            "scope": {
                "type": "enum",
                "enum": ["one", "two"]
            }
        }
    }));

    assert_eq!(
        schema,
        object(
            vec![
                ("kind", string_enum(vec![json!("tagged")])),
                (
                    "response_length",
                    string_enum(vec![json!("short"), json!("medium"), json!("long")]),
                ),
                ("scope", string_enum(vec![json!("one"), json!("two")])),
            ],
            None,
            None
        )
    );
}

/// A local `$ref` is kept with the definitions it reaches; the others are pruned.
#[test]
fn parse_tool_input_schema_preserves_refs_and_prunes_unreachable_defs() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "user": {"$ref": "#/$defs/User"}
        },
        "$defs": {
            "User": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                }
            },
            "Unused": {"type": "string"}
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": { "user": schema_ref("#/$defs/User") },
            "$defs": { "User": object(vec![("name", string())], None, None) }
        })
    );
}

/// A property named `$defs` is a field: its ref still reaches a root definition.
#[test]
fn parse_tool_input_schema_preserves_refs_from_properties_named_def_tables() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "$defs": {"$ref": "#/$defs/User"}
        },
        "$defs": {
            "User": {"type": "string"},
            "Unused": {"type": "boolean"}
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": { "$defs": schema_ref("#/$defs/User") },
            "$defs": { "User": string() }
        })
    );
}

/// Refs under `items`, `additionalProperties` and every composition keyword reach definitions.
#[test]
fn parse_tool_input_schema_collects_refs_from_schema_child_keywords() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "items_holder": {
                "type": "array",
                "items": {"$ref": "#/$defs/Item"}
            },
            "map_holder": {
                "type": "object",
                "additionalProperties": {"$ref": "#/$defs/Extra"}
            },
            "choice": {
                "anyOf": [
                    {"$ref": "#/$defs/Choice"},
                    {"type": "string"}
                ]
            },
            "exclusive_choice": {
                "oneOf": [
                    {"$ref": "#/$defs/ExclusiveChoice"},
                    {"type": "integer"}
                ]
            },
            "combined": {
                "allOf": [
                    {"$ref": "#/$defs/Combined"},
                    {"type": "object"}
                ]
            }
        },
        "$defs": {
            "Combined": {"type": "object"},
            "Choice": {"type": "boolean"},
            "ExclusiveChoice": {"type": "null"},
            "Extra": {"type": "number"},
            "Item": {"type": "string"},
            "Unused": {"type": "null"}
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "choice": {
                    "anyOf": [
                        {"$ref": "#/$defs/Choice"},
                        {"type": "string"}
                    ]
                },
                "combined": {
                    "allOf": [
                        {"$ref": "#/$defs/Combined"},
                        {"type": "object", "properties": {}}
                    ]
                },
                "exclusive_choice": {
                    "oneOf": [
                        {"$ref": "#/$defs/ExclusiveChoice"},
                        {"type": "integer"}
                    ]
                },
                "items_holder": {
                    "type": "array",
                    "items": {"$ref": "#/$defs/Item"}
                },
                "map_holder": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": {"$ref": "#/$defs/Extra"}
                }
            },
            "$defs": {
                "Combined": {"type": "object", "properties": {}},
                "Choice": {"type": "boolean"},
                "ExclusiveChoice": {"type": "null"},
                "Extra": {"type": "number"},
                "Item": {"type": "string"}
            }
        })
    );
}

/// Recursive refs are kept, and finding what they reach ends.
#[test]
fn parse_tool_input_schema_handles_cyclic_local_refs() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "node": {"$ref": "#/$defs/Node"}
        },
        "$defs": {
            "Node": {
                "type": "object",
                "properties": {
                    "next": {"$ref": "#/$defs/Node"}
                }
            }
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": { "node": schema_ref("#/$defs/Node") },
            "$defs": {
                "Node": object(vec![("next", schema_ref("#/$defs/Node"))], None, None)
            }
        })
    );
}

/// The legacy `definitions` table is kept, and refs are followed through it.
#[test]
fn parse_tool_input_schema_preserves_legacy_definitions() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "user": {"$ref": "#/definitions/User"}
        },
        "definitions": {
            "User": {
                "type": "object",
                "properties": {
                    "profile": {"$ref": "#/definitions/Profile"}
                }
            },
            "Profile": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                }
            },
            "Unused": {"type": "string"}
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": { "user": schema_ref("#/definitions/User") },
            "definitions": {
                "Profile": object(vec![("name", string())], None, None),
                "User": object(
                    vec![("profile", schema_ref("#/definitions/Profile"))],
                    None,
                    None
                )
            }
        })
    );
}

/// Unresolved local refs and external refs are kept; unreachable definitions are still pruned.
#[test]
fn parse_tool_input_schema_preserves_unresolved_and_external_refs() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "missing": {"$ref": "#/$defs/Missing"},
            "remote": {"$ref": "https://example.com/schema.json"}
        },
        "$defs": {
            "Unused": {"type": "string"}
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "missing": schema_ref("#/$defs/Missing"),
                "remote": schema_ref("https://example.com/schema.json")
            }
        })
    );
}

/// A ref into a definition keeps the whole parent definition.
#[test]
fn parse_tool_input_schema_preserves_nested_defs_ref_parent() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "name": {"$ref": "#/$defs/User/properties/name"}
        },
        "$defs": {
            "User": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                }
            },
            "name": {"type": "string"},
            "Unused": {"type": "boolean"}
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": { "name": schema_ref("#/$defs/User/properties/name") },
            "$defs": { "User": object(vec![("name", string())], None, None) }
        })
    );
}

/// A percent-encoded fragment is decoded before its JSON Pointer escapes (RFC 6901 §6), and the
/// refs themselves are kept as written.
#[test]
fn parse_tool_input_schema_preserves_percent_encoded_definition_refs() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "user": {"$ref": "#/$defs/User%20Name"},
            "profile": {"$ref": "#/%24defs/Profile%7E0Name"}
        },
        "$defs": {
            "User Name": {"type": "string"},
            "Profile~Name": {"type": "string"},
            "Unused": {"type": "boolean"}
        }
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "profile": schema_ref("#/%24defs/Profile%7E0Name"),
                "user": schema_ref("#/$defs/User%20Name")
            },
            "$defs": { "Profile~Name": string(), "User Name": string() }
        })
    );
}

/// A definition table that is not an object is dropped; the ref into it stays.
#[test]
fn parse_tool_input_schema_drops_malformed_definition_tables() {
    let schema = parse(json!({
        "type": "object",
        "properties": {
            "user": {"$ref": "#/$defs/User"}
        },
        "$defs": ["not", "an", "object"]
    }));

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": { "user": schema_ref("#/$defs/User") }
        })
    );
}

/// Pointers that are not local definition refs reach nothing: a fragment that is not a JSON
/// Pointer, an escape other than `~0` and `~1`, a table with no definition named, and a fragment
/// that is not UTF-8 once decoded.
#[test]
fn parse_tool_input_schema_does_not_resolve_malformed_definition_refs() {
    for target in [
        "#$defs/User",
        "#/$defs/Us~2er",
        "#/$defs",
        "#/$defs/%FF",
        "#/other/User",
    ] {
        let schema = parse(json!({
            "type": "object",
            "properties": { "user": {"$ref": target} },
            "$defs": { "User": {"type": "string"} }
        }));

        assert_eq!(
            schema,
            json!({
                "type": "object",
                "properties": { "user": schema_ref(target) }
            }),
            "{target}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

fn closed_object() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

fn dynamic_tool(
    namespace: Option<&str>,
    name: impl Into<String>,
    input_schema: Value,
    defer_loading: bool,
) -> DynamicToolSpec {
    let function =
        DynamicToolFunctionSpec::new(name, "test", input_schema).with_defer_loading(defer_loading);
    match namespace {
        Some(namespace) => DynamicToolSpec::Namespace(DynamicToolNamespaceSpec::new(
            namespace,
            "test namespace",
            [DynamicToolNamespaceTool::Function(function)],
        )),
        None => DynamicToolSpec::Function(function),
    }
}

#[test]
fn validate_dynamic_tools_rejects_unsupported_input_schema() {
    let tools = vec![dynamic_tool(
        None,
        "my_tool",
        json!({"type": "null"}),
        false,
    )];
    let err = validate_dynamic_tools(&tools).expect_err("invalid schema");
    assert_eq!(
        err,
        "dynamic tool input schema is not supported for my_tool: tool input schema must not be a singleton null type"
    );
}

/// A missing `type` is common, and the schema subset sanitizes it.
#[test]
fn validate_dynamic_tools_accepts_sanitizable_input_schema() {
    let tools = vec![dynamic_tool(
        None,
        "my_tool",
        json!({"properties": {}}),
        false,
    )];
    validate_dynamic_tools(&tools).expect("valid schema");
}

#[test]
fn validate_dynamic_tools_accepts_nullable_field_schema() {
    let tools = vec![dynamic_tool(
        None,
        "my_tool",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": ["string", "null"]}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        false,
    )];
    validate_dynamic_tools(&tools).expect("valid schema");
}

#[test]
fn validate_dynamic_tools_accepts_same_name_in_different_namespaces() {
    let tools = vec![
        dynamic_tool(Some("codex_app"), "my_tool", closed_object(), true),
        dynamic_tool(Some("other_app"), "my_tool", closed_object(), true),
    ];
    validate_dynamic_tools(&tools).expect("valid schema");
}

#[test]
fn validate_dynamic_tools_accepts_responses_compatible_identifiers() {
    let tools = vec![dynamic_tool(
        Some("Codex-App_2"),
        "lookup-ticket_2",
        closed_object(),
        true,
    )];
    validate_dynamic_tools(&tools).expect("valid schema");
}

#[test]
fn validate_dynamic_tools_rejects_duplicate_name_in_same_namespace() {
    let function = || {
        DynamicToolNamespaceTool::Function(
            DynamicToolFunctionSpec::new("my_tool", "test", closed_object())
                .with_defer_loading(true),
        )
    };
    let tools = vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec::new(
        "codex_app",
        "test namespace",
        [function(), function()],
    ))];
    let err = validate_dynamic_tools(&tools).expect_err("duplicate name");
    assert_eq!(
        err,
        "duplicate dynamic tool name in namespace codex_app: my_tool"
    );
}

/// Outside a namespace a name may not repeat either.
#[test]
fn validate_dynamic_tools_rejects_duplicate_top_level_name() {
    let tools = vec![
        dynamic_tool(None, "my_tool", closed_object(), false),
        dynamic_tool(None, "my_tool", closed_object(), false),
    ];
    let err = validate_dynamic_tools(&tools).expect_err("duplicate name");
    assert_eq!(err, "duplicate dynamic tool name: my_tool");
}

#[test]
fn validate_dynamic_tools_rejects_empty_namespace() {
    let tools = vec![dynamic_tool(Some(""), "my_tool", closed_object(), false)];
    let err = validate_dynamic_tools(&tools).expect_err("empty namespace");
    assert_eq!(err, "dynamic tool namespace must not be empty");
}

#[test]
fn validate_dynamic_tools_rejects_reserved_namespace() {
    let tools = vec![dynamic_tool(
        Some("mcp__server__"),
        "my_tool",
        closed_object(),
        false,
    )];
    let err = validate_dynamic_tools(&tools).expect_err("reserved namespace");
    assert_eq!(err, "dynamic tool namespace is reserved: mcp__server__");
}

#[test]
fn validate_dynamic_tools_rejects_name_not_supported_by_responses() {
    let tools = vec![dynamic_tool(None, "lookup.ticket", closed_object(), false)];
    let err = validate_dynamic_tools(&tools).expect_err("invalid name");
    assert_eq!(
        err,
        "dynamic tool name must match ^[a-zA-Z0-9_-]+$ to match Responses API: lookup.ticket"
    );
}

#[test]
fn validate_dynamic_tools_rejects_namespace_not_supported_by_responses() {
    let tools = vec![dynamic_tool(
        Some("codex.app"),
        "lookup_ticket",
        closed_object(),
        true,
    )];
    let err = validate_dynamic_tools(&tools).expect_err("invalid namespace");
    assert_eq!(
        err,
        "dynamic tool namespace must match ^[a-zA-Z0-9_-]+$ to match Responses API: codex.app"
    );
}

#[test]
fn validate_dynamic_tools_rejects_name_longer_than_responses_limit() {
    let long_name = "a".repeat(129);
    let tools = vec![dynamic_tool(
        None,
        long_name.clone(),
        closed_object(),
        false,
    )];
    let err = validate_dynamic_tools(&tools).expect_err("name too long");
    assert_eq!(
        err,
        format!(
            "dynamic tool name must be at most 128 characters to match Responses API: {long_name}"
        )
    );
    validate_dynamic_tools(&[dynamic_tool(None, "a".repeat(128), closed_object(), false)])
        .expect("128 characters fit");
}

#[test]
fn validate_dynamic_tools_rejects_namespace_fields_over_limits() {
    let long_namespace = "a".repeat(65);
    let tools = vec![dynamic_tool(
        Some(&long_namespace),
        "lookup_ticket",
        closed_object(),
        true,
    )];
    let err = validate_dynamic_tools(&tools).expect_err("namespace too long");
    assert_eq!(
        err,
        format!(
            "dynamic tool namespace must be at most 64 characters to match Responses API: {long_namespace}"
        )
    );

    let namespace = |description: String| {
        vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec::new(
            "tickets",
            description,
            [DynamicToolNamespaceTool::Function(
                DynamicToolFunctionSpec::new("lookup_ticket", "test", closed_object()),
            )],
        ))]
    };
    let err = validate_dynamic_tools(&namespace("a".repeat(1025)))
        .expect_err("namespace description too long");
    assert_eq!(
        err,
        "dynamic tool namespace description must be at most 1024 characters"
    );
    validate_dynamic_tools(&namespace("a".repeat(1024))).expect("1024 characters fit");
}

#[test]
fn validate_dynamic_tools_rejects_reserved_responses_namespace() {
    let tools = vec![dynamic_tool(
        Some("functions"),
        "lookup_ticket",
        closed_object(),
        true,
    )];
    let err = validate_dynamic_tools(&tools).expect_err("reserved Responses namespace");
    assert_eq!(
        err,
        "dynamic tool namespace collides with a reserved Responses API namespace: functions"
    );
}

/// The remaining checks, each with Codex's text: names and namespaces with surrounding
/// whitespace, an empty name, the reserved `mcp` names, a namespace with no tools, a repeated
/// namespace, and a deferred tool outside a namespace.
#[test]
fn validate_dynamic_tools_rejects_the_remaining_cases() {
    let namespace_of = |name: &str, tools: Vec<DynamicToolFunctionSpec>| {
        DynamicToolSpec::Namespace(DynamicToolNamespaceSpec::new(
            name,
            "test namespace",
            tools.into_iter().map(DynamicToolNamespaceTool::Function),
        ))
    };
    let function = |name: &str| DynamicToolFunctionSpec::new(name, "test", closed_object());
    for (tools, expected) in [
        (
            vec![dynamic_tool(None, " lookup", closed_object(), false)],
            "dynamic tool name has leading/trailing whitespace:  lookup",
        ),
        (
            vec![dynamic_tool(None, "\tlookup", closed_object(), false)],
            "dynamic tool name has leading/trailing whitespace: \\tlookup",
        ),
        (
            vec![dynamic_tool(None, "  ", closed_object(), false)],
            "dynamic tool name must not be empty",
        ),
        (
            vec![dynamic_tool(None, "mcp", closed_object(), false)],
            "dynamic tool name is reserved: mcp",
        ),
        (
            vec![dynamic_tool(
                None,
                "mcp__server__tool",
                closed_object(),
                false,
            )],
            "dynamic tool name is reserved: mcp__server__tool",
        ),
        (
            vec![dynamic_tool(
                Some("apps "),
                "lookup",
                closed_object(),
                false,
            )],
            "dynamic tool namespace has leading/trailing whitespace: apps ",
        ),
        (
            vec![dynamic_tool(Some("mcp"), "lookup", closed_object(), false)],
            "dynamic tool namespace is reserved: mcp",
        ),
        (
            vec![namespace_of("apps", Vec::new())],
            "dynamic tool namespace must contain at least one tool: apps",
        ),
        (
            vec![
                namespace_of("apps", vec![function("first")]),
                namespace_of("apps", vec![function("second")]),
            ],
            "duplicate dynamic tool namespace: apps",
        ),
        (
            vec![dynamic_tool(None, "hidden_tool", closed_object(), true)],
            "deferred dynamic tool must include a namespace: hidden_tool",
        ),
    ] {
        assert_eq!(
            validate_dynamic_tools(&tools).expect_err(expected),
            expected
        );
    }
}

/// A name may repeat between the top level and a namespace.
#[test]
fn validate_dynamic_tools_accepts_a_top_level_name_repeated_in_a_namespace() {
    let tools = vec![
        dynamic_tool(None, "lookup", closed_object(), false),
        dynamic_tool(Some("apps"), "lookup", closed_object(), false),
    ];
    validate_dynamic_tools(&tools).expect("valid tools");
}

// ---------------------------------------------------------------------------------------------
// The two shapes, and the call item
// ---------------------------------------------------------------------------------------------

/// Codex's `session_meta_normalizes_legacy_dynamic_tools`: flat tools are grouped by namespace,
/// with `exposeToContext: false` read as deferred unless `deferLoading` says otherwise.
#[test]
fn normalize_dynamic_tool_specs_groups_legacy_tools_by_namespace() {
    let tools = normalize_dynamic_tool_specs(vec![
        json!({
            "namespace": "legacy_app",
            "name": "lookup_ticket",
            "description": "Look up a ticket",
            "inputSchema": {"type": "object", "properties": {}},
            "exposeToContext": false
        }),
        json!({
            "namespace": "legacy_app",
            "name": "update_ticket",
            "description": "Update a ticket",
            "inputSchema": {"type": "object", "properties": {}},
            "deferLoading": false,
            "exposeToContext": false
        }),
    ])
    .unwrap();

    assert_eq!(
        tools,
        vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec::new(
            "legacy_app",
            "",
            [
                DynamicToolNamespaceTool::Function(
                    DynamicToolFunctionSpec::new(
                        "lookup_ticket",
                        "Look up a ticket",
                        json!({"type": "object", "properties": {}}),
                    )
                    .with_defer_loading(true),
                ),
                DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec::new(
                    "update_ticket",
                    "Update a ticket",
                    json!({"type": "object", "properties": {}}),
                )),
            ],
        ))]
    );
}

/// The legacy shape keeps tools without a namespace at the top level, in order, and reads
/// `exposeToContext: true` as not deferred; namespaces gather tools that are not adjacent.
#[test]
fn normalize_dynamic_tool_specs_keeps_top_level_legacy_tools_in_order() {
    let schema = json!({"type": "object", "properties": {}});
    let tools = normalize_dynamic_tool_specs(vec![
        json!({"namespace": "apps", "name": "first", "description": "1", "inputSchema": schema}),
        json!({"name": "plain", "description": "2", "inputSchema": schema, "exposeToContext": true}),
        json!({"namespace": "apps", "name": "second", "description": "3", "inputSchema": schema}),
    ])
    .unwrap();

    assert_eq!(
        tools,
        vec![
            DynamicToolSpec::Namespace(DynamicToolNamespaceSpec::new(
                "apps",
                "",
                [
                    DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec::new(
                        "first",
                        "1",
                        schema.clone()
                    )),
                    DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec::new(
                        "second",
                        "3",
                        schema.clone()
                    )),
                ],
            )),
            DynamicToolSpec::Function(DynamicToolFunctionSpec::new("plain", "2", schema)),
        ]
    );
}

/// An entry without a `type` is in the legacy shape even when it names no namespace and carries
/// no `exposeToContext`.
#[test]
fn normalize_dynamic_tool_specs_reads_untyped_tools_as_legacy() {
    let schema = json!({"type": "object", "properties": {}});
    let tools = normalize_dynamic_tool_specs(vec![
        json!({"name": "lookup", "description": "Look up", "inputSchema": schema}),
    ])
    .unwrap();

    assert_eq!(
        tools,
        vec![DynamicToolSpec::Function(DynamicToolFunctionSpec::new(
            "lookup", "Look up", schema
        ))]
    );
}

/// The canonical shape is read as it is and written back the same; a list mixing the two shapes
/// is refused with Codex's text, as is a canonical namespace holding a legacy tool.
#[test]
fn normalize_dynamic_tool_specs_reads_the_canonical_shape_and_refuses_mixed_shapes() {
    let canonical = vec![
        json!({
            "type": "function",
            "name": "lookup",
            "description": "Look up",
            "inputSchema": {"type": "object", "properties": {}},
            "deferLoading": true
        }),
        json!({
            "type": "namespace",
            "name": "apps",
            "description": "Apps",
            "tools": [{
                "type": "function",
                "name": "open",
                "description": "Open",
                "inputSchema": {"type": "object", "properties": {}}
            }]
        }),
    ];
    let tools = normalize_dynamic_tool_specs(canonical.clone()).unwrap();
    assert_eq!(serde_json::to_value(&tools).unwrap(), json!(canonical));

    for mixed in [
        vec![
            canonical[0].clone(),
            json!({"namespace": "apps", "name": "legacy", "description": "", "inputSchema": {}}),
        ],
        vec![json!({
            "type": "namespace",
            "name": "apps",
            "description": "Apps",
            "tools": [{
                "type": "function",
                "name": "open",
                "description": "Open",
                "inputSchema": {},
                "exposeToContext": false
            }]
        })],
    ] {
        assert_eq!(
            normalize_dynamic_tool_specs(mixed).unwrap_err().to_string(),
            "dynamic tools must use either canonical or legacy format consistently"
        );
    }
}

/// The call item of Codex's v2 conversion, as it is written.
#[test]
fn dynamic_tool_call_item_serializes_as_codex_writes_it() {
    let item: ThreadItem = DynamicToolCallItem::new(
        "dynamic-1",
        "lookup",
        json!({"id": "123"}),
        DynamicToolCallStatus::Completed,
    )
    .with_namespace("apps")
    .with_response(
        vec![
            DynamicToolCallOutputContentItem::input_text("ok"),
            DynamicToolCallOutputContentItem::input_image("data:image/png;base64,AAA"),
            DynamicToolCallOutputContentItem::input_audio("data:audio/wav;base64,YXVkaW8="),
        ],
        true,
    )
    .with_duration_ms(5)
    .into();

    let written = json!({
        "type": "dynamicToolCall",
        "id": "dynamic-1",
        "namespace": "apps",
        "tool": "lookup",
        "arguments": {"id": "123"},
        "status": "completed",
        "contentItems": [
            {"type": "inputText", "text": "ok"},
            {"type": "inputImage", "imageUrl": "data:image/png;base64,AAA"},
            {"type": "inputAudio", "audioUrl": "data:audio/wav;base64,YXVkaW8="}
        ],
        "success": true,
        "durationMs": 5
    });
    assert_eq!(serde_json::to_value(&item).unwrap(), written);
    assert_eq!(serde_json::from_value::<ThreadItem>(written).unwrap(), item);
    assert_eq!(item.id(), "dynamic-1");

    let started: ThreadItem = DynamicToolCallItem::new(
        "dynamic-2",
        "lookup",
        json!({}),
        DynamicToolCallStatus::InProgress,
    )
    .into();
    assert_eq!(
        serde_json::to_value(&started).unwrap(),
        json!({
            "type": "dynamicToolCall",
            "id": "dynamic-2",
            "namespace": null,
            "tool": "lookup",
            "arguments": {},
            "status": "inProgress",
            "contentItems": null,
            "success": null,
            "durationMs": null
        })
    );
}

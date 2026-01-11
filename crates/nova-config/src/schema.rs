use schemars::schema::{RootSchema, Schema};
use schemars::schema_for;
use serde_json::json;

use crate::NovaConfig;

/// JSON schema for `nova.toml`.
///
/// This schema is intended for editor tooling (TOML JSON schema integration) and CI validation.
#[must_use]
pub fn json_schema() -> RootSchema {
    let mut schema = schema_for!(NovaConfig);
    apply_semantic_constraints(&mut schema);
    schema
}

pub(crate) fn url_schema(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    let mut schema: schemars::schema::SchemaObject =
        <String as schemars::JsonSchema>::json_schema(generator).into();
    schema.format = Some("uri".to_owned());
    schema.into()
}

fn apply_semantic_constraints(schema: &mut RootSchema) {
    // Encode the most important semantic validation checks into the schema so editor/CI validation
    // catches obvious misconfiguration without running Nova.
    //
    // Note: JSON Schema does not apply defaults during validation, so these constraints only trigger
    // when the relevant keys are explicitly set in `nova.toml`.
    push_all_of(
        schema,
        schema_from_json(json!({
            "if": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["enabled", "provider"],
                        "properties": {
                            "enabled": { "const": true },
                            "provider": {
                                "required": ["kind"],
                                "properties": {
                                    "kind": { "enum": ["open_ai", "anthropic", "gemini", "azure_open_ai"] }
                                }
                            }
                        }
                    }
                }
            },
            "then": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["api_key"],
                        "properties": {
                            "api_key": { "type": "string", "minLength": 1 }
                        }
                    }
                }
            }
        })),
    );

    push_all_of(
        schema,
        schema_from_json(json!({
            "if": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["enabled", "provider"],
                        "properties": {
                            "enabled": { "const": true },
                            "provider": {
                                "required": ["kind"],
                                "properties": {
                                    "kind": { "const": "azure_open_ai" }
                                }
                            }
                        }
                    }
                }
            },
            "then": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["provider"],
                        "properties": {
                            "provider": {
                                "required": ["azure_deployment"],
                                "properties": {
                                    "azure_deployment": { "type": "string", "minLength": 1 }
                                }
                            }
                        }
                    }
                }
            }
        })),
    );

    push_all_of(
        schema,
        schema_from_json(json!({
            "if": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["enabled", "provider"],
                        "properties": {
                            "enabled": { "const": true },
                            "provider": {
                                "required": ["kind"],
                                "properties": {
                                    "kind": { "const": "in_process_llama" }
                                }
                            }
                        }
                    }
                }
            },
            "then": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["provider"],
                        "properties": {
                            "provider": {
                                "required": ["in_process_llama"],
                                "properties": {
                                    "in_process_llama": { "$ref": "#/definitions/InProcessLlamaConfig" }
                                }
                            }
                        }
                    }
                }
            }
        })),
    );

    // Cloud providers require explicitly opting out of `local_only` (defaults to true).
    push_all_of(
        schema,
        schema_from_json(json!({
            "if": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["enabled", "provider"],
                        "properties": {
                            "enabled": { "const": true },
                            "provider": {
                                "required": ["kind"],
                                "properties": {
                                    "kind": { "enum": ["open_ai", "anthropic", "gemini", "azure_open_ai"] }
                                }
                            }
                        }
                    }
                }
            },
            "then": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["privacy"],
                        "properties": {
                            "privacy": {
                                "required": ["local_only"],
                                "properties": {
                                    "local_only": { "const": false }
                                }
                            }
                        }
                    }
                }
            }
        })),
    );

    // When `local_only=true`, HTTP-backed providers must point at a loopback address.
    // This is a best-effort approximation of the runtime check in `nova-ai` (URL host parsing is
    // not something JSON Schema can do robustly).
    push_all_of(
        schema,
        schema_from_json(json!({
            "if": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["enabled", "privacy", "provider"],
                        "properties": {
                            "enabled": { "const": true },
                            "privacy": {
                                "required": ["local_only"],
                                "properties": {
                                    "local_only": { "const": true }
                                }
                            },
                            "provider": {
                                "required": ["kind"],
                                "properties": {
                                    "kind": { "enum": ["ollama", "open_ai_compatible", "http"] }
                                }
                            }
                        }
                    }
                }
            },
            "then": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["provider"],
                        "properties": {
                            "provider": {
                                "required": ["url"],
                                "properties": {
                                    "url": {
                                        "type": "string",
                                        "pattern": "^https?://(localhost|127\\\\.0\\\\.0\\\\.1|\\\\[::1\\\\])(:[0-9]+)?(/|\\\\?|#|$)"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })),
    );

    allow_deprecated_aliases(schema);
    disallow_alias_collisions(schema);

    // Minor semantic constraints that are easier to express by post-processing the generated schema.
    set_min_items(schema, "GeneratedSourcesConfig", "override_roots", 1);
    set_property_write_only(schema, "AiConfig", "api_key", true);
}

fn push_all_of(root: &mut RootSchema, schema: Schema) {
    let subschemas = root.schema.subschemas();
    subschemas.all_of.get_or_insert_with(Vec::new).push(schema);
}

fn schema_from_json(value: serde_json::Value) -> Schema {
    serde_json::from_value(value).expect("valid json schema")
}

fn allow_deprecated_aliases(schema: &mut RootSchema) {
    // Keep the schema aligned with what the runtime accepts (serde aliases), while still steering
    // users away from legacy keys.
    add_deprecated_property(
        schema,
        "JdkConfig",
        "jdk_home",
        schema_from_json(json!({
            "deprecated": true,
            "description": "Deprecated alias for `jdk.home`.",
            "default": null,
            "type": ["string", "null"]
        })),
    );

    add_deprecated_property(
        schema,
        "AiPrivacyConfig",
        "anonymize",
        schema_from_json(json!({
            "deprecated": true,
            "description": "Deprecated alias for `ai.privacy.anonymize_identifiers`.",
            "default": null,
            "type": ["boolean", "null"]
        })),
    );
}

fn disallow_alias_collisions(schema: &mut RootSchema) {
    // Serde aliases mean multiple TOML keys deserialize into the same struct field. If users specify
    // both forms, deserialization fails with a duplicate-field error. Encode the same constraint in
    // the schema so editor/CI validation can catch it early.
    disallow_both_required(schema, "JdkConfig", "home", "jdk_home");
    disallow_both_required(
        schema,
        "AiPrivacyConfig",
        "anonymize",
        "anonymize_identifiers",
    );
}

fn disallow_both_required(schema: &mut RootSchema, definition_name: &str, a: &str, b: &str) {
    let Some(definition) = schema.definitions.get_mut(definition_name) else {
        return;
    };

    let Schema::Object(obj) = definition else {
        return;
    };

    obj.subschemas()
        .all_of
        .get_or_insert_with(Vec::new)
        .push(schema_from_json(json!({ "not": { "required": [a, b] } })));
}

fn add_deprecated_property(
    schema: &mut RootSchema,
    definition_name: &str,
    property_name: &str,
    property_schema: Schema,
) {
    let Some(definition) = schema.definitions.get_mut(definition_name) else {
        return;
    };

    let Schema::Object(obj) = definition else {
        return;
    };

    let object_validation = obj.object();
    object_validation
        .properties
        .insert(property_name.to_string(), property_schema);
}

fn set_min_items(
    schema: &mut RootSchema,
    definition_name: &str,
    property_name: &str,
    min_items: u32,
) {
    let Some(definition) = schema.definitions.get_mut(definition_name) else {
        return;
    };

    let Schema::Object(obj) = definition else {
        return;
    };

    let object_validation = obj.object();
    let Some(prop_schema) = object_validation.properties.get_mut(property_name) else {
        return;
    };

    let Schema::Object(prop_obj) = prop_schema else {
        return;
    };

    prop_obj.array().min_items = Some(min_items);
}

fn set_property_write_only(
    schema: &mut RootSchema,
    definition_name: &str,
    property_name: &str,
    write_only: bool,
) {
    let Some(definition) = schema.definitions.get_mut(definition_name) else {
        return;
    };

    let Schema::Object(obj) = definition else {
        return;
    };

    let object_validation = obj.object();
    let Some(prop_schema) = object_validation.properties.get_mut(property_name) else {
        return;
    };

    let Schema::Object(prop_obj) = prop_schema else {
        return;
    };

    prop_obj.metadata().write_only = write_only;
}

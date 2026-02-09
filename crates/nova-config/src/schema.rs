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
    let _ = generator;
    schema_from_json(json!({
        "type": "string",
        "format": "uri",
        "pattern": "^https?://"
    }))
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
                            "api_key": { "type": "string", "minLength": 1, "pattern": "^\\S+$" }
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
                        "properties": {
                            "provider": {
                                "properties": {
                                    "url": {
                                        "type": "string",
                                        "pattern": "^https?://(localhost|127\\.0\\.0\\.1|\\[::1\\])(:[0-9]+)?(/|\\?|#|$)"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })),
    );

    // When `ai.provider.url` points at a non-loopback address, `local_only` must be set to `false`.
    //
    // This covers the common case where `ai.privacy.local_only` is omitted from `nova.toml` (it
    // defaults to true at runtime). Since JSON Schema doesn't apply defaults, the `local_only=true`
    // rule above would not fire, so we also encode the inverse: non-loopback URL implies explicit
    // opt-out of local-only mode.
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
                                "required": ["kind", "url"],
                                "properties": {
                                    "kind": { "enum": ["ollama", "open_ai_compatible", "http"] },
                                    "url": {
                                        "type": "string",
                                        "not": { "pattern": "^https?://(localhost|127\\.0\\.0\\.1|\\[::1\\])(:[0-9]+)?(/|\\?|#|$)" }
                                    }
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

    // `ai.audit_log.enabled` has no effect unless AI is enabled (audit events are only emitted when
    // `ai.enabled=true`).
    push_all_of(
        schema,
        schema_from_json(json!({
            "if": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["audit_log"],
                        "properties": {
                            "audit_log": {
                                "required": ["enabled"],
                                "properties": {
                                    "enabled": { "const": true }
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
                        "required": ["enabled"],
                        "properties": {
                            "enabled": { "const": true }
                        }
                    }
                }
            }
        })),
    );

    // Cloud code-editing requires a strict set of opt-ins because patches cannot be applied
    // reliably to anonymized identifiers and because code edits generally include more source code.
    //
    // This mirrors `nova_ai::code_edit_policy::enforce_code_edit_policy` at the schema level so
    // editor/CI validation catches misconfiguration early.
    push_all_of(
        schema,
        schema_from_json(json!({
            "if": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["enabled", "privacy"],
                        "properties": {
                            "enabled": { "const": true },
                            "privacy": {
                                "required": ["local_only", "allow_cloud_code_edits"],
                                "properties": {
                                    "local_only": { "const": false },
                                    "allow_cloud_code_edits": { "const": true }
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
                        "properties": {
                            "privacy": {
                                "required": ["allow_code_edits_without_anonymization"],
                                "properties": {
                                    "allow_code_edits_without_anonymization": { "const": true }
                                },
                                "anyOf": [
                                    {
                                        "required": ["anonymize_identifiers"],
                                        "properties": {
                                            "anonymize_identifiers": { "const": false }
                                        }
                                    },
                                    {
                                        "required": ["anonymize"],
                                        "properties": {
                                            "anonymize": { "const": false }
                                        }
                                    }
                                ]
                            }
                        }
                    }
                }
            }
        })),
    );

    // Cloud code edits require the base opt-in `allow_cloud_code_edits=true`. If the user sets the
    // more specific `allow_code_edits_without_anonymization=true` flag, they must set the base flag
    // as well (otherwise it has no effect).
    push_all_of(
        schema,
        schema_from_json(json!({
            "if": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["enabled", "privacy"],
                        "properties": {
                            "enabled": { "const": true },
                            "privacy": {
                                "required": ["local_only", "allow_code_edits_without_anonymization"],
                                "properties": {
                                    "local_only": { "const": false },
                                    "allow_code_edits_without_anonymization": { "const": true }
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
                        "properties": {
                            "privacy": {
                                "required": ["allow_cloud_code_edits"],
                                "properties": {
                                    "allow_cloud_code_edits": { "const": true }
                                }
                            }
                        }
                    }
                }
            }
        })),
    );

    // Cloud multi-token completions include identifier-heavy method/import lists, so they require
    // explicitly disabling identifier anonymization (similar to code-edit opt-in).
    push_all_of(
        schema,
        schema_from_json(json!({
            "if": {
                "required": ["ai"],
                "properties": {
                    "ai": {
                        "required": ["enabled", "privacy", "features"],
                        "properties": {
                            "enabled": { "const": true },
                            "privacy": {
                                "required": ["local_only"],
                                "properties": {
                                    "local_only": { "const": false }
                                }
                            },
                            "features": {
                                "required": ["multi_token_completion"],
                                "properties": {
                                    "multi_token_completion": { "const": true }
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
                        "properties": {
                            "privacy": {
                                "anyOf": [
                                    {
                                        "required": ["anonymize_identifiers"],
                                        "properties": {
                                            "anonymize_identifiers": { "const": false }
                                        }
                                    },
                                    {
                                        "required": ["anonymize"],
                                        "properties": {
                                            "anonymize": { "const": false }
                                        }
                                    }
                                ]
                            }
                        }
                    }
                }
            }
        })),
    );

    // Build integration: if the user opts into build tool invocation, at least one build tool must
    // remain enabled. Since per-tool toggles default to `true` and JSON Schema does not apply
    // defaults, we only encode the "all tools disabled" misconfiguration here.
    push_all_of(
        schema,
        schema_from_json(json!({
            "not": {
                "required": ["build"],
                "properties": {
                    "build": {
                        "required": ["enabled", "maven", "gradle"],
                        "properties": {
                            "enabled": { "const": true },
                            "maven": {
                                "required": ["enabled"],
                                "properties": {
                                    "enabled": { "const": false }
                                }
                            },
                            "gradle": {
                                "required": ["enabled"],
                                "properties": {
                                    "enabled": { "const": false }
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
    set_min_items(schema, "ExtensionsConfig", "allow", 1);
    set_min_length(schema, "AiProviderConfig", "model", 1);
    set_min_length(schema, "AiProviderConfig", "azure_deployment", 1);
    set_min_length(schema, "AiProviderConfig", "azure_api_version", 1);
    set_min_length(schema, "AiConfig", "api_key", 1);
    set_min_length(schema, "JdkConfig", "home", 1);
    set_min_length(schema, "JdkConfig", "jdk_home", 1);
    set_minimum(schema, "JdkConfig", "release", 1.0);
    set_minimum(schema, "JdkConfig", "target_release", 1.0);
    set_minimum(schema, "JdkToolchainConfig", "release", 1.0);
    set_min_length(schema, "JdkToolchainConfig", "home", 1);
    set_min_length(schema, "LoggingConfig", "file", 1);
    set_min_length(schema, "AuditLogConfig", "path", 1);
    set_min_length(schema, "AiEmbeddingsConfig", "model", 1);
    set_min_length(schema, "AiEmbeddingsConfig", "model_dir", 1);
    set_min_length(schema, "InProcessLlamaConfig", "model_path", 1);
    set_array_item_min_length(schema, "AiPrivacyConfig", "excluded_paths", 1);
    set_array_item_min_length(schema, "AiPrivacyConfig", "redact_patterns", 1);
    set_array_item_min_length(schema, "ExtensionsConfig", "wasm_paths", 1);
    set_array_item_min_length(schema, "ExtensionsConfig", "allow", 1);
    set_array_item_min_length(schema, "ExtensionsConfig", "deny", 1);
    set_array_item_min_length(schema, "GeneratedSourcesConfig", "additional_roots", 1);
    set_array_item_min_length(schema, "GeneratedSourcesConfig", "override_roots", 1);
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
        "JdkConfig",
        "target_release",
        schema_from_json(json!({
            "deprecated": true,
            "description": "Deprecated alias for `jdk.release`.",
            "default": null,
            "type": ["integer", "null"],
            "format": "uint16",
            "minimum": 1
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
    disallow_both_required(schema, "JdkConfig", "release", "target_release");
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

fn set_min_length(
    schema: &mut RootSchema,
    definition_name: &str,
    property_name: &str,
    min_length: u32,
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

    prop_obj.string().min_length = Some(min_length);
}

fn set_array_item_min_length(
    schema: &mut RootSchema,
    definition_name: &str,
    property_name: &str,
    min_length: u32,
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

    let Some(items) = prop_obj.array().items.as_mut() else {
        return;
    };

    match items {
        schemars::schema::SingleOrVec::Single(item_schema) => {
            set_schema_min_length(item_schema, min_length);
        }
        schemars::schema::SingleOrVec::Vec(item_schemas) => {
            for item_schema in item_schemas.iter_mut() {
                set_schema_min_length(item_schema, min_length);
            }
        }
    }
}

fn set_minimum(schema: &mut RootSchema, definition_name: &str, property_name: &str, minimum: f64) {
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

    prop_obj.number().minimum = Some(minimum);
}

fn set_schema_min_length(schema: &mut Schema, min_length: u32) {
    if let Schema::Object(obj) = schema {
        obj.string().min_length = Some(min_length);
    }
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

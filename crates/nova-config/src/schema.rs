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
}

fn push_all_of(root: &mut RootSchema, schema: Schema) {
    let subschemas = root.schema.subschemas();
    subschemas.all_of.get_or_insert_with(Vec::new).push(schema);
}

fn schema_from_json(value: serde_json::Value) -> Schema {
    serde_json::from_value(value).expect("valid json schema")
}

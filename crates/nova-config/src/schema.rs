use schemars::schema::RootSchema;
use schemars::schema_for;

use crate::NovaConfig;

/// JSON schema for `nova.toml`.
///
/// This schema is intended for editor tooling (TOML JSON schema integration) and CI validation.
#[must_use]
pub fn json_schema() -> RootSchema {
    schema_for!(NovaConfig)
}


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

pub(crate) fn url_schema(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    let mut schema: schemars::schema::SchemaObject =
        <String as schemars::JsonSchema>::json_schema(generator).into();
    schema.format = Some("uri".to_owned());
    schema.into()
}

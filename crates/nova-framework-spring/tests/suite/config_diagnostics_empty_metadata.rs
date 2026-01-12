use std::path::Path;

use nova_config_metadata::MetadataIndex;
use nova_framework_spring::{diagnostics_for_config_file, SPRING_UNKNOWN_CONFIG_KEY};

#[test]
fn does_not_report_unknown_keys_when_metadata_is_empty() {
    let metadata = MetadataIndex::new();
    let text = "server.port=8080\n";

    let diags = diagnostics_for_config_file(Path::new("application.properties"), text, &metadata);

    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == SPRING_UNKNOWN_CONFIG_KEY),
        "expected no {SPRING_UNKNOWN_CONFIG_KEY} diagnostics when metadata is empty; got {diags:?}"
    );
}

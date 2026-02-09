mod support;

// AI-related integration tests require the `ai` feature. Keep them gated so the crate can be
// compiled/tested with `--no-default-features` (e.g. for faster CI or minimal builds).
#[cfg(feature = "ai")]
#[path = "suite/ai_code_actions.rs"]
mod ai_code_actions;
#[cfg(feature = "ai")]
#[path = "suite/ai_custom_requests.rs"]
mod ai_custom_requests;
#[cfg(feature = "ai")]
#[path = "suite/ai_completion_more.rs"]
mod ai_completion_more;
#[cfg(feature = "ai")]
#[path = "suite/ai_excluded_paths.rs"]
mod ai_excluded_paths;
#[cfg(feature = "ai")]
#[path = "suite/stdio_ai_cancellation.rs"]
mod stdio_ai_cancellation;
#[path = "suite/cli_help.rs"]
mod cli_help;
#[path = "suite/completion_resolve.rs"]
mod completion_resolve;
#[path = "suite/config_stdio.rs"]
mod config_stdio;
#[path = "suite/extensions_aware_helpers.rs"]
mod extensions_aware_helpers;
#[path = "suite/extensions_stdio.rs"]
mod extensions_stdio;
#[path = "suite/extract_method.rs"]
mod extract_method;
#[path = "suite/file_operations.rs"]
mod file_operations;
#[path = "suite/framework_analyzer_adapter.rs"]
mod framework_analyzer_adapter;
#[path = "suite/framework_analyzer_integration.rs"]
mod framework_analyzer_integration;
#[path = "suite/framework_analyzer_registry_integration.rs"]
mod framework_analyzer_registry_integration;
#[path = "suite/ide_extensions_completion.rs"]
mod ide_extensions_completion;
#[path = "suite/ide_extensions_navigation.rs"]
mod ide_extensions_navigation;
#[path = "suite/mapstruct_completions.rs"]
mod mapstruct_completions;
#[path = "suite/mapstruct_diagnostics.rs"]
mod mapstruct_diagnostics;
#[path = "suite/mapstruct_goto_definition.rs"]
mod mapstruct_goto_definition;
#[path = "suite/mapstruct_implementation.rs"]
mod mapstruct_implementation;
#[path = "suite/metrics.rs"]
mod metrics;
#[path = "suite/micronaut_extensions.rs"]
mod micronaut_extensions;
#[path = "suite/navigation.rs"]
mod navigation;
#[path = "suite/refactor_variable.rs"]
mod refactor_variable;
#[path = "suite/refactor_workspace_snapshot.rs"]
mod refactor_workspace_snapshot;
#[path = "suite/references.rs"]
mod references;
#[cfg(debug_assertions)]
#[path = "suite/salsa_cancellation.rs"]
mod salsa_cancellation;
#[path = "suite/stdio_call_hierarchy.rs"]
mod stdio_call_hierarchy;
#[path = "suite/stdio_did_save.rs"]
mod stdio_did_save;
#[path = "suite/stdio_distributed_workspace_symbol.rs"]
mod stdio_distributed_workspace_symbol;
#[path = "suite/stdio_document_symbol.rs"]
mod stdio_document_symbol;
#[path = "suite/stdio_extract_method.rs"]
mod stdio_extract_method;
#[path = "suite/stdio_hierarchy.rs"]
mod stdio_hierarchy;
#[path = "suite/stdio_hover_signature_help.rs"]
mod stdio_hover_signature_help;
#[path = "suite/stdio_hover_signature_references.rs"]
mod stdio_hover_signature_references;
#[path = "suite/stdio_jdk_definition.rs"]
mod stdio_jdk_definition;
#[path = "suite/stdio_jdk_import_definition.rs"]
mod stdio_jdk_import_definition;
#[path = "suite/stdio_jdk_import_nested_definition.rs"]
mod stdio_jdk_import_nested_definition;
#[path = "suite/stdio_jdk_member_definition.rs"]
mod stdio_jdk_member_definition;
#[path = "suite/stdio_jdk_qualified_definition.rs"]
mod stdio_jdk_qualified_definition;
#[path = "suite/stdio_jdk_type_definition.rs"]
mod stdio_jdk_type_definition;
#[path = "suite/stdio_lifecycle.rs"]
mod stdio_lifecycle;
#[path = "suite/stdio_misc_language_features.rs"]
mod stdio_misc_language_features;
#[path = "suite/stdio_navigation.rs"]
mod stdio_navigation;
#[path = "suite/stdio_organize_imports.rs"]
mod stdio_organize_imports;
#[path = "suite/stdio_project_extensions.rs"]
mod stdio_project_extensions;
#[path = "suite/stdio_publish_diagnostics.rs"]
mod stdio_publish_diagnostics;
#[path = "suite/stdio_refactor_code_actions.rs"]
mod stdio_refactor_code_actions;
#[path = "suite/stdio_references.rs"]
mod stdio_references;
#[path = "suite/stdio_safe_delete.rs"]
mod stdio_safe_delete;
#[path = "suite/stdio_safe_mode_enforcement.rs"]
mod stdio_safe_mode_enforcement;
#[path = "suite/stdio_semantic_tokens.rs"]
mod stdio_semantic_tokens;
#[path = "suite/stdio_server.rs"]
mod stdio_server;
#[path = "suite/stdio_type_hierarchy.rs"]
mod stdio_type_hierarchy;
#[path = "suite/stdio_unresolved_type_code_actions.rs"]
mod stdio_unresolved_type_code_actions;
#[path = "suite/stdio_utf16_refactors.rs"]
mod stdio_utf16_refactors;
#[path = "suite/stdio_will_save.rs"]
mod stdio_will_save;
#[path = "suite/stdio_workspace_symbol.rs"]
mod stdio_workspace_symbol;
#[path = "suite/test_extensions.rs"]
mod test_extensions;
#[path = "suite/test_formatting.rs"]
mod test_formatting;
#[path = "suite/utf16_text_pos.rs"]
mod utf16_text_pos;
#[path = "suite/watched_files.rs"]
mod watched_files;
#[path = "suite/workspace_config.rs"]
mod workspace_config;
#[path = "suite/workspace_notifications.rs"]
mod workspace_notifications;

// Some CI/scripts run tests by filtering on `suite::<module>` (e.g.
// `cargo test --test tests suite::stdio_codelens`). Keep those modules nested to
// make the filter stable without changing the on-disk layout.
mod suite {
    #[path = "semantic_search_index_status_stdio.rs"]
    mod semantic_search_index_status_stdio;
    #[path = "semantic_search_search_stdio.rs"]
    mod semantic_search_search_stdio;
    #[cfg(feature = "ai")]
    #[path = "semantic_search_reindex_stdio.rs"]
    mod semantic_search_reindex_stdio;
    #[cfg(feature = "ai")]
    #[path = "semantic_search_workspace_indexing.rs"]
    mod semantic_search_workspace_indexing;
    #[cfg(feature = "ai")]
    #[path = "stdio_ai_completion_more.rs"]
    mod stdio_ai_completion_more;
    #[cfg(feature = "ai")]
    #[path = "stdio_ai_env_overrides.rs"]
    mod stdio_ai_env_overrides;
    #[path = "stdio_codelens.rs"]
    mod stdio_codelens;
}

#[test]
fn tests_dir_contains_only_tests_rs_at_root() {
    let tests_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");

    let mut root_rs_files: Vec<String> = std::fs::read_dir(&tests_dir)
        .unwrap_or_else(|err| panic!("failed to read tests dir `{}`: {err}", tests_dir.display()))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .filter_map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    root_rs_files.sort();

    assert_eq!(
        root_rs_files,
        vec!["tests.rs"],
        "expected only `tests.rs` at the root of `crates/nova-lsp/tests/`.\n\
         Put other integration tests under `crates/nova-lsp/tests/suite/` and include them from \
         `crates/nova-lsp/tests/tests.rs` using `#[path = \"suite/<file>.rs\"] mod <name>;`.\n\
         Found: {root_rs_files:?}",
    );
}

#[test]
fn stdio_config_spawns_scrub_legacy_ai_env() {
    let suite_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("suite");
    let required = [
        "NOVA_AI_PROVIDER",
        "NOVA_AI_ENDPOINT",
        "NOVA_AI_MODEL",
        "NOVA_AI_API_KEY",
        "NOVA_AI_AUDIT_LOGGING",
    ];

    for entry in std::fs::read_dir(&suite_dir)
        .unwrap_or_else(|err| panic!("failed to read suite dir `{}`: {err}", suite_dir.display()))
    {
        let entry = entry.expect("suite dir entry");
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "rs") {
            continue;
        }

        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read `{}`: {err}", path.display()));
        let lines = text.lines().collect::<Vec<_>>();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if !line.contains(".arg(\"--config\")") {
                continue;
            }

            let mut seen_spawn = false;
            let mut scrubbed = vec![false; required.len()];
            for lookahead in 0..80usize {
                let Some(scan) = lines.get(i + lookahead) else {
                    break;
                };
                for (idx, key) in required.iter().enumerate() {
                    if scan.contains(&format!(".env_remove(\"{key}\")")) {
                        scrubbed[idx] = true;
                    }
                }
                if scan.contains(".spawn()") || scan.contains(".output()") {
                    seen_spawn = true;
                    break;
                }
            }

            // Avoid false positives if a comment or assertion happens to contain `.arg("--config")`.
            if !seen_spawn {
                continue;
            }

            let missing = required
                .iter()
                .zip(scrubbed.iter())
                .filter_map(|(key, present)| (!present).then_some(*key))
                .collect::<Vec<_>>();
            assert!(
                missing.is_empty(),
                "found `nova-lsp --config` spawn without legacy AI env scrubbing.\n\
                 file: {}\n\
                 line: {}\n\
                 missing env_remove: {missing:?}\n\
                 \n\
                 NOTE: nova-lsp loads legacy AI env config (NOVA_AI_*) early and it can override the\n\
                 config file; tests that pass --config must remove these env vars to remain\n\
                 deterministic in developer shells.\n\
                 ",
                path.display(),
                i + 1,
            );
        }
    }
}

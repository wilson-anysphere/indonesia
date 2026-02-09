use nova_config::NovaConfig;

#[test]
fn ai_embeddings_max_memory_bytes_accepts_human_friendly_sizes() {
    let text = r#"
[ai]
enabled = true

[ai.embeddings]
enabled = true
max_memory_bytes = "1MiB"
"#;

    let (config, diagnostics) =
        NovaConfig::load_from_str_with_diagnostics(text).expect("config should parse");

    assert!(
        diagnostics.errors.is_empty(),
        "expected no diagnostics errors, got: {:?}",
        diagnostics.errors
    );
    assert_eq!(config.ai.embeddings.max_memory_bytes.0, 1024 * 1024);
}


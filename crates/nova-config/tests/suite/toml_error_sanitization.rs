use nova_config::{ConfigError, NovaConfig};

#[test]
fn config_toml_errors_do_not_echo_string_values_with_escaped_quotes() {
    let secret_suffix = "nova-config-super-secret-token";
    let text = format!(
        r#"
[logging]
json = "prefix\"{secret_suffix}"
"#
    );

    let raw_err = toml::from_str::<NovaConfig>(&text).expect_err("expected type mismatch");
    let raw_message = raw_err.message();
    assert!(
        raw_message.contains(secret_suffix),
        "expected raw toml error message to include the string value so this test would catch leaks: {raw_message}"
    );

    let err = NovaConfig::load_from_str_with_diagnostics(&text).expect_err("expected parse error");
    assert!(
        matches!(err, ConfigError::Toml(_)),
        "expected ConfigError::Toml, got {err:?}"
    );

    let message = err.to_string();
    assert!(
        !message.contains(secret_suffix),
        "expected ConfigError toml message to omit string values: {message}"
    );
    assert!(
        message.contains("<redacted>"),
        "expected ConfigError toml message to include redaction marker: {message}"
    );
}

#[test]
fn config_toml_errors_do_not_echo_backticked_values_with_embedded_expected_delimiter() {
    let secret_suffix = "nova-config-backticked-secret-token";
    let secret = format!("prefix`, expected {secret_suffix}");
    let text = format!(
        r#"
[ai]
enabled = true

[ai.provider]
kind = "{secret}"
"#
    );

    let raw_err = toml::from_str::<NovaConfig>(&text).expect_err("expected unknown variant error");
    let raw_message = raw_err.message();
    assert!(
        raw_message.contains(secret_suffix),
        "expected raw toml error message to include the string value so this test would catch leaks: {raw_message}"
    );

    let err = NovaConfig::load_from_str_with_diagnostics(&text).expect_err("expected parse error");
    assert!(
        matches!(err, ConfigError::Toml(_)),
        "expected ConfigError::Toml, got {err:?}"
    );

    let message = err.to_string();
    assert!(
        !message.contains(secret_suffix),
        "expected ConfigError toml message to omit backticked values: {message}"
    );
    assert!(
        message.contains("<redacted>"),
        "expected ConfigError toml message to include redaction marker: {message}"
    );
}

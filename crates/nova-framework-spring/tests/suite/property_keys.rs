use nova_framework_spring::property_keys_from_configs;

#[test]
fn property_keys_from_configs_handles_windows_style_paths() {
    let path = r"C:\workspace\src\main\resources\application.properties";
    let text = "server.port=8080\ncustom.key=value\n";

    let keys = property_keys_from_configs(&[(path, text)]);

    assert!(
        keys.contains("custom.key"),
        "expected keys to be discovered from config file; got {keys:?}"
    );
    assert!(keys.contains("server.port"));

    // If the config file is skipped, the extractor falls back to a stub list. Make sure we don't
    // return those stub keys when the file is successfully parsed.
    assert!(!keys.contains("spring.application.name"));
    assert!(!keys.contains("logging.level.root"));
    assert_eq!(keys.len(), 2);
}

#[test]
fn property_keys_from_configs_handles_posix_paths() {
    let path = "src/main/resources/application.properties";
    let text = "server.port=8080\ncustom.key=value\n";

    let keys = property_keys_from_configs(&[(path, text)]);

    assert!(keys.contains("custom.key"));
    assert!(keys.contains("server.port"));
    assert!(!keys.contains("spring.application.name"));
    assert!(!keys.contains("logging.level.root"));
    assert_eq!(keys.len(), 2);
}

#[test]
fn property_keys_from_configs_is_case_insensitive() {
    let path = "src/main/resources/Application.PROPERTIES";
    let text = "server.port=8080\ncustom.key=value\n";

    let keys = property_keys_from_configs(&[(path, text)]);

    assert!(keys.contains("custom.key"));
    assert!(keys.contains("server.port"));
    assert!(!keys.contains("spring.application.name"));
    assert!(!keys.contains("logging.level.root"));
    assert_eq!(keys.len(), 2);
}

#[test]
fn property_keys_from_configs_extracts_yaml_keys() {
    let path = "src/main/resources/application.yml";
    let text = r#"
server:
  port: 8080
spring:
  main:
    banner-mode: off
"#;

    let keys = property_keys_from_configs(&[(path, text)]);

    assert!(
        keys.contains("server.port"),
        "expected server.port; got {keys:?}"
    );
    assert!(
        keys.contains("spring.main.banner-mode"),
        "expected spring.main.banner-mode; got {keys:?}"
    );
    assert!(!keys.contains("spring.application.name"));
    assert!(!keys.contains("logging.level.root"));
}

#[test]
fn property_keys_from_configs_extracts_yaml_keys_with_uppercase_extension() {
    let path = "src/main/resources/application.YML";
    let text = "server:\n  port: 8080\n";

    let keys = property_keys_from_configs(&[(path, text)]);

    assert!(
        keys.contains("server.port"),
        "expected server.port; got {keys:?}"
    );
    assert!(!keys.contains("spring.application.name"));
}

use nova_lsp::extensions::micronaut::SCHEMA_VERSION;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::TempDir;

fn project_root_params(project_root: &Path) -> serde_json::Value {
    serde_json::Value::Object({
        let mut params = serde_json::Map::new();
        params.insert(
            "projectRoot".to_string(),
            serde_json::Value::String(project_root.to_string_lossy().to_string()),
        );
        params
    })
}

#[test]
fn lsp_micronaut_endpoints_extension_discovers_controller_methods() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    let src_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_dir).unwrap();

    std::fs::write(
        src_dir.join("HelloController.java"),
        r#"
            package com.example;

            import io.micronaut.http.annotation.Controller;
            import io.micronaut.http.annotation.Get;
            import io.micronaut.http.annotation.Post;

            @Controller("/hello")
            class HelloController {
                @Get("/world")
                String world() { return "ok"; }

                @Post("/")
                void create() {}
            }
        "#,
    )
    .unwrap();

    let params = project_root_params(root);

    let value =
        nova_lsp::handle_custom_request(nova_lsp::MICRONAUT_ENDPOINTS_METHOD, params).unwrap();

    assert_eq!(
        value
            .get("schemaVersion")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        Some(SCHEMA_VERSION)
    );

    let endpoints = value
        .get("endpoints")
        .and_then(|v| v.as_array())
        .expect("endpoints array");
    assert_eq!(endpoints.len(), 2);
    assert!(endpoints.iter().any(|e| {
        e.get("method").and_then(|v| v.as_str()) == Some("GET")
            && e.get("path").and_then(|v| v.as_str()) == Some("/hello/world")
    }));
    assert!(endpoints.iter().any(|e| {
        e.get("method").and_then(|v| v.as_str()) == Some("POST")
            && e.get("path").and_then(|v| v.as_str()) == Some("/hello")
    }));
}

#[test]
fn lsp_micronaut_beans_extension_lists_singletons() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    let src_dir = root.join("src/main/java/com/example");
    std::fs::create_dir_all(&src_dir).unwrap();

    std::fs::write(
        src_dir.join("Bar.java"),
        r#"
            package com.example;

            import io.micronaut.context.annotation.Singleton;

            @Singleton
            class Bar {}
        "#,
    )
    .unwrap();

    std::fs::write(
        src_dir.join("Foo.java"),
        r#"
            package com.example;

            import io.micronaut.context.annotation.Singleton;
            import jakarta.inject.Inject;

            @Singleton
            class Foo {
                @Inject Bar bar;
            }
        "#,
    )
    .unwrap();

    let params = project_root_params(root);

    let value = nova_lsp::handle_custom_request(nova_lsp::MICRONAUT_BEANS_METHOD, params).unwrap();

    assert_eq!(
        value
            .get("schemaVersion")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        Some(SCHEMA_VERSION)
    );

    let beans = value
        .get("beans")
        .and_then(|v| v.as_array())
        .expect("beans array");
    assert!(beans
        .iter()
        .any(|b| b.get("ty").and_then(|v| v.as_str()) == Some("Foo")));
    assert!(beans
        .iter()
        .any(|b| b.get("ty").and_then(|v| v.as_str()) == Some("Bar")));
}

use nova_lsp::extensions::micronaut::{
    MicronautBeansResponse, MicronautEndpointsResponse, SCHEMA_VERSION,
};
use pretty_assertions::assert_eq;
use tempfile::TempDir;

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

    let params = serde_json::json!({
        "projectRoot": root.to_string_lossy(),
    });

    let value =
        nova_lsp::handle_custom_request(nova_lsp::MICRONAUT_ENDPOINTS_METHOD, params).unwrap();
    let resp: MicronautEndpointsResponse = serde_json::from_value(value).unwrap();

    assert_eq!(resp.schema_version, SCHEMA_VERSION);
    assert_eq!(resp.endpoints.len(), 2);
    assert!(resp
        .endpoints
        .iter()
        .any(|e| e.method == "GET" && e.path == "/hello/world"));
    assert!(resp
        .endpoints
        .iter()
        .any(|e| e.method == "POST" && e.path == "/hello"));
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

    let params = serde_json::json!({
        "projectRoot": root.to_string_lossy(),
    });

    let value = nova_lsp::handle_custom_request(nova_lsp::MICRONAUT_BEANS_METHOD, params).unwrap();
    let resp: MicronautBeansResponse = serde_json::from_value(value).unwrap();

    assert_eq!(resp.schema_version, SCHEMA_VERSION);
    assert!(resp.beans.iter().any(|b| b.ty == "Foo"));
    assert!(resp.beans.iter().any(|b| b.ty == "Bar"));
}

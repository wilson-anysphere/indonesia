use std::path::PathBuf;

use nova_ai::{ContextRequest, PrivacyMode, SemanticContextBuilder, VirtualWorkspace};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiEmbeddingsConfig, AiFeaturesConfig};
use nova_db::{InMemoryFileStore, SalsaDatabase};

fn test_db() -> VirtualWorkspace {
    VirtualWorkspace::new([
        (
            "src/Hello.java".to_string(),
            "public class Hello { public String helloWorld() { return \"hello world\"; } }"
                .to_string(),
        ),
        (
            "src/Other.java".to_string(),
            "public class Other { public String goodbye() { return \"goodbye\"; } }".to_string(),
        ),
    ])
}

fn request() -> ContextRequest {
    ContextRequest {
        file_path: None,
        focal_code: "return \"hello world\";".to_string(),
        enclosing_context: None,
        project_context: None,
        semantic_context: None,
        related_symbols: Vec::new(),
        related_code: Vec::new(),
        cursor: None,
        diagnostics: Vec::new(),
        extra_files: Vec::new(),
        doc_comments: None,
        include_doc_comments: false,
        token_budget: 200,
        privacy: PrivacyMode {
            anonymize_identifiers: false,
            include_file_paths: true,
            ..PrivacyMode::default()
        },
    }
}

#[test]
fn semantic_context_builder_adds_related_code_when_enabled() {
    let db = test_db();

    let cfg = AiConfig {
        enabled: true,
        embeddings: AiEmbeddingsConfig {
            enabled: true,
            backend: AiEmbeddingsBackend::Hash,
            ..AiEmbeddingsConfig::default()
        },
        features: AiFeaturesConfig {
            semantic_search: true,
            ..AiFeaturesConfig::default()
        },
        ..AiConfig::default()
    };

    let mut builder = SemanticContextBuilder::new(&cfg).expect("semantic context builder");
    builder.index_project(&db);

    let ctx = builder.build(request(), 1);
    assert!(ctx.text.contains("## Related code"));
    assert!(ctx.text.contains("helloWorld"));
}

#[test]
fn semantic_context_builder_skips_related_code_when_disabled() {
    let db = test_db();

    let cfg = AiConfig::default();
    let mut builder = SemanticContextBuilder::new(&cfg).expect("semantic context builder");
    builder.index_project(&db);

    let ctx = builder.build(request(), 1);
    assert!(!ctx.text.contains("## Related code"));
    assert!(!ctx.text.contains("helloWorld"));
}

#[test]
fn semantic_context_builder_can_index_incrementally() {
    let cfg = AiConfig {
        enabled: true,
        embeddings: AiEmbeddingsConfig {
            enabled: true,
            backend: AiEmbeddingsBackend::Hash,
            ..AiEmbeddingsConfig::default()
        },
        features: AiFeaturesConfig {
            semantic_search: true,
            ..AiFeaturesConfig::default()
        },
        ..AiConfig::default()
    };

    let mut builder = SemanticContextBuilder::new(&cfg).expect("semantic context builder");
    builder.index_file(
        PathBuf::from("src/Hello.java"),
        "public class Hello { public String helloWorld() { return \"hello world\"; } }".to_string(),
    );
    builder.index_file(
        PathBuf::from("src/Other.java"),
        "public class Other { public String goodbye() { return \"goodbye\"; } }".to_string(),
    );

    let ctx = builder.build(request(), 1);
    assert!(ctx.text.contains("## Related code"));
    assert!(ctx.text.contains("helloWorld"));

    builder.remove_file(PathBuf::from("src/Hello.java").as_path());
    let ctx = builder.build(request(), 1);
    assert!(!ctx.text.contains("helloWorld"));
}

#[test]
fn semantic_context_builder_can_index_database() {
    let cfg = AiConfig {
        enabled: true,
        embeddings: AiEmbeddingsConfig {
            enabled: false,
            ..AiEmbeddingsConfig::default()
        },
        features: AiFeaturesConfig {
            semantic_search: true,
            ..AiFeaturesConfig::default()
        },
        ..AiConfig::default()
    };

    let mut store = InMemoryFileStore::new();
    let file_id = store.file_id_for_path("src/Main.java");
    store.set_file_text(
        file_id,
        "public class Main { public String hello() { return \"hello world\"; } }".to_string(),
    );

    let mut builder = SemanticContextBuilder::new(&cfg).expect("semantic context builder");
    builder.index_database(&store);

    let ctx = builder.build(request(), 1);
    assert!(ctx.text.contains("## Related code"));
    assert!(ctx.text.contains("hello world"));
}

#[test]
fn semantic_context_builder_can_index_source_database() {
    let cfg = AiConfig {
        enabled: true,
        embeddings: AiEmbeddingsConfig {
            enabled: false,
            ..AiEmbeddingsConfig::default()
        },
        features: AiFeaturesConfig {
            semantic_search: true,
            ..AiFeaturesConfig::default()
        },
        ..AiConfig::default()
    };

    let db = SalsaDatabase::new();
    let file_id = nova_db::FileId::from_raw(0);
    db.set_file_text(
        file_id,
        "public class Main { public String hello() { return \"hello world\"; } }".to_string(),
    );
    db.set_file_path(file_id, "src/Main.java");
    let snap = db.snapshot();

    let mut builder = SemanticContextBuilder::new(&cfg).expect("semantic context builder");
    builder.index_source_database(&snap);

    let ctx = builder.build(request(), 1);
    assert!(ctx.text.contains("## Related code"));
    assert!(ctx.text.contains("hello world"));
}

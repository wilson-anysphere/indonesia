use std::path::{Path, PathBuf};

use nova_ai::{ContextRequest, PrivacyMode, SemanticContextBuilder};
use nova_config::{AiConfig, AiEmbeddingsConfig, AiFeaturesConfig};
use nova_core::ProjectDatabase;

#[derive(Debug)]
struct MemDb(Vec<(PathBuf, String)>);

impl ProjectDatabase for MemDb {
    fn project_files(&self) -> Vec<PathBuf> {
        self.0.iter().map(|(p, _)| p.clone()).collect()
    }

    fn file_text(&self, path: &Path) -> Option<String> {
        self.0
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, text)| text.clone())
    }
}

fn test_db() -> MemDb {
    MemDb(vec![
        (
            PathBuf::from("src/Hello.java"),
            "public class Hello { public String helloWorld() { return \"hello world\"; } }"
                .to_string(),
        ),
        (
            PathBuf::from("src/Other.java"),
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
            ..AiEmbeddingsConfig::default()
        },
        features: AiFeaturesConfig {
            semantic_search: true,
            ..AiFeaturesConfig::default()
        },
        ..AiConfig::default()
    };

    let mut builder = SemanticContextBuilder::new(&cfg);
    builder.index_project(&db);

    let ctx = builder.build(request(), 1);
    assert!(ctx.text.contains("## Related code"));
    assert!(ctx.text.contains("helloWorld"));
}

#[test]
fn semantic_context_builder_skips_related_code_when_disabled() {
    let db = test_db();

    let cfg = AiConfig::default();
    let mut builder = SemanticContextBuilder::new(&cfg);
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
            ..AiEmbeddingsConfig::default()
        },
        features: AiFeaturesConfig {
            semantic_search: true,
            ..AiFeaturesConfig::default()
        },
        ..AiConfig::default()
    };

    let mut builder = SemanticContextBuilder::new(&cfg);
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

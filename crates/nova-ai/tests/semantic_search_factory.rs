use std::path::{Path, PathBuf};

use nova_ai::semantic_search_from_config;
use nova_config::{AiConfig, AiEmbeddingsConfig};
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

#[test]
fn semantic_search_from_config_respects_feature_flag() {
    let db = MemDb(vec![(
        PathBuf::from("src/Hello.java"),
        r#"
            public class Hello {
                public String helloWorld() {
                    return "hello world";
                }
            }
        "#
        .to_string(),
    )]);

    let cfg = AiConfig {
        embeddings: AiEmbeddingsConfig {
            enabled: true,
            ..AiEmbeddingsConfig::default()
        },
        features: nova_config::AiFeaturesConfig {
            semantic_search: true,
            ..nova_config::AiFeaturesConfig::default()
        },
        ..AiConfig::default()
    };

    let mut search = semantic_search_from_config(&cfg);
    search.index_project(&db);
    let results = search.search("hello world");
    assert!(!results.is_empty());

    let expected_kind = if cfg!(feature = "embeddings") {
        "method"
    } else {
        "file"
    };
    assert_eq!(results[0].kind, expected_kind);
}

#[test]
fn semantic_search_from_config_disabled_returns_empty() {
    let cfg = AiConfig::default();
    let mut search = semantic_search_from_config(&cfg);
    search.index_project(&MemDb(Vec::new()));
    assert!(search.search("hello world").is_empty());
}

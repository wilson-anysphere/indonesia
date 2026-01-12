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
        enabled: true,
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

#[test]
fn semantic_search_from_config_supports_incremental_updates() {
    let cfg = AiConfig {
        enabled: true,
        features: nova_config::AiFeaturesConfig {
            semantic_search: true,
            ..nova_config::AiFeaturesConfig::default()
        },
        embeddings: AiEmbeddingsConfig {
            enabled: false,
            ..AiEmbeddingsConfig::default()
        },
        ..AiConfig::default()
    };

    let mut search = semantic_search_from_config(&cfg);
    let path = PathBuf::from("src/a.txt");

    search.index_file(path.clone(), "hello old".to_string());
    let first = search.search("hello old");
    assert!(!first.is_empty());
    assert_eq!(first[0].path, path);
    assert!(first[0].snippet.to_lowercase().contains("hello old"));

    search.index_file(path.clone(), "hello new".to_string());
    let second = search.search("hello new");
    assert!(!second.is_empty());
    assert_eq!(second[0].path, path);
    assert!(second[0].snippet.to_lowercase().contains("hello new"));
    assert!(!second[0].snippet.to_lowercase().contains("hello old"));

    search.remove_file(path.as_path());
    assert!(search.search("hello new").is_empty());

    search.index_file(path.clone(), "hello final".to_string());
    search.clear();
    assert!(search.search("hello final").is_empty());
}

#[cfg(feature = "embeddings")]
#[test]
fn semantic_search_from_config_embeddings_supports_incremental_updates() {
    let cfg = AiConfig {
        enabled: true,
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
    let path = PathBuf::from("src/Hello.java");

    search.index_file(
        path.clone(),
        "public class Hello { public String helloWorld() { return \"hello world\"; } }".to_string(),
    );
    let first = search.search("hello world");
    assert!(!first.is_empty());
    assert_eq!(first[0].path, path);

    search.index_file(
        path.clone(),
        "public class Hello { public String greetings() { return \"hello world\"; } }".to_string(),
    );
    let second = search.search("hello world");
    assert!(!second.is_empty());
    assert_eq!(second[0].path, path);
    assert!(second[0].snippet.contains("greetings"));

    search.remove_file(path.as_path());
    assert!(search.search("hello world").is_empty());
}

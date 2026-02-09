use std::path::PathBuf;

use nova_ai::{semantic_search_from_config, VirtualWorkspace};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiEmbeddingsConfig};

#[test]
fn semantic_search_from_config_respects_feature_flag() {
    let db = VirtualWorkspace::new([(
        "src/Hello.java".to_string(),
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
            backend: AiEmbeddingsBackend::Hash,
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

#[cfg(all(feature = "embeddings", not(feature = "embeddings-local")))]
#[test]
fn semantic_search_from_config_local_backend_without_feature_falls_back_to_hash_embedder() {
    let db = VirtualWorkspace::new([(
        "src/Hello.java".to_string(),
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
            backend: nova_config::AiEmbeddingsBackend::Local,
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
    assert_eq!(
        results[0].kind, "method",
        "expected embedding-backed semantic search to fall back to HashEmbedder when `embeddings-local` is disabled"
    );
}

#[test]
fn semantic_search_from_config_disabled_returns_empty() {
    let cfg = AiConfig::default();
    let mut search = semantic_search_from_config(&cfg);
    search.index_project(&VirtualWorkspace::new([]));
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
            backend: AiEmbeddingsBackend::Hash,
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

#[test]
fn semantic_search_from_config_provider_backend_falls_back_when_provider_kind_unsupported() {
    let db = VirtualWorkspace::new([(
        "src/Hello.java".to_string(),
        r#"
            public class Hello {
                public String helloWorld() {
                    return "hello world";
                }
            }
        "#
        .to_string(),
    )]);

    let mut cfg = AiConfig {
        enabled: true,
        embeddings: AiEmbeddingsConfig {
            enabled: true,
            backend: AiEmbeddingsBackend::Provider,
            ..AiEmbeddingsConfig::default()
        },
        features: nova_config::AiFeaturesConfig {
            semantic_search: true,
            ..nova_config::AiFeaturesConfig::default()
        },
        ..AiConfig::default()
    };
    cfg.provider.kind = nova_config::AiProviderKind::Anthropic;

    let mut search = semantic_search_from_config(&cfg);
    search.index_project(&db);
    let results = search.search("hello world");
    assert!(!results.is_empty());
    assert_eq!(results[0].path, PathBuf::from("src/Hello.java"));

    let expected_kind = if cfg!(feature = "embeddings") {
        "method"
    } else {
        "file"
    };
    assert_eq!(results[0].kind, expected_kind);
}

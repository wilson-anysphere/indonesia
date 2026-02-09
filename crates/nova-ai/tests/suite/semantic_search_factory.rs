use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[cfg(feature = "embeddings")]
use httpmock::prelude::*;
use nova_ai::{semantic_search_from_config, VirtualWorkspace};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiEmbeddingsConfig, AiProviderKind};
#[cfg(feature = "embeddings")]
use serde_json::json;
#[cfg(feature = "embeddings")]
use url::Url;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct SharedLogBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedLogBuffer {
    fn as_string(&self) -> String {
        let bytes = self.0.lock().expect("log buffer mutex poisoned");
        String::from_utf8_lossy(&bytes).to_string()
    }
}

struct SharedLogWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for SharedLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut out = self.0.lock().expect("log buffer mutex poisoned");
        out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SharedLogBuffer {
    type Writer = SharedLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedLogWriter(self.0.clone())
    }
}

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
fn semantic_search_from_config_provider_backend_supports_ollama_embeddings() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/api/embeddings");
        then.status(200).json_body(json!({ "embedding": [1.0, 0.0, 0.0] }));
    });

    let db = VirtualWorkspace::new([(
        "src/example.txt".to_string(),
        "hello world".to_string(),
    )]);

    let cfg = AiConfig {
        enabled: true,
        provider: nova_config::AiProviderConfig {
            kind: AiProviderKind::Ollama,
            url: Url::parse(&server.base_url()).unwrap(),
            ..nova_config::AiProviderConfig::default()
        },
        embeddings: AiEmbeddingsConfig {
            enabled: true,
            backend: AiEmbeddingsBackend::Provider,
            model: Some("nomic-embed-text".to_string()),
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
    assert_eq!(results[0].path, PathBuf::from("src/example.txt"));

    // One request for indexing, one request for query embedding.
    mock.assert_hits(2);
}

#[cfg(feature = "embeddings")]
#[test]
fn semantic_search_from_config_provider_backend_supports_openai_compatible_embeddings() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200)
            .json_body(json!({ "data": [{ "embedding": [1.0, 0.0, 0.0] }] }));
    });

    let db = VirtualWorkspace::new([(
        "src/example.txt".to_string(),
        "hello world".to_string(),
    )]);

    let cfg = AiConfig {
        enabled: true,
        provider: nova_config::AiProviderConfig {
            kind: AiProviderKind::OpenAiCompatible,
            url: Url::parse(&format!("{}/v1", server.base_url())).unwrap(),
            ..nova_config::AiProviderConfig::default()
        },
        embeddings: AiEmbeddingsConfig {
            enabled: true,
            backend: AiEmbeddingsBackend::Provider,
            model: Some("text-embedding-3-small".to_string()),
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
    assert_eq!(results[0].path, PathBuf::from("src/example.txt"));

    mock.assert_hits(2);
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
fn semantic_search_from_config_provider_backend_with_unsupported_provider_falls_back() {
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
        provider: nova_config::AiProviderConfig {
            kind: AiProviderKind::Anthropic,
            ..nova_config::AiProviderConfig::default()
        },
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

    let logs = SharedLogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::WARN)
        .with_writer(logs.clone())
        .finish();

    let results = tracing::subscriber::with_default(subscriber, || {
        let mut search = semantic_search_from_config(&cfg);
        search.index_project(&db);
        search.search("hello world")
    });

    assert!(!results.is_empty());
    assert_eq!(results[0].path, PathBuf::from("src/Hello.java"));
    let expected_kind = if cfg!(feature = "embeddings") {
        "method"
    } else {
        "file"
    };
    assert_eq!(results[0].kind, expected_kind);

    let text = logs.as_string();
    if cfg!(feature = "embeddings") {
        assert!(
            text.contains("falling back to hash embeddings"),
            "expected provider embedder fallback warning, got:\n{text}"
        );
    } else {
        assert!(
            text.contains("falling back to trigram search"),
            "expected trigram fallback warning, got:\n{text}"
        );
    }
}

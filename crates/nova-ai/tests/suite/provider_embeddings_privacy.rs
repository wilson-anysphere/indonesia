#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::embeddings::{embeddings_client_from_config, EmbeddingInputKind};
use nova_ai::{semantic_search_from_config, VirtualWorkspace};
use nova_config::{AiConfig, AiEmbeddingsBackend, AiProviderKind};
use serde_json::json;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use url::Url;

#[test]
fn provider_embeddings_remote_url_falls_back_to_hash_in_local_only_mode() {
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

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.features.semantic_search = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse("https://api.openai.com/v1").unwrap();
    cfg.provider.model = "text-embedding-3-small".to_string();

    // Must not panic or attempt a remote request in local-only mode. The embedder should fall back
    // to the local hash embedder and still return embedding-backed results.
    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_project(&db);
    let results = search.search("hello world");
    assert!(!results.is_empty());
    assert_eq!(results[0].kind, "method");
}

#[test]
fn provider_embeddings_loopback_url_is_allowed_in_local_only_mode() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains("\"input\"")
            .body_contains("\"model\"");
        then.status(200).json_body(json!({
            "data": [{
                "embedding": [1.0, 0.0, 0.0],
                "index": 0,
                "object": "embedding"
            }]
        }));
    });

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

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.features.semantic_search = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url =
        Url::parse(&format!("http://localhost:{}/v1", server.port())).unwrap();
    cfg.provider.model = "test-embed-model".to_string();

    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_project(&db);
    let results = search.search("hello world");
    assert!(!results.is_empty());
    assert_eq!(results[0].kind, "method");

    let hits = mock.hits();
    assert!(
        hits >= 2,
        "expected at least 2 embedding requests (index + query), got {hits}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn provider_embeddings_client_remote_url_falls_back_to_hash_in_local_only_mode() {
    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse("https://api.openai.com/v1").unwrap();
    cfg.provider.model = "text-embedding-3-small".to_string();
    cfg.provider.timeout_ms = 10;

    let client = embeddings_client_from_config(&cfg).expect("embeddings client");
    let out = client
        .embed(
            &["hello".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].len(), 256, "expected hash embedder fallback");
}

#[tokio::test(flavor = "current_thread")]
async fn provider_embeddings_client_loopback_url_is_allowed_in_local_only_mode() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(json!({
            "data": [{
                "embedding": [1.0, 2.0, 3.0],
                "index": 0
            }]
        }));
    });

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse(&format!("http://localhost:{}/v1", server.port())).unwrap();
    cfg.provider.model = "test-embed-model".to_string();
    cfg.provider.timeout_ms = 1_000;

    let client = embeddings_client_from_config(&cfg).expect("embeddings client");
    let out = client
        .embed(
            &["hello".to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    mock.assert_hits(1);
    assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
}

#[test]
fn provider_embeddings_redact_absolute_paths_in_cloud_mode_openai_compatible() {
    let server = MockServer::start();

    let dir = tempdir().expect("tempdir");
    let dir = dir.path().canonicalize().expect("canonicalize tempdir");
    let abs_path = dir.join("src").join("example.txt");
    let abs_path_str = abs_path.to_string_lossy().to_string();
    let abs_path_in_body = json_string_contents(&abs_path_str);

    let leaky = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains(&abs_path_in_body);
        then.status(500)
            .json_body(json!({"error": "absolute path leaked to provider"}));
    });

    let redacted = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings").body_contains("[PATH]");
        then.status(200).json_body(json!({
            "data": [{"embedding": [1.0, 0.0, 0.0], "index": 0}],
        }));
    });

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.features.semantic_search = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse(&server.base_url()).unwrap();
    cfg.provider.model = "test-embed-model".to_string();

    // Cloud privacy mode: redact absolute paths before sending to a provider.
    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.privacy.redact_sensitive_strings = Some(false);
    cfg.privacy.redact_numeric_literals = Some(false);
    cfg.privacy.strip_or_redact_comments = Some(false);

    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_file(abs_path.clone(), "hello world".to_string());
    let _results = search.search(&format!("find refs in {abs_path_str}"));

    leaky.assert_hits(0);
    // One embed call for indexing + one for query.
    redacted.assert_hits(2);
}

#[test]
fn provider_embeddings_include_file_paths_opt_in_allows_absolute_paths_in_cloud_mode_openai_compatible() {
    let server = MockServer::start();

    let dir = tempdir().expect("tempdir");
    let dir = dir.path().canonicalize().expect("canonicalize tempdir");
    let abs_path = dir.join("src").join("example.txt");
    let abs_path_str = abs_path.to_string_lossy().to_string();
    let abs_path_in_body = json_string_contents(&abs_path_str);

    let redacted = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings").body_contains("[PATH]");
        then.status(500).json_body(json!({
            "error": "path should not be redacted when ai.privacy.include_file_paths=true"
        }));
    });

    let unredacted = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains(&abs_path_in_body);
        then.status(200).json_body(json!({
            "data": [{"embedding": [1.0, 0.0, 0.0], "index": 0}],
        }));
    });

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.features.semantic_search = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse(&server.base_url()).unwrap();
    cfg.provider.model = "test-embed-model".to_string();

    cfg.privacy.local_only = false;
    cfg.privacy.include_file_paths = true;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.privacy.redact_sensitive_strings = Some(false);
    cfg.privacy.redact_numeric_literals = Some(false);
    cfg.privacy.strip_or_redact_comments = Some(false);

    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_file(abs_path.clone(), "hello world".to_string());
    let _results = search.search(&format!("find refs in {abs_path_str}"));

    redacted.assert_hits(0);
    // One embed call for indexing + one for query.
    unredacted.assert_hits(2);
}

#[test]
fn provider_embeddings_redact_absolute_paths_in_cloud_mode_ollama() {
    let server = MockServer::start();

    let dir = tempdir().expect("tempdir");
    let dir = dir.path().canonicalize().expect("canonicalize tempdir");
    let abs_path = dir.join("src").join("example.txt");
    let abs_path_str = abs_path.to_string_lossy().to_string();
    let abs_path_in_body = json_string_contents(&abs_path_str);

    let leaky = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embeddings")
            .body_contains(&abs_path_in_body);
        then.status(500)
            .json_body(json!({"error": "absolute path leaked to provider"}));
    });

    let redacted = server.mock(|when, then| {
        when.method(POST)
            .path("/api/embeddings")
            .body_contains("[PATH]");
        then.status(200).json_body(json!({
            "embedding": [1.0, 0.0, 0.0],
        }));
    });

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.features.semantic_search = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::Ollama;
    cfg.provider.url = Url::parse(&server.base_url()).unwrap();
    cfg.provider.model = "nomic-embed-text".to_string();

    // Cloud privacy mode: redact absolute paths before sending to a provider.
    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.privacy.redact_sensitive_strings = Some(false);
    cfg.privacy.redact_numeric_literals = Some(false);
    cfg.privacy.strip_or_redact_comments = Some(false);

    let mut search = semantic_search_from_config(&cfg).expect("semantic search should build");
    search.index_file(abs_path.clone(), "hello world".to_string());
    let _results = search.search(&format!("find refs in {abs_path_str}"));

    leaky.assert_hits(0);
    // One embed call for indexing + one for query.
    redacted.assert_hits(2);
}

#[tokio::test(flavor = "current_thread")]
async fn provider_embeddings_client_redacts_absolute_paths_in_cloud_mode_openai_compatible() {
    let server = MockServer::start();

    let dir = tempdir().expect("tempdir");
    let dir = dir.path().canonicalize().expect("canonicalize tempdir");
    let abs_path = dir.join("src").join("example.txt");
    let abs_path_str = abs_path.to_string_lossy().to_string();
    let abs_path_in_body = json_string_contents(&abs_path_str);

    let leaky = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains(&abs_path_in_body);
        then.status(500)
            .json_body(json!({"error": "absolute path leaked to provider"}));
    });

    let redacted = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings").body_contains("[PATH]");
        then.status(200).json_body(json!({
            "data": [{
                "embedding": [1.0, 2.0, 3.0],
                "index": 0
            }]
        }));
    });

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse(&server.base_url()).unwrap();
    cfg.provider.model = "test-embed-model".to_string();

    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.privacy.redact_sensitive_strings = Some(false);
    cfg.privacy.redact_numeric_literals = Some(false);
    cfg.privacy.strip_or_redact_comments = Some(false);

    let client = embeddings_client_from_config(&cfg).expect("embeddings client");
    let out = client
        .embed(
            &[format!("find refs in {abs_path_str}").to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    leaky.assert_hits(0);
    redacted.assert_hits(1);
    assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
}

#[tokio::test(flavor = "current_thread")]
async fn provider_embeddings_client_include_file_paths_opt_in_allows_absolute_paths_in_cloud_mode_openai_compatible(
) {
    let server = MockServer::start();

    let dir = tempdir().expect("tempdir");
    let dir = dir.path().canonicalize().expect("canonicalize tempdir");
    let abs_path = dir.join("src").join("example.txt");
    let abs_path_str = abs_path.to_string_lossy().to_string();
    let abs_path_in_body = json_string_contents(&abs_path_str);

    let redacted = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings").body_contains("[PATH]");
        then.status(500)
            .json_body(json!({"error": "path was redacted unexpectedly"}));
    });

    let unredacted = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains(&abs_path_in_body);
        then.status(200).json_body(json!({
            "data": [{
                "embedding": [1.0, 2.0, 3.0],
                "index": 0
            }]
        }));
    });

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::OpenAiCompatible;
    cfg.provider.url = Url::parse(&server.base_url()).unwrap();
    cfg.provider.model = "test-embed-model".to_string();

    cfg.privacy.local_only = false;
    cfg.privacy.include_file_paths = true;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.privacy.redact_sensitive_strings = Some(false);
    cfg.privacy.redact_numeric_literals = Some(false);
    cfg.privacy.strip_or_redact_comments = Some(false);

    let client = embeddings_client_from_config(&cfg).expect("embeddings client");
    let out = client
        .embed(
            &[format!("find refs in {abs_path_str}").to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    redacted.assert_hits(0);
    unredacted.assert_hits(1);
    assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
}

#[tokio::test(flavor = "current_thread")]
async fn provider_embeddings_client_redacts_absolute_paths_in_cloud_mode_ollama() {
    let server = MockServer::start();

    let dir = tempdir().expect("tempdir");
    let dir = dir.path().canonicalize().expect("canonicalize tempdir");
    let abs_path = dir.join("src").join("example.txt");
    let abs_path_str = abs_path.to_string_lossy().to_string();
    let abs_path_in_body = json_string_contents(&abs_path_str);

    let leaky = server.mock(|when, then| {
        when.method(POST).path("/api/embed").body_contains(&abs_path_in_body);
        then.status(500)
            .json_body(json!({"error": "absolute path leaked to provider"}));
    });

    let redacted = server.mock(|when, then| {
        when.method(POST).path("/api/embed").body_contains("[PATH]");
        then.status(200).json_body(json!({
            "embeddings": [[1.0, 2.0, 3.0]]
        }));
    });

    let mut cfg = AiConfig::default();
    cfg.enabled = true;
    cfg.embeddings.enabled = true;
    cfg.embeddings.backend = AiEmbeddingsBackend::Provider;
    cfg.provider.kind = AiProviderKind::Ollama;
    cfg.provider.url = Url::parse(&server.base_url()).unwrap();
    cfg.provider.model = "nomic-embed-text".to_string();

    cfg.privacy.local_only = false;
    cfg.privacy.anonymize_identifiers = Some(false);
    cfg.privacy.redact_sensitive_strings = Some(false);
    cfg.privacy.redact_numeric_literals = Some(false);
    cfg.privacy.strip_or_redact_comments = Some(false);

    let client = embeddings_client_from_config(&cfg).expect("embeddings client");
    let out = client
        .embed(
            &[format!("find refs in {abs_path_str}").to_string()],
            EmbeddingInputKind::Query,
            CancellationToken::new(),
        )
        .await
        .expect("embed");

    leaky.assert_hits(0);
    redacted.assert_hits(1);
    assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
}

fn json_string_contents(text: &str) -> String {
    // Provider embedding requests are JSON; paths containing backslashes will be escaped.
    let json = serde_json::to_string(text).expect("json string");
    json.strip_prefix('\"')
        .and_then(|s| s.strip_suffix('\"'))
        .unwrap_or(&json)
        .to_string()
}

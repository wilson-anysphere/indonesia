use nova_ai::{Embedder, LocalEmbedder};
use nova_config::{AiEmbeddingsBackend, AiEmbeddingsConfig};
use std::path::PathBuf;

/// Optional smoke test for the in-process neural embeddings backend.
///
/// This test is gated by the `NOVA_TEST_EMBEDDINGS_MODEL_DIR` env var so CI can
/// remain offline (no model downloads). Point it at a directory containing a
/// pre-populated `fastembed` cache for the chosen model.
#[test]
fn local_embedder_can_embed_with_prepared_model_dir() {
    let model_dir = match std::env::var("NOVA_TEST_EMBEDDINGS_MODEL_DIR") {
        Ok(value) => value,
        Err(_) => return,
    };

    let model_dir = PathBuf::from(model_dir);
    assert!(
        model_dir.is_dir(),
        "NOVA_TEST_EMBEDDINGS_MODEL_DIR must be an existing directory: {}",
        model_dir.display()
    );

    let model_id = std::env::var("NOVA_TEST_EMBEDDINGS_MODEL")
        .unwrap_or_else(|_| "all-MiniLM-L6-v2".to_string());

    let cfg = AiEmbeddingsConfig {
        enabled: true,
        backend: AiEmbeddingsBackend::Local,
        local_model: model_id.clone(),
        model_dir: model_dir.clone(),
        batch_size: 2,
        ..AiEmbeddingsConfig::default()
    };

    let embedder = LocalEmbedder::from_config(&cfg)
        .unwrap_or_else(|err| panic!("failed to init local embedder for {model_id:?}: {err}"));

    let inputs = vec![
        "hello world".to_string(),
        "goodbye world".to_string(),
        "java semantic search".to_string(),
        "vector embeddings".to_string(),
        "fastembed smoke test".to_string(),
    ];

    let embeddings = embedder
        .embed_batch(&inputs)
        .expect("local embedding batch should succeed");

    assert_eq!(
        embeddings.len(),
        inputs.len(),
        "expected one embedding per input"
    );

    let dims = embeddings
        .first()
        .map(|vec| vec.len())
        .unwrap_or_default();
    assert!(dims > 0, "expected non-empty embedding vectors");

    for (idx, vec) in embeddings.iter().enumerate() {
        assert_eq!(vec.len(), dims, "embedding dims must be consistent");
        assert!(
            vec.iter().any(|value| *value != 0.0),
            "embedding {idx} unexpectedly all zeros"
        );
    }

    let differs = embeddings[0]
        .iter()
        .zip(&embeddings[1])
        .any(|(a, b)| (a - b).abs() > 1e-6);
    assert!(
        differs,
        "expected different inputs to produce different embeddings"
    );
}


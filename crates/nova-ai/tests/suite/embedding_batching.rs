#![cfg(feature = "embeddings")]

use httpmock::prelude::*;
use nova_ai::{Embedder, OpenAiCompatibleEmbedder};
use serde_json::json;
use std::time::Duration;
use url::Url;

#[test]
fn openai_compatible_embed_batch_chunks_by_configured_batch_size() {
    let server = MockServer::start();

    let first = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .json_body(json!({
                "model": "test-model",
                "input": ["a", "b"],
            }));
        then.status(200).json_body(json!({
            "data": [
                { "index": 0, "embedding": [1.0] },
                { "index": 1, "embedding": [2.0] },
            ]
        }));
    });

    let second = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .json_body(json!({
                "model": "test-model",
                "input": ["c"],
            }));
        then.status(200).json_body(json!({
            "data": [
                { "index": 0, "embedding": [3.0] },
            ]
        }));
    });

    let embedder = OpenAiCompatibleEmbedder::new(
        Url::parse(&server.base_url()).expect("base url"),
        "test-model",
        Duration::from_secs(1),
        None,
        /*batch_size=*/ 2,
    )
    .expect("build embedder");

    let inputs = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let embeddings = embedder
        .embed_batch(&inputs)
        .expect("batch embedding succeeds");
    assert_eq!(embeddings, vec![vec![1.0], vec![2.0], vec![3.0]]);

    first.assert_hits(1);
    second.assert_hits(1);
}

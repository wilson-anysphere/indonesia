use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use nova_ai::NovaAi;
use nova_config::{AiConfig, AiPrivacyConfig, AiProviderConfig, AiProviderKind};
use serde_json::Value;
use std::{convert::Infallible, net::SocketAddr};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use url::Url;

fn extract_diff_block(prompt: &str) -> Option<&str> {
    let (_, rest) = prompt.split_once("```diff\n")?;
    let (diff, _) = rest.split_once("\n```")?;
    Some(diff)
}

fn spawn_server<F, Fut>(handler: F) -> (SocketAddr, JoinHandle<()>)
where
    F: Fn(Request<Body>) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Response<Body>> + Send + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("server addr");
    listener
        .set_nonblocking(true)
        .expect("set nonblocking listener");

    let make_svc = make_service_fn(move |_conn| {
        let handler = handler.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                let handler = handler.clone();
                async move { Ok::<_, Infallible>(handler(req).await) }
            }))
        }
    });

    let server = Server::from_tcp(listener)
        .expect("create server")
        .serve(make_svc);

    let handle = tokio::spawn(async move {
        let _ = server.await;
    });

    (addr, handle)
}

fn base_config(url: Url) -> AiConfig {
    AiConfig {
        provider: AiProviderConfig {
            kind: AiProviderKind::OpenAiCompatible,
            url,
            model: "test-model".to_string(),
            max_tokens: 128,
            timeout_ms: 1_000,
            concurrency: Some(1),
            ..AiProviderConfig::default()
        },
        privacy: AiPrivacyConfig {
            local_only: true,
            anonymize_identifiers: Some(false),
            ..AiPrivacyConfig::default()
        },
        enabled: true,
        ..AiConfig::default()
    }
}

fn cloud_config(url: Url) -> AiConfig {
    AiConfig {
        provider: AiProviderConfig {
            kind: AiProviderKind::OpenAiCompatible,
            url,
            model: "test-model".to_string(),
            max_tokens: 128,
            timeout_ms: 1_000,
            concurrency: Some(1),
            ..AiProviderConfig::default()
        },
        privacy: AiPrivacyConfig {
            // Enable cloud-mode defaults (identifier anonymization + redaction).
            local_only: false,
            anonymize_identifiers: None,
            ..AiPrivacyConfig::default()
        },
        enabled: true,
        ..AiConfig::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn code_review_truncates_large_diffs_before_sending() {
    let limit = 200usize;

    let header = "diff --git a/src/Main.java b/src/Main.java\n";
    let tail = "TAIL_MARKER_9876543210\n";
    let diff = format!("{header}{}\n{tail}", "A".repeat(1_000));

    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/chat/completions");
        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("body bytes");
        let json: Value = serde_json::from_slice(&bytes).expect("json body");

        let user = json["messages"][1]["content"]
            .as_str()
            .expect("messages[1].content string");
        let diff_part = extract_diff_block(user).expect("diff fenced block present");
        assert!(
            diff_part.contains("[diff truncated: omitted "),
            "expected truncation marker in diff block; got: {diff_part}"
        );

        assert!(
            diff_part.starts_with(header),
            "expected diff to keep beginning; got: {diff_part}"
        );
        assert!(
            diff_part.ends_with(tail),
            "expected diff to keep end; got: {diff_part}"
        );
        assert!(
            diff_part.chars().count() <= limit,
            "expected diff part <= {limit} chars; got {} chars",
            diff_part.chars().count()
        );

        Response::new(Body::from(r#"{"choices":[{"message":{"content":"ok"}}]}"#))
    };

    let (addr, handle) = spawn_server(handler);

    let mut cfg = base_config(Url::parse(&format!("http://{addr}")).unwrap());
    cfg.features.code_review_max_diff_chars = limit;

    let ai = NovaAi::new(&cfg).expect("NovaAi builds");
    let out = ai
        .code_review(&diff, CancellationToken::new())
        .await
        .expect("code_review succeeds");
    assert_eq!(out, "ok");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn code_review_truncates_multi_file_diffs_at_file_boundaries() {
    let limit = 450usize;

    let file_a = "\
diff --git a/src/A.java b/src/A.java\n\
--- a/src/A.java\n\
+++ b/src/A.java\n\
@@ -1 +1 @@\n\
-class A {}\n\
+class A { int x; }\n";

    let file_b_header = "diff --git a/src/B.java b/src/B.java\n";
    let file_b = format!(
        "{file_b_header}--- a/src/B.java\n+++ b/src/B.java\n@@ -1 +1 @@\n-{}\n+{}\n",
        "B".repeat(2_000),
        "C".repeat(2_000)
    );

    let file_c = "\
diff --git a/src/C.java b/src/C.java\n\
--- a/src/C.java\n\
+++ b/src/C.java\n\
@@ -1 +1 @@\n\
-class C {}\n\
+class C { int y; }\n";

    let diff = format!("{file_a}{file_b}{file_c}");

    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/chat/completions");
        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("body bytes");
        let json: Value = serde_json::from_slice(&bytes).expect("json body");

        let user = json["messages"][1]["content"]
            .as_str()
            .expect("messages[1].content string");
        let diff_part = extract_diff_block(user).expect("diff fenced block present");

        assert!(
            diff_part.contains("diff --git a/src/A.java b/src/A.java\n"),
            "expected diff to preserve complete header for file A; got: {diff_part}"
        );
        assert!(
            diff_part.contains("diff --git a/src/C.java b/src/C.java\n"),
            "expected diff to preserve complete header for file C; got: {diff_part}"
        );
        assert!(
            !diff_part.contains(file_b_header),
            "expected middle file section to be omitted entirely; got: {diff_part}"
        );

        let marker_count = diff_part.matches("[diff truncated: omitted ").count();
        assert_eq!(
            marker_count, 1,
            "expected exactly one truncation marker; got {marker_count} in: {diff_part}"
        );

        assert!(
            diff_part.chars().count() <= limit,
            "expected diff part <= {limit} chars; got {} chars",
            diff_part.chars().count()
        );

        Response::new(Body::from(r#"{"choices":[{"message":{"content":"ok"}}]}"#))
    };

    let (addr, handle) = spawn_server(handler);

    let mut cfg = base_config(Url::parse(&format!("http://{addr}")).unwrap());
    cfg.features.code_review_max_diff_chars = limit;

    let ai = NovaAi::new(&cfg).expect("NovaAi builds");
    let out = ai
        .code_review(&diff, CancellationToken::new())
        .await
        .expect("code_review succeeds");
    assert_eq!(out, "ok");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn code_review_truncates_unified_multi_file_diffs_at_file_boundaries() {
    let limit = 450usize;

    let file_a = "\
--- src/A.java\n\
+++ src/A.java\n\
@@ -1 +1 @@\n\
-class A {}\n\
+class A { int x; }\n";

    let file_b_header = "--- src/B.java\n";
    let file_b = format!(
        "{file_b_header}+++ src/B.java\n@@ -1 +1 @@\n-{}\n+{}\n",
        "B".repeat(2_000),
        "C".repeat(2_000)
    );

    let file_c = "\
--- src/C.java\n\
+++ src/C.java\n\
@@ -1 +1 @@\n\
-class C {}\n\
+class C { int y; }\n";

    let diff = format!("{file_a}{file_b}{file_c}");

    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/chat/completions");
        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("body bytes");
        let json: Value = serde_json::from_slice(&bytes).expect("json body");

        let user = json["messages"][1]["content"]
            .as_str()
            .expect("messages[1].content string");
        let diff_part = extract_diff_block(user).expect("diff fenced block present");

        assert!(
            diff_part.contains("--- src/A.java\n+++ src/A.java\n"),
            "expected diff to preserve complete header for file A; got: {diff_part}"
        );
        assert!(
            diff_part.contains("--- src/C.java\n+++ src/C.java\n"),
            "expected diff to preserve complete header for file C; got: {diff_part}"
        );
        assert!(
            !diff_part.contains(file_b_header),
            "expected middle file section to be omitted entirely; got: {diff_part}"
        );

        let marker_count = diff_part.matches("[diff truncated: omitted ").count();
        assert_eq!(
            marker_count, 1,
            "expected exactly one truncation marker; got {marker_count} in: {diff_part}"
        );

        assert!(
            diff_part.chars().count() <= limit,
            "expected diff part <= {limit} chars; got {} chars",
            diff_part.chars().count()
        );

        Response::new(Body::from(r#"{"choices":[{"message":{"content":"ok"}}]}"#))
    };

    let (addr, handle) = spawn_server(handler);

    let mut cfg = base_config(Url::parse(&format!("http://{addr}")).unwrap());
    cfg.features.code_review_max_diff_chars = limit;

    let ai = NovaAi::new(&cfg).expect("NovaAi builds");
    let out = ai
        .code_review(&diff, CancellationToken::new())
        .await
        .expect("code_review succeeds");
    assert_eq!(out, "ok");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn code_review_truncates_two_file_diffs_by_keeping_complete_file_sections() {
    // Intentionally set a limit that can't keep both file sections, but can keep at least one
    // complete file section plus the truncation marker.
    let limit = 260usize;

    let file_a = "\
diff --git a/src/A.java b/src/A.java\n\
--- a/src/A.java\n\
+++ b/src/A.java\n\
@@ -1 +1 @@\n\
-class A {}\n\
+class A { int x; }\n";

    let file_b_header = "diff --git a/src/B.java b/src/B.java\n";
    let file_b = format!(
        "{file_b_header}--- a/src/B.java\n+++ b/src/B.java\n@@ -1 +1 @@\n-{}\n+{}\n",
        "B".repeat(2_000),
        "C".repeat(2_000)
    );

    let diff = format!("{file_a}{file_b}");

    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/chat/completions");
        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("body bytes");
        let json: Value = serde_json::from_slice(&bytes).expect("json body");

        let user = json["messages"][1]["content"]
            .as_str()
            .expect("messages[1].content string");
        let diff_part = extract_diff_block(user).expect("diff fenced block present");

        let marker_count = diff_part.matches("[diff truncated: omitted ").count();
        assert_eq!(
            marker_count, 1,
            "expected exactly one truncation marker; got {marker_count} in: {diff_part}"
        );

        // Ensure at least one complete file header is preserved (file-section-aware truncation).
        assert!(
            diff_part.contains("diff --git a/src/A.java b/src/A.java\n")
                || diff_part.contains(file_b_header),
            "expected at least one complete file section header to be preserved; got: {diff_part}"
        );

        assert!(
            diff_part.chars().count() <= limit,
            "expected diff part <= {limit} chars; got {} chars",
            diff_part.chars().count()
        );

        Response::new(Body::from(r#"{"choices":[{"message":{"content":"ok"}}]}"#))
    };

    let (addr, handle) = spawn_server(handler);

    let mut cfg = base_config(Url::parse(&format!("http://{addr}")).unwrap());
    cfg.features.code_review_max_diff_chars = limit;

    let ai = NovaAi::new(&cfg).expect("NovaAi builds");
    let out = ai
        .code_review(&diff, CancellationToken::new())
        .await
        .expect("code_review succeeds");
    assert_eq!(out, "ok");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn code_review_truncates_single_file_diffs_at_hunk_boundaries() {
    let limit = 600usize;

    let mut diff = "\
diff --git a/src/Main.java b/src/Main.java\n\
--- a/src/Main.java\n\
+++ b/src/Main.java\n"
    .to_string();

    for idx in 0..20usize {
        diff.push_str(&format!(
            "@@ -{idx},1 +{idx},1 @@\n\
-OLD_{idx}_{}\n\
+NEW_{idx}_{}\n",
            "X".repeat(40),
            "Y".repeat(40)
        ));
    }

    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/chat/completions");
        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("body bytes");
        let json: Value = serde_json::from_slice(&bytes).expect("json body");

        let user = json["messages"][1]["content"]
            .as_str()
            .expect("messages[1].content string");
        let diff_part = extract_diff_block(user).expect("diff fenced block present");

        let marker_count = diff_part.matches("[diff truncated: omitted ").count();
        assert_eq!(
            marker_count, 1,
            "expected exactly one truncation marker; got {marker_count} in: {diff_part}"
        );

        let marker_start = diff_part
            .find("\"[diff truncated: omitted ")
            .expect("marker present");
        let after_marker = &diff_part[marker_start..];
        let marker_end = after_marker
            .find(" chars]\"\n")
            .expect("marker end present");
        let after_marker = &after_marker[marker_end + " chars]\"\n".len()..];
        assert!(
            after_marker.starts_with("@@"),
            "expected truncation to resume at a hunk header; got: {after_marker}"
        );

        assert!(
            diff_part.starts_with("diff --git a/src/Main.java b/src/Main.java\n"),
            "expected diff to keep beginning; got: {diff_part}"
        );
        assert!(
            diff_part.chars().count() <= limit,
            "expected diff part <= {limit} chars; got {} chars",
            diff_part.chars().count()
        );

        Response::new(Body::from(r#"{"choices":[{"message":{"content":"ok"}}]}"#))
    };

    let (addr, handle) = spawn_server(handler);

    let mut cfg = base_config(Url::parse(&format!("http://{addr}")).unwrap());
    cfg.features.code_review_max_diff_chars = limit;

    let ai = NovaAi::new(&cfg).expect("NovaAi builds");
    let out = ai
        .code_review(&diff, CancellationToken::new())
        .await
        .expect("code_review succeeds");
    assert_eq!(out, "ok");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn code_review_truncates_unified_single_file_diffs_at_hunk_boundaries() {
    let limit = 600usize;

    let mut diff = "\
--- src/Main.java\n\
+++ src/Main.java\n"
        .to_string();

    for idx in 0..20usize {
        diff.push_str(&format!(
            "@@ -{idx},1 +{idx},1 @@\n\
-OLD_{idx}_{}\n\
+NEW_{idx}_{}\n",
            "X".repeat(40),
            "Y".repeat(40)
        ));
    }

    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/chat/completions");
        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("body bytes");
        let json: Value = serde_json::from_slice(&bytes).expect("json body");

        let user = json["messages"][1]["content"]
            .as_str()
            .expect("messages[1].content string");
        let diff_part = extract_diff_block(user).expect("diff fenced block present");

        let marker_count = diff_part.matches("[diff truncated: omitted ").count();
        assert_eq!(
            marker_count, 1,
            "expected exactly one truncation marker; got {marker_count} in: {diff_part}"
        );

        let marker_start = diff_part
            .find("\"[diff truncated: omitted ")
            .expect("marker present");
        let after_marker = &diff_part[marker_start..];
        let marker_end = after_marker
            .find(" chars]\"\n")
            .expect("marker end present");
        let after_marker = &after_marker[marker_end + " chars]\"\n".len()..];
        assert!(
            after_marker.starts_with("@@"),
            "expected truncation to resume at a hunk header; got: {after_marker}"
        );

        assert!(
            diff_part.starts_with("--- src/Main.java\n+++ src/Main.java\n"),
            "expected diff to keep beginning; got: {diff_part}"
        );
        assert!(
            diff_part.chars().count() <= limit,
            "expected diff part <= {limit} chars; got {} chars",
            diff_part.chars().count()
        );

        Response::new(Body::from(r#"{"choices":[{"message":{"content":"ok"}}]}"#))
    };

    let (addr, handle) = spawn_server(handler);

    let mut cfg = base_config(Url::parse(&format!("http://{addr}")).unwrap());
    cfg.features.code_review_max_diff_chars = limit;

    let ai = NovaAi::new(&cfg).expect("NovaAi builds");
    let out = ai
        .code_review(&diff, CancellationToken::new())
        .await
        .expect("code_review succeeds");
    assert_eq!(out, "ok");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn code_review_does_not_change_small_diffs() {
    let limit = 1_000usize;
    let diff = "diff --git a/src/Main.java b/src/Main.java\n+class Main {}\n";

    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/chat/completions");
        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("body bytes");
        let json: Value = serde_json::from_slice(&bytes).expect("json body");

        let user = json["messages"][1]["content"]
            .as_str()
            .expect("messages[1].content string");

        let diff_part = extract_diff_block(user).expect("diff fenced block present");
        assert!(
            !diff_part.contains("[diff truncated: omitted "),
            "did not expect truncation marker in diff block for small diff; got: {diff_part}"
        );

        assert_eq!(diff_part, diff);

        Response::new(Body::from(r#"{"choices":[{"message":{"content":"ok"}}]}"#))
    };

    let (addr, handle) = spawn_server(handler);

    let mut cfg = base_config(Url::parse(&format!("http://{addr}")).unwrap());
    cfg.features.code_review_max_diff_chars = limit;

    let ai = NovaAi::new(&cfg).expect("NovaAi builds");
    let out = ai
        .code_review(diff, CancellationToken::new())
        .await
        .expect("code_review succeeds");
    assert_eq!(out, "ok");

    handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn code_review_truncation_marker_survives_identifier_anonymization() {
    let limit = 200usize;
    let diff = format!(
        "diff --git a/src/Main.java b/src/Main.java\n\
\"HEAD_MARKER\"\n\
{}\n\
\"TAIL_MARKER\"\n",
        "~".repeat(2_000)
    );

    let handler = move |req: Request<Body>| async move {
        assert_eq!(req.uri().path(), "/v1/chat/completions");
        let bytes = hyper::body::to_bytes(req.into_body())
            .await
            .expect("body bytes");
        let json: Value = serde_json::from_slice(&bytes).expect("json body");

        let user = json["messages"][1]["content"]
            .as_str()
            .expect("messages[1].content string");
        let diff_part = extract_diff_block(user).expect("diff fenced block present");
        assert!(
            diff_part.contains("[diff truncated: omitted "),
            "expected truncation marker to survive anonymization; got: {diff_part}"
        );
        assert!(
            diff_part.contains("\"HEAD_MARKER\""),
            "expected head marker to remain in diff block: {diff_part}"
        );
        assert!(
            diff_part.contains("\"TAIL_MARKER\""),
            "expected tail marker to remain in diff block: {diff_part}"
        );
        assert!(
            diff_part.chars().count() <= limit,
            "expected diff part <= {limit} chars; got {} chars",
            diff_part.chars().count()
        );

        Response::new(Body::from(r#"{"choices":[{"message":{"content":"ok"}}]}"#))
    };

    let (addr, handle) = spawn_server(handler);

    let mut cfg = cloud_config(Url::parse(&format!("http://{addr}")).unwrap());
    cfg.features.code_review_max_diff_chars = limit;

    let ai = NovaAi::new(&cfg).expect("NovaAi builds");
    let out = ai
        .code_review(&diff, CancellationToken::new())
        .await
        .expect("code_review succeeds");
    assert_eq!(out, "ok");

    handle.abort();
}

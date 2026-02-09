use async_trait::async_trait;
use futures::stream;
use nova_ai::{AiError, AiStream, ChatRequest, LlmClient, NovaAi};
use nova_config::{AiConfig, AiPrivacyConfig};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Default, Clone)]
struct CapturingLlmClient {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl CapturingLlmClient {
    fn last_prompt(&self) -> String {
        let requests = self.requests.lock().unwrap();
        let request = requests
            .last()
            .expect("expected at least one chat request");
        request
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl LlmClient for CapturingLlmClient {
    async fn chat(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<String, AiError> {
        self.requests.lock().unwrap().push(request);
        Ok("ok".to_string())
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<AiStream, AiError> {
        Ok(Box::pin(stream::empty::<Result<String, AiError>>()))
    }

    async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AiError> {
        Ok(Vec::new())
    }
}

fn config_with_excluded_secrets() -> AiConfig {
    let mut cfg = AiConfig::default();
    cfg.privacy = AiPrivacyConfig {
        excluded_paths: vec!["src/secrets/**".to_string()],
        ..AiPrivacyConfig::default()
    };
    cfg
}

#[tokio::test]
async fn code_review_omits_diffs_for_excluded_paths() {
    let ai = NovaAi::new(&config_with_excluded_secrets()).expect("NovaAi builds");
    let client = CapturingLlmClient::default();

    let secret_marker = "DO_NOT_LEAK_THIS_SECRET_MARKER";
    let diff = format!(
        r#"diff --git a/src/secrets/Secret.java b/src/secrets/Secret.java
index 0000000..1111111 100644
--- a/src/secrets/Secret.java
+++ b/src/secrets/Secret.java
@@ -1,1 +1,1 @@
-class Secret {{ String v = "old"; }}
+class Secret {{ String v = "{secret_marker}"; }}
"#
    );

    ai.code_review_with_llm(&client, &diff, CancellationToken::new())
        .await
        .expect("code review");

    let prompt = client.last_prompt();
    assert!(
        !prompt.contains(secret_marker),
        "excluded diff content leaked into prompt: {prompt}"
    );
    assert!(
        prompt.contains("[diff omitted due to excluded_paths]"),
        "expected omission placeholder in prompt; got: {prompt}"
    );
    assert!(
        !prompt.contains("src/secrets/Secret.java"),
        "excluded file path should not appear in prompt: {prompt}"
    );
}

#[tokio::test]
async fn code_review_keeps_allowed_file_sections_intact() {
    let ai = NovaAi::new(&config_with_excluded_secrets()).expect("NovaAi builds");
    let client = CapturingLlmClient::default();

    let secret_marker = "DO_NOT_LEAK_THIS_SECRET_MARKER";
    let allowed_marker = "ALLOWED_MARKER_SHOULD_REMAIN";
    let diff = format!(
        r#"diff --git a/src/secrets/Secret.java b/src/secrets/Secret.java
index 0000000..1111111 100644
--- a/src/secrets/Secret.java
+++ b/src/secrets/Secret.java
@@ -1,1 +1,1 @@
-class Secret {{ String v = "old"; }}
+class Secret {{ String v = "{secret_marker}"; }}
diff --git a/src/Main.java b/src/Main.java
index 2222222..3333333 100644
--- a/src/Main.java
+++ b/src/Main.java
@@ -1,1 +1,2 @@
 class Main {{}}
+// {allowed_marker}
"#
    );

    ai.code_review_with_llm(&client, &diff, CancellationToken::new())
        .await
        .expect("code review");

    let prompt = client.last_prompt();
    assert!(
        !prompt.contains(secret_marker),
        "excluded diff content leaked into prompt: {prompt}"
    );
    assert!(
        prompt.contains("[diff omitted due to excluded_paths]"),
        "expected omission placeholder in prompt; got: {prompt}"
    );
    assert!(
        prompt.contains(allowed_marker),
        "allowed diff content should remain in prompt: {prompt}"
    );
    assert!(
        prompt.contains("diff --git a/src/Main.java b/src/Main.java"),
        "expected allowed file diff header to remain: {prompt}"
    );
}


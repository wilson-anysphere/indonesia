use crate::cloud::{CloudLlmClient, CloudLlmError, GenerateRequest};
use crate::privacy::{redact_suspicious_literals, PrivacyMode};
use crate::provider::{AiProviderError, MultiTokenCompletionProvider};
use crate::{AdditionalEdit, MultiTokenCompletion, MultiTokenInsertTextFormat};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::debug;

#[derive(Debug, Clone)]
pub struct CloudMultiTokenCompletionProvider {
    llm: CloudLlmClient,
    max_output_tokens: u32,
    temperature: f32,
    privacy: PrivacyMode,
    max_insert_text_chars: usize,
    max_label_chars: usize,
    max_additional_edits: usize,
}

impl CloudMultiTokenCompletionProvider {
    pub fn new(llm: CloudLlmClient) -> Self {
        Self {
            llm,
            max_output_tokens: 256,
            temperature: 0.2,
            privacy: PrivacyMode {
                anonymize_identifiers: true,
                include_file_paths: false,
                ..PrivacyMode::default()
            },
            max_insert_text_chars: 4_096,
            max_label_chars: 120,
            max_additional_edits: 8,
        }
    }

    pub fn with_max_output_tokens(mut self, max: u32) -> Self {
        self.max_output_tokens = max;
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
        self
    }

    pub fn with_privacy_mode(mut self, privacy: PrivacyMode) -> Self {
        self.privacy = privacy;
        self
    }

    pub fn with_max_insert_text_chars(mut self, max: usize) -> Self {
        self.max_insert_text_chars = max;
        self
    }
}

impl MultiTokenCompletionProvider for CloudMultiTokenCompletionProvider {
    fn complete_multi_token<'a>(
        &'a self,
        prompt: String,
        max_items: usize,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<MultiTokenCompletion>, AiProviderError>> {
        Box::pin(async move {
            let max_items = max_items.clamp(0, 32);
            if max_items == 0 {
                return Ok(Vec::new());
            }

            let sanitized_prompt = sanitize_prompt(&prompt, &self.privacy);
            let full_prompt = format!("{sanitized_prompt}\n\n{}", json_instructions(max_items));

            // Use a child token so dropping this request doesn't cancel the parent session token.
            let cancel = cancel.child_token();
            let _guard = cancel.clone().drop_guard();
            let response = self
                .llm
                .generate(
                    GenerateRequest {
                        prompt: full_prompt,
                        max_tokens: self.max_output_tokens,
                        temperature: self.temperature,
                    },
                    cancel,
                )
                .await
                .map_err(map_cloud_error)?;

            Ok(parse_completions(
                &response,
                max_items,
                self.max_insert_text_chars,
                self.max_label_chars,
                self.max_additional_edits,
            ))
        })
    }
}

fn map_cloud_error(err: CloudLlmError) -> AiProviderError {
    match err {
        CloudLlmError::Cancelled => AiProviderError::Cancelled,
        other => AiProviderError::Provider(other.to_string()),
    }
}

fn sanitize_prompt(prompt: &str, privacy: &PrivacyMode) -> String {
    // Always apply literal redaction to reduce the chance of leaking tokens or IDs.
    let mut out = redact_suspicious_literals(prompt, &privacy.redaction);

    // `CompletionContextBuilder` does not include file paths today, but be defensive.
    if !privacy.include_file_paths {
        out = redact_file_paths(&out);
    }

    if privacy.anonymize_identifiers {
        out = anonymize_prompt_context(&out);
    }

    out
}

fn anonymize_prompt_context(prompt: &str) -> String {
    // The completion prompt has a stable structure: only anonymize the values in the
    // semantic-context sections (types + surrounding code) so we don't corrupt the
    // instructions or the structured output schema below.
    let mut anonymizer = crate::anonymizer::CodeAnonymizer::new(crate::CodeAnonymizerOptions {
        anonymize_identifiers: true,
        redact_sensitive_strings: false,
        redact_numeric_literals: false,
        strip_or_redact_comments: true,
    });

    let mut out = String::with_capacity(prompt.len());
    let mut lines = prompt.lines();
    let mut in_java_block = false;
    let mut java_block = String::new();

    while let Some(line) = lines.next() {
        if in_java_block {
            if line.trim() == "```" {
                let sanitized = anonymizer.anonymize(&java_block);
                out.push_str(&sanitized);
                if !sanitized.ends_with('\n') {
                    out.push('\n');
                }
                java_block.clear();
                in_java_block = false;
                out.push_str("```\n");
            } else {
                java_block.push_str(line);
                java_block.push('\n');
            }
            continue;
        }

        if line.starts_with("Receiver type: ") {
            let value = line.trim_start_matches("Receiver type: ");
            let sanitized = anonymizer.anonymize(value);
            out.push_str("Receiver type: ");
            out.push_str(&sanitized);
            out.push('\n');
            continue;
        }

        if line.starts_with("Expected type: ") {
            let value = line.trim_start_matches("Expected type: ");
            let sanitized = anonymizer.anonymize(value);
            out.push_str("Expected type: ");
            out.push_str(&sanitized);
            out.push('\n');
            continue;
        }

        if line.trim() == "```java" {
            in_java_block = true;
            out.push_str("```java\n");
            continue;
        }

        out.push_str(line);
        out.push('\n');
    }

    if in_java_block {
        out.push_str(&anonymizer.anonymize(&java_block));
    }

    out
}

fn redact_file_paths(text: &str) -> String {
    // Absolute *nix paths.
    static UNIX_PATH_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?m)(?P<path>/[A-Za-z0-9._\\-]+(?:/[A-Za-z0-9._\\-]+)+)")
            .expect("valid unix path regex")
    });
    // Basic Windows drive paths.
    static WINDOWS_PATH_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| {
            regex::Regex::new(r"(?m)(?P<path>[A-Za-z]:\\\\[A-Za-z0-9._\\-\\\\]+)")
                .expect("valid windows path regex")
        });

    let out = UNIX_PATH_RE.replace_all(text, "[PATH]").into_owned();
    WINDOWS_PATH_RE.replace_all(&out, "[PATH]").into_owned()
}

fn json_instructions(max_items: usize) -> String {
    format!(
        "Return JSON only. Do not wrap it in markdown fences and do not include any extra keys.\n\
         The JSON schema is:\n\
         {{\"completions\":[{{\"label\":\"...\",\"insert_text\":\"...\",\"format\":\"snippet|plain\",\"additional_edits\":[{{\"add_import\":\"java.util.List\"}}],\"confidence\":0.0}}]}}\n\
         Rules:\n\
         - Return at most {max_items} items in \"completions\".\n\
         - \"confidence\" must be a number in [0,1].\n\
         - \"format\" must be either \"snippet\" or \"plain\".\n\
         - \"additional_edits\" may be omitted or empty; the only allowed edit is {{\"add_import\":\"<fully.qualified.Name>\"}}.\n\
         - Never include file paths.\n"
    )
}

fn parse_completions(
    raw: &str,
    max_items: usize,
    max_insert_text_chars: usize,
    max_label_chars: usize,
    max_additional_edits: usize,
) -> Vec<MultiTokenCompletion> {
    let Some(value) = extract_json(raw) else {
        debug!(response_preview = %preview(raw), "multi-token completion parse error: no json object found");
        return Vec::new();
    };

    let Some(items) = value
        .get("completions")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter())
    else {
        debug!(response_preview = %preview(raw), "multi-token completion parse error: missing completions array");
        return Vec::new();
    };

    let mut out = Vec::new();
    for item in items.take(max_items) {
        let Some(obj) = item.as_object() else {
            continue;
        };

        let insert_text = obj
            .get("insert_text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if insert_text.is_empty() {
            continue;
        }

        let insert_text = clamp_chars(insert_text, max_insert_text_chars);

        let label = obj
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        let label = if label.is_empty() {
            default_label(&insert_text)
        } else {
            clamp_chars(label, max_label_chars)
        };

        let format = match obj
            .get("format")
            .and_then(|v| v.as_str())
            .unwrap_or("plain")
            .to_ascii_lowercase()
            .as_str()
        {
            "snippet" => MultiTokenInsertTextFormat::Snippet,
            "plain" => MultiTokenInsertTextFormat::PlainText,
            _ => MultiTokenInsertTextFormat::PlainText,
        };

        let confidence = clamp_confidence(obj.get("confidence"));

        let additional_edits =
            parse_additional_edits(obj.get("additional_edits"), max_additional_edits);

        out.push(MultiTokenCompletion {
            label,
            insert_text,
            format,
            additional_edits,
            confidence,
        });
    }

    out
}

fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if looks_like_completion_payload(&value) {
            return Some(value);
        }
    }

    for (idx, _) in text.match_indices('{').take(32) {
        let sub = &text[idx..];
        let mut de = serde_json::Deserializer::from_str(sub);
        let value = match Value::deserialize(&mut de) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if looks_like_completion_payload(&value) {
            return Some(value);
        }
    }

    None
}

fn looks_like_completion_payload(value: &Value) -> bool {
    value
        .get("completions")
        .and_then(|v| v.as_array())
        .is_some()
}

fn clamp_confidence(value: Option<&Value>) -> f32 {
    let conf = match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    let conf = if conf.is_finite() { conf } else { 0.0 };
    conf.clamp(0.0, 1.0) as f32
}

fn parse_additional_edits(
    value: Option<&Value>,
    max_additional_edits: usize,
) -> Vec<AdditionalEdit> {
    let Some(items) = value.and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for item in items.iter().take(max_additional_edits) {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let Some(path) = obj.get("add_import").and_then(|v| v.as_str()) else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        out.push(AdditionalEdit::AddImport {
            path: path.to_string(),
        });
    }

    out
}

fn clamp_chars(mut s: String, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if s.chars().count() <= max_chars {
        return s;
    }
    s = s.chars().take(max_chars).collect();
    s
}

fn default_label(insert_text: &str) -> String {
    let mut label = insert_text
        .lines()
        .next()
        .unwrap_or(insert_text)
        .trim()
        .to_string();
    if label.is_empty() {
        label = "completion".to_string();
    }
    clamp_chars(label, 60)
}

fn preview(text: &str) -> String {
    let trimmed = text.trim();
    let max = 200usize;
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let prefix: String = trimmed.chars().take(max).collect();
    format!("{prefix}…")
}

use crate::client::LlmClient;
use crate::privacy::{redact_file_paths, redact_suspicious_literals, PrivacyMode};
use crate::provider::{AiProviderError, MultiTokenCompletionProvider, MultiTokenCompletionRequest};
use crate::{
    AdditionalEdit, AiError, ChatMessage, ChatRequest, MultiTokenCompletion,
    MultiTokenInsertTextFormat,
};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

#[derive(Clone)]
pub struct CloudMultiTokenCompletionProvider {
    llm: Arc<dyn LlmClient>,
    max_output_tokens: u32,
    temperature: f32,
    privacy: PrivacyMode,
    max_insert_text_chars: usize,
    max_label_chars: usize,
    max_additional_edits: usize,
}

impl std::fmt::Debug for CloudMultiTokenCompletionProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudMultiTokenCompletionProvider")
            .field("max_output_tokens", &self.max_output_tokens)
            .field("temperature", &self.temperature)
            .field("privacy", &self.privacy)
            .field("max_insert_text_chars", &self.max_insert_text_chars)
            .field("max_label_chars", &self.max_label_chars)
            .field("max_additional_edits", &self.max_additional_edits)
            .finish()
    }
}

impl CloudMultiTokenCompletionProvider {
    pub fn new(llm: Arc<dyn LlmClient>) -> Self {
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
        request: MultiTokenCompletionRequest,
    ) -> BoxFuture<'a, Result<Vec<MultiTokenCompletion>, AiProviderError>> {
        Box::pin(async move {
            let max_items = request.max_items.clamp(0, 32);
            if max_items == 0 {
                return Ok(Vec::new());
            }

            let (sanitized_prompt, reverse_map) = sanitize_prompt(&request.prompt, &self.privacy);
            let full_prompt = format!("{sanitized_prompt}\n\n{}", json_instructions(max_items));

            // Use a child token so dropping this request cancels only this request (and not the
            // parent token if it's shared).
            let cancel = request.cancel.child_token();
            let _guard = cancel.clone().drop_guard();

            let fut = self.llm.chat(
                ChatRequest {
                    messages: vec![ChatMessage::user(full_prompt)],
                    max_tokens: Some(self.max_output_tokens),
                    temperature: Some(self.temperature),
                },
                cancel,
            );

            let response = if request.timeout.is_zero() {
                fut.await.map_err(map_ai_error)?
            } else {
                tokio::time::timeout(request.timeout, fut)
                    .await
                    .map_err(|_| AiProviderError::Timeout)?
                    .map_err(map_ai_error)?
            };
            let mut completions = parse_completions(
                &response,
                max_items,
                self.max_insert_text_chars,
                self.max_label_chars,
                self.max_additional_edits,
            );

            if let Some(reverse_map) = reverse_map {
                deanonymize_completions(
                    &mut completions,
                    &reverse_map,
                    self.max_insert_text_chars,
                    self.max_label_chars,
                );
            }

            Ok(completions)
        })
    }
}

fn map_ai_error(err: AiError) -> AiProviderError {
    match err {
        AiError::Cancelled => AiProviderError::Cancelled,
        AiError::Timeout => AiProviderError::Timeout,
        other => AiProviderError::Provider(other.to_string()),
    }
}

type ReverseIdentifierMap = HashMap<String, String>;

fn sanitize_prompt(prompt: &str, privacy: &PrivacyMode) -> (String, Option<ReverseIdentifierMap>) {
    // Always apply literal redaction to reduce the chance of leaking tokens or IDs.
    let mut out = redact_suspicious_literals(prompt, &privacy.redaction);

    // `CompletionContextBuilder` does not include file paths today, but be defensive.
    if !privacy.include_file_paths {
        out = redact_file_paths(&out);
    }

    if privacy.anonymize_identifiers {
        let (anonymized, reverse_map) =
            anonymize_prompt_context(&out, privacy.redaction.redact_comments);
        debug!(
            reverse_map_len = reverse_map.len(),
            "cloud multi-token completion prompt anonymized identifiers"
        );
        out = anonymized;
        return (out, Some(reverse_map));
    }

    (out, None)
}

fn anonymize_prompt_context(
    prompt: &str,
    redact_comments: bool,
) -> (String, ReverseIdentifierMap) {
    // The completion prompt has a stable structure: only anonymize the values in the
    // semantic-context sections (types + surrounding code) so we don't corrupt the
    // instructions or the structured output schema below.
    let mut anonymizer = crate::anonymizer::CodeAnonymizer::new(crate::CodeAnonymizerOptions {
        anonymize_identifiers: true,
        redact_sensitive_strings: false,
        redact_numeric_literals: false,
        strip_or_redact_comments: redact_comments,
    });

    let mut out = String::with_capacity(prompt.len());
    let mut lines = prompt.lines();
    let mut in_java_block = false;
    let mut java_block = String::new();
    let mut list_section: Option<ListSection> = None;

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

        if line == "Available methods:" {
            list_section = Some(ListSection::AvailableMethods);
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if line == "Importable symbols:" {
            list_section = Some(ListSection::ImportableSymbols);
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if list_section.is_some() && line.trim_start().starts_with("- ") {
            let indent_len = line.len() - line.trim_start().len();
            let indent = &line[..indent_len];
            let rest = &line[indent_len..];
            if let Some(value) = rest.strip_prefix("- ") {
                let sanitized = anonymizer.anonymize(value);
                out.push_str(indent);
                out.push_str("- ");
                out.push_str(&sanitized);
                out.push('\n');
                continue;
            }
        } else if list_section.is_some() && line.trim().is_empty() {
            // Blank line ends the list section.
            list_section = None;
        } else if list_section.is_some() && !line.trim_start().starts_with("- ") {
            // A non-bullet line ends the list section; fall through to normal handling.
            list_section = None;
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

    (out, anonymizer.reverse_identifier_map())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListSection {
    AvailableMethods,
    ImportableSymbols,
}

fn deanonymize_completions(
    completions: &mut [MultiTokenCompletion],
    reverse_map: &ReverseIdentifierMap,
    max_insert_text_chars: usize,
    max_label_chars: usize,
) {
    for completion in completions {
        completion.label = clamp_chars(
            deanonymize_identifiers(&completion.label, reverse_map),
            max_label_chars,
        );
        completion.insert_text = clamp_chars(
            deanonymize_identifiers(&completion.insert_text, reverse_map),
            max_insert_text_chars,
        );

        for edit in &mut completion.additional_edits {
            match edit {
                AdditionalEdit::AddImport { path } => {
                    *path = deanonymize_identifiers(path, reverse_map);
                }
            }
        }
    }
}

fn deanonymize_identifiers(text: &str, reverse_map: &ReverseIdentifierMap) -> String {
    if reverse_map.is_empty() || text.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if is_ident_start(ch) {
            let mut ident = String::new();
            ident.push(ch);
            while let Some(&next) = chars.peek() {
                if is_ident_continue(next) {
                    ident.push(next);
                    chars.next();
                } else {
                    break;
                }
            }

            if let Some(original) = reverse_map.get(&ident) {
                out.push_str(original);
            } else {
                out.push_str(&ident);
            }
        } else {
            out.push(ch);
        }
    }

    out
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_ascii_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
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

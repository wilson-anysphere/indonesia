use serde::{Deserialize, Serialize};

/// ABI version implemented by a guest module.
pub type AbiVersion = u32;

/// Nova Extension WASM ABI v1.
pub const ABI_V1: AbiVersion = 1;

// === Common types =============================================================

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpanV1 {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SeverityV1 {
    Error,
    Warning,
    Info,
}

// === Diagnostics ==============================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsRequestV1 {
    pub project_id: u32,
    pub file_id: u32,
    #[serde(default)]
    pub file_path: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticV1 {
    pub message: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub severity: Option<SeverityV1>,
    #[serde(default)]
    pub span: Option<SpanV1>,
}

// === Completions ==============================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompletionsRequestV1 {
    pub project_id: u32,
    pub file_id: u32,
    pub offset: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompletionItemV1 {
    pub label: String,
    #[serde(default)]
    pub detail: Option<String>,
}

// === Code actions =============================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodeActionsRequestV1 {
    pub project_id: u32,
    pub file_id: u32,
    #[serde(default)]
    pub span: Option<SpanV1>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodeActionV1 {
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
}

// === Navigation ===============================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NavigationRequestV1 {
    pub project_id: u32,
    pub symbol: SymbolV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "id", rename_all = "lowercase")]
pub enum SymbolV1 {
    File(u32),
    Class(u32),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NavigationTargetV1 {
    pub file_id: u32,
    #[serde(default)]
    pub span: Option<SpanV1>,
    pub label: String,
}

// === Inlay hints ==============================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InlayHintsRequestV1 {
    pub project_id: u32,
    pub file_id: u32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InlayHintV1 {
    #[serde(default)]
    pub span: Option<SpanV1>,
    pub label: String,
}

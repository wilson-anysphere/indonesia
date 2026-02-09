//! AI-related IDE primitives (code actions + arguments).
//!
//! This module is protocol-agnostic: `nova-lsp` is responsible for converting
//! these types into concrete LSP objects.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CODE_ACTION_KIND_EXPLAIN: &str = "nova.explain";
pub const CODE_ACTION_KIND_AI_GENERATE: &str = "nova.ai.generate";
pub const CODE_ACTION_KIND_AI_TESTS: &str = "nova.ai.tests";

pub const COMMAND_EXPLAIN_ERROR: &str = "nova.ai.explainError";
pub const COMMAND_GENERATE_METHOD_BODY: &str = "nova.ai.generateMethodBody";
pub const COMMAND_GENERATE_TESTS: &str = "nova.ai.generateTests";
pub const COMMAND_CODE_REVIEW: &str = "nova.ai.codeReview";

/// A protocol-agnostic representation of an editor code action.
#[derive(Debug, Clone, PartialEq)]
pub struct NovaCodeAction {
    pub title: String,
    pub kind: &'static str,
    pub command: NovaCommand,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NovaCommand {
    pub name: String,
    pub arguments: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainErrorArgs {
    #[serde(alias = "diagnostic_message")]
    pub diagnostic_message: String,

    /// Optional source snippet around the diagnostic location.
    pub code: Option<String>,

    /// URI of the document containing the diagnostic.
    #[serde(default)]
    pub uri: Option<String>,

    /// Optional range associated with the diagnostic.
    #[serde(default)]
    pub range: Option<LspRange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateMethodBodyArgs {
    /// The method signature, including modifiers, return type and name.
    #[serde(alias = "method_signature")]
    pub method_signature: String,

    /// Optional surrounding context (enclosing class, other members, etc).
    pub context: Option<String>,

    /// URI of the document containing the method.
    #[serde(default)]
    pub uri: Option<String>,

    /// Range covering the selected method snippet (best-effort).
    #[serde(default)]
    pub range: Option<LspRange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateTestsArgs {
    /// A description of the test target (method or class signature).
    pub target: String,

    /// Optional surrounding context.
    pub context: Option<String>,

    /// URI of the document containing the selected target.
    #[serde(default)]
    pub uri: Option<String>,

    /// Range covering the selected target snippet (best-effort).
    #[serde(default)]
    pub range: Option<LspRange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeReviewArgs {
    /// Unified diff (or similar) to be reviewed by the AI model.
    pub diff: String,

    /// URI of the document associated with the diff (if available).
    #[serde(default)]
    pub uri: Option<String>,
}

pub fn explain_error_action(args: ExplainErrorArgs) -> NovaCodeAction {
    NovaCodeAction {
        title: "Explain this error".to_string(),
        kind: CODE_ACTION_KIND_EXPLAIN,
        command: NovaCommand {
            name: COMMAND_EXPLAIN_ERROR.to_string(),
            arguments: vec![serde_json::to_value(args).expect("ExplainErrorArgs is serializable")],
        },
    }
}

pub fn generate_method_body_action(args: GenerateMethodBodyArgs) -> NovaCodeAction {
    NovaCodeAction {
        title: "Generate method body with AI".to_string(),
        kind: CODE_ACTION_KIND_AI_GENERATE,
        command: NovaCommand {
            name: COMMAND_GENERATE_METHOD_BODY.to_string(),
            arguments: vec![
                serde_json::to_value(args).expect("GenerateMethodBodyArgs is serializable")
            ],
        },
    }
}

pub fn generate_tests_action(args: GenerateTestsArgs) -> NovaCodeAction {
    NovaCodeAction {
        title: "Generate tests with AI".to_string(),
        kind: CODE_ACTION_KIND_AI_TESTS,
        command: NovaCommand {
            name: COMMAND_GENERATE_TESTS.to_string(),
            arguments: vec![serde_json::to_value(args).expect("GenerateTestsArgs is serializable")],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_actions_round_trip_arguments() {
        let action = explain_error_action(ExplainErrorArgs {
            diagnostic_message: "cannot find symbol".to_string(),
            code: Some("foo.bar()".to_string()),
            uri: Some("file:///Test.java".to_string()),
            range: Some(LspRange {
                start: LspPosition {
                    line: 0,
                    character: 0,
                },
                end: LspPosition {
                    line: 0,
                    character: 10,
                },
            }),
        });

        let arg0 = action.command.arguments[0].as_object().expect("args object");
        assert!(
            arg0.contains_key("diagnosticMessage"),
            "expected ExplainErrorArgs to serialize as camelCase"
        );
        assert!(
            !arg0.contains_key("diagnostic_message"),
            "expected ExplainErrorArgs not to serialize as snake_case"
        );
        let args: ExplainErrorArgs =
            serde_json::from_value(action.command.arguments[0].clone()).unwrap();
        assert_eq!(args.diagnostic_message, "cannot find symbol");
        assert_eq!(args.code.as_deref(), Some("foo.bar()"));
        assert_eq!(args.uri.as_deref(), Some("file:///Test.java"));
        assert!(args.range.is_some());

        let action = generate_method_body_action(GenerateMethodBodyArgs {
            method_signature: "public int add(int a, int b)".to_string(),
            context: Some("class Test {}".to_string()),
            uri: Some("file:///Test.java".to_string()),
            range: None,
        });
        let arg0 = action.command.arguments[0].as_object().expect("args object");
        assert!(
            arg0.contains_key("methodSignature"),
            "expected GenerateMethodBodyArgs to serialize as camelCase"
        );
        assert!(
            !arg0.contains_key("method_signature"),
            "expected GenerateMethodBodyArgs not to serialize as snake_case"
        );
        let args: GenerateMethodBodyArgs =
            serde_json::from_value(action.command.arguments[0].clone()).unwrap();
        assert_eq!(args.method_signature, "public int add(int a, int b)");

        let action = generate_tests_action(GenerateTestsArgs {
            target: "add".to_string(),
            context: None,
            uri: Some("file:///Test.java".to_string()),
            range: None,
        });
        let arg0 = action.command.arguments[0].as_object().expect("args object");
        assert!(arg0.contains_key("target"));
        let args: GenerateTestsArgs =
            serde_json::from_value(action.command.arguments[0].clone()).unwrap();
        assert_eq!(args.target, "add");
    }

    #[test]
    fn ai_args_deserialize_legacy_snake_case_payloads() {
        let args: ExplainErrorArgs = serde_json::from_value(serde_json::json!({
            "diagnostic_message": "cannot find symbol",
            "code": "foo.bar()",
            "uri": "file:///Test.java",
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 10 }
            }
        }))
        .expect("ExplainErrorArgs should accept snake_case legacy payloads");
        assert_eq!(args.diagnostic_message, "cannot find symbol");

        let args: GenerateMethodBodyArgs = serde_json::from_value(serde_json::json!({
            "method_signature": "public int add(int a, int b)",
            "context": null,
            "uri": "file:///Test.java",
            "range": null
        }))
        .expect("GenerateMethodBodyArgs should accept snake_case legacy payloads");
        assert_eq!(args.method_signature, "public int add(int a, int b)");
    }
}

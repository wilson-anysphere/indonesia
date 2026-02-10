use std::collections::HashSet;

use nova_ai::context::{ContextBuilder, ContextRequest, RelatedSymbol};
use nova_ai::{
    filter_duplicates_against_insert_text_set, parse_structured_patch,
    safety::{enforce_no_new_imports, enforce_patch_safety},
    validate_multi_token_completion, AdditionalEdit, AiClient, MultiTokenCompletion,
    MultiTokenCompletionContext, MultiTokenInsertTextFormat, PatchSafetyConfig, PrivacyMode,
    SafetyError, VirtualWorkspace,
};
use nova_config::{AiConfig, AiPrivacyConfig, AiProviderConfig, AiProviderKind};
use tempfile::TempDir;
use url::Url;

fn dummy_ai_client_config(privacy: AiPrivacyConfig) -> AiConfig {
    AiConfig {
        provider: AiProviderConfig {
            kind: AiProviderKind::Ollama,
            url: Url::parse("http://localhost:11434").expect("valid url"),
            model: "llama3".to_string(),
            max_tokens: 128,
            timeout_ms: 5_000,
            concurrency: Some(1),
            ..AiProviderConfig::default()
        },
        privacy,
        enabled: true,
        ..AiConfig::default()
    }
}

#[test]
fn privacy_excluded_paths_omit_snippet() {
    let client = AiClient::from_config(&dummy_ai_client_config(AiPrivacyConfig {
        local_only: true,
        anonymize_identifiers: Some(true),
        excluded_paths: vec!["src/secrets/**".to_string()],
        ..AiPrivacyConfig::default()
    }))
    .expect("client");

    let excluded = nova_ai::CodeSnippet::new(
        "src/secrets/Secret.java",
        "class Secret { String token = \"sk-verysecretstringthatislong\"; }",
    );
    assert!(
        client.sanitize_snippet(&excluded).is_none(),
        "excluded_paths must prevent code from reaching the prompt builder"
    );

    let allowed = nova_ai::CodeSnippet::new("src/Main.java", "class Main {}");
    assert!(client.sanitize_snippet(&allowed).is_some());
}

#[test]
fn privacy_excluded_paths_omit_snippet_for_absolute_path() {
    let client = AiClient::from_config(&dummy_ai_client_config(AiPrivacyConfig {
        local_only: true,
        anonymize_identifiers: Some(true),
        excluded_paths: vec!["src/secrets/**".to_string()],
        ..AiPrivacyConfig::default()
    }))
    .expect("client");

    // In LSP usage snippet paths are usually absolute.
    let excluded = nova_ai::CodeSnippet::new(
        "/home/user/project/src/secrets/Secret.java",
        "class Secret { String token = \"sk-verysecretstringthatislong\"; }",
    );
    assert!(
        client.sanitize_snippet(&excluded).is_none(),
        "workspace-relative excluded_paths globs must also match absolute paths"
    );

    #[cfg(windows)]
    {
        let excluded = nova_ai::CodeSnippet::new(
            r"C:\Users\user\project\src\secrets\Secret.java",
            "class Secret { String token = \"sk-verysecretstringthatislong\"; }",
        );
        assert!(client.sanitize_snippet(&excluded).is_none());
    }
}

#[test]
fn privacy_excluded_paths_omit_snippet_for_absolute_path_with_dotdot_and_dot_segments() {
    let client = AiClient::from_config(&dummy_ai_client_config(AiPrivacyConfig {
        local_only: true,
        anonymize_identifiers: Some(true),
        excluded_paths: vec!["src/secrets/**".to_string()],
        ..AiPrivacyConfig::default()
    }))
    .expect("client");

    let tmp = TempDir::new().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonicalize");

    let excluded_with_parent = nova_ai::CodeSnippet::new(
        root.join("workspace")
            .join("src")
            .join("public")
            .join("..")
            .join("secrets")
            .join("Secret.java"),
        "class Secret { String token = \"sk-verysecretstringthatislong\"; }",
    );
    assert!(
        client.sanitize_snippet(&excluded_with_parent).is_none(),
        "`..` segments must not bypass excluded_paths matching"
    );

    let excluded_with_curdir = nova_ai::CodeSnippet::new(
        root.join("workspace")
            .join("src")
            .join(".")
            .join("secrets")
            .join("Secret.java"),
        "class Secret { String token = \"sk-verysecretstringthatislong\"; }",
    );
    assert!(
        client.sanitize_snippet(&excluded_with_curdir).is_none(),
        "`.` segments must not bypass excluded_paths matching"
    );
}

#[test]
fn privacy_excluded_paths_do_not_match_absolute_paths_via_empty_suffix() {
    // Regression test: if absolute-path suffix scanning ever starts trying to match an empty
    // suffix (`PathBuf::new()`), patterns that match the empty path would incorrectly exclude any
    // absolute path.
    let client = AiClient::from_config(&dummy_ai_client_config(AiPrivacyConfig {
        local_only: true,
        anonymize_identifiers: Some(true),
        excluded_paths: vec!["".to_string()],
        ..AiPrivacyConfig::default()
    }))
    .expect("client");

    let tmp = TempDir::new().expect("tempdir");
    let root = std::fs::canonicalize(tmp.path()).expect("canonicalize");

    let snippet = nova_ai::CodeSnippet::new(
        root.join("workspace").join("src").join("Main.java"),
        "class Main {}",
    );
    assert!(
        client.sanitize_snippet(&snippet).is_some(),
        "empty excluded_paths patterns must not exclude absolute paths"
    );
}

#[test]
fn privacy_excluded_paths_allow_non_excluded_absolute_path() {
    let client = AiClient::from_config(&dummy_ai_client_config(AiPrivacyConfig {
        local_only: true,
        anonymize_identifiers: Some(true),
        excluded_paths: vec!["src/secrets/**".to_string()],
        ..AiPrivacyConfig::default()
    }))
    .expect("client");

    let allowed = nova_ai::CodeSnippet::new(
        "/home/user/project/src/Main.java",
        "class Main { String token = \"sk-verysecretstringthatislong\"; }",
    );
    assert!(
        client.sanitize_snippet(&allowed).is_some(),
        "excluded_paths must not match unrelated absolute paths"
    );
}

#[test]
fn cloud_mode_disabling_identifier_anonymization_still_redacts_string_literals_by_default() {
    let client = AiClient::from_config(&dummy_ai_client_config(AiPrivacyConfig {
        local_only: false,
        anonymize_identifiers: Some(false),
        ..AiPrivacyConfig::default()
    }))
    .expect("client");

    let snippet = nova_ai::CodeSnippet::new(
        "src/Main.java",
        r#"class Main { String token = "sk-verysecretstringthatislong"; }"#,
    );
    let sanitized = client
        .sanitize_snippet(&snippet)
        .expect("snippet should be included");

    assert!(sanitized.contains("class Main"), "{sanitized}");
    assert!(sanitized.contains("\"[REDACTED]\""), "{sanitized}");
    assert!(!sanitized.contains("sk-verysecret"), "{sanitized}");
}

#[test]
fn privacy_prompt_sanitization_is_deterministic_and_redacts_literals_and_comments() {
    let builder = ContextBuilder::new();

    let focal_code = r#"
// token: sk-verysecretstringthatislong
class SecretService {
  private final String apiKey = "sk-verysecretstringthatislong";
  private final long accountId = 12345678901234567890L;

  public void run() {
    System.out.println(apiKey + accountId);
  }
}
"#
    .trim_start()
    .to_string();

    let req = ContextRequest {
        file_path: Some("/home/user/project/src/SecretService.java".to_string()),
        focal_code,
        enclosing_context: None,
        project_context: None,
        semantic_context: None,
        related_symbols: vec![RelatedSymbol {
            name: "SecretService".to_string(),
            kind: "class".to_string(),
            snippet: "class SecretService {}".to_string(),
        }],
        related_code: vec![],
        cursor: None,
        diagnostics: Vec::new(),
        extra_files: Vec::new(),
        doc_comments: Some("/** SecretService docs */".to_string()),
        include_doc_comments: true,
        token_budget: 10_000,
        privacy: PrivacyMode {
            anonymize_identifiers: true,
            include_file_paths: false,
            ..PrivacyMode::default()
        },
    };

    let built = builder.build(req.clone());
    let built2 = builder.build(req);

    // Deterministic output.
    assert_eq!(built.text, built2.text);

    // Paths excluded.
    assert!(!built.text.contains("/home/user/project"));

    // String literal redaction.
    assert!(built.text.contains("\"[REDACTED]\""));
    assert!(!built.text.contains("sk-verysecret"));

    // Numeric redaction.
    assert!(!built.text.contains("12345678901234567890"));

    // Comment redaction (outside of string literals).
    assert!(built.text.contains("// [REDACTED]"));

    // Identifiers anonymized and stable across sections.
    assert!(!built.text.contains("SecretService"));
    assert!(built.text.matches("id_0").count() >= 2);
}

#[test]
fn patch_pipeline_parses_and_applies_json_patch() {
    let before = r#"package com.example;

public class Main {
  public int answer() {
    return 41;
  }
}
"#;

    let ws = VirtualWorkspace::new([("src/Main.java".to_string(), before.to_string())]);
    let raw_patch = r#"
{
  "edits": [
    {
      "file": "src/Main.java",
      "range": { "start": { "line": 4, "character": 11 }, "end": { "line": 4, "character": 13 } },
      "text": "42"
    }
  ]
}
"#;

    let patch = parse_structured_patch(raw_patch).expect("parse patch");

    let safety_cfg = PatchSafetyConfig::default();
    enforce_patch_safety(&patch, &ws, &safety_cfg).expect("patch safety");

    let applied = ws.apply_patch(&patch).expect("apply patch");
    let after = applied.workspace.get("src/Main.java").expect("file exists");
    assert!(after.contains("return 42;"));
    assert!(!after.contains("return 41;"));
}

#[test]
fn patch_pipeline_parses_and_applies_unified_diff() {
    let before = "package com.example;\n\npublic class Main {\n  public void hello() {\n    System.out.println(\"hi\");\n  }\n}\n";
    let ws = VirtualWorkspace::new([("src/Main.java".to_string(), before.to_string())]);

    let raw_patch = r#"diff --git a/src/Main.java b/src/Main.java
--- a/src/Main.java
+++ b/src/Main.java
@@ -1,7 +1,8 @@
 package com.example;
 
 public class Main {
   public void hello() {
     System.out.println("hi");
+    System.out.println("bye");
   }
 }
"#;

    let patch = parse_structured_patch(raw_patch).expect("parse patch");
    enforce_patch_safety(&patch, &ws, &PatchSafetyConfig::default()).expect("patch safety");

    let applied = ws.apply_patch(&patch).expect("apply patch");
    let after = applied.workspace.get("src/Main.java").expect("file exists");
    assert!(after.contains("System.out.println(\"bye\");"));
}

#[test]
fn patch_safety_rejects_new_imports_when_configured() {
    let ws = VirtualWorkspace::new([(
        "src/Main.java".to_string(),
        "package com.example;\n\npublic class Main {}\n".to_string(),
    )]);
    let raw_patch = r#"
{
  "edits": [
    {
      "file": "src/Main.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
      "text": "import java.util.List;\n"
    }
  ]
}
"#;
    let patch = parse_structured_patch(raw_patch).expect("parse patch");

    let mut cfg = PatchSafetyConfig::default();
    cfg.no_new_imports = true;

    enforce_patch_safety(&patch, &ws, &cfg).expect("patch safety");
    let applied = ws.apply_patch(&patch).expect("apply patch");
    let err = enforce_no_new_imports(&ws, &applied.workspace, &applied)
        .expect_err("should reject import insertion");
    assert!(matches!(err, SafetyError::NewImports { .. }));
}

#[test]
fn patch_safety_enforces_file_and_size_limits() {
    let raw_patch = r#"
{
  "edits": [
    {
      "file": "src/A.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
      "text": "class A {}\n"
    },
    {
      "file": "src/B.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
      "text": "class B {}\n"
    }
  ]
}
"#;
    let patch = parse_structured_patch(raw_patch).expect("parse patch");

    let ws = VirtualWorkspace::default();

    let mut cfg = PatchSafetyConfig::default();
    cfg.max_files = 1;
    let err = enforce_patch_safety(&patch, &ws, &cfg).expect_err("should reject too many files");
    assert!(matches!(err, SafetyError::TooManyFiles { .. }));

    let mut cfg = PatchSafetyConfig::default();
    cfg.max_total_inserted_chars = 4;
    let err = enforce_patch_safety(&patch, &ws, &cfg)
        .expect_err("should reject patch that inserts too much");
    assert!(matches!(err, SafetyError::TooManyInsertedChars { .. }));
}

#[test]
fn multi_token_completion_validation_and_deduplication() {
    let ctx = MultiTokenCompletionContext {
        receiver_type: Some("Stream<Person>".into()),
        expected_type: Some("List<String>".into()),
        surrounding_code: "people.stream().".into(),
        available_methods: vec!["filter".into(), "map".into(), "collect".into()],
        importable_paths: vec!["java.util.stream.Collectors".into()],
    };

    let valid = MultiTokenCompletion {
        label: "chain".into(),
        insert_text: "filter(p -> p.isActive()).map(Person::getName).collect(Collectors.toList())"
            .into(),
        format: MultiTokenInsertTextFormat::PlainText,
        additional_edits: vec![AdditionalEdit::AddImport {
            path: "java.util.stream.Collectors".into(),
        }],
        confidence: 0.9,
    };
    assert!(validate_multi_token_completion(&ctx, &valid, 3, 64));

    let unknown_method = MultiTokenCompletion {
        label: "bad".into(),
        insert_text: "unknown().map(x -> x)".into(),
        format: MultiTokenInsertTextFormat::PlainText,
        additional_edits: vec![],
        confidence: 0.1,
    };
    assert!(!validate_multi_token_completion(
        &ctx,
        &unknown_method,
        3,
        64
    ));

    let unknown_import = MultiTokenCompletion {
        label: "bad import".into(),
        insert_text: "filter(x -> true)".into(),
        format: MultiTokenInsertTextFormat::PlainText,
        additional_edits: vec![AdditionalEdit::AddImport {
            path: "com.example.NotAllowed".into(),
        }],
        confidence: 0.5,
    };
    assert!(!validate_multi_token_completion(
        &ctx,
        &unknown_import,
        3,
        64
    ));

    let mut ai_items = vec![
        MultiTokenCompletion {
            label: "dup".into(),
            insert_text: "filter".into(),
            format: MultiTokenInsertTextFormat::PlainText,
            additional_edits: vec![],
            confidence: 0.8,
        },
        valid,
    ];
    let standard_insert_texts: HashSet<String> = ["filter".to_string()].into_iter().collect();
    filter_duplicates_against_insert_text_set(&mut ai_items, &standard_insert_texts, |item| {
        Some(item.insert_text.as_str())
    });
    assert_eq!(ai_items.len(), 1);
    assert!(ai_items[0].insert_text.contains("filter("));
}

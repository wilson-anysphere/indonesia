use nova_ai::patch::{parse_structured_patch, Patch, PatchParseError};
use nova_ai::safety::{enforce_patch_safety, PatchSafetyConfig, SafetyError};
use nova_ai::workspace::{PatchApplyConfig, VirtualWorkspace};

fn has_lone_lf(text: &str) -> bool {
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            return true;
        }
    }
    false
}

#[test]
fn unified_diff_preserves_crlf() {
    let original = "class Foo {\r\n    void a() {}\r\n}\r\n";
    let ws = VirtualWorkspace::new(vec![("Foo.java".to_string(), original.to_string())]);

    let diff = r#"--- a/Foo.java
+++ b/Foo.java
@@ -1,3 +1,3 @@
 class Foo {
-    void a() {}
+    void b() {}
 }
"#;

    let patch = parse_structured_patch(diff).expect("parse diff");
    let applied = ws.apply_patch(&patch).expect("apply diff");
    let updated = applied.workspace.get("Foo.java").expect("file exists");

    assert!(updated.contains("void b()"));
    assert!(updated.contains("\r\n"), "expected CRLF line endings");
    assert!(
        !has_lone_lf(updated),
        "should not introduce lone LF characters"
    );
    assert!(
        updated.ends_with("\r\n"),
        "should preserve trailing CRLF newline"
    );
}

#[test]
fn unified_diff_create_infers_crlf_style_from_workspace() {
    let original = "class Foo {\r\n    void a() {}\r\n}\r\n";
    let ws = VirtualWorkspace::new(vec![
        ("src/Foo.java".to_string(), original.to_string()),
        // Include a file in a different directory with LF to ensure we prefer the same-directory
        // style.
        ("Other.java".to_string(), "line1\nline2\n".to_string()),
    ]);

    let diff = r#"diff --git a/src/New.java b/src/New.java
new file mode 100644
--- /dev/null
+++ b/src/New.java
@@ -0,0 +1,3 @@
+class New {
+    void b() {}
+}
"#;

    let patch = parse_structured_patch(diff).expect("parse diff");
    let applied = ws
        .apply_patch_with_config(
            &patch,
            &PatchApplyConfig {
                allow_new_files: true,
            },
        )
        .expect("apply diff");
    let created = applied.workspace.get("src/New.java").expect("file exists");

    assert!(created.contains("class New"));
    assert!(created.contains("\r\n"), "expected CRLF line endings");
    assert!(
        !has_lone_lf(created),
        "should not introduce lone LF characters"
    );
    assert!(
        created.ends_with("\r\n"),
        "should infer trailing CRLF newline"
    );
}

#[test]
fn unified_diff_create_infers_missing_trailing_newline() {
    // CRLF file with no trailing newline: the created file should inherit both the line ending
    // style and the lack of trailing newline.
    let original = "a\r\nb";
    let ws = VirtualWorkspace::new(vec![("Example.java".to_string(), original.to_string())]);

    let diff = r#"diff --git a/New.java b/New.java
new file mode 100644
--- /dev/null
+++ b/New.java
@@ -0,0 +1,2 @@
+line1
+line2
"#;

    let patch = parse_structured_patch(diff).expect("parse diff");
    let applied = ws
        .apply_patch_with_config(
            &patch,
            &PatchApplyConfig {
                allow_new_files: true,
            },
        )
        .expect("apply diff");
    let created = applied.workspace.get("New.java").expect("file exists");

    assert_eq!(created, "line1\r\nline2");
    assert!(
        !created.ends_with('\n') && !created.ends_with('\r'),
        "should not add a trailing newline"
    );
}

#[test]
fn unified_diff_preserves_trailing_newline() {
    let original = "a\nline2\n";
    let ws = VirtualWorkspace::new(vec![("Example.java".to_string(), original.to_string())]);

    let diff = r#"--- a/Example.java
+++ b/Example.java
@@ -1,2 +1,2 @@
 a
-line2
+line3
"#;

    let patch = parse_structured_patch(diff).expect("parse diff");
    let applied = ws.apply_patch(&patch).expect("apply diff");
    let updated = applied.workspace.get("Example.java").expect("file exists");

    assert!(updated.ends_with('\n'), "should preserve trailing newline");
    assert!(updated.contains("line3"));
}

#[test]
fn unified_diff_rename_and_delete_are_supported() {
    let ws = VirtualWorkspace::new(vec![
        ("Old.java".to_string(), "class Old {}\n".to_string()),
        ("Delete.java".to_string(), "a\n".to_string()),
    ]);

    let rename = r#"diff --git a/Old.java b/New.java
similarity index 100%
rename from Old.java
rename to New.java
"#;

    let patch = parse_structured_patch(rename).expect("parse rename diff");
    let applied = ws.apply_patch(&patch).expect("apply rename");

    assert!(applied.workspace.get("Old.java").is_none());
    assert_eq!(applied.workspace.get("New.java").unwrap(), "class Old {}\n");

    let delete = r#"diff --git a/Delete.java b/Delete.java
deleted file mode 100644
index 1234567..0000000
--- a/Delete.java
+++ /dev/null
@@ -1 +0,0 @@
-a
"#;

    let patch = parse_structured_patch(delete).expect("parse delete diff");
    let applied = applied.workspace.apply_patch(&patch).expect("apply delete");

    assert!(applied.workspace.get("Delete.java").is_none());
}

#[test]
fn json_patch_ops_create_rename_delete() {
    let ws = VirtualWorkspace::new(vec![("Foo.java".to_string(), "class Foo {}\n".to_string())]);

    let patch = r#"{
  "ops": [
    { "op": "rename", "from": "Foo.java", "to": "Bar.java" },
    { "op": "create", "file": "New.java", "text": "class New {}\n" },
    { "op": "delete", "file": "New.java" }
  ],
  "edits": [
    {
      "file": "Bar.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
      "text": "// renamed\n"
    }
  ]
    }"#;

    let patch = parse_structured_patch(patch).expect("parse json ops");
    let applied = ws
        .apply_patch_with_config(
            &patch,
            &PatchApplyConfig {
                allow_new_files: true,
            },
        )
        .expect("apply json ops");

    assert!(applied.workspace.get("Foo.java").is_none());
    assert!(applied.workspace.get("New.java").is_none());
    let bar = applied
        .workspace
        .get("Bar.java")
        .expect("renamed file exists");
    assert!(bar.starts_with("// renamed\n"));
    assert!(bar.contains("class Foo"));
    assert_eq!(applied.renamed_files.get("Bar.java").unwrap(), "Foo.java");
}

#[test]
fn safety_rejects_bad_paths_and_large_patches() {
    let ws = VirtualWorkspace::new(vec![("Example.java".to_string(), "abcd\n".to_string())]);

    let non_relative = r#"{
  "edits": [
    {
      "file": "/etc/passwd",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
      "text": "x"
    }
  ]
}"#;
    let patch = parse_structured_patch(non_relative).expect("parse json");
    let err = enforce_patch_safety(&patch, &ws, &PatchSafetyConfig::default()).unwrap_err();
    assert!(matches!(err, SafetyError::NonRelativePath { .. }));

    let excluded = r#"{
  "edits": [
    {
      "file": "secret/Config.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
      "text": "x"
    }
  ]
}"#;
    let patch = parse_structured_patch(excluded).expect("parse json");
    let mut config = PatchSafetyConfig::default();
    config.excluded_path_prefixes = vec!["secret/".into()];
    let err = enforce_patch_safety(&patch, &ws, &config).unwrap_err();
    assert!(matches!(err, SafetyError::ExcludedPath { .. }));

    let too_large_insert = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
      "text": "abcdef"
    }
  ]
}"#;
    let patch = parse_structured_patch(too_large_insert).expect("parse json");
    let mut config = PatchSafetyConfig::default();
    config.max_total_inserted_chars = 3;
    let err = enforce_patch_safety(&patch, &ws, &config).unwrap_err();
    assert!(matches!(err, SafetyError::TooManyInsertedChars { .. }));

    let too_large_delete = r#"{
  "edits": [
    {
      "file": "Example.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 4 } },
      "text": ""
    }
  ]
}"#;
    let patch = parse_structured_patch(too_large_delete).expect("parse json");
    let mut config = PatchSafetyConfig::default();
    config.max_total_deleted_chars = 3;
    let err = enforce_patch_safety(&patch, &ws, &config).unwrap_err();
    assert!(matches!(err, SafetyError::TooManyDeletedChars { .. }));
}

fn sample_json_patch() -> &'static str {
    r#"{
  "edits": [
    {
      "file": "Foo.java",
      "range": {
        "start": { "line": 0, "character": 0 },
        "end": { "line": 0, "character": 0 }
      },
      "text": "// hello\n"
    }
  ]
}"#
}

fn sample_diff_patch() -> &'static str {
    r#"diff --git a/foo.txt b/foo.txt
index e69de29..4b825dc 100644
--- a/foo.txt
+++ b/foo.txt
@@ -0,0 +1,1 @@
+hello
"#
}

#[test]
fn structured_patch_extracts_from_second_fence_when_first_is_not_a_patch() {
    let raw = format!(
        "Here's what I found.\n\n```json\n{{\"foo\":\"bar\"}}\n```\n\nNow the patch:\n\n```json\n{}\n```\n",
        sample_json_patch()
    );
    let patch = parse_structured_patch(&raw).expect("parse patch");
    assert!(matches!(patch, Patch::Json(_)));
}

#[test]
fn structured_patch_accepts_jsonc_fence() {
    let raw = format!("```jsonc\n{}\n```\n", sample_json_patch());
    let patch = parse_structured_patch(&raw).expect("parse patch");
    assert!(matches!(patch, Patch::Json(_)));
}

#[test]
fn structured_patch_accepts_indented_udiff_fences() {
    let raw = r#"Here you go:

    ```udiff
    diff --git a/foo.txt b/foo.txt
    index e69de29..4b825dc 100644
    --- a/foo.txt
    +++ b/foo.txt
    @@ -0,0 +1,1 @@
    +hello
    ```
"#;

    let patch = parse_structured_patch(raw).expect("parse patch");
    assert!(matches!(patch, Patch::UnifiedDiff(_)));
}

#[test]
fn structured_patch_prefers_first_successfully_parsing_fence() {
    let raw = format!(
        "```diff\n{}\n```\n\n```json\n{}\n```\n",
        sample_diff_patch(),
        sample_json_patch()
    );
    let patch = parse_structured_patch(&raw).expect("parse patch");
    assert!(matches!(patch, Patch::UnifiedDiff(_)));
}

#[test]
fn structured_patch_fallback_error_prefers_most_patch_like_fence() {
    let raw = r#"```json
{"foo":"bar"}
```

```diff
diff --git a/foo.txt b/foo.txt
--- a/foo.txt
+++ b/foo.txt
@@ -1,1 +1,1 @@
-hello
+world
BROKEN
```"#;

    let err = parse_structured_patch(raw).expect_err("expected parse failure");
    assert!(matches!(err, PatchParseError::InvalidDiff(_)));
}

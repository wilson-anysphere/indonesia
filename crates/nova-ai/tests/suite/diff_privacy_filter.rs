use nova_ai::filter_diff_for_excluded_paths_for_tests;
use std::path::Path;

const OMITTED_SENTINEL: &str = "\"__NOVA_AI_DIFF_OMITTED__\"";

fn sentinel_line(newline: &str) -> String {
    format!("{OMITTED_SENTINEL}{newline}")
}

#[test]
fn git_diff_quoted_paths_with_spaces_are_filtered() {
    let excluded_path = "src/secrets/secret file.txt";

    let excluded_section = r#"diff --git "a/src/secrets/secret file.txt" "b/src/secrets/secret file.txt"
index 1111111..2222222 100644
--- "a/src/secrets/secret file.txt"
+++ "b/src/secrets/secret file.txt"
@@ -1 +1 @@
-old
+SECRET_MARKER
"#;

    let allowed_section = r#"diff --git a/src/Main.java b/src/Main.java
index 3333333..4444444 100644
--- a/src/Main.java
+++ b/src/Main.java
@@ -1 +1 @@
-class Main {}
+class Main { /* ok */ }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("SECRET_MARKER"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_unquoted_paths_with_spaces_are_filtered() {
    // This matches `git diff` output when `core.quotePath=false`.
    let excluded_path = "src/secrets/secret file.txt";

    let excluded_section = r#"diff --git a/src/secrets/secret file.txt b/src/secrets/secret file.txt
index 1111111..2222222 100644
--- a/src/secrets/secret file.txt	
+++ b/src/secrets/secret file.txt	
@@ -1 +1 @@
-old
+SECRET_MARKER
"#;

    let allowed_section = r#"diff --git a/src/Main.java b/src/Main.java
index 3333333..4444444 100644
--- a/src/Main.java
+++ b/src/Main.java
@@ -1 +1 @@
-class Main {}
+class Main { /* ok */ }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("SECRET_MARKER"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_rename_header_is_excluded_if_either_path_matches() {
    let excluded_path = "src/secrets/old-name.txt";

    let excluded_section = r#"diff --git a/src/secrets/old-name.txt b/src/public/new-name.txt
similarity index 100%
rename from src/secrets/old-name.txt
rename to src/public/new-name.txt
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 0000000..1111111 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 1; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("old-name.txt"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_new_file_dev_null_is_excluded_based_on_remaining_path() {
    let excluded_path = "src/secrets/new.txt";

    let excluded_section = r#"diff --git /dev/null b/src/secrets/new.txt
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/src/secrets/new.txt
@@ -0,0 +1 @@
+NEW_SECRET
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 2; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("NEW_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_combined_cc_header_is_supported() {
    let excluded_path = "src/secrets/merge.txt";

    let excluded_section = r#"diff --cc src/secrets/merge.txt
index 1111111,2222222..3333333
--- a/src/secrets/merge.txt
+++ b/src/secrets/merge.txt
@@@ -1,1 -1,1 +1,1 @@@
-OLD
+MERGE_SECRET
"#;

    let allowed_section = r#"diff --cc src/Ok.java
index 4444444,5555555..6666666
--- a/src/Ok.java
+++ b/src/Ok.java
@@@ -1,1 -1,1 +1,1 @@@
-class Ok {}
+class Ok { int x = 3; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("MERGE_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_combined_diff_combined_header_is_supported() {
    let excluded_path = "src/secrets/combined.txt";

    let excluded_section = r#"diff --combined src/secrets/combined.txt
index 1111111,2222222..3333333
--- a/src/secrets/combined.txt
+++ b/src/secrets/combined.txt
@@@ -1,1 -1,1 +1,1 @@@
-OLD
+COMBINED_SECRET
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 7777777..8888888 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 4; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("COMBINED_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn unified_diff_with_timestamps_is_supported() {
    let excluded_path = "src/secrets/secret.txt";

    let excluded_section = "--- a/src/secrets/secret.txt\t2026-02-10 12:00:00.000000000 +0000\n\
+++ b/src/secrets/secret.txt\t2026-02-10 12:00:00.000000000 +0000\n\
@@ -1 +1 @@\n\
-old\n\
+SECRET_TS\n";

    let allowed_section = "--- a/src/Ok.java\t2026-02-10 12:00:00.000000000 +0000\n\
+++ b/src/Ok.java\t2026-02-10 12:00:00.000000000 +0000\n\
@@ -1 +1 @@\n\
-class Ok {}\n\
+class Ok { int x = 5; }\n";

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("SECRET_TS"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn unified_diff_windows_paths_with_backslashes_and_timestamps_are_supported() {
    let excluded_path = r"C:\Users\alice\secrets\secret.txt";

    let excluded_section = format!(
        "--- {excluded_path}\t2026-02-10 12:00:00.000000000 +0000\n\
+++ {excluded_path}\t2026-02-10 12:00:00.000000000 +0000\n\
@@ -1 +1 @@\n\
-old\n\
+WIN_SECRET\n"
    );

    let allowed_section = "--- a/src/Ok.java\t2026-02-10 12:00:00.000000000 +0000\n\
+++ b/src/Ok.java\t2026-02-10 12:00:00.000000000 +0000\n\
@@ -1 +1 @@\n\
-class Ok {}\n\
+class Ok { int x = 7; }\n";

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("WIN_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn unified_diff_with_ambiguous_quoted_headers_fails_closed() {
    let diff = "--- \"a/src/Ok.java\" \"b/src/secrets/Secret.java\"\n\
+++ \"b/src/Ok.java\" \"b/src/secrets/Secret.java\"\n\
@@ -1 +1 @@\n\
-old\n\
+LEAK\n";

    let filtered = filter_diff_for_excluded_paths_for_tests(diff, |_| false);
    assert_eq!(filtered, sentinel_line("\n"));
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
}

#[test]
fn git_diff_token_parsing_supports_octal_escapes() {
    let excluded_path = "src/secrets/café.txt";

    // `\303\251` are the UTF-8 bytes for "é".
    let excluded_section = r#"diff --git "a/src/secrets/caf\303\251.txt" "b/src/secrets/caf\303\251.txt"
index 1111111..2222222 100644
--- "a/src/secrets/caf\303\251.txt"
+++ "b/src/secrets/caf\303\251.txt"
@@ -1 +1 @@
-old
+OCTAL_SECRET
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 9999999..aaaaaaa 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 6; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("OCTAL_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn parsing_failure_fails_closed_with_single_sentinel_line_and_preserves_newline_style() {
    let diff_lf = "diff --git \"a/src/secrets/bad.txt b/src/secrets/bad.txt\n+LEAK\n";
    let diff_crlf = diff_lf.replace('\n', "\r\n");

    let filtered = filter_diff_for_excluded_paths_for_tests(&diff_crlf, |_| false);
    assert_eq!(filtered, sentinel_line("\r\n"));
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
}

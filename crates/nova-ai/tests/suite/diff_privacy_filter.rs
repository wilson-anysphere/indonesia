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
fn git_diff_no_prefix_with_spaces_is_filtered_via_unified_headers() {
    // `git diff --no-prefix` emits `diff --git <path> <path>` without `a/` and `b/`, making the
    // `diff --git` line ambiguous when paths contain spaces. Git's `---`/`+++` headers are still
    // parseable thanks to their tab separator.
    let excluded_path = "src/secrets/secret file.txt";

    let excluded_section = r#"diff --git src/secrets/secret file.txt src/secrets/secret file.txt
index 1111111..2222222 100644
--- src/secrets/secret file.txt	
+++ src/secrets/secret file.txt	
@@ -1 +1 @@
-old
+NO_PREFIX_SECRET
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
    assert!(!filtered.contains("NO_PREFIX_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_no_prefix_rename_with_spaces_is_filtered_via_rename_headers() {
    // `git diff --no-prefix --cached` rename-only diffs do not include `---`/`+++` headers; rely
    // on `rename from` / `rename to` lines for path extraction.
    let excluded_path = "src/old name.txt";

    let excluded_section = r#"diff --git src/old name.txt src/new name.txt
similarity index 100%
rename from src/old name.txt
rename to src/new name.txt
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 0000000..1111111 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 9; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("old name.txt"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_rename_with_mixed_quoted_and_unquoted_spaces_is_filtered_via_rename_headers() {
    // Real `git diff --cached` output can have mixed quoting: git C-quotes non-ASCII paths, but
    // does not quote spaces in the `diff --git` header. Ensure we don't fail closed on this valid
    // format, and can still extract paths from `rename from/to` metadata.
    let excluded_path = "src/a and b.txt";

    let excluded_section = r#"diff --git "a/src/caf\303\251.txt" b/src/a and b.txt
similarity index 100%
rename from "src/caf\303\251.txt"
rename to src/a and b.txt
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 0000000..1111111 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 10; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("a and b.txt"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_binary_files_line_with_spaces_is_filtered() {
    // Binary diffs may omit `---`/`+++` headers and include only a `Binary files ... differ` line.
    // Ensure we can still determine the file path and omit excluded sections.
    let excluded_path = "src/secrets/secret file.bin";

    let excluded_section = r#"diff --git src/secrets/secret file.bin src/secrets/secret file.bin
index 06de4c6..ee2f3c8 100644
Binary files src/secrets/secret file.bin and src/secrets/secret file.bin differ
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 16; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("secret file.bin"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_binary_files_line_with_octal_escapes_is_filtered() {
    let excluded_path = "src/café file.bin";

    let excluded_section = r#"diff --git "src/caf\303\251 file.bin" "src/caf\303\251 file.bin"
new file mode 100644
index 0000000..b11aa54
Binary files /dev/null and "src/caf\303\251 file.bin" differ
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 17; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("caf"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_binary_files_line_with_custom_prefixes_is_filtered() {
    let excluded_path = "src/secrets/secret.bin";

    let excluded_section = r#"diff --git old/src/secrets/secret.bin new/src/secrets/secret.bin
index 06de4c6..ee2f3c8 100644
Binary files old/src/secrets/secret.bin and new/src/secrets/secret.bin differ
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 18; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("secret.bin"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_binary_files_line_with_custom_prefixes_and_and_in_path_is_filtered() {
    let excluded_path = "src/a and b.bin";

    let excluded_section = r#"diff --git old/src/a and b.bin new/src/a and b.bin
index cec215b..ee58f53 100644
Binary files old/src/a and b.bin and new/src/a and b.bin differ
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 21; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("a and b.bin"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_binary_files_line_with_and_in_path_is_filtered() {
    let excluded_path = "src/a and b.bin";

    let excluded_section = r#"diff --git a/src/a and b.bin b/src/a and b.bin
index cec215b..ee58f53 100644
Binary files a/src/a and b.bin and b/src/a and b.bin differ
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 19; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("a and b.bin"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_binary_files_line_with_dev_null_and_and_in_path_is_filtered() {
    let excluded_path = "src/a and b.bin";

    let excluded_section = r#"diff --git a/src/a and b.bin b/src/a and b.bin
new file mode 100644
index 0000000..cec215b
Binary files /dev/null and b/src/a and b.bin differ
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 20; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("a and b.bin"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_no_prefix_paths_starting_with_a_are_not_stripped_for_exclusion_matching() {
    // When using `git diff --no-prefix`, paths are not prefixed with `a/` and `b/`. If the real
    // path itself starts with `a/`, we must not treat it as a pseudo prefix for exclusion checks.
    let excluded_path = "a/src/secrets/Secret.java";

    let excluded_section = r#"diff --git a/src/secrets/Secret.java a/src/secrets/Secret.java
index 1111111..2222222 100644
--- a/src/secrets/Secret.java
+++ a/src/secrets/Secret.java
@@ -1 +1 @@
-old
+NO_PREFIX_A_DIR_SECRET
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 0000000..1111111 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 12; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("NO_PREFIX_A_DIR_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_custom_src_dst_prefixes_are_stripped_for_exclusion_matching() {
    // Git can customize `a/` and `b/` prefixes via `--src-prefix` / `--dst-prefix`. Ensure we can
    // still match repo-relative excluded_paths by considering the common suffix of old/new paths.
    let excluded_path = "src/secrets/Secret.java";

    let excluded_section = r#"diff --git old/src/secrets/Secret.java new/src/secrets/Secret.java
index 0000000..1111111 100644
--- old/src/secrets/Secret.java
+++ new/src/secrets/Secret.java
@@ -1 +1 @@
-old
+CUSTOM_PREFIX_SECRET
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 13; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("CUSTOM_PREFIX_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_custom_prefixes_with_spaces_are_filtered() {
    let excluded_path = "src/secrets/secret file.txt";

    let excluded_section = r#"diff --git old/src/secrets/secret file.txt new/src/secrets/secret file.txt
index 1111111..2222222 100644
--- old/src/secrets/secret file.txt	
+++ new/src/secrets/secret file.txt	
@@ -1 +1 @@
-old
+CUSTOM_PREFIX_SPACE_SECRET
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 14; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("CUSTOM_PREFIX_SPACE_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn git_diff_rename_does_not_use_suffix_only_candidate_for_exclusion() {
    // Regression test: for rename/copy diffs, suffix-only candidates like `foo.txt` are too
    // ambiguous; rely on `rename from/to` metadata instead.
    let excluded_path = "foo.txt";

    let diff = r#"diff --git old/src/foo.txt new/other/foo.txt
similarity index 100%
rename from src/foo.txt
rename to other/foo.txt
"#;

    let filtered = filter_diff_for_excluded_paths_for_tests(diff, |path| {
        path == Path::new(excluded_path)
    });

    assert_eq!(filtered, diff);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 0);
}

#[test]
fn git_diff_unquoted_paths_with_backslashes_are_not_unescaped() {
    // Some diffs may contain literal backslashes in file names (valid on Unix) when
    // `core.quotePath=false`. These must be treated as literal characters, not C-style escapes.
    let excluded_path = r"src\secrets\secret.txt";

    let excluded_section = r#"diff --git a/src\secrets\secret.txt b/src\secrets\secret.txt
index 1111111..2222222 100644
--- a/src\secrets\secret.txt
+++ b/src\secrets\secret.txt
@@ -1 +1 @@
-old
+BACKSLASH_SECRET
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
    assert!(!filtered.contains("BACKSLASH_SECRET"));
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
fn git_diff_combined_cc_header_with_spaces_is_supported() {
    let excluded_path = "src/secrets/merge file.txt";

    let excluded_section = r#"diff --cc src/secrets/merge file.txt
index 1111111,2222222..3333333
--- a/src/secrets/merge file.txt
+++ b/src/secrets/merge file.txt
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
fn git_diff_combined_cc_header_does_not_strip_a_prefix_from_real_paths() {
    let excluded_path = "a/src/secrets/merge.txt";

    let excluded_section = r#"diff --cc a/src/secrets/merge.txt
index 1111111,2222222..3333333
--- a/a/src/secrets/merge.txt
+++ b/a/src/secrets/merge.txt
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
fn git_diff_combined_diff_combined_header_with_spaces_is_supported() {
    let excluded_path = "src/secrets/combined file.txt";

    let excluded_section = r#"diff --combined src/secrets/combined file.txt
index 1111111,2222222..3333333
--- a/src/secrets/combined file.txt
+++ b/src/secrets/combined file.txt
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
fn unified_diff_with_diff_u_preamble_line_is_supported() {
    // diffutils-style diffs include a `diff -u old new` line before the `---` / `+++` headers.
    // Ensure we treat that as part of the file section so it doesn't leak excluded paths.
    let excluded_path = "src/secrets/secret.txt";

    let excluded_section = "diff -u src/secrets/secret.txt src/secrets/secret.txt\n\
--- src/secrets/secret.txt\t2026-02-10 12:00:00.000000000 +0000\n\
+++ src/secrets/secret.txt\t2026-02-10 12:00:00.000000000 +0000\n\
@@ -1 +1 @@\n\
-old\n\
+DIFF_U_SECRET\n";

    let allowed_section = "diff -u src/Ok.java src/Ok.java\n\
--- src/Ok.java\t2026-02-10 12:00:00.000000000 +0000\n\
+++ src/Ok.java\t2026-02-10 12:00:00.000000000 +0000\n\
@@ -1 +1 @@\n\
-class Ok {}\n\
+class Ok { int x = 22; }\n";

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("diff -u src/secrets/secret.txt"));
    assert!(!filtered.contains("DIFF_U_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn unified_diff_with_index_section_headers_is_supported() {
    // SVN-style diffs include `Index: path` and an `====...` delimiter before the `---` / `+++`
    // headers. Ensure these lines are omitted along with excluded file sections.
    let excluded_path = "src/secrets/secret file.txt";

    let excluded_section = "Index: src/secrets/secret file.txt\n\
===================================================================\n\
--- src/secrets/secret file.txt\t2026-02-10 12:00:00.000000000 +0000\n\
+++ src/secrets/secret file.txt\t2026-02-10 12:00:00.000000000 +0000\n\
@@ -1 +1 @@\n\
-old\n\
+INDEX_SECRET\n";

    let allowed_section = "Index: src/Ok.java\n\
===================================================================\n\
--- src/Ok.java\t2026-02-10 12:00:00.000000000 +0000\n\
+++ src/Ok.java\t2026-02-10 12:00:00.000000000 +0000\n\
@@ -1 +1 @@\n\
-class Ok {}\n\
+class Ok { int x = 23; }\n";

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("Index: src/secrets/secret file.txt"));
    assert!(!filtered.contains("INDEX_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn unified_diff_does_not_strip_real_a_directory_prefix_from_paths() {
    let excluded_path = "a/src/secrets/secret.txt";

    let excluded_section = "--- a/src/secrets/secret.txt\n\
+++ a/src/secrets/secret.txt\n\
@@ -1 +1 @@\n\
-old\n\
+REAL_A_SECRET\n";

    let allowed_section = "--- src/Ok.java\n\
+++ src/Ok.java\n\
@@ -1 +1 @@\n\
-class Ok {}\n\
+class Ok { int x = 8; }\n";

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("REAL_A_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn mixed_unified_then_git_diff_filters_the_unified_preamble_section() {
    let excluded_path = "src/secrets/Secret.java";

    let excluded_preamble = r#"--- a/src/secrets/Secret.java
+++ b/src/secrets/Secret.java
@@ -1 +1 @@
-old
+MIXED_SECRET
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 11; }
"#;

    let diff = format!("{excluded_preamble}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("MIXED_SECRET"));
    assert!(filtered.contains(allowed_section));
}

#[test]
fn mixed_git_section_with_multiple_unified_headers_fails_closed() {
    let diff = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 11; }
--- a/src/secrets/Secret.java
+++ b/src/secrets/Secret.java
@@ -1 +1 @@
-old
+LEAK
"#;

    let filtered = filter_diff_for_excluded_paths_for_tests(diff, |_| false);
    assert_eq!(filtered, sentinel_line("\n"));
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
}

#[test]
fn git_section_uses_unified_headers_to_prevent_excluded_paths_bypass() {
    let excluded_path = "src/secrets/Secret.java";

    // Malformed section: the diff header claims this is for Ok.java, but the unified headers
    // identify a different file. The filter should still omit the section if the unified headers
    // match an excluded path.
    let diff = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/secrets/Secret.java
+++ b/src/secrets/Secret.java
@@ -1 +1 @@
-old
+MISMATCH_SECRET
"#;

    let filtered = filter_diff_for_excluded_paths_for_tests(diff, |path| {
        path == Path::new(excluded_path)
    });

    assert_eq!(filtered, sentinel_line("\n"));
    assert!(!filtered.contains("MISMATCH_SECRET"));
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
fn git_diff_token_parsing_supports_hex_escapes() {
    let excluded_path = "src/secrets/café.txt";

    // `\xC3\xA9` are the UTF-8 bytes for "é".
    let excluded_section = r#"diff --git "a/src/secrets/caf\xC3\xA9.txt" "b/src/secrets/caf\xC3\xA9.txt"
index 1111111..2222222 100644
--- "a/src/secrets/caf\xC3\xA9.txt"
+++ "b/src/secrets/caf\xC3\xA9.txt"
@@ -1 +1 @@
-old
+HEX_SECRET
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 9999999..aaaaaaa 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 15; }
"#;

    let diff = format!("{excluded_section}{allowed_section}");
    let filtered = filter_diff_for_excluded_paths_for_tests(&diff, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_section}", sentinel_line("\n"));
    assert_eq!(filtered, expected);
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
    assert!(!filtered.contains("HEX_SECRET"));
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

#[test]
fn malformed_quoted_diff_git_header_fails_closed_even_if_section_contains_paths() {
    // Malformed quoted header (unterminated quote) should fail closed even if the section contains
    // rename metadata that could otherwise provide paths.
    let diff = "diff --git \"a/src/Ok.java b/src/Ok.java\nrename from src/Ok.java\nrename to src/Ok.java\n";

    let filtered = filter_diff_for_excluded_paths_for_tests(diff, |_| false);
    assert_eq!(filtered, sentinel_line("\n"));
    assert_eq!(filtered.matches(OMITTED_SENTINEL).count(), 1);
}

#[test]
fn crlf_newlines_are_preserved_on_successful_filtering() {
    let excluded_path = "src/secrets/Secret.java";

    let excluded_section = r#"diff --git a/src/secrets/Secret.java b/src/secrets/Secret.java
index 0000000..1111111 100644
--- a/src/secrets/Secret.java
+++ b/src/secrets/Secret.java
@@ -1 +1 @@
-old
+CRLF_SECRET
"#;

    let allowed_section = r#"diff --git a/src/Ok.java b/src/Ok.java
index 2222222..3333333 100644
--- a/src/Ok.java
+++ b/src/Ok.java
@@ -1 +1 @@
-class Ok {}
+class Ok { int x = 10; }
"#;

    let diff_lf = format!("{excluded_section}{allowed_section}");
    let diff_crlf = diff_lf.replace('\n', "\r\n");
    let allowed_crlf = allowed_section.replace('\n', "\r\n");

    let filtered = filter_diff_for_excluded_paths_for_tests(&diff_crlf, |path| {
        path == Path::new(excluded_path)
    });

    let expected = format!("{}{allowed_crlf}", sentinel_line("\r\n"));
    assert_eq!(filtered, expected);
    assert!(!filtered.contains("CRLF_SECRET"));
    assert!(filtered.contains(&allowed_crlf));
}

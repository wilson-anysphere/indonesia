use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::{fs, io};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildFileFingerprint {
    pub digest: String,
}

impl BuildFileFingerprint {
    pub fn from_files(project_root: &Path, mut files: Vec<PathBuf>) -> io::Result<Self> {
        files.sort();
        files.dedup();

        let mut hasher = Sha256::new();
        for path in files {
            let rel = path.strip_prefix(project_root).unwrap_or(&path);
            hasher.update(rel.to_string_lossy().as_bytes());
            hasher.update([0]);

            let bytes = fs::read(&path)?;
            hasher.update(&bytes);
            hasher.update([0]);
        }

        Ok(Self {
            digest: hex::encode(hasher.finalize()),
        })
    }
}

pub fn collect_gradle_build_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_gradle_build_files_rec(root, root, &mut out)?;

    // Composite builds: best-effort include build scripts from `includeBuild(...)` roots.
    //
    // These can affect dependency resolution/classpaths, so they should contribute to Gradle
    // snapshot fingerprints (used by both `nova-build` and `nova-project`).
    for included_root in included_build_roots_from_settings(root)? {
        collect_gradle_build_files_rec(&included_root, &included_root, &mut out)?;
    }

    // Stable sort for hashing.
    out.sort_by(|a, b| {
        let ra = a.strip_prefix(root).unwrap_or(a);
        let rb = b.strip_prefix(root).unwrap_or(b);
        ra.cmp(rb)
    });
    out.dedup();
    Ok(out)
}

fn included_build_roots_from_settings(workspace_root: &Path) -> io::Result<Vec<PathBuf>> {
    let settings_path = ["settings.gradle.kts", "settings.gradle"]
        .into_iter()
        .map(|name| workspace_root.join(name))
        .find(|p| p.is_file());
    let Some(settings_path) = settings_path else {
        return Ok(Vec::new());
    };

    let contents = fs::read_to_string(&settings_path)?;
    let include_builds = parse_gradle_settings_included_builds(&contents);

    let mut roots: Vec<PathBuf> = include_builds
        .into_iter()
        .map(|dir_rel| workspace_root.join(dir_rel))
        .filter(|p| p.is_dir())
        .collect();
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn parse_gradle_settings_included_builds(contents: &str) -> Vec<String> {
    // Best-effort extraction of Gradle composite builds:
    // - Groovy: `includeBuild 'build-logic'`
    // - Groovy/Kotlin: `includeBuild("build-logic")`
    //
    // We intentionally only extract the first quoted string argument per call; `includeBuild`
    // accepts a single path argument.
    let contents = strip_gradle_comments(contents);

    let mut out: BTreeSet<String> = BTreeSet::new();

    for start in find_keyword_outside_strings(&contents, "includeBuild") {
        let mut idx = start + "includeBuild".len();
        let bytes = contents.as_bytes();
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() {
            continue;
        }

        // Support optional parens: `includeBuild("...")`.
        let has_parens = bytes[idx] == b'(';
        if has_parens {
            idx += 1;
        }

        // Scan forward until we find the first quote character.
        while idx < bytes.len() && !matches!(bytes[idx], b'\'' | b'"') {
            // Stop at EOL when there are no parens: `includeBuild '../dir'`.
            if !has_parens && bytes[idx] == b'\n' {
                break;
            }
            // Stop at the end of the call when we have parentheses but didn't find a string.
            if has_parens && bytes[idx] == b')' {
                break;
            }
            idx += 1;
        }
        if idx >= bytes.len() || !(bytes[idx] == b'\'' || bytes[idx] == b'"') {
            continue;
        }

        let quote = bytes[idx];
        idx += 1;
        let start_idx = idx;
        while idx < bytes.len() && bytes[idx] != quote {
            // Best-effort escape handling.
            if bytes[idx] == b'\\' {
                idx = (idx + 2).min(bytes.len());
                continue;
            }
            idx += 1;
        }
        if idx >= bytes.len() {
            continue;
        }

        let raw_dir = &contents[start_idx..idx];
        let raw_dir = raw_dir.trim();
        if raw_dir.is_empty() {
            continue;
        }

        // Keep behavior consistent with Nova's project discovery: ignore absolute paths.
        let Some(dir_rel) = normalize_dir_rel(raw_dir) else {
            continue;
        };
        out.insert(dir_rel);
    }

    out.into_iter().collect()
}

fn strip_gradle_comments(contents: &str) -> String {
    // Best-effort comment stripping to avoid parsing commented-out `includeBuild` lines.
    //
    // This is intentionally conservative and only strips:
    // - `// ...` to end-of-line
    // - `/* ... */` block comments
    // while preserving quoted strings (`'...'` / `"..."` / `'''...'''` / `"""..."""`).
    let bytes = contents.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());

    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_triple_single = false;
    let mut in_triple_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
                out.push(b'\n');
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if in_triple_single {
            out.push(b);
            if bytes[i..].starts_with(b"'''") {
                in_triple_single = false;
                out.extend_from_slice(b"''");
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }

        if in_triple_double {
            out.push(b);
            if bytes[i..].starts_with(b"\"\"\"") {
                in_triple_double = false;
                out.extend_from_slice(b"\"\"");
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }

        if in_single {
            out.push(b);
            if b == b'\\' {
                if let Some(next) = bytes.get(i + 1) {
                    out.push(*next);
                    i += 2;
                    continue;
                }
            } else if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        if in_double {
            out.push(b);
            if b == b'\\' {
                if let Some(next) = bytes.get(i + 1) {
                    out.push(*next);
                    i += 2;
                    continue;
                }
            } else if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            in_line_comment = true;
            i += 2;
            continue;
        }

        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            in_block_comment = true;
            i += 2;
            continue;
        }

        if bytes[i..].starts_with(b"'''") {
            in_triple_single = true;
            out.extend_from_slice(b"'''");
            i += 3;
            continue;
        }

        if bytes[i..].starts_with(b"\"\"\"") {
            in_triple_double = true;
            out.extend_from_slice(b"\"\"\"");
            i += 3;
            continue;
        }

        if b == b'\'' {
            in_single = true;
            out.push(b'\'');
            i += 1;
            continue;
        }

        if b == b'"' {
            in_double = true;
            out.push(b'"');
            i += 1;
            continue;
        }

        out.push(b);
        i += 1;
    }

    String::from_utf8(out).unwrap_or_else(|_| contents.to_string())
}

fn is_word_byte(b: u8) -> bool {
    // Keep semantics aligned with Regex `\b` for ASCII: alphanumeric + underscore.
    b.is_ascii_alphanumeric() || b == b'_'
}

fn find_keyword_outside_strings(contents: &str, keyword: &str) -> Vec<usize> {
    let bytes = contents.as_bytes();
    let kw = keyword.as_bytes();
    if kw.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();

    let mut in_single = false;
    let mut in_double = false;
    let mut in_triple_single = false;
    let mut in_triple_double = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];

        if in_triple_single {
            if bytes[i..].starts_with(b"'''") {
                in_triple_single = false;
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }

        if in_triple_double {
            if bytes[i..].starts_with(b"\"\"\"") {
                in_triple_double = false;
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }

        if in_single {
            if b == b'\\' {
                i = (i + 2).min(bytes.len());
                continue;
            }
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        if in_double {
            if b == b'\\' {
                i = (i + 2).min(bytes.len());
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        if bytes[i..].starts_with(b"'''") {
            in_triple_single = true;
            i += 3;
            continue;
        }

        if bytes[i..].starts_with(b"\"\"\"") {
            in_triple_double = true;
            i += 3;
            continue;
        }

        if b == b'\'' {
            in_single = true;
            i += 1;
            continue;
        }

        if b == b'"' {
            in_double = true;
            i += 1;
            continue;
        }

        if bytes[i..].starts_with(kw) {
            let prev_is_word = i
                .checked_sub(1)
                .and_then(|idx| bytes.get(idx))
                .is_some_and(|b| is_word_byte(*b));
            let next_is_word = bytes.get(i + kw.len()).is_some_and(|b| is_word_byte(*b));
            if !prev_is_word && !next_is_word {
                out.push(i);
                i += kw.len();
                continue;
            }
        }

        i += 1;
    }

    out
}

fn normalize_dir_rel(dir_rel: &str) -> Option<String> {
    let mut dir_rel = dir_rel.trim().replace('\\', "/");
    while let Some(stripped) = dir_rel.strip_prefix("./") {
        dir_rel = stripped.to_string();
    }
    while dir_rel.ends_with('/') {
        dir_rel.pop();
    }

    if dir_rel.is_empty() {
        return Some(".".to_string());
    }

    // Avoid accidentally escaping the workspace root by joining with an absolute path.
    let is_absolute_unix = dir_rel.starts_with('/');
    let is_windows_drive = dir_rel.as_bytes().get(1).is_some_and(|b| *b == b':')
        && dir_rel
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic());
    if is_absolute_unix || is_windows_drive {
        return None;
    }

    Some(dir_rel)
}

fn collect_gradle_build_files_rec(
    root: &Path,
    dir: &Path,
    out: &mut Vec<PathBuf>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        if path.is_dir() {
            // Avoid scanning huge non-source directories that commonly show up in mono-repos.
            // These trees can contain many files that look like build files but should not
            // influence Nova's build fingerprint (e.g. vendored JS dependencies).
            if file_name == "node_modules" {
                continue;
            }
            // Bazel output trees are typically created at the workspace root and can be enormous.
            // Skip any `bazel-*` entries (`bazel-out`, `bazel-bin`, `bazel-testlogs`,
            // `bazel-<workspace>`, etc).
            if file_name.starts_with("bazel-") {
                continue;
            }
            if file_name == ".git"
                || file_name == ".gradle"
                || file_name == "build"
                || file_name == "target"
                || file_name == ".nova"
                || file_name == ".idea"
            {
                continue;
            }
            collect_gradle_build_files_rec(root, &path, out)?;
            continue;
        }

        let name = file_name.as_ref();

        // Gradle dependency locking can change resolved classpaths without modifying any build
        // scripts, so include lockfiles in the fingerprint.
        //
        // Patterns:
        // - `gradle.lockfile` at any depth.
        // - `*.lockfile` under any `dependency-locks/` directory (covers Gradle's default
        //   `gradle/dependency-locks/` location).
        if name == "gradle.lockfile" {
            out.push(path);
            continue;
        }
        if name.ends_with(".lockfile")
            && path.ancestors().any(|dir| {
                dir.file_name()
                    .is_some_and(|name| name == "dependency-locks")
            })
        {
            out.push(path);
            continue;
        }

        // Match `nova-build` build-file watcher semantics by including any
        // `build.gradle*` / `settings.gradle*` variants.
        if name.starts_with("build.gradle") || name.starts_with("settings.gradle") {
            out.push(path);
            continue;
        }

        // Applied Gradle script plugins can influence dependencies and tasks
        // without being named `build.gradle*` / `settings.gradle*`.
        if name.ends_with(".gradle") || name.ends_with(".gradle.kts") {
            out.push(path);
            continue;
        }

        // Gradle version catalogs can define dependency versions and thus affect resolved
        // classpaths. In addition to the default `gradle/libs.versions.toml`, Gradle supports
        // custom catalogs referenced from `settings.gradle*` (e.g. `gradle/foo.versions.toml`).
        //
        // Only include catalogs that are direct children of a directory named `gradle` to avoid
        // accidentally picking up unrelated `*.toml` files elsewhere in the repo (including under
        // `node_modules/`).
        if name.ends_with(".versions.toml")
            && path
                .parent()
                .and_then(|parent| parent.file_name())
                .is_some_and(|dir| dir == "gradle")
        {
            out.push(path);
            continue;
        }

        match name {
            "gradle.properties" => out.push(path),
            "libs.versions.toml" => out.push(path),
            "gradlew" | "gradlew.bat" => {
                if path == root.join(name) {
                    out.push(path);
                }
            }
            "gradle-wrapper.properties" => {
                if path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.properties")) {
                    out.push(path);
                }
            }
            "gradle-wrapper.jar" => {
                if path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.jar")) {
                    out.push(path);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn collect_gradle_build_files_filters_ignored_dirs_and_matches_expected_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Included files.
        write_file(&root.join("build.gradle"), b"plugins {}");
        write_file(
            &root.join("settings.gradle.kts"),
            b"rootProject.name = \"demo\"",
        );
        write_file(&root.join("gradle.properties"), b"org.gradle.daemon=true");
        write_file(&root.join("gradle.lockfile"), b"# lockfile");
        write_file(
            &root.join("libs.versions.toml"),
            b"[versions]\nroot = \"1\"",
        );
        write_file(&root.join("random.lockfile"), b"should-not-be-included");

        write_file(&root.join("gradlew"), b"#!/bin/sh\necho gradlew");
        write_file(&root.join("gradlew.bat"), b"@echo off\r\necho gradlew");

        write_file(
            &root.join("gradle/libs.versions.toml"),
            b"[versions]\nfoo = \"1.0\"",
        );
        write_file(
            &root.join("gradle/custom.versions.toml"),
            b"[versions]\nbar = \"2.0\"",
        );
        write_file(
            &root.join("gradle/wrapper/gradle-wrapper.jar"),
            b"jar-bytes",
        );
        write_file(
            &root.join("gradle/wrapper/gradle-wrapper.properties"),
            b"distributionUrl=https\\://services.gradle.org/distributions/gradle-8.0-bin.zip",
        );

        write_file(&root.join("scripts/plugin.gradle"), b"println(\"hi\")");
        write_file(&root.join("sub/module.gradle.kts"), b"// module script");
        write_file(&root.join("sub/gradle.lockfile"), b"# sub lockfile");
        write_file(
            &root.join("gradle/dependency-locks/compileClasspath.lockfile"),
            b"# deps lockfile",
        );
        write_file(
            &root.join("other/dependency-locks/custom.lockfile"),
            b"# deps lockfile 2",
        );

        // Should *not* be included: non-gradle version catalogs outside `gradle/`.
        write_file(
            &root.join("config/other.versions.toml"),
            b"[versions]\nnope = \"1\"",
        );

        // Ignored directories.
        write_file(
            &root.join("node_modules/build.gradle"),
            b"should-be-ignored",
        );
        write_file(&root.join("build/settings.gradle"), b"should-be-ignored");
        write_file(&root.join("target/foo.gradle"), b"should-be-ignored");
        write_file(&root.join(".git/build.gradle"), b"should-be-ignored");
        write_file(&root.join(".gradle/build.gradle"), b"should-be-ignored");
        write_file(&root.join(".nova/build.gradle"), b"should-be-ignored");
        write_file(&root.join(".idea/build.gradle"), b"should-be-ignored");
        write_file(&root.join("bazel-out/build.gradle"), b"should-be-ignored");
        // Ignore Bazel output trees anywhere, not just at the root.
        write_file(
            &root.join("nested/bazel-out/build.gradle"),
            b"should-be-ignored",
        );
        write_file(
            &root.join("nested/bazel-myworkspace/build.gradle"),
            b"should-be-ignored",
        );

        let files = collect_gradle_build_files(root).unwrap();
        let rel: BTreeSet<PathBuf> = files
            .into_iter()
            .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
            .collect();

        let expected: BTreeSet<PathBuf> = [
            PathBuf::from("build.gradle"),
            PathBuf::from("gradle.lockfile"),
            PathBuf::from("gradle/custom.versions.toml"),
            PathBuf::from("gradle/dependency-locks/compileClasspath.lockfile"),
            PathBuf::from("gradle/libs.versions.toml"),
            PathBuf::from("gradle/wrapper/gradle-wrapper.jar"),
            PathBuf::from("gradle/wrapper/gradle-wrapper.properties"),
            PathBuf::from("gradle.properties"),
            PathBuf::from("gradlew"),
            PathBuf::from("gradlew.bat"),
            PathBuf::from("libs.versions.toml"),
            PathBuf::from("other/dependency-locks/custom.lockfile"),
            PathBuf::from("scripts/plugin.gradle"),
            PathBuf::from("settings.gradle.kts"),
            PathBuf::from("sub/gradle.lockfile"),
            PathBuf::from("sub/module.gradle.kts"),
        ]
        .into_iter()
        .collect();

        assert_eq!(rel, expected);
    }

    #[test]
    fn fingerprint_is_deterministic_independent_of_input_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let a = root.join("build.gradle");
        let b = root.join("settings.gradle");
        write_file(&a, b"a");
        write_file(&b, b"b");

        let first = BuildFileFingerprint::from_files(root, vec![a.clone(), b.clone()]).unwrap();
        let second = BuildFileFingerprint::from_files(root, vec![b, a]).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn collect_gradle_build_files_includes_build_files_from_include_builds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let included = dir.path().join("included");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&included).unwrap();

        write_file(
            &root.join("settings.gradle"),
            b"includeBuild(\n    \"../included\"\n)\n",
        );
        write_file(&included.join("build.gradle"), b"plugins { id 'java' }\n");

        let files = collect_gradle_build_files(&root).unwrap();
        let expected_included_build_gradle = root.join("../included/build.gradle");
        assert!(
            files.contains(&expected_included_build_gradle),
            "expected included build.gradle to be included in build file collection; got: {files:?}"
        );
    }
}

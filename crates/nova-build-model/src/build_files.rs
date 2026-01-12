use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
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

        // Best-effort canonicalization to make the fingerprint resilient to callers mixing
        // canonical/non-canonical paths (e.g. macOS `/var` vs `/private/var`, or symlinked
        // workspace roots).
        //
        // We intentionally treat canonicalization failures as "no canonical root" to avoid
        // introducing new IO errors in edge cases (if callers are hashing files outside
        // `project_root`, for example).
        let canonical_root = project_root.canonicalize().ok();

        let mut hasher = Sha256::new();
        for path in files {
            if let Ok(rel) = path.strip_prefix(project_root) {
                // `collect_gradle_build_files` can yield paths like `<root>/../included/...` for
                // Gradle composite builds (`includeBuild("../included")`).
                //
                // These paths lexically start with `project_root`, but may not be *within* the
                // canonical workspace root. Hashing `../included/...` directly would make
                // fingerprints unstable when callers use symlinked or otherwise non-canonical
                // workspace roots.
                let rel_has_dot_segments = rel.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::CurDir | std::path::Component::ParentDir
                    )
                });
                if !rel_has_dot_segments {
                    hasher.update(rel.to_string_lossy().as_bytes());
                } else if let Some(canonical_root) = canonical_root.as_ref() {
                    if let Ok(canonical_path) = path.canonicalize() {
                        if let Ok(rel) = canonical_path.strip_prefix(canonical_root) {
                            hasher.update(rel.to_string_lossy().as_bytes());
                        } else {
                            hasher.update(canonical_path.to_string_lossy().as_bytes());
                        }
                    } else {
                        hasher.update(rel.to_string_lossy().as_bytes());
                    }
                } else {
                    hasher.update(rel.to_string_lossy().as_bytes());
                }
            } else if let Some(canonical_root) = canonical_root.as_ref() {
                if let Ok(canonical_path) = path.canonicalize() {
                    if let Ok(rel) = canonical_path.strip_prefix(canonical_root) {
                        hasher.update(rel.to_string_lossy().as_bytes());
                    } else {
                        // For paths outside the (canonical) project root, use the canonical file
                        // path for fingerprinting.
                        hasher.update(canonical_path.to_string_lossy().as_bytes());
                    }
                } else {
                    hasher.update(path.to_string_lossy().as_bytes());
                }
            } else {
                hasher.update(path.to_string_lossy().as_bytes());
            }
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
    // Composite builds: best-effort include build scripts from `includeBuild(...)` roots.
    //
    // Composite builds can be nested (an included build can itself include other builds). Walk the
    // include graph recursively and include build scripts from each discovered build root.
    //
    // These can affect dependency resolution/classpaths, so they should contribute to Gradle
    // snapshot fingerprints (used by both `nova-build` and `nova-project`).
    let mut visited_build_roots: BTreeSet<PathBuf> = BTreeSet::new();
    let mut pending_build_roots = std::collections::VecDeque::new();
    pending_build_roots.push_back(root.to_path_buf());
    visited_build_roots.insert(root.canonicalize().unwrap_or_else(|_| root.to_path_buf()));

    while let Some(build_root) = pending_build_roots.pop_front() {
        collect_gradle_build_files_rec(&build_root, &build_root, &mut out)?;

        // Some Gradle settings constructs (e.g. `includeFlat`, `projectDir = file("../...")`) can
        // introduce module roots outside the workspace root or point at directory symlinks.
        //
        // These directories are not covered by the recursive scan:
        // - outside the root: we don't walk upward into `..`
        // - directory symlinks: the walker skips them to avoid cycles
        //
        // Best-effort include build scripts from those declared project directories so Gradle
        // fingerprints change when external/symlinked module build files change.
        for project_root in project_roots_from_settings(&build_root)? {
            collect_gradle_build_files_rec(&build_root, &project_root, &mut out)?;
        }

        for included_root in included_build_roots_from_settings(&build_root)? {
            let key = included_root
                .canonicalize()
                .unwrap_or_else(|_| included_root.clone());
            if visited_build_roots.insert(key) {
                pending_build_roots.push_back(included_root);
            }
        }
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

pub fn is_gradle_marker_root(root: &Path) -> bool {
    root.join("settings.gradle").is_file()
        || root.join("settings.gradle.kts").is_file()
        || root.join("build.gradle").is_file()
        || root.join("build.gradle.kts").is_file()
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

    // Best-effort: canonicalize roots for dedup so equivalent paths like `../included` and
    // `../included/.` don't cause the included build to be scanned multiple times.
    let mut seen_roots: BTreeSet<PathBuf> = BTreeSet::new();
    let mut roots: Vec<PathBuf> = Vec::new();
    for dir_rel in include_builds {
        let root = workspace_root.join(dir_rel);
        if !root.is_dir() || !is_gradle_marker_root(&root) {
            continue;
        }
        let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
        if !seen_roots.insert(canonical) {
            continue;
        }
        roots.push(root);
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn project_roots_from_settings(workspace_root: &Path) -> io::Result<Vec<PathBuf>> {
    let settings_path = ["settings.gradle.kts", "settings.gradle"]
        .into_iter()
        .map(|name| workspace_root.join(name))
        .find(|p| p.is_file());
    let Some(settings_path) = settings_path else {
        return Ok(Vec::new());
    };

    let contents = fs::read_to_string(&settings_path)?;
    let project_dirs = parse_gradle_settings_project_dirs(&contents);

    let mut roots = Vec::new();
    for dir_rel in project_dirs {
        if dir_rel == "." {
            continue;
        }

        let candidate = workspace_root.join(&dir_rel);
        // Gradle subprojects can inherit all configuration from the root build script (via
        // `allprojects {}` / `subprojects {}`), in which case they may not have a `build.gradle*`
        // file at all. Still consider `gradle.properties` as a marker so changes to project-level
        // properties contribute to fingerprints.
        if !candidate.is_dir()
            || !(is_gradle_marker_root(&candidate) || candidate.join("gradle.properties").is_file())
        {
            continue;
        }

        let has_parent_dir = Path::new(&dir_rel)
            .components()
            .any(|c| c == std::path::Component::ParentDir);

        // Directory symlinks are skipped by the main recursive scan to avoid cycles. If any
        // component of this project directory path is a symlink, we need to explicitly scan it.
        let has_symlink_component =
            !has_parent_dir && dir_rel_has_symlink_component(workspace_root, &dir_rel);

        if has_parent_dir || has_symlink_component {
            roots.push(candidate);
        }
    }

    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn dir_rel_has_symlink_component(workspace_root: &Path, dir_rel: &str) -> bool {
    let mut cursor = workspace_root.to_path_buf();
    for component in Path::new(dir_rel).components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        cursor.push(name);
        if fs::symlink_metadata(&cursor)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn parse_gradle_settings_project_dirs(contents: &str) -> Vec<String> {
    let contents = strip_gradle_comments(contents);

    let mut projects = parse_gradle_settings_included_projects(&contents);
    let include_flat_dirs = parse_gradle_settings_include_flat_project_dirs(&contents);
    projects.extend(include_flat_dirs.keys().cloned());
    if projects.is_empty() {
        return Vec::new();
    }

    let overrides = parse_gradle_settings_project_dir_overrides(&contents);

    let projects: BTreeSet<_> = projects.into_iter().collect();
    let mut out = BTreeSet::new();
    for project_path in projects {
        if project_path == ":" {
            continue;
        }

        let dir_rel = overrides
            .get(&project_path)
            .cloned()
            .or_else(|| include_flat_dirs.get(&project_path).cloned())
            .unwrap_or_else(|| heuristic_dir_rel_for_project_path(&project_path));
        let Some(dir_rel) = normalize_dir_rel(&dir_rel) else {
            continue;
        };
        if dir_rel == "." {
            continue;
        }
        out.insert(dir_rel);
    }

    out.into_iter().collect()
}

fn parse_gradle_settings_included_projects(contents: &str) -> Vec<String> {
    let mut projects = Vec::new();
    for start in find_keyword_outside_strings(contents, "include") {
        let mut idx = start + "include".len();
        let bytes = contents.as_bytes();
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() {
            continue;
        }

        let args = if bytes[idx] == b'(' {
            extract_balanced_parens(contents, idx)
                .map(|(args, _end)| args)
                .unwrap_or_default()
        } else {
            extract_unparenthesized_args_until_eol_or_continuation(contents, idx)
        };

        projects.extend(
            extract_quoted_strings(&args)
                .into_iter()
                .map(|s| normalize_project_path(&s)),
        );
    }

    projects
}

fn parse_gradle_settings_include_flat_project_dirs(contents: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();

    for start in find_keyword_outside_strings(contents, "includeFlat") {
        let mut idx = start + "includeFlat".len();
        let bytes = contents.as_bytes();
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() {
            continue;
        }

        let args = if bytes[idx] == b'(' {
            extract_balanced_parens(contents, idx)
                .map(|(args, _end)| args)
                .unwrap_or_default()
        } else {
            extract_unparenthesized_args_until_eol_or_continuation(contents, idx)
        };

        for raw in extract_quoted_strings(&args) {
            let project_path = normalize_project_path(&raw);
            let name = raw.trim().trim_start_matches(':').replace([':', '\\'], "/");
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let dir_rel = format!("../{name}");
            let Some(dir_rel) = normalize_dir_rel(&dir_rel) else {
                continue;
            };
            out.insert(project_path, dir_rel);
        }
    }

    out
}

fn parse_gradle_settings_project_dir_overrides(contents: &str) -> BTreeMap<String, String> {
    // Common overrides:
    //   project(':app').projectDir = file('modules/app')
    //   project(':lib').projectDir = new File(settingsDir, 'modules/lib')
    //   project(":app").projectDir = file("modules/app") (Kotlin DSL)
    let mut overrides = BTreeMap::new();
    let bytes = contents.as_bytes();

    for start in find_keyword_outside_strings(contents, "project") {
        let mut idx = start + "project".len();
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if bytes.get(idx) != Some(&b'(') {
            continue;
        }

        let Some((project_args, after_project_parens)) = extract_balanced_parens(contents, idx)
        else {
            continue;
        };
        let Some(project_path) = extract_quoted_strings(&project_args).into_iter().next() else {
            continue;
        };
        let project_path = normalize_project_path(&project_path);

        // Parse:
        //   project(...).projectDir = ...
        //              ^^^^^^^^^^
        let mut cursor = after_project_parens;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'.') {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if !bytes[cursor..].starts_with(b"projectDir") {
            continue;
        }
        cursor += "projectDir".len();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }

        // Parse RHS:
        // - file("modules/app")
        // - new File(settingsDir, "modules/app")
        // - java.io.File(settingsDir, "modules/app")
        let dir = if bytes
            .get(cursor..)
            .is_some_and(|rest| rest.starts_with(b"file"))
        {
            cursor += "file".len();
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if bytes.get(cursor) != Some(&b'(') {
                continue;
            }
            let Some((args, _end)) = extract_balanced_parens(contents, cursor) else {
                continue;
            };
            extract_quoted_strings(&args).into_iter().next()
        } else {
            // Optional `new`.
            if bytes
                .get(cursor..)
                .is_some_and(|rest| rest.starts_with(b"new"))
            {
                cursor += "new".len();
                while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
            }

            // Optional `java.io.` prefix.
            if bytes
                .get(cursor..)
                .is_some_and(|rest| rest.starts_with(b"java.io."))
            {
                cursor += "java.io.".len();
            }

            if !bytes
                .get(cursor..)
                .is_some_and(|rest| rest.starts_with(b"File"))
            {
                continue;
            }
            cursor += "File".len();
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if bytes.get(cursor) != Some(&b'(') {
                continue;
            }
            let Some((args, _end)) = extract_balanced_parens(contents, cursor) else {
                continue;
            };

            // Best-effort: accept `File(settingsDir, "...")` and `File(rootDir, "...")`.
            let args_trim = args.trim_start();
            if !(args_trim.starts_with("settingsDir") || args_trim.starts_with("rootDir")) {
                continue;
            }
            extract_quoted_strings(&args).into_iter().next()
        };

        let Some(dir) = dir.as_deref().map(str::trim).filter(|d| !d.is_empty()) else {
            continue;
        };
        let Some(dir_rel) = normalize_dir_rel(dir) else {
            continue;
        };
        overrides.insert(project_path, dir_rel);
    }
    overrides
}

pub fn parse_gradle_settings_included_builds(contents: &str) -> Vec<String> {
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
        // Support Gradle/Kotlin/Groovy raw strings: `'''...'''` / `"""..."""`.
        let raw_dir = if quote == b'\'' && bytes[idx..].starts_with(b"'''") {
            idx += 3;
            let start_idx = idx;
            while idx + 2 < bytes.len() && !bytes[idx..].starts_with(b"'''") {
                idx += 1;
            }
            if idx + 2 >= bytes.len() {
                continue;
            }
            &contents[start_idx..idx]
        } else if quote == b'"' && bytes[idx..].starts_with(b"\"\"\"") {
            idx += 3;
            let start_idx = idx;
            while idx + 2 < bytes.len() && !bytes[idx..].starts_with(b"\"\"\"") {
                idx += 1;
            }
            if idx + 2 >= bytes.len() {
                continue;
            }
            &contents[start_idx..idx]
        } else {
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
            &contents[start_idx..idx]
        };
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

fn normalize_project_path(project_path: &str) -> String {
    let project_path = project_path.trim();
    if project_path.is_empty() || project_path == ":" {
        return ":".to_string();
    }
    if project_path.starts_with(':') {
        project_path.to_string()
    } else {
        format!(":{project_path}")
    }
}

fn heuristic_dir_rel_for_project_path(project_path: &str) -> String {
    let dir_rel = project_path.trim_start_matches(':').replace(':', "/");
    if dir_rel.trim().is_empty() {
        ".".to_string()
    } else {
        dir_rel
    }
}

fn extract_quoted_strings(text: &str) -> Vec<String> {
    // Best-effort string literal extraction for Gradle settings parsing.
    //
    // Supports:
    // - `'...'`
    // - `"..."` (with backslash escapes)
    // - `'''...'''` / `"""..."""` (Groovy / Kotlin raw strings)
    //
    // Note: we intentionally do not unescape contents; callers normalize/trim as needed.
    let bytes = text.as_bytes();
    let mut out = Vec::new();

    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"'''") {
            let start = i + 3;
            i = start;
            while i < bytes.len() && !bytes[i..].starts_with(b"'''") {
                i += 1;
            }
            if i < bytes.len() {
                if start < i {
                    out.push(text[start..i].to_string());
                }
                i += 3;
            }
            continue;
        }

        if bytes[i..].starts_with(b"\"\"\"") {
            let start = i + 3;
            i = start;
            while i < bytes.len() && !bytes[i..].starts_with(b"\"\"\"") {
                i += 1;
            }
            if i < bytes.len() {
                if start < i {
                    out.push(text[start..i].to_string());
                }
                i += 3;
            }
            continue;
        }

        if bytes[i] == b'\'' {
            let start = i + 1;
            i = start;
            while i < bytes.len() {
                let b = bytes[i];
                if b == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if b == b'\'' {
                    if start < i {
                        out.push(text[start..i].to_string());
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        if bytes[i] == b'"' {
            let start = i + 1;
            i = start;
            while i < bytes.len() {
                let b = bytes[i];
                if b == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if b == b'"' {
                    if start < i {
                        out.push(text[start..i].to_string());
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        i += 1;
    }

    out
}

fn extract_unparenthesized_args_until_eol_or_continuation(contents: &str, start: usize) -> String {
    // Groovy allows method calls without parentheses:
    //   include ':app', ':lib'
    // and can span lines after commas:
    //   include ':app',
    //           ':lib'
    let len = contents.len();
    let mut cursor = start;

    loop {
        let rest = &contents[cursor..];
        let line_break = rest.find('\n').map(|off| cursor + off).unwrap_or(len);
        let line = &contents[cursor..line_break];
        if line.trim_end().ends_with(',') && line_break < len {
            cursor = line_break + 1;
            continue;
        }
        return contents[start..line_break].to_string();
    }
}

fn extract_balanced_parens(contents: &str, open_paren_index: usize) -> Option<(String, usize)> {
    let bytes = contents.as_bytes();
    if bytes.get(open_paren_index) != Some(&b'(') {
        return None;
    }

    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_triple_single = false;
    let mut in_triple_double = false;

    let mut i = open_paren_index;
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
                i += 2;
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
                i += 2;
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

        match b {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
                if depth == 0 {
                    let args = &contents[open_paren_index + 1..i - 1];
                    return Some((args.to_string(), i));
                }
            }
            _ => i += 1,
        }
    }

    None
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
            // Avoid following directory symlinks: a workspace can contain symlink cycles (e.g. a
            // symlink pointing to an ancestor directory), which would otherwise lead to infinite
            // recursion and potentially scanning outside the workspace root.
            if fs::symlink_metadata(&path)?.file_type().is_symlink() {
                continue;
            }
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
    #[cfg(unix)]
    fn collect_gradle_build_files_does_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_file(&root.join("build.gradle"), b"plugins {}");
        // Create a symlink cycle (`loop/` points back to the workspace root). The file walker
        // should not recurse into the symlink directory.
        symlink(root, root.join("loop")).unwrap();

        let files = collect_gradle_build_files(root).unwrap();
        let rel: BTreeSet<PathBuf> = files
            .into_iter()
            .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
            .collect();

        let expected: BTreeSet<PathBuf> = [PathBuf::from("build.gradle")].into_iter().collect();
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

    #[test]
    fn collect_gradle_build_files_includes_build_files_from_nested_include_builds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let included1 = dir.path().join("included1");
        let included2 = dir.path().join("included2");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&included1).unwrap();
        std::fs::create_dir_all(&included2).unwrap();

        write_file(
            &root.join("settings.gradle"),
            b"includeBuild(\"../included1\")\n",
        );
        write_file(
            &included1.join("settings.gradle"),
            b"includeBuild(\"../included2\")\n",
        );
        write_file(&included2.join("build.gradle"), b"plugins { id 'java' }\n");

        let files = collect_gradle_build_files(&root).unwrap();
        let expected = root.join("../included1/../included2/build.gradle");
        assert!(
            files.contains(&expected),
            "expected nested included build.gradle to be included in build file collection; got: {files:?}"
        );
    }

    #[test]
    fn collect_gradle_build_files_dedups_equivalent_include_build_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let included = dir.path().join("included");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&included).unwrap();

        // `../included` and `../included/.` resolve to the same directory. Ensure we only scan the
        // included build once so its build files do not appear multiple times in the fingerprint.
        write_file(
            &root.join("settings.gradle"),
            b"includeBuild(\"../included\")\nincludeBuild(\"../included/.\")\n",
        );
        write_file(&included.join("build.gradle"), b"plugins { id 'java' }\n");

        let files = collect_gradle_build_files(&root).unwrap();
        let canonical_included_build_gradle =
            std::fs::canonicalize(included.join("build.gradle")).unwrap();
        let included_matches = files
            .iter()
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .filter(|path| path == &canonical_included_build_gradle)
            .count();
        assert_eq!(
            included_matches, 1,
            "expected included build.gradle to appear once; got: {files:?}"
        );
    }

    #[test]
    fn collect_gradle_build_files_skips_include_build_roots_without_gradle_markers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let included = dir.path().join("included");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(included.join("scripts")).unwrap();

        write_file(
            &root.join("settings.gradle"),
            b"includeBuild(\"../included\")\n",
        );

        // Included build dir exists, but is missing `settings.gradle(.kts)` and `build.gradle(.kts)`.
        // Ensure we don't fingerprint arbitrary `.gradle` files under it.
        write_file(
            &included.join("scripts/plugin.gradle"),
            b"// script plugin\n",
        );

        let files = collect_gradle_build_files(&root).unwrap();
        let unexpected = root.join("../included/scripts/plugin.gradle");
        assert!(
            !files.contains(&unexpected),
            "did not expect non-build includeBuild root to contribute files; got: {files:?}"
        );
    }

    #[test]
    fn collect_gradle_build_files_parses_include_build_with_file_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let included = dir.path().join("included");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&included).unwrap();

        // Kotlin DSL often wraps paths with `file(...)`.
        write_file(
            &root.join("settings.gradle.kts"),
            b"includeBuild(file(\"../included\"))\n",
        );
        write_file(&included.join("build.gradle"), b"plugins { id 'java' }\n");

        let files = collect_gradle_build_files(&root).unwrap();
        let expected = root.join("../included/build.gradle");
        assert!(
            files.contains(&expected),
            "expected included build.gradle to be included in build file collection; got: {files:?}"
        );
    }

    #[test]
    fn collect_gradle_build_files_parses_include_build_with_triple_double_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let included = dir.path().join("included");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&included).unwrap();

        // Kotlin raw string literal (settings.gradle.kts).
        write_file(
            &root.join("settings.gradle.kts"),
            b"includeBuild(\"\"\"../included\"\"\")\n",
        );
        write_file(&included.join("build.gradle"), b"plugins { id 'java' }\n");

        let files = collect_gradle_build_files(&root).unwrap();
        let expected = root.join("../included/build.gradle");
        assert!(
            files.contains(&expected),
            "expected included build.gradle to be included in build file collection; got: {files:?}"
        );
    }

    #[test]
    fn collect_gradle_build_files_parses_include_build_with_triple_single_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let included = dir.path().join("included");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&included).unwrap();

        // Groovy raw string literal (settings.gradle).
        write_file(
            &root.join("settings.gradle"),
            b"includeBuild '''../included'''\n",
        );
        write_file(&included.join("build.gradle"), b"plugins { id 'java' }\n");

        let files = collect_gradle_build_files(&root).unwrap();
        let expected = root.join("../included/build.gradle");
        assert!(
            files.contains(&expected),
            "expected included build.gradle to be included in build file collection; got: {files:?}"
        );
    }

    #[test]
    fn collect_gradle_build_files_includes_build_files_from_include_flat_projects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let external = dir.path().join("external");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        write_file(&root.join("settings.gradle"), b"includeFlat 'external'\n");
        write_file(&external.join("build.gradle"), b"plugins { id 'java' }\n");

        let files = collect_gradle_build_files(&root).unwrap();
        let expected = root.join("../external/build.gradle");
        assert!(
            files.contains(&expected),
            "expected external includeFlat build.gradle to be included in build file collection; got: {files:?}"
        );
    }

    #[test]
    fn collect_gradle_build_files_includes_gradle_properties_from_include_flat_projects_without_build_scripts(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let external = dir.path().join("external");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        write_file(&root.join("settings.gradle"), b"includeFlat 'external'\n");
        write_file(
            &external.join("gradle.properties"),
            b"org.gradle.jvmargs=-Xmx1g\n",
        );

        let files = collect_gradle_build_files(&root).unwrap();
        let expected = root.join("../external/gradle.properties");
        assert!(
            files.contains(&expected),
            "expected external gradle.properties to be included in build file collection; got: {files:?}"
        );
    }

    #[test]
    fn collect_gradle_build_files_includes_build_files_from_project_dir_overrides_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let external = dir.path().join("external");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        write_file(
            &root.join("settings.gradle"),
            b"include ':external'\nproject(':external').projectDir = file('../external')\n",
        );
        write_file(&external.join("build.gradle.kts"), b"plugins { java }\n");

        let files = collect_gradle_build_files(&root).unwrap();
        let expected = root.join("../external/build.gradle.kts");
        assert!(
            files.contains(&expected),
            "expected external projectDir build.gradle.kts to be included in build file collection; got: {files:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn collect_gradle_build_files_includes_build_files_from_symlinked_project_dirs() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let real_module = dir.path().join("real-module");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&real_module).unwrap();

        write_file(&root.join("settings.gradle"), b"include ':module'\n");
        write_file(
            &real_module.join("build.gradle"),
            b"plugins { id 'java' }\n",
        );

        // Gradle multi-project builds sometimes symlink module directories; the build file
        // collector should not miss build scripts for such modules.
        symlink(&real_module, root.join("module")).unwrap();

        let files = collect_gradle_build_files(&root).unwrap();
        let expected = root.join("module/build.gradle");
        assert!(
            files.contains(&expected),
            "expected symlinked project build.gradle to be included in build file collection; got: {files:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn collect_gradle_build_files_includes_build_files_from_projects_under_symlinked_ancestors() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let real_sub = dir.path().join("real-sub");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(real_sub.join("module")).unwrap();

        write_file(&root.join("settings.gradle"), b"include ':sub:module'\n");
        write_file(
            &real_sub.join("module/build.gradle"),
            b"plugins { id 'java' }\n",
        );

        // The `sub/` directory itself is a symlink, but the Gradle project root lives under it.
        // The build file collector should still pick up build scripts for the declared project.
        symlink(&real_sub, root.join("sub")).unwrap();

        let files = collect_gradle_build_files(&root).unwrap();
        let expected = root.join("sub").join("module").join("build.gradle");
        assert!(
            files.contains(&expected),
            "expected build.gradle under symlinked ancestor dir to be included in build file collection; got: {files:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fingerprint_is_stable_when_project_root_is_a_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real");
        std::fs::create_dir_all(&real_root).unwrap();
        let file = real_root.join("build.gradle");
        write_file(&file, b"plugins {}");

        let link_root = dir.path().join("link");
        symlink(&real_root, &link_root).unwrap();

        // Simulate callers passing a symlinked `project_root` but a "real" file path (common when
        // some codepaths canonicalize paths and others don't).
        let fp_real =
            BuildFileFingerprint::from_files(&link_root, vec![file]).expect("fingerprint");
        let fp_link =
            BuildFileFingerprint::from_files(&link_root, vec![link_root.join("build.gradle")])
                .expect("fingerprint");

        assert_eq!(fp_real, fp_link);
    }

    #[test]
    fn parse_gradle_settings_included_builds_ignores_commented_out_calls() {
        let contents = r#"
            // includeBuild("../ignored-line")
            includeBuild("../included")
            /* includeBuild("../ignored-block") */
        "#;

        assert_eq!(
            parse_gradle_settings_included_builds(contents),
            vec!["../included".to_string()]
        );
    }

    #[test]
    fn parse_gradle_settings_included_builds_ignores_keywords_inside_strings() {
        let contents = r#"
            println("includeBuild('../ignored')")
            includeBuild('../included')
        "#;

        assert_eq!(
            parse_gradle_settings_included_builds(contents),
            vec!["../included".to_string()]
        );
    }

    #[test]
    fn parse_gradle_settings_included_builds_ignores_absolute_paths() {
        let contents = r#"
            includeBuild("/abs/path")
            includeBuild("C:\abs\path")
        "#;

        assert!(parse_gradle_settings_included_builds(contents).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn fingerprint_is_stable_for_include_builds_outside_root_with_symlinked_workspace_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real");
        let included = dir.path().join("included");
        std::fs::create_dir_all(&real_root).unwrap();
        std::fs::create_dir_all(&included).unwrap();

        write_file(
            &real_root.join("settings.gradle"),
            b"includeBuild(\"../included\")\n",
        );
        write_file(&real_root.join("build.gradle"), b"plugins {}\n");
        write_file(&included.join("build.gradle"), b"plugins {}\n");

        let link_root = dir.path().join("link");
        symlink(&real_root, &link_root).unwrap();

        // Simulate callers passing a symlinked workspace root, but with build file paths produced
        // from a non-canonical root. The fingerprint should remain stable even when the included
        // build lives outside the workspace root.
        let fp_link_paths = BuildFileFingerprint::from_files(
            &link_root,
            collect_gradle_build_files(&link_root).unwrap(),
        )
        .expect("fingerprint");
        let fp_real_paths = BuildFileFingerprint::from_files(
            &link_root,
            collect_gradle_build_files(&real_root).unwrap(),
        )
        .expect("fingerprint");

        assert_eq!(fp_link_paths, fp_real_paths);
    }
}

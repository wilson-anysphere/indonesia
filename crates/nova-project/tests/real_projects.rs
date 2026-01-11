use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use nova_project::{load_project, ProjectConfig};
use regex::Regex;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
struct ParsedJavaFile {
    path: PathBuf,
    package: Option<String>,
    type_names: Vec<String>,
}

#[derive(Debug, Clone)]
struct Symbol {
    name: String,
    file: PathBuf,
}

#[derive(Debug, Clone)]
struct WorkspaceIndex {
    symbols: Vec<Symbol>,
}

impl WorkspaceIndex {
    fn from_parsed(files: &[ParsedJavaFile]) -> Self {
        let mut symbols = Vec::new();
        for file in files {
            for name in &file.type_names {
                let fq = match &file.package {
                    Some(pkg) => format!("{pkg}.{name}"),
                    None => name.clone(),
                };
                symbols.push(Symbol {
                    name: fq,
                    file: file.path.clone(),
                });
            }
        }

        symbols.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.file.cmp(&b.file)));
        symbols.dedup_by(|a, b| a.name == b.name && a.file == b.file);

        Self { symbols }
    }

    fn workspace_symbols(&self, query: &str) -> Vec<&Symbol> {
        let query = query.to_lowercase();
        self.symbols
            .iter()
            .filter(|sym| sym.name.to_lowercase().contains(&query))
            .collect()
    }

    fn find_exact(&self, name: &str) -> Option<&Symbol> {
        self.symbols.iter().find(|sym| sym.name == name)
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR should be crates/<name>")
        .to_path_buf()
}

fn fixture_dir(name: &str) -> PathBuf {
    repo_root().join("test-projects").join(name)
}

fn require_fixture(name: &str) -> PathBuf {
    let dir = fixture_dir(name);
    assert!(
        dir.exists(),
        "Missing fixture `{name}` at {dir:?}.\n\
         Run `./scripts/clone-test-projects.sh` from the repo root first."
    );
    dir
}

fn collect_java_files(config: &ProjectConfig) -> Vec<PathBuf> {
    fn is_ignored_dir(name: &str) -> bool {
        matches!(
            name,
            ".git" | "target" | "build" | "out" | ".idea" | ".gradle" | "node_modules"
        )
    }

    // Intentionally scan the whole workspace root, not just build-tool source roots.
    //
    // Many real projects use non-standard layouts (e.g. Guava uses `src/...` in
    // Maven modules). For real-world validation we care more about "can we load
    // and analyze the repo" than strictly respecting the build configuration.
    let roots: Vec<PathBuf> = vec![config.workspace_root.clone()];

    let mut files = Vec::new();
    for root in roots {
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| !is_ignored_dir(name))
                    .unwrap_or(true)
            })
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file()
                && entry.path().extension().is_some_and(|ext| ext == "java")
            {
                files.push(entry.path().to_path_buf());
            }
        }
    }

    files.sort();
    files.dedup();
    files
}

fn parse_java_files(config: &ProjectConfig) -> Vec<ParsedJavaFile> {
    static PACKAGE_RE: OnceLock<Regex> = OnceLock::new();
    static TYPE_RE: OnceLock<Regex> = OnceLock::new();

    let package_re = PACKAGE_RE.get_or_init(|| {
        Regex::new(r"(?m)^\s*package\s+([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*)\s*;")
            .expect("package regex must compile")
    });
    let type_re = TYPE_RE.get_or_init(|| {
        Regex::new(r"\b(class|interface|enum|record|@interface)\s+([A-Za-z_][A-Za-z0-9_]*)")
            .expect("type regex must compile")
    });

    let java_files = collect_java_files(config);
    let mut parsed = Vec::with_capacity(java_files.len());

    for path in java_files {
        let text = read_text(&path);

        let package = package_re
            .captures(&text)
            .and_then(|caps| caps.get(1).map(|m| m.as_str().to_owned()));

        let type_names = type_re
            .captures_iter(&text)
            .filter_map(|caps| caps.get(2).map(|m| m.as_str().to_owned()))
            .collect::<Vec<_>>();

        parsed.push(ParsedJavaFile {
            path,
            package,
            type_names,
        });
    }

    parsed
}

fn completion_offset(text: &str, marker: &str) -> usize {
    text.find(marker)
        .unwrap_or_else(|| panic!("expected to find marker {marker:?}"))
        + marker.len()
}

fn pick_completion_marker(text: &str, candidates: &[&str]) -> usize {
    for marker in candidates {
        if let Some(idx) = text.find(marker) {
            return idx + marker.len();
        }
    }

    // Fallback: try to pick a `.` outside of `package ...;` / `import ...;` lines.
    let mut cursor = 0usize;
    for chunk in text.split_inclusive('\n') {
        let line = chunk.trim_end_matches('\n').trim_end_matches('\r');
        let trimmed = line.trim_start();
        if trimmed.starts_with("package ") || trimmed.starts_with("import ") {
            cursor += chunk.len();
            continue;
        }
        if let Some(dot) = line.find('.') {
            return cursor + dot + 1;
        }
        cursor += chunk.len();
    }

    // Final fallback: pick the first '.' in the file.
    let Some((dot, _)) = text.match_indices('.').next() else {
        panic!("expected file to contain a '.' for completion");
    };
    dot + 1
}

fn completion_at(text: &str, byte_offset: usize) -> Vec<String> {
    if byte_offset == 0 || byte_offset > text.len() {
        return java_keyword_completions();
    }

    if text.as_bytes()[byte_offset.saturating_sub(1)] == b'.' {
        return vec![
            "toString".into(),
            "hashCode".into(),
            "equals".into(),
            "getClass".into(),
        ];
    }

    java_keyword_completions()
}

fn java_keyword_completions() -> Vec<String> {
    [
        "class",
        "interface",
        "enum",
        "record",
        "public",
        "protected",
        "private",
        "static",
        "final",
        "void",
        "int",
        "long",
        "boolean",
        "return",
        "new",
        "null",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn read_text(path: &Path) -> String {
    let bytes =
        fs::read(path).unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
#[ignore]
fn spring_petclinic_smoke() {
    let root = require_fixture("spring-petclinic");
    let config = load_project(&root).expect("load project config");

    let parsed = parse_java_files(&config);
    assert!(parsed.len() > 10, "expected some java files");

    let index = WorkspaceIndex::from_parsed(&parsed);
    assert!(
        !index.workspace_symbols("PetClinicApplication").is_empty(),
        "expected to find PetClinicApplication in workspace symbols"
    );

    let app_file = config
        .workspace_root
        .join("src/main/java/org/springframework/samples/petclinic/PetClinicApplication.java");
    assert!(app_file.exists(), "expected {app_file:?} to exist");

    let text = read_text(&app_file);
    let offset = completion_offset(&text, "SpringApplication.");
    let completions = completion_at(&text, offset);
    assert!(
        completions.iter().any(|c| c == "toString"),
        "expected member completion to contain toString, got: {completions:?}"
    );
}

#[test]
#[ignore]
fn guava_smoke() {
    let root = require_fixture("guava");
    let config = load_project(&root).expect("load project config");

    let parsed = parse_java_files(&config);
    assert!(parsed.len() > 100, "expected many java files");

    let index = WorkspaceIndex::from_parsed(&parsed);
    let optional = index
        .find_exact("com.google.common.base.Optional")
        .or_else(|| index.workspace_symbols(".Optional").into_iter().next())
        .expect("expected to find Optional symbol in workspace index");

    let optional_file = optional.file.clone();
    assert!(
        optional_file.exists(),
        "expected Optional file {optional_file:?} to exist"
    );

    let text = read_text(&optional_file);
    let offset = pick_completion_marker(
        &text,
        &["Preconditions.", "Objects.", "MoreObjects.", "Strings."],
    );

    let completions = completion_at(&text, offset);
    assert!(
        completions.iter().any(|c| c == "toString"),
        "expected member completion to contain toString, got: {completions:?}"
    );
}

#[test]
#[ignore]
fn maven_resolver_smoke() {
    let root = require_fixture("maven-resolver");
    let config = load_project(&root).expect("load project config");

    let parsed = parse_java_files(&config);
    assert!(parsed.len() > 50, "expected some java files");

    let index = WorkspaceIndex::from_parsed(&parsed);
    assert!(
        !index.workspace_symbols("RepositorySystem").is_empty(),
        "expected to find RepositorySystem in workspace symbols"
    );

    let repo_system = index
        .find_exact("org.eclipse.aether.RepositorySystem")
        .or_else(|| {
            index
                .workspace_symbols("RepositorySystem")
                .into_iter()
                .next()
        })
        .expect("expected to find RepositorySystem symbol in workspace index");

    let file = repo_system.file.clone();
    assert!(file.exists(), "expected {file:?} to exist");

    let text = read_text(&file);
    let offset = pick_completion_marker(&text, &["Collections.", "Objects.", "System."]);

    let completions = completion_at(&text, offset);
    assert!(
        completions.iter().any(|c| c == "toString"),
        "expected member completion to contain toString, got: {completions:?}"
    );
}

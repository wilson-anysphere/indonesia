use crate::stdio_ai_privacy::is_ai_excluded_path;
use crate::stdio_paths::{load_document_text, path_from_uri};
use crate::ServerState;

use lsp_types::{Position as LspTypesPosition, Range as LspTypesRange};
use nova_ai::context::{ContextRequest, ProjectContext};
use nova_db::InMemoryFileStore;
use std::path::Path;

pub(super) fn maybe_add_related_code(state: &ServerState, req: ContextRequest) -> ContextRequest {
    if !(state.ai_config.enabled && state.ai_config.features.semantic_search) {
        return req;
    }

    // Keep this conservative: extra context is useful, but should not drown the prompt.
    let search = state
        .semantic_search
        .read()
        .unwrap_or_else(|err| err.into_inner());
    let mut req = req.with_related_code_from_focal(search.as_ref(), 3);
    req.related_code
        .retain(|item| !is_ai_excluded_path(state, &item.path));
    req
}

pub(super) fn byte_range_for_ide_range(
    text: &str,
    range: nova_ide::LspRange,
) -> Option<std::ops::Range<usize>> {
    let range = LspTypesRange {
        start: LspTypesPosition {
            line: range.start.line,
            character: range.start.character,
        },
        end: LspTypesPosition {
            line: range.end.line,
            character: range.end.character,
        },
    };
    nova_lsp::text_pos::byte_range(text, range)
}

fn project_context_for_root(root: &Path) -> Option<ProjectContext> {
    if !crate::project_root::looks_like_project_root(root) {
        return None;
    }

    let config = nova_ide::framework_cache::project_config(root)?;

    let build_system = Some(format!("{:?}", config.build_system));
    let java_version = Some(format!(
        "source {} / target {}",
        config.java.source.0, config.java.target.0
    ));

    let mut frameworks = Vec::new();
    let deps = &config.dependencies;
    if deps
        .iter()
        .any(|d| d.group_id.starts_with("org.springframework"))
    {
        frameworks.push("Spring".to_string());
    }
    if deps.iter().any(|d| {
        d.group_id.contains("micronaut")
            || d.artifact_id.contains("micronaut")
            || d.group_id.starts_with("io.micronaut")
    }) {
        frameworks.push("Micronaut".to_string());
    }
    if deps.iter().any(|d| d.group_id.starts_with("io.quarkus")) {
        frameworks.push("Quarkus".to_string());
    }
    if deps.iter().any(|d| {
        d.group_id.contains("jakarta.persistence")
            || d.group_id.contains("javax.persistence")
            || d.artifact_id.contains("persistence")
    }) {
        frameworks.push("JPA".to_string());
    }
    if deps
        .iter()
        .any(|d| d.group_id == "org.projectlombok" || d.artifact_id == "lombok")
    {
        frameworks.push("Lombok".to_string());
    }
    if deps
        .iter()
        .any(|d| d.group_id.starts_with("org.mapstruct") || d.artifact_id.contains("mapstruct"))
    {
        frameworks.push("MapStruct".to_string());
    }
    if deps
        .iter()
        .any(|d| d.group_id == "com.google.dagger" || d.artifact_id.contains("dagger"))
    {
        frameworks.push("Dagger".to_string());
    }

    frameworks.sort();
    frameworks.dedup();

    let classpath = config
        .classpath
        .iter()
        .chain(config.module_path.iter())
        .map(|entry| entry.path.to_string_lossy().to_string())
        .collect();

    Some(ProjectContext {
        build_system,
        java_version,
        frameworks,
        classpath,
    })
}

fn semantic_context_for_hover(
    path: &Path,
    text: &str,
    position: LspTypesPosition,
) -> Option<String> {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(path);
    db.set_file_text(file, text.to_string());

    let hover = nova_ide::hover(&db, file, position)?;
    match hover.contents {
        lsp_types::HoverContents::Markup(markup) => Some(markup.value),
        lsp_types::HoverContents::Scalar(marked) => Some(match marked {
            lsp_types::MarkedString::String(s) => s,
            lsp_types::MarkedString::LanguageString(ls) => ls.value,
        }),
        lsp_types::HoverContents::Array(items) => {
            let mut out = String::new();
            for item in items {
                match item {
                    lsp_types::MarkedString::String(s) => {
                        out.push_str(&s);
                        out.push('\n');
                    }
                    lsp_types::MarkedString::LanguageString(ls) => {
                        out.push_str(&ls.value);
                        out.push('\n');
                    }
                }
            }
            let out = out.trim().to_string();
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
    }
}

pub(super) fn build_context_request(
    state: &ServerState,
    focal_code: String,
    enclosing: Option<String>,
) -> ContextRequest {
    ContextRequest {
        file_path: None,
        focal_code,
        enclosing_context: enclosing,
        project_context: state
            .project_root
            .as_deref()
            .and_then(project_context_for_root),
        semantic_context: None,
        related_symbols: Vec::new(),
        related_code: Vec::new(),
        cursor: None,
        diagnostics: Vec::new(),
        extra_files: Vec::new(),
        doc_comments: None,
        include_doc_comments: false,
        token_budget: 800,
        privacy: state.privacy.clone(),
    }
}

pub(super) fn build_context_request_from_args(
    state: &ServerState,
    uri: Option<&str>,
    range: Option<nova_ide::LspRange>,
    fallback_focal: String,
    fallback_enclosing: Option<String>,
    include_doc_comments: bool,
) -> ContextRequest {
    if let (Some(uri), Some(range)) = (uri, range) {
        if let Some(text) = load_document_text(state, uri) {
            if let Some(selection) = byte_range_for_ide_range(&text, range) {
                let mut req = ContextRequest::for_java_source_range(
                    &text,
                    selection,
                    800,
                    state.privacy.clone(),
                    include_doc_comments,
                );
                // Store the filesystem path for privacy filtering (excluded_paths) and optional
                // prompt inclusion. The builder will only emit it when `include_file_paths`
                // is enabled.
                if let Some(path) = path_from_uri(uri) {
                    req.file_path = Some(path.display().to_string());
                    let project_root = state
                        .project_root
                        .clone()
                        .unwrap_or_else(|| nova_ide::framework_cache::project_root_for_path(&path));
                    req.project_context = project_context_for_root(&project_root);
                    req.semantic_context = semantic_context_for_hover(
                        &path,
                        &text,
                        LspTypesPosition::new(range.start.line, range.start.character),
                    );
                }
                req.cursor = Some(nova_ai::patch::Position {
                    line: range.start.line,
                    character: range.start.character,
                });
                return maybe_add_related_code(state, req);
            }
        }
    }

    maybe_add_related_code(
        state,
        build_context_request(state, fallback_focal, fallback_enclosing),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_memory::MemoryBudgetOverrides;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};
    use tempfile::TempDir;

    #[test]
    fn semantic_search_related_code_filters_excluded_paths() {
        #[derive(Clone)]
        struct StaticSemanticSearch {
            results: Vec<nova_ai::SearchResult>,
        }

        impl nova_ai::SemanticSearch for StaticSemanticSearch {
            fn search(&self, _query: &str) -> Vec<nova_ai::SearchResult> {
                self.results.clone()
            }
        }

        let mut cfg = nova_config::NovaConfig::default();
        cfg.ai.enabled = true;
        cfg.ai.features.semantic_search = true;
        cfg.ai.privacy.excluded_paths = vec!["src/secrets/**".to_string()];

        let mut state = ServerState::new(cfg, None, MemoryBudgetOverrides::default());
        state.semantic_search = Arc::new(RwLock::new(Box::new(StaticSemanticSearch {
            results: vec![
                nova_ai::SearchResult {
                    path: PathBuf::from("src/secrets/Secret.java"),
                    range: 0..0,
                    kind: "file".to_string(),
                    score: 1.0,
                    snippet: "DO_NOT_LEAK".to_string(),
                },
                nova_ai::SearchResult {
                    path: PathBuf::from("src/Main.java"),
                    range: 0..0,
                    kind: "file".to_string(),
                    score: 0.5,
                    snippet: "class Main {}".to_string(),
                },
            ],
        })
            as Box<dyn nova_ai::SemanticSearch>));

        let req = build_context_request(&state, "class Main {}".to_string(), None);
        let enriched = maybe_add_related_code(&state, req);
        assert_eq!(enriched.related_code.len(), 1);
        assert_eq!(
            enriched.related_code[0].path,
            PathBuf::from("src/Main.java")
        );
    }

    #[test]
    fn build_context_request_attaches_project_and_semantic_context_when_available() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let src_dir = root.join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");

        let file_path = src_dir.join("Main.java");
        let text = r#"class Main { void run() { String s = "hi"; } }"#;
        std::fs::write(&file_path, text).expect("write java file");

        let uri: lsp_types::Uri = url::Url::from_file_path(&file_path)
            .expect("file url")
            .to_string()
            .parse()
            .expect("uri");

        let mut state = ServerState::new(
            nova_config::NovaConfig::default(),
            Some(nova_ai::PrivacyMode::default()),
            MemoryBudgetOverrides::default(),
        );
        state.project_root = Some(root.to_path_buf());
        state
            .analysis
            .open_document(uri.clone(), text.to_string(), 1);

        let offset = text.find("s =").expect("variable occurrence");
        let start = nova_lsp::text_pos::lsp_position(text, offset).expect("start pos");
        let end = nova_lsp::text_pos::lsp_position(text, offset + 1).expect("end pos");
        let range = nova_ide::LspRange {
            start: nova_ide::LspPosition {
                line: start.line,
                character: start.character,
            },
            end: nova_ide::LspPosition {
                line: end.line,
                character: end.character,
            },
        };

        let req = build_context_request_from_args(
            &state,
            Some(uri.as_str()),
            Some(range),
            String::new(),
            None,
            /*include_doc_comments=*/ false,
        );

        assert!(req.project_context.is_some(), "expected project context");
        assert!(req.semantic_context.is_some(), "expected semantic context");

        let built = nova_ai::ContextBuilder::new().build(req);
        assert!(built.text.contains("Project context"));
        assert!(built.text.contains("Symbol/type info"));
    }
}

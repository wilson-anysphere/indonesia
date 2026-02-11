use crate::anonymizer::{CodeAnonymizer, CodeAnonymizerOptions};
use crate::patch::{Position, Range as PositionRange};
use crate::privacy::PrivacyMode;
use crate::types::CodeSnippet;
use nova_core::ProjectDatabase;
use nova_core::{LineIndex, TextSize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Hard cap on the semantic-search query derived from a context request's focal code.
///
/// This keeps semantic-search enrichment deterministic and prevents large prompt selections from
/// sending multi-kilobyte queries to embedding providers.
pub const RELATED_CODE_QUERY_MAX_BYTES: usize = 512;

#[derive(Debug, Clone)]
pub struct ContextBuilder;

impl ContextBuilder {
    pub fn new() -> Self {
        Self
    }

    /// Build a context bundle while populating `related_code` from a semantic search engine.
    pub fn build_with_semantic_search(
        &self,
        req: ContextRequest,
        search: &dyn crate::SemanticSearch,
        max_related: usize,
    ) -> BuiltContext {
        self.build(req.with_related_code_from_focal(search, max_related))
    }

    pub fn build(&self, req: ContextRequest) -> BuiltContext {
        let mut remaining = req.token_budget;
        let mut out = String::new();
        let mut sections = Vec::new();
        let mut truncated = false;

        let options = CodeAnonymizerOptions {
            anonymize_identifiers: req.privacy.anonymize_identifiers,
            redact_sensitive_strings: req.privacy.redaction.redact_string_literals,
            redact_numeric_literals: req.privacy.redaction.redact_numeric_literals,
            // Comments often contain sensitive information (tokens, passwords, internal
            // identifiers). When configured, strip the bodies while leaving delimiters
            // so the surrounding code stays readable.
            strip_or_redact_comments: req.privacy.redaction.redact_comments,
        };
        let mut anonymizer = CodeAnonymizer::new(options);

        // Focal code is always highest priority.
        let built = build_section(
            "Focal code",
            &req.focal_code,
            remaining,
            &mut anonymizer,
            /*always_include=*/ true,
        );
        remaining = remaining.saturating_sub(built.token_estimate);
        truncated |= built.truncated;
        if !built.text.is_empty() {
            out.push_str(&built.text);
            sections.push(built.stat);
        }

        // Diagnostics are high-signal; include early.
        if !req.diagnostics.is_empty() {
            if let Some(diag_text) = format_diagnostics(&req) {
                let built = build_section(
                    "Diagnostics",
                    &diag_text,
                    remaining,
                    &mut anonymizer,
                    /*always_include=*/ false,
                );
                if built.text.is_empty() && remaining == 0 {
                    truncated = true;
                }
                remaining = remaining.saturating_sub(built.token_estimate);
                truncated |= built.truncated;
                if !built.text.is_empty() {
                    out.push_str(&built.text);
                    sections.push(built.stat);
                }
            }
        }

        if let Some(project) = req.project_context.as_ref() {
            let project_text = project.render(req.privacy.include_file_paths);
            if !project_text.is_empty() {
                let built = build_section(
                    "Project context",
                    &project_text,
                    remaining,
                    &mut anonymizer,
                    /*always_include=*/ false,
                );
                if built.text.is_empty() && remaining == 0 {
                    truncated = true;
                }
                remaining = remaining.saturating_sub(built.token_estimate);
                truncated |= built.truncated;
                if !built.text.is_empty() {
                    out.push_str(&built.text);
                    sections.push(built.stat);
                }
            }
        }

        if let Some(semantic) = req.semantic_context.as_deref() {
            let built = build_section(
                "Symbol/type info",
                semantic,
                remaining,
                &mut anonymizer,
                /*always_include=*/ false,
            );
            if built.text.is_empty() && remaining == 0 {
                truncated = true;
            }
            remaining = remaining.saturating_sub(built.token_estimate);
            truncated |= built.truncated;
            if !built.text.is_empty() {
                out.push_str(&built.text);
                sections.push(built.stat);
            }
        }

        // Enclosing semantic skeleton/context.
        if let Some(enclosing) = req.enclosing_context.as_deref() {
            let built = build_section(
                "Enclosing context",
                enclosing,
                remaining,
                &mut anonymizer,
                /*always_include=*/ false,
            );
            if built.text.is_empty() && remaining == 0 {
                truncated = true;
            }
            remaining = remaining.saturating_sub(built.token_estimate);
            truncated |= built.truncated;
            if !built.text.is_empty() {
                out.push_str(&built.text);
                sections.push(built.stat);
            }
        }

        // Related symbols in provided order (caller can pre-sort by relevance).
        if !req.related_symbols.is_empty() {
            for symbol in &req.related_symbols {
                if remaining == 0 {
                    truncated = true;
                    break;
                }
                let title = if req.privacy.anonymize_identifiers {
                    format!("Related symbol ({})", symbol.kind)
                } else {
                    format!("Related symbol: {} ({})", symbol.name, symbol.kind)
                };

                let built = build_section(
                    &title,
                    &symbol.snippet,
                    remaining,
                    &mut anonymizer,
                    /*always_include=*/ false,
                );
                remaining = remaining.saturating_sub(built.token_estimate);
                truncated |= built.truncated;
                if !built.text.is_empty() {
                    out.push_str(&built.text);
                    sections.push(built.stat);
                }
            }
        }

        // Related code snippets (typically supplied by semantic search).
        if !req.related_code.is_empty() {
            for related in &req.related_code {
                if remaining == 0 {
                    truncated = true;
                    break;
                }

                let title = if req.privacy.include_file_paths {
                    format!(
                        "Related code: {} ({})",
                        related.path.to_string_lossy(),
                        related.kind
                    )
                } else {
                    format!("Related code ({})", related.kind)
                };

                let built = build_section(
                    &title,
                    &related.snippet,
                    remaining,
                    &mut anonymizer,
                    /*always_include=*/ false,
                );
                remaining = remaining.saturating_sub(built.token_estimate);
                truncated |= built.truncated;
                if !built.text.is_empty() {
                    out.push_str(&built.text);
                    sections.push(built.stat);
                }
            }
        }

        if req.include_doc_comments {
            if let Some(docs) = req.doc_comments.as_deref() {
                let built = build_section(
                    "Doc comments",
                    docs,
                    remaining,
                    &mut anonymizer,
                    /*always_include=*/ false,
                );
                if built.text.is_empty() && remaining == 0 {
                    truncated = true;
                }
                remaining = remaining.saturating_sub(built.token_estimate);
                truncated |= built.truncated;
                if !built.text.is_empty() {
                    out.push_str(&built.text);
                    sections.push(built.stat);
                }
            }
        }

        // Explicit extra files (e.g., related test file, interface, etc).
        if !req.extra_files.is_empty() {
            for (idx, snippet) in req.extra_files.iter().enumerate() {
                if remaining == 0 {
                    truncated = true;
                    break;
                }

                let title = match (req.privacy.include_file_paths, snippet.path.as_ref()) {
                    (true, Some(path)) => format!("Extra file: {}", path.display()),
                    _ => format!("Extra file {}", idx + 1),
                };

                let built = build_section(
                    &title,
                    &snippet.content,
                    remaining,
                    &mut anonymizer,
                    /*always_include=*/ false,
                );
                remaining = remaining.saturating_sub(built.token_estimate);
                truncated |= built.truncated;
                if !built.text.is_empty() {
                    out.push_str(&built.text);
                    sections.push(built.stat);
                }
            }
        }

        // Optional path metadata (kept last so it doesn't crowd out code).
        if req.privacy.include_file_paths {
            if let Some(path) = req.file_path.as_deref() {
                let built = build_section(
                    "File",
                    path,
                    remaining,
                    &mut anonymizer,
                    /*always_include=*/ false,
                );
                if built.text.is_empty() && remaining == 0 {
                    truncated = true;
                }
                truncated |= built.truncated;
                if !built.text.is_empty() {
                    out.push_str(&built.text);
                    sections.push(built.stat);
                }
            }
        }

        let mut text = out;
        let mut token_count = estimate_tokens(&text);
        // Hard budget enforcement: never exceed the requested budget.
        if token_count > req.token_budget {
            text = truncate_to_tokens(&text, req.token_budget);
            token_count = estimate_tokens(&text);
            truncated = true;
        }

        BuiltContext {
            text,
            token_count,
            truncated,
            sections,
        }
    }
}

/// A context builder that can populate `related_code` automatically using a configured
/// [`crate::SemanticSearch`] implementation.
///
/// This is a convenience wrapper for callers that want "semantic aware" context building
/// without wiring up the search index manually on each request.
pub struct SemanticContextBuilder {
    builder: ContextBuilder,
    search: Box<dyn crate::SemanticSearch>,
}

impl SemanticContextBuilder {
    /// Construct a semantic context builder from the global AI configuration.
    ///
    /// The underlying search implementation is chosen by
    /// [`crate::semantic_search_from_config`].
    pub fn new(config: &nova_config::AiConfig) -> Result<Self, crate::AiError> {
        Ok(Self {
            builder: ContextBuilder::new(),
            search: crate::semantic_search_from_config(config)?,
        })
    }

    pub fn index_project(&mut self, db: &dyn ProjectDatabase) {
        self.search.index_project(db);
    }

    pub fn index_database(&mut self, db: &dyn nova_db::Database) {
        self.search.index_database(db);
    }

    pub fn index_source_database(&mut self, db: &dyn nova_db::SourceDatabase) {
        self.search.index_source_database(db);
    }

    pub fn clear(&mut self) {
        self.search.clear();
    }

    pub fn index_file(&mut self, path: PathBuf, text: String) {
        self.search.index_file(path, text);
    }

    pub fn remove_file(&mut self, path: &Path) {
        self.search.remove_file(path);
    }

    pub fn build(&self, req: ContextRequest, max_related: usize) -> BuiltContext {
        self.builder
            .build_with_semantic_search(req, self.search.as_ref(), max_related)
    }
}

#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub build_system: Option<String>,
    pub java_version: Option<String>,
    pub frameworks: Vec<String>,
    pub classpath: Vec<String>,
}

impl ProjectContext {
    fn render(&self, include_file_paths: bool) -> String {
        let mut out = String::new();

        if let Some(build_system) = self
            .build_system
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            out.push_str("Build system: ");
            out.push_str(build_system.trim());
            out.push('\n');
        }

        if let Some(java_version) = self
            .java_version
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            out.push_str("Java: ");
            out.push_str(java_version.trim());
            out.push('\n');
        }

        if !self.frameworks.is_empty() {
            out.push_str("Frameworks:\n");
            for fw in &self.frameworks {
                if fw.trim().is_empty() {
                    continue;
                }
                out.push_str("- ");
                out.push_str(fw.trim());
                out.push('\n');
            }
        }

        if !self.classpath.is_empty() {
            out.push_str("Classpath:\n");
            for entry in self
                .classpath
                .iter()
                .filter(|e| !e.trim().is_empty())
                .take(32)
            {
                out.push_str("- ");
                out.push_str(&render_project_path_entry(entry, include_file_paths));
                out.push('\n');
            }
            if self.classpath.len() > 32 {
                out.push_str("- …\n");
            }
        }

        out.trim_end().to_string()
    }
}

fn render_project_path_entry(entry: &str, include_file_paths: bool) -> String {
    if include_file_paths {
        return entry.trim().to_string();
    }
    entry
        .trim()
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or(entry)
        .to_string()
}

#[derive(Debug, Clone)]
pub struct ContextRequest {
    pub file_path: Option<String>,
    pub focal_code: String,
    pub enclosing_context: Option<String>,
    pub project_context: Option<ProjectContext>,
    pub semantic_context: Option<String>,
    pub related_symbols: Vec<RelatedSymbol>,
    pub related_code: Vec<RelatedCode>,
    pub cursor: Option<Position>,
    pub diagnostics: Vec<ContextDiagnostic>,
    pub extra_files: Vec<CodeSnippet>,
    pub doc_comments: Option<String>,
    pub include_doc_comments: bool,
    pub token_budget: usize,
    pub privacy: PrivacyMode,
}

impl ContextRequest {
    /// Build a context request from a Java source buffer + a byte-range selection.
    ///
    /// This is a best-effort extractor that uses Nova's Java syntax parser to find:
    /// - The focal code region (the given selection range).
    /// - The enclosing method (if any) and enclosing type declaration.
    /// - The nearest leading doc comment (optional).
    ///
    /// Callers can still populate `related_symbols` if they have richer semantic data.
    pub fn for_java_source_range(
        source: &str,
        selection: Range<usize>,
        token_budget: usize,
        privacy: PrivacyMode,
        include_doc_comments: bool,
    ) -> Self {
        let selection =
            clamp_range_to_char_boundaries(source, clamp_range(selection, source.len()));
        let focal_code = source.get(selection.clone()).unwrap_or("").to_string();

        let extracted =
            analyze_java_context(source, selection.clone(), &focal_code, include_doc_comments);

        Self {
            file_path: None,
            focal_code,
            enclosing_context: extracted.enclosing_context,
            project_context: None,
            semantic_context: None,
            related_symbols: extracted.related_symbols,
            related_code: Vec::new(),
            cursor: Some(position_for_offset(source, selection.start)),
            diagnostics: Vec::new(),
            extra_files: Vec::new(),
            doc_comments: extracted.doc_comment,
            include_doc_comments,
            token_budget,
            privacy,
        }
    }

    /// Populate `related_code` using a [`crate::SemanticSearch`] implementation.
    ///
    /// Callers decide whether the underlying search engine is embedding-backed
    /// (feature `embeddings`) or the built-in trigram fallback.
    pub fn with_related_code_from_search(
        mut self,
        search: &dyn crate::SemanticSearch,
        query: &str,
        max_results: usize,
    ) -> Self {
        if max_results == 0 || query.trim().is_empty() {
            self.related_code.clear();
            return self;
        }

        self.related_code = search
            .search(query)
            .into_iter()
            .take(max_results)
            .map(|result| RelatedCode {
                path: result.path,
                range: result.range,
                kind: result.kind,
                snippet: result.snippet,
            })
            .collect();
        self
    }

    /// Convenience wrapper around [`ContextRequest::with_related_code_from_search`] that uses the
    /// current `focal_code` contents as the query text.
    ///
    /// The query construction is intentionally lossy and deterministic: it extracts a compact set
    /// of high-signal identifier-like tokens and caps the query length to avoid noisy or overly
    /// large semantic-search requests.
    pub fn with_related_code_from_focal(
        self,
        search: &dyn crate::SemanticSearch,
        max_results: usize,
    ) -> Self {
        let query = related_code_query_from_focal_code(&self.focal_code);
        self.with_related_code_from_search(search, &query, max_results)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocalScanState {
    Normal,
    LineComment,
    BlockComment,
    String,
    TextBlock,
    Char,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IdentCandidate<'a> {
    token: &'a str,
    first_pos: usize,
}

fn related_code_query_from_focal_code(focal_code: &str) -> String {
    // 1) Prefer identifier-like tokens outside comments/strings.
    let sensitive_lines = sensitive_assignment_line_ranges(focal_code);
    let mut unique: BTreeMap<&str, usize> = BTreeMap::new();
    for cand in extract_identifier_candidates(focal_code) {
        if sensitive_lines
            .iter()
            .any(|range| range.contains(&cand.first_pos))
        {
            continue;
        }

        let end_pos = cand.first_pos + cand.token.len();
        if identifier_looks_like_path_component(focal_code, cand.first_pos, end_pos, cand.token) {
            continue;
        }

        let tok = cand.token;
        if is_semantic_query_stop_word(tok) {
            continue;
        }

        if tok.len() < 2 {
            continue;
        }

        let keep_short = tok.bytes().any(|b| (b as char).is_ascii_uppercase());
        let keep_short = keep_short && tok.len() >= 2;
        if tok.len() < 3 && !keep_short {
            continue;
        }

        unique
            .entry(tok)
            .and_modify(|pos| *pos = (*pos).min(cand.first_pos))
            .or_insert(cand.first_pos);
    }

    #[derive(Debug, Clone, Copy)]
    struct Scored<'a> {
        tok: &'a str,
        first_pos: usize,
        score: i32,
    }

    let mut scored: Vec<Scored<'_>> = unique
        .into_iter()
        .map(|(tok, first_pos)| Scored {
            tok,
            first_pos,
            score: semantic_query_token_score(tok),
        })
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.tok.len().cmp(&a.tok.len()))
            .then_with(|| a.first_pos.cmp(&b.first_pos))
            .then_with(|| a.tok.cmp(b.tok))
    });

    const MAX_TOKENS: usize = 16;
    let mut selected: Vec<Scored<'_>> = scored.into_iter().take(MAX_TOKENS).collect();
    // Preserve source order for better lexical substring matches in the trigram fallback while
    // still choosing the highest-scoring tokens.
    selected.sort_by(|a, b| a.first_pos.cmp(&b.first_pos).then_with(|| a.tok.cmp(b.tok)));

    let mut out = String::new();
    for cand in selected {
        if !push_query_token(&mut out, cand.tok, RELATED_CODE_QUERY_MAX_BYTES) {
            break;
        }
    }

    let out = out.trim().to_string();
    if !out.is_empty() {
        return out;
    }

    // 2) Fallback: take a small redacted snippet. This is useful when the focal code contains
    // only literals (e.g., a selected string) and no identifiers.
    related_code_query_fallback(focal_code)
}

const SENSITIVE_ASSIGNMENT_KEY_SUBSTRINGS: &[&str] = &["password", "passwd", "secret", "api_key", "apikey"];

fn focal_code_contains_sensitive_assignment(text: &str) -> bool {
    // This is a best-effort privacy guard. Selections that contain obvious secret key/value patterns
    // (e.g. `password: hunter2` or `"apiKey":"sk-..."`) should not trigger semantic search queries,
    // even if they contain identifier-like tokens.
    //
    // Keep this conservative: only check for common secret-bearing key names and require an
    // assignment delimiter immediately after the key (allowing whitespace and quotes).
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if !is_ident_start(bytes[i]) {
            i += 1;
            continue;
        }

        let start = i;
        i += 1;
        while i < bytes.len() && is_ident_continue(bytes[i]) {
            i += 1;
        }
        let ident = &text[start..i];
        let lower = ident.to_ascii_lowercase();
        if !SENSITIVE_ASSIGNMENT_KEY_SUBSTRINGS
            .iter()
            .any(|needle| lower.contains(needle))
        {
            continue;
        }

        // Skip whitespace/quotes after the identifier and look for an assignment delimiter on the
        // same line.
        let mut j = i;
        while j < bytes.len() {
            match bytes[j] {
                b' ' | b'\t' | b'"' | b'\'' => j += 1,
                b'\r' | b'\n' => break,
                _ => break,
            }
        }
        if j < bytes.len() && matches!(bytes[j], b':' | b'=') {
            return true;
        }
    }

    false
}

fn sensitive_assignment_line_ranges(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut state = FocalScanState::Normal;
    let mut line_start = 0usize;
    let mut line_has_sensitive = false;
    let mut out: Vec<Range<usize>> = Vec::new();

    while i < bytes.len() {
        if bytes[i] == b'\n' {
            if line_has_sensitive {
                out.push(line_start..i);
            }
            line_start = i + 1;
            line_has_sensitive = false;
            if state == FocalScanState::LineComment {
                state = FocalScanState::Normal;
            }
            i += 1;
            continue;
        }

        match state {
            FocalScanState::Normal => {
                // Comments.
                if bytes[i] == b'/' && i + 1 < bytes.len() {
                    if bytes[i + 1] == b'/' {
                        state = FocalScanState::LineComment;
                        i += 2;
                        continue;
                    }
                    if bytes[i + 1] == b'*' {
                        state = FocalScanState::BlockComment;
                        i += 2;
                        continue;
                    }
                }

                // Strings/chars.
                if bytes[i] == b'"' {
                    if i + 2 < bytes.len() && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                        state = FocalScanState::TextBlock;
                        i += 3;
                    } else {
                        state = FocalScanState::String;
                        i += 1;
                    }
                    continue;
                }
                if bytes[i] == b'\'' {
                    state = FocalScanState::Char;
                    i += 1;
                    continue;
                }

                if is_ident_start(bytes[i]) {
                    let start = i;
                    i += 1;
                    while i < bytes.len() && is_ident_continue(bytes[i]) {
                        i += 1;
                    }

                    // Safe: identifier scanning only slices on ASCII boundaries.
                    let ident = &text[start..i];
                    let lower = ident.to_ascii_lowercase();
                    if SENSITIVE_ASSIGNMENT_KEY_SUBSTRINGS
                        .iter()
                        .any(|needle| lower.contains(needle))
                    {
                        let mut j = i;
                        while j < bytes.len() {
                            match bytes[j] {
                                b' ' | b'\t' | b'\r' => j += 1,
                                b'\n' => break,
                                _ => break,
                            }
                        }
                        if j < bytes.len() && matches!(bytes[j], b':' | b'=') {
                            line_has_sensitive = true;
                        }
                    }

                    continue;
                }

                i += 1;
            }
            FocalScanState::LineComment => {
                // Newlines are handled at the top of the loop.
                i += 1;
            }
            FocalScanState::BlockComment => {
                if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    state = FocalScanState::Normal;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            FocalScanState::String => {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if bytes[i] == b'"' {
                    state = FocalScanState::Normal;
                }
                i += 1;
            }
            FocalScanState::TextBlock => {
                if bytes[i] == b'"'
                    && i + 2 < bytes.len()
                    && bytes[i + 1] == b'"'
                    && bytes[i + 2] == b'"'
                {
                    state = FocalScanState::Normal;
                    i += 3;
                    continue;
                }
                i += 1;
            }
            FocalScanState::Char => {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if bytes[i] == b'\'' {
                    state = FocalScanState::Normal;
                }
                i += 1;
            }
        }
    }

    if line_has_sensitive {
        out.push(line_start..bytes.len());
    }

    out
}

fn identifier_looks_like_path_component(text: &str, start: usize, end: usize, tok: &str) -> bool {
    let bytes = text.as_bytes();
    if start > 0 {
        let prev = bytes[start - 1];
        if prev == b'/' || prev == b'\\' {
            return true;
        }
    }
    if end < bytes.len() {
        let next = bytes[end];
        if next == b'/' || next == b'\\' {
            return true;
        }
    }

    // IPv6 addresses contain hex-like "identifiers" separated by `:` characters. Avoid leaking
    // these fragments (e.g. `db8`) into semantic-search queries.
    if identifier_looks_like_ipv6_segment(text, start, end, tok) {
        return true;
    }

    // Host:port patterns (`localhost:8080`) are low-signal and can leak infrastructure metadata.
    if end + 1 < bytes.len() && bytes[end] == b':' && bytes[end + 1].is_ascii_digit() {
        return true;
    }

    // Treat URI schemes like `file:///...` or `https://...` as path-like so we don't end up with
    // low-signal queries such as `file` when the selection is primarily a path/URL.
    if end < bytes.len() && bytes[end] == b':' {
        // Common shape: `scheme:/...` or `scheme:\\...`.
        if end + 1 < bytes.len() && matches!(bytes[end + 1], b'/' | b'\\') {
            return true;
        }

        // Nested schemes like `jar:file:/...` should also be treated as path-like for the outer
        // scheme.
        let mut i = end + 1;
        if i < bytes.len() && is_ident_start(bytes[i]) {
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b':' {
                if i + 1 < bytes.len() && matches!(bytes[i + 1], b'/' | b'\\') {
                    return true;
                }
            }
        }
    }

    // If the identifier appears inside a token that itself is delimited by a path separator, treat
    // it as path-like even if the identifier isn't immediately adjacent to the separator. This
    // catches segments such as `/my-secret-project/` where the middle identifier (`secret`) would
    // otherwise be included.
    {
        let bounds = surrounding_token_bounds(text, start, end);
        if !bounds.is_empty() {
            let token = &text[bounds.clone()];
            if looks_like_email_address(token)
                || looks_like_ipv4_address(token)
                || looks_like_mac_address_token(token)
                || looks_like_uuid_token(token)
                || looks_like_jwt_token(token)
                || token_contains_obvious_secret_fragment(token)
                || token_contains_sensitive_assignment(token)
            {
                return true;
            }

            let before = bounds.start.checked_sub(1).and_then(|idx| bytes.get(idx));
            let after = bytes.get(bounds.end);
            let before_is_sep = before.is_some_and(|b| *b == b'/' || *b == b'\\');
            let after_is_sep = after.is_some_and(|b| *b == b'/' || *b == b'\\');
            if before_is_sep || after_is_sep {
                return true;
            }

            // URI schemes can include punctuation (e.g. `vscode-remote://...`). If the surrounding
            // token is immediately followed by `:/` or `:\\`, treat *all* identifiers within it as
            // path-like so we don't emit low-signal queries such as `vscode`.
            if after.is_some_and(|b| *b == b':')
                && bytes
                    .get(bounds.end + 1)
                    .is_some_and(|b| *b == b'/' || *b == b'\\')
            {
                return true;
            }
        }
    }

    // Skip file-name-like tokens such as `Secret-config.properties`. This uses a lightweight
    // "token" scan around the identifier and is careful to stop at quote boundaries so a string
    // literal like `".../Secret.java"` does not cause us to drop surrounding identifiers.
    if identifier_in_file_name_token(text, start, end, tok) {
        return true;
    }

    false
}

fn identifier_looks_like_ipv6_segment(text: &str, start: usize, end: usize, tok: &str) -> bool {
    // IPv6 segments are 1-4 hex digits.
    if tok.is_empty() || tok.len() > 4 {
        return false;
    }
    if !tok.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }

    let bytes = text.as_bytes();
    let prev_colon = start > 0 && bytes[start - 1] == b':';
    let next_colon = end < bytes.len() && bytes[end] == b':';
    if !(prev_colon || next_colon) {
        return false;
    }

    // Require at least two `:` bytes in the surrounding window so we don't treat single-colon
    // constructs like `label:` as IPv6.
    let window_start = start.saturating_sub(32);
    let window_end = (end + 32).min(bytes.len());
    let colon_count = bytes[window_start..window_end]
        .iter()
        .filter(|b| **b == b':')
        .count();
    colon_count >= 2
}

fn identifier_in_file_name_token(text: &str, start: usize, end: usize, _tok: &str) -> bool {
    let bounds = surrounding_token_bounds(text, start, end);
    if bounds.is_empty() {
        return false;
    }

    looks_like_file_name(&text[bounds])
}

fn surrounding_token_bounds(text: &str, start: usize, end: usize) -> Range<usize> {
    fn is_boundary(b: u8) -> bool {
        // Treat non-ASCII bytes as boundaries so we never slice on invalid UTF-8 boundaries.
        if !b.is_ascii() {
            return true;
        }
        b.is_ascii_whitespace()
            || matches!(
                b,
                // Quote boundaries keep string literals from suppressing surrounding identifiers.
                b'"' | b'\''
                // Path separators.
                | b'/' | b'\\'
                // Common punctuation that delimits file names in stack traces / logs.
                | b':' | b',' | b';'
                | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'<' | b'>'
            )
    }

    let bytes = text.as_bytes();
    let mut token_start = start.min(bytes.len());
    let mut token_end = end.min(bytes.len()).max(token_start);

    while token_start > 0 && !is_boundary(bytes[token_start - 1]) {
        token_start -= 1;
    }
    while token_end < bytes.len() && !is_boundary(bytes[token_end]) {
        token_end += 1;
    }

    token_start..token_end
}

fn looks_like_file_name(token: &str) -> bool {
    // Keep this conservative: only treat well-known source/doc extensions as file paths.
    // Trim common leading/trailing punctuation (e.g. `Foo.java.` at end of sentence) before
    // extension detection, while preserving internal `.` characters for qualified names / file
    // names.
    //
    // Note: `trim_matches` only trims at the edges, so this does not remove the `.` that separates
    // a base name from its extension.
    let token = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());

    let Some((_base, ext_raw)) = token.rsplit_once('.') else {
        return false;
    };

    // Allow suffixes like `Foo.java:123` by only considering the leading alphanumeric run of the
    // extension.
    let ext_end = ext_raw
        .as_bytes()
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric())
        .count();
    if ext_end == 0 {
        return false;
    }
    let ext = ext_raw[..ext_end].to_ascii_lowercase();

    is_known_file_extension(&ext)
}

fn is_known_file_extension(ext: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        "java",
        "kt",
        "kts",
        "md",
        "gradle",
        "xml",
        "json",
        "toml",
        "yml",
        "yaml",
        "txt",
        "properties",
    ];

    let lower = ext.to_ascii_lowercase();
    EXTENSIONS.iter().any(|candidate| lower == *candidate)
}

fn related_code_query_fallback(focal_code: &str) -> String {
    let redacted = crate::privacy::redact_file_paths(focal_code);
    if focal_code_contains_sensitive_assignment(&redacted) {
        return String::new();
    }
    let mut out = String::new();

    for tok in redacted.split_whitespace() {
        let tok = clean_query_word(tok);
        if tok.is_empty() {
            continue;
        }
        // Avoid sending obvious `key=value` credential-like strings as semantic-search queries.
        if tok.contains('=') {
            continue;
        }
        // Avoid sending obvious secret/token strings as semantic-search queries. This is
        // intentionally conservative: if we see a secret-like substring (e.g. a JSON token that
        // includes `"apiKey":"sk-..."`), skip the entire whitespace token.
        if token_contains_secret_fragment(tok) {
            continue;
        }
        // Numeric literals are very low-signal as embedding queries and can contain unique IDs
        // (e.g. `0xDEADBEEF`) that we should not send to providers.
        if looks_like_numeric_literal_token(tok) {
            continue;
        }
        // Avoid sending phone/SSN-like delimited number tokens (e.g. `123-45-6789`,
        // `1-202-555-0143`). These are low-signal for semantic search and can leak PII.
        if looks_like_delimited_number_token(tok) {
            continue;
        }
        // Network endpoints (IPv6, host:port) are similarly low-signal and can leak infrastructure
        // metadata.
        if looks_like_ipv6_address_token(tok) || looks_like_host_port_token(tok) {
            continue;
        }
        // Hardware/network addresses are similarly low-signal and can leak infrastructure metadata.
        if looks_like_mac_address_token(tok) {
            continue;
        }
        if tok
            .bytes()
            .all(|b| b == b'_' || b == b'$' || b.is_ascii_digit())
        {
            // Purely numeric / underscore / dollar tokens are very low signal and tend to produce
            // noisy trigram matches.
            continue;
        }

        if tok.len() < 2 {
            continue;
        }

        let keep_short = tok.bytes().any(|b| b.is_ascii_uppercase());
        let keep_short = keep_short && tok.len() >= 2;
        if tok.len() < 3 && !keep_short {
            continue;
        }

        // Avoid leaking file paths (absolute or relative) via the query text.
        if tok.contains('/') || tok.contains('\\') {
            continue;
        }

        let lower = tok.to_ascii_lowercase();
        if matches!(lower.as_str(), "path" | "redacted") {
            continue;
        }
        if is_semantic_query_stop_word(lower.as_str()) {
            continue;
        }

        if looks_like_file_name(tok) {
            continue;
        }

        if !push_query_token(&mut out, tok, RELATED_CODE_QUERY_MAX_BYTES) {
            break;
        }
    }

    let out = out.trim();
    out.to_string()
}

fn looks_like_numeric_literal_token(tok: &str) -> bool {
    fn consume_digits(bytes: &[u8], mut i: usize, is_digit: impl Fn(u8) -> bool) -> usize {
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'_' || is_digit(b) {
                i += 1;
                continue;
            }
            break;
        }
        i
    }

    let bytes = tok.as_bytes();
    if bytes.is_empty() {
        return false;
    }

    // Hex/binary literals.
    if bytes.len() >= 3 && bytes[0] == b'0' {
        match bytes[1] {
            b'x' | b'X' => {
                let mut i = 2;
                let digits_start = i;
                i = consume_digits(bytes, i, |b| b.is_ascii_hexdigit());
                if i == digits_start {
                    return false;
                }

                // Optional fractional part.
                if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_hexdigit() {
                    i += 1;
                    let frac_start = i;
                    i = consume_digits(bytes, i, |b| b.is_ascii_hexdigit());
                    if i == frac_start {
                        return false;
                    }
                }

                // Hex float exponent.
                if i < bytes.len() && matches!(bytes[i], b'p' | b'P') {
                    i += 1;
                    if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
                        i += 1;
                    }
                    let exp_digits = i;
                    i = consume_digits(bytes, i, |b| b.is_ascii_digit());
                    if i == exp_digits {
                        return false;
                    }
                    if i < bytes.len() && matches!(bytes[i], b'f' | b'F' | b'd' | b'D') {
                        i += 1;
                    }
                    return i == bytes.len();
                }

                // Integer suffix.
                if i < bytes.len() && matches!(bytes[i], b'l' | b'L') {
                    i += 1;
                }
                return i == bytes.len();
            }
            b'b' | b'B' => {
                let mut i = 2;
                let digits_start = i;
                i = consume_digits(bytes, i, |b| matches!(b, b'0' | b'1'));
                if i == digits_start {
                    return false;
                }
                if i < bytes.len() && matches!(bytes[i], b'l' | b'L') {
                    i += 1;
                }
                return i == bytes.len();
            }
            _ => {}
        }
    }

    // Decimal literals.
    if !bytes[0].is_ascii_digit() {
        return false;
    }

    let mut i = 0usize;
    i = consume_digits(bytes, i, |b| b.is_ascii_digit());

    // Optional fractional part.
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        i = consume_digits(bytes, i, |b| b.is_ascii_digit());
    }

    // Optional exponent.
    if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
        i += 1;
        if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
            i += 1;
        }
        let exp_digits = i;
        i = consume_digits(bytes, i, |b| b.is_ascii_digit());
        if i == exp_digits {
            return false;
        }
    }

    if i < bytes.len() && matches!(bytes[i], b'f' | b'F' | b'd' | b'D' | b'l' | b'L') {
        i += 1;
    }

    i == bytes.len()
}

fn looks_like_delimited_number_token(tok: &str) -> bool {
    let bytes = tok.as_bytes();
    if bytes.is_empty() {
        return false;
    }

    let mut digits = 0usize;
    let mut separators = 0usize;
    for &b in bytes {
        if b.is_ascii_digit() {
            digits += 1;
            continue;
        }
        if matches!(b, b'+' | b'-' | b'.' | b'(' | b')') {
            separators += 1;
            continue;
        }
        return false;
    }

    digits >= 6 && separators > 0
}

fn looks_like_host_port_token(tok: &str) -> bool {
    let (host, port) = match tok.split_once(':') {
        Some(parts) => parts,
        None => return false,
    };
    if host.is_empty() || port.is_empty() {
        return false;
    }
    // IPv6 uses multiple `:` separators.
    if tok.as_bytes().iter().filter(|b| **b == b':').count() != 1 {
        return false;
    }
    if !host.bytes().any(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    if host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return false;
    }
    if port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Ok(port_num) = port.parse::<u32>() else {
        return false;
    };
    (1..=65_535).contains(&port_num)
}

fn looks_like_ipv6_address_token(tok: &str) -> bool {
    let mut token = tok;
    if let Some(idx) = token.find(']') {
        token = &token[..idx];
    }
    if token.starts_with('[') {
        token = &token[1..];
    }
    if let Some(idx) = token.find('%') {
        token = &token[..idx];
    }
    if token.is_empty() {
        return false;
    }

    let bytes = token.as_bytes();
    let colon_count = bytes.iter().filter(|b| **b == b':').count();
    if colon_count < 2 {
        return false;
    }

    // Avoid obviously invalid tokens.
    if bytes
        .windows(3)
        .any(|window| window == [b':', b':', b':'])
    {
        return false;
    }

    // Common embedded IPv4 form: `::ffff:192.168.0.1`.
    if token.contains('.') {
        if let Some(last) = token.rsplit(':').next() {
            if looks_like_ipv4_address(last) {
                return true;
            }
        }
        // For other `:`+`.` tokens, treat them as non-IPv6 and let other heuristics handle them.
        return false;
    }

    let mut double_colon_runs = 0usize;
    for (idx, window) in bytes.windows(2).enumerate() {
        if window == [b':', b':'] && (idx == 0 || bytes[idx - 1] != b':') {
            double_colon_runs += 1;
        }
    }
    if double_colon_runs > 1 {
        return false;
    }
    let has_double_colon = double_colon_runs == 1;

    let mut segments = 0usize;
    for part in token.split(':') {
        if part.is_empty() {
            continue;
        }
        if part.len() > 4 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
        segments += 1;
        if segments > 8 {
            return false;
        }
    }

    if has_double_colon {
        segments <= 8
    } else {
        segments == 8
    }
}

fn looks_like_mac_address_token(tok: &str) -> bool {
    let token = tok.trim_matches(|c: char| !(c.is_ascii_hexdigit() || matches!(c, ':' | '-' | '.')));
    if token.is_empty() {
        return false;
    }

    let has_colon = token.contains(':');
    let has_dash = token.contains('-');
    if has_colon || has_dash {
        if has_colon && has_dash {
            return false;
        }
        let sep = if has_colon { ':' } else { '-' };
        let mut segments = 0usize;
        for part in token.split(sep) {
            segments += 1;
            if segments > 6 {
                return false;
            }
            if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
                return false;
            }
        }
        return segments == 6;
    }

    if token.contains('.') {
        let mut segments = 0usize;
        for part in token.split('.') {
            segments += 1;
            if segments > 3 {
                return false;
            }
            if part.len() != 4 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
                return false;
            }
        }
        return segments == 3;
    }

    false
}

fn looks_like_uuid_token(tok: &str) -> bool {
    let token = tok.trim_matches(|c: char| !c.is_ascii_hexdigit() && c != '-');
    if token.len() != 36 {
        return false;
    }
    let mut parts = token.split('-');
    let expected = [8usize, 4, 4, 4, 12];
    for &len in &expected {
        let Some(part) = parts.next() else {
            return false;
        };
        if part.len() != len || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    parts.next().is_none()
}

fn looks_like_jwt_token(tok: &str) -> bool {
    let token = tok.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')));
    if token.len() < 60 {
        return false;
    }

    let mut parts = token.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    let Some(second) = parts.next() else {
        return false;
    };
    let Some(third) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }

    // Typical JWTs base64url-encode a JSON header, which starts with `{"` and therefore encodes to
    // a string that begins with `eyJ`. This reduces false positives on dotted package/class names.
    if !first.starts_with("eyJ") {
        return false;
    }

    fn is_base64url_segment(seg: &str) -> bool {
        seg.len() >= 10
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    }

    is_base64url_segment(first) && is_base64url_segment(second) && is_base64url_segment(third)
}

fn push_query_token(out: &mut String, tok: &str, max_bytes: usize) -> bool {
    if out.len() >= max_bytes {
        return false;
    }

    if out.is_empty() {
        let tok = truncate_utf8_to_bytes(tok, max_bytes);
        out.push_str(tok);
        return !tok.is_empty();
    }

    let space = 1usize;
    if out.len().saturating_add(space) >= max_bytes {
        return false;
    }
    let remaining = max_bytes - out.len() - space;
    if remaining == 0 {
        return false;
    }

    // Only add a token if it fits without truncation; truncating mid-token tends to produce very
    // low-signal fragments.
    if tok.len() > remaining {
        return false;
    }

    out.push(' ');
    out.push_str(tok);
    true
}

fn truncate_utf8_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn clean_query_word(tok: &str) -> &str {
    tok.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
}

fn token_contains_sensitive_assignment(tok: &str) -> bool {
    if !tok.contains('=') {
        return false;
    }

    let lower = tok.to_ascii_lowercase();
    lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("secret")
        || lower.contains("token")
        || lower.contains("api_key")
        || lower.contains("apikey")
}

fn token_contains_obvious_secret_fragment(tok: &str) -> bool {
    fn is_token_char(c: char) -> bool {
        // Keep this conservative: split on `=`/`:` so key-value patterns are broken into fragments.
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+' | '@')
    }

    if tok.is_empty() {
        return false;
    }

    tok.split(|c: char| !is_token_char(c))
        .filter(|segment| !segment.is_empty())
        .any(looks_like_obvious_secret_token)
}

fn token_contains_secret_fragment(tok: &str) -> bool {
    fn is_token_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '=' | '+' | '/' | '.' | '@')
    }

    if tok.is_empty() {
        return false;
    }

    tok.split(|c: char| !is_token_char(c))
        .filter(|segment| !segment.is_empty())
        .any(looks_like_secret_token)
}

fn looks_like_obvious_secret_token(tok: &str) -> bool {
    let trimmed = tok.trim();
    if trimmed.is_empty() {
        return false;
    }

    if looks_like_email_address(trimmed) {
        return true;
    }
    if looks_like_ipv4_address(trimmed) {
        return true;
    }

    if trimmed.starts_with("sk-") && trimmed.len() >= 20 {
        return true;
    }

    if trimmed.starts_with("AKIA") && trimmed.len() >= 16 {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("ghp_") && trimmed.len() >= 20 {
        return true;
    }

    if trimmed.contains("-----BEGIN") {
        return true;
    }

    false
}

fn looks_like_secret_token(tok: &str) -> bool {
    let trimmed = tok.trim();
    if trimmed.is_empty() {
        return false;
    }

    if looks_like_email_address(trimmed) {
        return true;
    }
    if looks_like_ipv4_address(trimmed) {
        return true;
    }

    if trimmed.starts_with("sk-") && trimmed.len() >= 20 {
        return true;
    }

    if trimmed.starts_with("AKIA") && trimmed.len() >= 16 {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("ghp_") && trimmed.len() >= 20 {
        return true;
    }

    if trimmed.contains("-----BEGIN") {
        return true;
    }

    // Heuristic: long-ish base64/hex-ish strings.
    trimmed.len() >= 32 && is_mostly_alnum_or_symbols(trimmed)
}

fn looks_like_email_address(token: &str) -> bool {
    let token = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let Some((local, domain)) = token.split_once('@') else {
        return false;
    };
    if local.is_empty() || domain.is_empty() {
        return false;
    }
    // Avoid treating Java annotations like `@Override` as email-like tokens.
    if local.is_empty() && token.starts_with('@') {
        return false;
    }
    if domain.starts_with('@') {
        return false;
    }
    if domain.starts_with('.') || domain.ends_with('.') {
        return false;
    }
    if !domain.contains('.') {
        return false;
    }

    let local_ok = local.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-')
    });
    if !local_ok {
        return false;
    }
    let domain_ok = domain
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'));
    if !domain_ok {
        return false;
    }

    let tld = domain.rsplit('.').next().unwrap_or("");
    if tld.len() < 2 || tld.len() > 24 {
        return false;
    }
    if !tld.bytes().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }

    true
}

fn looks_like_ipv4_address(token: &str) -> bool {
    let token = token.trim_matches(|c: char| !(c.is_ascii_digit() || c == '.' || c == ':'));
    let ip = token.split_once(':').map(|(ip, _port)| ip).unwrap_or(token);
    let mut parts = ip.split('.');

    let mut count = 0usize;
    while let Some(part) = parts.next() {
        count += 1;
        if count > 4 {
            return false;
        }
        if part.is_empty() || part.len() > 3 {
            return false;
        }
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let Ok(num) = part.parse::<u16>() else {
            return false;
        };
        if num > 255 {
            return false;
        }
    }

    count == 4
}

fn is_mostly_alnum_or_symbols(s: &str) -> bool {
    let mut good = 0usize;
    let mut total = 0usize;

    for c in s.chars() {
        total += 1;
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '=' | '+' | '/' | '.') {
            good += 1;
        }
    }

    // Avoid redacting natural language strings; require the vast majority to be "token-like".
    total > 0 && good * 100 / total >= 95
}

fn is_semantic_query_stop_word(ident: &str) -> bool {
    // Java keywords + common literals.
    matches!(
        ident,
        // Keywords
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "void"
            | "volatile"
            | "while"
            // Newer Java keywords/types (kept as stop words to avoid noise)
            | "var"
            | "record"
            | "yield"
            | "sealed"
            | "permits"
            // Literals
            | "true"
            | "false"
            | "null"
    )
}

fn semantic_query_token_score(tok: &str) -> i32 {
    let len = tok.len() as i32;
    let mut score = len;

    let bytes = tok.as_bytes();
    let starts_upper = bytes.first().is_some_and(|b| b.is_ascii_uppercase());
    let has_lower = bytes.iter().any(|b| b.is_ascii_lowercase());
    let has_upper = bytes.iter().any(|b| b.is_ascii_uppercase());
    let internal_upper = bytes.iter().skip(1).any(|b| b.is_ascii_uppercase());

    // Prefer CamelCase/PascalCase tokens with internal word boundaries; they are usually more
    // specific than ubiquitous types like `String`.
    if internal_upper && has_lower {
        score += 25;
    } else if starts_upper && has_lower {
        score += 5;
    } else if has_upper && !has_lower {
        // Acronyms / constants (e.g. `URL`, `MAX_VALUE`) are often low-signal; keep a small boost
        // so they don't dominate the query.
        score += 8;
    }
    if tok.contains('_') {
        score += 3;
    }
    if bytes.iter().any(|b| b.is_ascii_digit()) {
        score += 2;
    }

    score
}

fn extract_identifier_candidates(text: &str) -> Vec<IdentCandidate<'_>> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut state = FocalScanState::Normal;
    let mut out: Vec<IdentCandidate<'_>> = Vec::new();

    while i < bytes.len() {
        match state {
            FocalScanState::Normal => {
                // Comments.
                if bytes[i] == b'/' && i + 1 < bytes.len() {
                    if bytes[i + 1] == b'/' {
                        state = FocalScanState::LineComment;
                        i += 2;
                        continue;
                    }
                    if bytes[i + 1] == b'*' {
                        state = FocalScanState::BlockComment;
                        i += 2;
                        continue;
                    }
                }

                // Strings/chars.
                if bytes[i] == b'"' {
                    if i + 2 < bytes.len() && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                        // Java text blocks (`""" ... """`) can contain large SQL/JSON snippets that
                        // are usually low-signal for semantic code search. Treat them like string
                        // literals so we don't accidentally flood the query with their contents.
                        state = FocalScanState::TextBlock;
                        i += 3;
                    } else {
                        state = FocalScanState::String;
                        i += 1;
                    }
                    continue;
                }
                if bytes[i] == b'\'' {
                    state = FocalScanState::Char;
                    i += 1;
                    continue;
                }

                // Numeric literals can contain alphabetic characters (`0xDEADBEEF`, `1e10`,
                // `0x1.ffffp10`) which would otherwise be misclassified as identifier candidates.
                if bytes[i].is_ascii_digit() {
                    i = skip_number_literal(bytes, i);
                    continue;
                }

                if is_ident_start(bytes[i]) {
                    // Avoid capturing numeric-literal fragments like `123abc` or `0xDEADBEEF` as
                    // identifiers; this is noise at best and can leak potentially sensitive IDs.
                    if i > 0 && bytes[i - 1].is_ascii_digit() {
                        i += 1;
                        while i < bytes.len() && is_ident_continue(bytes[i]) {
                            i += 1;
                        }
                        continue;
                    }

                    let start = i;
                    i += 1;
                    while i < bytes.len() && is_ident_continue(bytes[i]) {
                        i += 1;
                    }
                    // Safe: we only slice on ASCII boundaries.
                    let token = &text[start..i];
                    out.push(IdentCandidate {
                        token,
                        first_pos: start,
                    });
                    continue;
                }

                i += 1;
            }
            FocalScanState::LineComment => {
                if bytes[i] == b'\n' {
                    state = FocalScanState::Normal;
                }
                i += 1;
            }
            FocalScanState::BlockComment => {
                if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    state = FocalScanState::Normal;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            FocalScanState::String => {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if bytes[i] == b'"' {
                    state = FocalScanState::Normal;
                }
                i += 1;
            }
            FocalScanState::TextBlock => {
                if bytes[i] == b'"'
                    && i + 2 < bytes.len()
                    && bytes[i + 1] == b'"'
                    && bytes[i + 2] == b'"'
                {
                    state = FocalScanState::Normal;
                    i += 3;
                    continue;
                }
                i += 1;
            }
            FocalScanState::Char => {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if bytes[i] == b'\'' {
                    state = FocalScanState::Normal;
                }
                i += 1;
            }
        }
    }

    out
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn skip_number_literal(bytes: &[u8], start: usize) -> usize {
    fn consume_digits(bytes: &[u8], mut i: usize, is_digit: impl Fn(u8) -> bool) -> usize {
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'_' || is_digit(b) {
                i += 1;
                continue;
            }
            break;
        }
        i
    }

    let mut i = start;
    if i >= bytes.len() {
        return i;
    }
    if !bytes[i].is_ascii_digit() {
        return i.saturating_add(1).min(bytes.len());
    }

    // Hex/binary prefixes.
    if bytes[i] == b'0' && i + 1 < bytes.len() {
        match bytes[i + 1] {
            b'x' | b'X' => {
                i += 2;
                let digits_start = i;
                i = consume_digits(bytes, i, |b| b.is_ascii_hexdigit());
                if i == digits_start {
                    return start + 1;
                }

                // Hex floats: optional fractional part.
                if i + 1 < bytes.len()
                    && bytes[i] == b'.'
                    && bytes[i + 1].is_ascii_hexdigit()
                {
                    i += 1;
                    i = consume_digits(bytes, i, |b| b.is_ascii_hexdigit());
                }

                // Hex float exponent.
                if i < bytes.len() && matches!(bytes[i], b'p' | b'P') {
                    let exp_pos = i;
                    i += 1;
                    if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
                        i += 1;
                    }
                    let exp_digits = i;
                    i = consume_digits(bytes, i, |b| b.is_ascii_digit());
                    if i == exp_digits {
                        return exp_pos;
                    }
                    if i < bytes.len() && matches!(bytes[i], b'f' | b'F' | b'd' | b'D') {
                        i += 1;
                    }
                    return i;
                }

                // Integer suffix.
                if i < bytes.len() && matches!(bytes[i], b'l' | b'L') {
                    i += 1;
                }
                return i;
            }
            b'b' | b'B' => {
                i += 2;
                let digits_start = i;
                i = consume_digits(bytes, i, |b| matches!(b, b'0' | b'1'));
                if i == digits_start {
                    return start + 1;
                }
                if i < bytes.len() && matches!(bytes[i], b'l' | b'L') {
                    i += 1;
                }
                return i;
            }
            _ => {}
        }
    }

    // Decimal digits.
    i = consume_digits(bytes, i, |b| b.is_ascii_digit());

    // Fractional part: only treat `.` as part of the number if it is followed by a digit so we
    // don't swallow Kotlin-style calls like `1.toString()`.
    if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
        i += 1;
        i = consume_digits(bytes, i, |b| b.is_ascii_digit());
    }

    // Exponent (scientific notation).
    if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
        let exp_pos = i;
        let mut j = i + 1;
        if j < bytes.len() && matches!(bytes[j], b'+' | b'-') {
            j += 1;
        }
        let exp_digits = j;
        j = consume_digits(bytes, j, |b| b.is_ascii_digit());
        if j > exp_digits {
            i = j;
        } else {
            i = exp_pos;
        }
    }

    // Decimal suffixes.
    if i < bytes.len() && matches!(bytes[i], b'f' | b'F' | b'd' | b'D' | b'l' | b'L') {
        i += 1;
    }

    i.max(start + 1)
}

#[derive(Debug, Clone)]
pub struct RelatedSymbol {
    pub name: String,
    pub kind: String,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct RelatedCode {
    pub path: PathBuf,
    pub range: Range<usize>,
    pub kind: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextDiagnosticSeverity {
    Error,
    Warning,
    Info,
}

impl ContextDiagnosticSeverity {
    fn as_str(self) -> &'static str {
        match self {
            ContextDiagnosticSeverity::Error => "error",
            ContextDiagnosticSeverity::Warning => "warning",
            ContextDiagnosticSeverity::Info => "info",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextDiagnosticKind {
    Syntax,
    Type,
    Lint,
    Other,
}

impl ContextDiagnosticKind {
    fn as_str(self) -> &'static str {
        match self {
            ContextDiagnosticKind::Syntax => "syntax",
            ContextDiagnosticKind::Type => "type",
            ContextDiagnosticKind::Lint => "lint",
            ContextDiagnosticKind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextDiagnostic {
    pub file: Option<String>,
    pub range: Option<PositionRange>,
    pub severity: ContextDiagnosticSeverity,
    pub message: String,
    pub kind: Option<ContextDiagnosticKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSectionStat {
    pub title: String,
    pub token_estimate: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltContext {
    pub text: String,
    pub token_count: usize,
    pub truncated: bool,
    pub sections: Vec<ContextSectionStat>,
}

#[derive(Debug, Clone)]
struct ExtractedJavaContext {
    enclosing_context: Option<String>,
    doc_comment: Option<String>,
    related_symbols: Vec<RelatedSymbol>,
}

fn clamp_range(range: Range<usize>, len: usize) -> Range<usize> {
    let start = range.start.min(len);
    let end = range.end.min(len).max(start);
    start..end
}

fn clamp_range_to_char_boundaries(text: &str, range: Range<usize>) -> Range<usize> {
    let mut start = range.start.min(text.len());
    let mut end = range.end.min(text.len()).max(start);

    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }

    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }

    start..end
}

fn analyze_java_context(
    source: &str,
    selection: Range<usize>,
    focal_code: &str,
    include_doc_comments: bool,
) -> ExtractedJavaContext {
    use nova_syntax::java;

    if source.is_empty() {
        return ExtractedJavaContext {
            enclosing_context: None,
            doc_comment: None,
            related_symbols: Vec::new(),
        };
    }

    let selection = clamp_range(selection, source.len());
    let offset = selection.start.min(source.len());
    let parsed = java::parse(source);
    let unit = parsed.compilation_unit();

    let enclosing_type = find_enclosing_type(&unit.types, offset);
    let enclosing_callable = enclosing_type.and_then(|ty| find_enclosing_callable(ty, offset));

    let mut parts: Vec<String> = Vec::new();
    if let Some(pkg) = unit.package.as_ref() {
        parts.push(format!("// Package\npackage {};", pkg.name));
    }

    if !unit.imports.is_empty() {
        let imports: Vec<String> = unit.imports.iter().map(render_import_decl).collect();
        parts.push(format!("// Imports\n{}", imports.join("\n")));
    }

    if let Some(ty) = enclosing_type {
        parts.push(format!(
            "// Enclosing type (skeleton)\n{}",
            render_type_skeleton(ty)
        ));
    }

    if let Some(callable) = enclosing_callable.as_ref() {
        parts.push(format!(
            "// Enclosing member (skeleton)\n{}",
            render_callable_skeleton(callable)
        ));
    }

    let doc_comment = if include_doc_comments {
        enclosing_callable
            .as_ref()
            .and_then(|callable| find_doc_comment_before_offset(source, callable.range_start()))
            .or_else(|| {
                enclosing_type
                    .and_then(|ty| find_doc_comment_before_offset(source, ty.range().start))
            })
    } else {
        None
    };

    let mut decls = Vec::new();
    collect_symbol_decls(&unit.types, &mut decls, None);

    let mut exclude = HashSet::new();
    if let Some(ty) = enclosing_type {
        exclude.insert(ty.name().to_string());
    }
    if let Some(callable) = enclosing_callable.as_ref() {
        exclude.insert(callable.name().to_string());
    }

    let decl_names: HashSet<&str> = decls.iter().map(|decl| decl.name.as_str()).collect();
    let referenced = extract_referenced_identifiers(focal_code, &exclude, &decl_names);
    let related_symbols = related_symbols_from_references(&referenced, &decls);

    ExtractedJavaContext {
        enclosing_context: if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        },
        doc_comment,
        related_symbols,
    }
}

fn find_doc_comment_before_offset(source: &str, offset: usize) -> Option<String> {
    use nova_syntax::SyntaxKind;

    let tokens = nova_syntax::lex(source);
    let mut idx = 0usize;
    while idx < tokens.len() {
        let end = tokens[idx].range.end as usize;
        if end > offset {
            break;
        }
        idx += 1;
    }

    while idx > 0 {
        idx -= 1;
        let tok = &tokens[idx];
        match tok.kind {
            SyntaxKind::Whitespace | SyntaxKind::LineComment | SyntaxKind::BlockComment => continue,
            SyntaxKind::DocComment => return Some(tok.text(source).to_string()),
            _ => break,
        }
    }

    None
}

fn format_diagnostics(req: &ContextRequest) -> Option<String> {
    let mut out = String::new();
    let mut first = true;

    for diag in req
        .diagnostics
        .iter()
        .filter(|diag| diagnostic_is_relevant(diag, req.file_path.as_deref(), req.cursor))
    {
        if !first {
            out.push('\n');
        }
        first = false;

        out.push('[');
        out.push_str(diag.severity.as_str());
        out.push(']');
        if let Some(kind) = diag.kind {
            out.push('[');
            out.push_str(kind.as_str());
            out.push(']');
        }

        if req.privacy.include_file_paths {
            if let Some(file) = diag.file.as_deref() {
                out.push(' ');
                out.push_str(file);
            }
        }

        if let Some(range) = diag.range.as_ref() {
            out.push_str(&format!(
                " L{}:{}-L{}:{}",
                range.start.line + 1,
                range.start.character + 1,
                range.end.line + 1,
                range.end.character + 1
            ));
        }

        out.push_str(": ");
        out.push_str(&diag.message);
    }

    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn diagnostic_is_relevant(
    diag: &ContextDiagnostic,
    file_path: Option<&str>,
    cursor: Option<Position>,
) -> bool {
    if let Some(file_path) = file_path {
        if let Some(file) = diag.file.as_deref() {
            if file != file_path {
                return false;
            }
        }
    }

    let Some(cursor) = cursor else {
        return true;
    };
    let Some(range) = diag.range.as_ref() else {
        return true;
    };
    range_contains(range, cursor)
}

fn range_contains(range: &PositionRange, pos: Position) -> bool {
    if pos.line < range.start.line || pos.line > range.end.line {
        return false;
    }

    if pos.line == range.start.line && pos.character < range.start.character {
        return false;
    }

    if pos.line == range.end.line && pos.character > range.end.character {
        return false;
    }

    true
}

#[derive(Debug, Clone)]
struct BuiltSection {
    text: String,
    token_estimate: usize,
    truncated: bool,
    stat: ContextSectionStat,
}

fn build_section(
    title: &str,
    raw_content: &str,
    remaining: usize,
    anonymizer: &mut CodeAnonymizer,
    always_include: bool,
) -> BuiltSection {
    if remaining == 0 {
        let truncated = !raw_content.trim().is_empty();
        return BuiltSection {
            text: String::new(),
            token_estimate: 0,
            truncated,
            stat: ContextSectionStat {
                title: title.to_string(),
                token_estimate: 0,
                truncated,
            },
        };
    }

    let header = format!("## {title}\n");
    let header_tokens = estimate_tokens(&header);

    if header_tokens >= remaining {
        if !always_include {
            let truncated = !raw_content.trim().is_empty();
            return BuiltSection {
                text: String::new(),
                token_estimate: 0,
                truncated,
                stat: ContextSectionStat {
                    title: title.to_string(),
                    token_estimate: 0,
                    truncated,
                },
            };
        }

        let text = truncate_to_tokens(&header, remaining);
        let token_estimate = estimate_tokens(&text);
        let stat = ContextSectionStat {
            title: title.to_string(),
            token_estimate,
            truncated: true,
        };
        return BuiltSection {
            text,
            token_estimate,
            truncated: true,
            stat,
        };
    }

    let content = anonymizer.anonymize(raw_content);
    let allowed_tokens = remaining.saturating_sub(header_tokens);
    let current_tokens = estimate_tokens(&content);
    let content_truncated = current_tokens > allowed_tokens;
    let content = truncate_to_tokens(&content, allowed_tokens);
    let text = format!("{header}{content}\n\n");

    let token_estimate = estimate_tokens(&text);
    let stat = ContextSectionStat {
        title: title.to_string(),
        token_estimate,
        truncated: content_truncated,
    };
    BuiltSection {
        text,
        token_estimate,
        truncated: content_truncated,
        stat,
    }
}

fn estimate_tokens(text: &str) -> usize {
    let mut tokens = 0usize;
    let mut in_word = false;

    for ch in text.chars() {
        if ch.is_whitespace() {
            in_word = false;
            continue;
        }

        if is_word_char(ch) {
            if !in_word {
                tokens += 1;
                in_word = true;
            }
        } else {
            tokens += 1;
            in_word = false;
        }
    }

    tokens
}

fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    if max_tokens == 0 {
        return String::new();
    }

    let mut token_count = 0usize;
    let mut in_word = false;
    let mut last_good_end = 0usize;

    for (idx, ch) in text.char_indices() {
        if ch.is_whitespace() {
            in_word = false;
            continue;
        }

        if is_word_char(ch) {
            if !in_word {
                token_count += 1;
                if token_count > max_tokens {
                    break;
                }
                in_word = true;
            }
        } else {
            token_count += 1;
            if token_count > max_tokens {
                break;
            }
            in_word = false;
        }

        last_good_end = idx + ch.len_utf8();
    }

    text[..last_good_end].to_string()
}

fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
}

fn position_for_offset(text: &str, offset: usize) -> Position {
    let offset = offset.min(text.len());
    let offset_u32 = u32::try_from(offset).unwrap_or(u32::MAX);
    let index = LineIndex::new(text);
    let pos = index.position(text, TextSize::from(offset_u32));
    Position {
        line: pos.line,
        character: pos.character,
    }
}

fn render_import_decl(imp: &nova_syntax::java::ast::ImportDecl) -> String {
    let mut out = String::new();
    out.push_str("import ");
    if imp.is_static {
        out.push_str("static ");
    }
    out.push_str(&imp.path);
    if imp.is_star {
        out.push_str(".*");
    }
    out.push(';');
    out
}

fn find_enclosing_type<'a>(
    types: &'a [nova_syntax::java::ast::TypeDecl],
    offset: usize,
) -> Option<&'a nova_syntax::java::ast::TypeDecl> {
    for ty in types {
        let range = ty.range();
        if !span_contains(range.start, range.end, offset) {
            continue;
        }

        if let Some(nested) = find_enclosing_type_in_members(ty.members(), offset) {
            return Some(nested);
        }
        return Some(ty);
    }
    None
}

fn find_enclosing_type_in_members<'a>(
    members: &'a [nova_syntax::java::ast::MemberDecl],
    offset: usize,
) -> Option<&'a nova_syntax::java::ast::TypeDecl> {
    for member in members {
        let nova_syntax::java::ast::MemberDecl::Type(ty) = member else {
            continue;
        };
        let range = ty.range();
        if !span_contains(range.start, range.end, offset) {
            continue;
        }
        if let Some(nested) = find_enclosing_type_in_members(ty.members(), offset) {
            return Some(nested);
        }
        return Some(ty);
    }
    None
}

#[derive(Debug, Clone, Copy)]
enum EnclosingCallable<'a> {
    Method(&'a nova_syntax::java::ast::MethodDecl),
    Constructor(&'a nova_syntax::java::ast::ConstructorDecl),
}

impl<'a> EnclosingCallable<'a> {
    fn name(self) -> &'a str {
        match self {
            EnclosingCallable::Method(m) => &m.name,
            EnclosingCallable::Constructor(c) => &c.name,
        }
    }

    fn range_start(self) -> usize {
        match self {
            EnclosingCallable::Method(m) => m.range.start,
            EnclosingCallable::Constructor(c) => c.range.start,
        }
    }
}

fn find_enclosing_callable<'a>(
    ty: &'a nova_syntax::java::ast::TypeDecl,
    offset: usize,
) -> Option<EnclosingCallable<'a>> {
    for member in ty.members() {
        match member {
            nova_syntax::java::ast::MemberDecl::Method(method) => {
                if span_contains(method.range.start, method.range.end, offset) {
                    return Some(EnclosingCallable::Method(method));
                }
            }
            nova_syntax::java::ast::MemberDecl::Constructor(cons) => {
                if span_contains(cons.range.start, cons.range.end, offset) {
                    return Some(EnclosingCallable::Constructor(cons));
                }
            }
            _ => {}
        }
    }
    None
}

fn span_contains(span_start: usize, span_end: usize, offset: usize) -> bool {
    offset >= span_start && offset < span_end
}

fn render_type_skeleton(ty: &nova_syntax::java::ast::TypeDecl) -> String {
    let mut out = String::new();
    out.push_str(type_kind_keyword(ty));
    out.push(' ');
    out.push_str(ty.name());
    out.push_str(" {\n");

    let mut wrote_member = false;
    for member in ty.members() {
        match member {
            nova_syntax::java::ast::MemberDecl::Field(field) => {
                wrote_member = true;
                out.push_str("  ");
                out.push_str(&field.ty.text);
                out.push(' ');
                out.push_str(&field.name);
                out.push_str(";\n");
            }
            nova_syntax::java::ast::MemberDecl::Type(nested) => {
                wrote_member = true;
                out.push_str("  ");
                out.push_str(type_kind_keyword(nested));
                out.push(' ');
                out.push_str(nested.name());
                out.push_str(" { ... }\n");
            }
            _ => {}
        }
    }

    if !wrote_member {
        out.push_str("  // ...\n");
    }

    out.push('}');
    out
}

fn type_kind_keyword(ty: &nova_syntax::java::ast::TypeDecl) -> &'static str {
    match ty {
        nova_syntax::java::ast::TypeDecl::Class(_) => "class",
        nova_syntax::java::ast::TypeDecl::Interface(_) => "interface",
        nova_syntax::java::ast::TypeDecl::Enum(_) => "enum",
        nova_syntax::java::ast::TypeDecl::Record(_) => "record",
        nova_syntax::java::ast::TypeDecl::Annotation(_) => "@interface",
    }
}

fn type_kind_label(ty: &nova_syntax::java::ast::TypeDecl) -> &'static str {
    match ty {
        nova_syntax::java::ast::TypeDecl::Class(_) => "class",
        nova_syntax::java::ast::TypeDecl::Interface(_) => "interface",
        nova_syntax::java::ast::TypeDecl::Enum(_) => "enum",
        nova_syntax::java::ast::TypeDecl::Record(_) => "record",
        nova_syntax::java::ast::TypeDecl::Annotation(_) => "annotation",
    }
}

fn render_callable_skeleton(callable: &EnclosingCallable<'_>) -> String {
    match *callable {
        EnclosingCallable::Method(method) => {
            let params = render_param_list(&method.params);
            let body = if method.body.is_some() {
                " { ... }"
            } else {
                ";"
            };
            format!(
                "{} {}({}){}",
                method.return_ty.text, method.name, params, body
            )
        }
        EnclosingCallable::Constructor(cons) => {
            let params = render_param_list(&cons.params);
            format!("{}({}) {{ ... }}", cons.name, params)
        }
    }
}

fn render_param_list(params: &[nova_syntax::java::ast::ParamDecl]) -> String {
    params
        .iter()
        .map(|p| format!("{} {}", p.ty.text, p.name))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone)]
struct SymbolDecl {
    name: String,
    kind: String,
    snippet: String,
    range_start: usize,
}

fn collect_symbol_decls(
    types: &[nova_syntax::java::ast::TypeDecl],
    out: &mut Vec<SymbolDecl>,
    owner: Option<&str>,
) {
    for ty in types {
        collect_symbol_decls_for_type(ty, out, owner);
    }
}

fn collect_symbol_decls_for_type(
    ty: &nova_syntax::java::ast::TypeDecl,
    out: &mut Vec<SymbolDecl>,
    owner: Option<&str>,
) {
    let ty_kind = type_kind_label(ty).to_string();
    let mut type_snippet = String::new();
    if let Some(owner) = owner {
        type_snippet.push_str(&format!("// nested in {owner}\n"));
    }
    type_snippet.push_str(&render_type_skeleton(ty));

    out.push(SymbolDecl {
        name: ty.name().to_string(),
        kind: ty_kind,
        snippet: type_snippet,
        range_start: ty.range().start,
    });

    let this_owner = ty.name();
    for member in ty.members() {
        match member {
            nova_syntax::java::ast::MemberDecl::Field(field) => {
                let snippet = format!("// in {this_owner}\n{} {};", field.ty.text, field.name);
                out.push(SymbolDecl {
                    name: field.name.clone(),
                    kind: "field".to_string(),
                    snippet,
                    range_start: field.range.start,
                });
            }
            nova_syntax::java::ast::MemberDecl::Method(method) => {
                let snippet = format!(
                    "// in {this_owner}\n{}",
                    render_callable_skeleton(&EnclosingCallable::Method(method))
                );
                out.push(SymbolDecl {
                    name: method.name.clone(),
                    kind: "method".to_string(),
                    snippet,
                    range_start: method.range.start,
                });
            }
            nova_syntax::java::ast::MemberDecl::Constructor(cons) => {
                let snippet = format!(
                    "// in {this_owner}\n{}",
                    render_callable_skeleton(&EnclosingCallable::Constructor(cons))
                );
                out.push(SymbolDecl {
                    name: cons.name.clone(),
                    kind: "constructor".to_string(),
                    snippet,
                    range_start: cons.range.start,
                });
            }
            nova_syntax::java::ast::MemberDecl::Type(nested) => {
                collect_symbol_decls_for_type(nested, out, Some(this_owner));
            }
            _ => {}
        }
    }
}

fn extract_referenced_identifiers(
    code: &str,
    exclude: &HashSet<String>,
    decl_names: &HashSet<&str>,
) -> Vec<String> {
    const MAX_IDENTIFIERS: usize = 12;

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for tok in nova_syntax::lex(code) {
        if tok.kind != nova_syntax::SyntaxKind::Identifier {
            continue;
        }
        let ident = tok.text(code);
        if ident.is_empty() || is_java_keyword(ident) {
            continue;
        }
        if exclude.contains(ident) {
            continue;
        }
        if !decl_names.contains(ident) {
            continue;
        }
        if seen.insert(ident.to_string()) {
            out.push(ident.to_string());
            if out.len() >= MAX_IDENTIFIERS {
                break;
            }
        }
    }
    out
}

fn is_java_keyword(ident: &str) -> bool {
    matches!(
        ident,
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "void"
            | "volatile"
            | "while"
            | "null"
            | "true"
            | "false"
    )
}

fn related_symbols_from_references(
    referenced: &[String],
    decls: &[SymbolDecl],
) -> Vec<RelatedSymbol> {
    const MAX_RELATED: usize = 8;
    const MAX_PER_NAME: usize = 3;

    let mut by_name: HashMap<&str, Vec<&SymbolDecl>> = HashMap::new();
    for decl in decls {
        by_name.entry(decl.name.as_str()).or_default().push(decl);
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for name in referenced {
        let Some(mut matches) = by_name.get(name.as_str()).cloned() else {
            continue;
        };
        matches.sort_by_key(|decl| decl.range_start);
        for decl in matches.into_iter().take(MAX_PER_NAME) {
            let key = (decl.name.clone(), decl.kind.clone(), decl.range_start);
            if !seen.insert(key) {
                continue;
            }
            out.push(RelatedSymbol {
                name: decl.name.clone(),
                kind: decl.kind.clone(),
                snippet: decl.snippet.clone(),
            });
            if out.len() >= MAX_RELATED {
                return out;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_builder_enforces_budget_and_privacy() {
        let builder = ContextBuilder::new();
        let req = ContextRequest {
            file_path: Some("/home/user/project/Secret.java".to_string()),
            focal_code: r#"class Secret { String apiKey = "sk-verysecretstringthatislong"; }"#
                .to_string(),
            enclosing_context: Some("package com.example;\n".to_string()),
            project_context: None,
            semantic_context: None,
            related_symbols: vec![RelatedSymbol {
                name: "Secret".to_string(),
                kind: "class".to_string(),
                snippet: "class Secret {}".to_string(),
            }],
            related_code: vec![],
            cursor: Some(Position {
                line: 0,
                character: 0,
            }),
            diagnostics: vec![ContextDiagnostic {
                file: Some("/home/user/project/Secret.java".to_string()),
                range: Some(PositionRange {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 10,
                    },
                }),
                severity: ContextDiagnosticSeverity::Error,
                message: "cannot find symbol: Secret".to_string(),
                kind: Some(ContextDiagnosticKind::Type),
            }],
            extra_files: vec![CodeSnippet::new(
                "/home/user/project/Other.java",
                r#"class Other { String password = "supersecretpassword"; }"#,
            )],
            doc_comments: Some("/** Javadoc mentioning Secret */".to_string()),
            include_doc_comments: true,
            token_budget: 20,
            privacy: PrivacyMode {
                anonymize_identifiers: true,
                include_file_paths: false,
                ..PrivacyMode::default()
            },
        };

        let built = builder.build(req.clone());
        assert!(built.token_count <= 20);

        // Paths excluded.
        assert!(!built.text.contains("/home/user"));

        // Suspicious string redacted.
        assert!(built.text.contains("\"[REDACTED]\""));

        // Identifiers anonymized.
        assert!(!built.text.contains("Secret"));

        // Stability: same input yields same output.
        let built2 = builder.build(req);
        assert_eq!(built.text, built2.text);
        assert_eq!(built.sections, built2.sections);
    }

    #[test]
    fn context_builder_redacts_comments_without_identifier_anonymization_when_configured() {
        let builder = ContextBuilder::new();
        let req = ContextRequest {
            file_path: None,
            focal_code: "// token: sk-verysecretstringthatislong\nclass Foo {}\n".to_string(),
            enclosing_context: None,
            project_context: None,
            semantic_context: None,
            related_symbols: Vec::new(),
            related_code: Vec::new(),
            cursor: None,
            diagnostics: Vec::new(),
            extra_files: Vec::new(),
            doc_comments: None,
            include_doc_comments: false,
            token_budget: 1_000,
            privacy: PrivacyMode {
                anonymize_identifiers: false,
                include_file_paths: false,
                redaction: crate::RedactionConfig {
                    redact_string_literals: false,
                    redact_numeric_literals: false,
                    redact_comments: true,
                },
            },
        };

        let built = builder.build(req);
        assert!(
            built.text.contains("class Foo"),
            "identifiers should be preserved"
        );
        assert!(built.text.contains("// [REDACTED]"), "{:?}", built.text);
        assert!(!built.text.contains("sk-verysecret"), "{:?}", built.text);
    }

    #[test]
    fn java_source_range_extracts_enclosing_context_and_docs() {
        let source = r#"
 package com.example;
 
/** Class docs */
public class Foo {
  /** Method docs */
  public void bar() {
    int x = 0;
  }
}
"#;

        let start = source.find("int x").unwrap();
        let end = start + "int x = 0;".len();

        let req = ContextRequest::for_java_source_range(
            source,
            start..end,
            200,
            PrivacyMode {
                anonymize_identifiers: false,
                include_file_paths: false,
                ..PrivacyMode::default()
            },
            /*include_doc_comments=*/ true,
        );

        let enclosing = req.enclosing_context.as_deref().unwrap();
        assert!(enclosing.contains("package com.example"));
        assert!(enclosing.contains("class Foo"));
        assert!(enclosing.contains("void bar("));

        let docs = req.doc_comments.as_deref().unwrap();
        assert!(docs.contains("Method docs"));
    }

    #[test]
    fn java_source_range_does_not_panic_on_non_char_boundary_selection() {
        // 😀 is 4 bytes in UTF-8. A selection that lands inside its byte sequence
        // should not panic when building the focal snippet.
        let source = "class A { String s = \"😀\"; }\n";
        let emoji = source.find('😀').expect("emoji present");

        // Pick an intentionally invalid UTF-8 slice boundary inside the emoji bytes.
        let selection = (emoji + 1)..(emoji + 3);

        let req = ContextRequest::for_java_source_range(
            source,
            selection,
            200,
            PrivacyMode::default(),
            /*include_doc_comments=*/ false,
        );

        assert!(
            req.focal_code.contains('😀') || req.focal_code.is_empty(),
            "expected focal_code to be empty or include the emoji; got {:?}",
            req.focal_code
        );
    }

    #[test]
    fn position_for_offset_uses_utf16_code_units() {
        // 😀 is a surrogate pair in UTF-16.
        let text = "a😀b\n";
        let offset_after_emoji = text.find('b').expect("b");
        let pos = position_for_offset(text, offset_after_emoji);

        assert_eq!(pos.line, 0);
        // a = 1 code unit, 😀 = 2 code units -> column 3.
        assert_eq!(pos.character, 3);
    }

    #[test]
    fn symbol_extraction_populates_related_symbols_deterministically() {
        let source = r#"
package com.example;
 
class Foo {
  int count;
 
  void helper() {}
 
  void increment() {
    count++;
    helper();
  }
}
"#;

        let start = source.find("count++;").unwrap();
        let end = source.find("helper();").unwrap() + "helper();".len();

        let req = ContextRequest::for_java_source_range(
            source,
            start..end,
            400,
            PrivacyMode {
                anonymize_identifiers: false,
                include_file_paths: false,
                ..PrivacyMode::default()
            },
            /*include_doc_comments=*/ false,
        );

        assert_eq!(
            req.related_symbols
                .iter()
                .map(|s| (s.name.as_str(), s.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![("count", "field"), ("helper", "method")]
        );

        let builder = ContextBuilder::new();
        let built1 = builder.build(req.clone());
        let built2 = builder.build(req);
        assert_eq!(built1.text, built2.text);
    }

    #[test]
    fn diagnostics_section_included_when_provided() {
        let builder = ContextBuilder::new();
        let req = ContextRequest {
            file_path: None,
            focal_code: "x = y;".to_string(),
            enclosing_context: None,
            project_context: None,
            semantic_context: None,
            related_symbols: Vec::new(),
            related_code: Vec::new(),
            cursor: Some(Position {
                line: 0,
                character: 0,
            }),
            diagnostics: vec![ContextDiagnostic {
                file: None,
                range: None,
                severity: ContextDiagnosticSeverity::Error,
                message: "cannot find symbol: y".to_string(),
                kind: Some(ContextDiagnosticKind::Type),
            }],
            extra_files: Vec::new(),
            doc_comments: None,
            include_doc_comments: false,
            token_budget: 200,
            privacy: PrivacyMode {
                anonymize_identifiers: false,
                include_file_paths: false,
                ..PrivacyMode::default()
            },
        };

        let built = builder.build(req.clone());
        assert!(built.text.contains("## Diagnostics"));
        assert!(built.text.contains("cannot find symbol"));

        let built2 = builder.build(req);
        assert_eq!(built.text, built2.text);
        assert_eq!(built.sections, built2.sections);
    }

    #[test]
    fn project_context_strips_paths_unless_opted_in() {
        let builder = ContextBuilder::new();

        let req = ContextRequest {
            file_path: Some("/home/user/project/src/Example.java".to_string()),
            focal_code: "class Example {}".to_string(),
            enclosing_context: None,
            project_context: Some(ProjectContext {
                build_system: Some("maven".to_string()),
                java_version: Some("17".to_string()),
                frameworks: vec!["Spring".to_string()],
                classpath: vec![
                    "/home/user/.m2/repo/org/example/example-1.0.0.jar".to_string(),
                    "build/classes/java/main".to_string(),
                ],
            }),
            semantic_context: Some("Type info: Example".to_string()),
            related_symbols: Vec::new(),
            related_code: Vec::new(),
            cursor: None,
            diagnostics: Vec::new(),
            extra_files: Vec::new(),
            doc_comments: None,
            include_doc_comments: false,
            token_budget: 400,
            privacy: PrivacyMode {
                anonymize_identifiers: false,
                include_file_paths: false,
                ..PrivacyMode::default()
            },
        };

        let built = builder.build(req);
        assert!(
            built.text.contains("## Project context"),
            "{:?}",
            built.text
        );
        assert!(
            built.text.contains("Build system: maven"),
            "{:?}",
            built.text
        );
        assert!(built.text.contains("Java: 17"), "{:?}", built.text);
        assert!(built.text.contains("Spring"), "{:?}", built.text);
        // Basename only, no absolute path.
        assert!(built.text.contains("example-1.0.0.jar"), "{:?}", built.text);
        assert!(!built.text.contains("/home/user"), "{:?}", built.text);
    }

    #[test]
    fn budget_enforced_with_many_sections() {
        let builder = ContextBuilder::new();
        let req = ContextRequest {
            file_path: None,
            focal_code: "class Foo { void bar() { int x = 0; int y = 1; } }".to_string(),
            enclosing_context: Some("class Foo { int a; int b; int c; }".to_string()),
            project_context: None,
            semantic_context: None,
            related_symbols: vec![RelatedSymbol {
                name: "bar".to_string(),
                kind: "method".to_string(),
                snippet: "void bar(int x, int y) { ... }".to_string(),
            }],
            related_code: Vec::new(),
            cursor: None,
            diagnostics: vec![ContextDiagnostic {
                file: None,
                range: None,
                severity: ContextDiagnosticSeverity::Warning,
                message: "unused variable: y".to_string(),
                kind: None,
            }],
            extra_files: vec![CodeSnippet::ad_hoc(
                "class Extra { String s = \"sk-verysecretstringthatislong\"; }",
            )],
            doc_comments: Some("/** docs */".to_string()),
            include_doc_comments: true,
            token_budget: 30,
            privacy: PrivacyMode {
                anonymize_identifiers: false,
                include_file_paths: false,
                ..PrivacyMode::default()
            },
        };

        let built = builder.build(req);
        assert!(built.token_count <= 30);
        assert!(built.truncated);
    }

    #[test]
    fn truncated_when_section_skipped_due_to_header_budget() {
        let builder = ContextBuilder::new();
        let req = ContextRequest {
            file_path: None,
            focal_code: "x".to_string(),
            enclosing_context: None,
            project_context: None,
            semantic_context: None,
            related_symbols: Vec::new(),
            related_code: Vec::new(),
            cursor: None,
            diagnostics: vec![ContextDiagnostic {
                file: None,
                range: None,
                severity: ContextDiagnosticSeverity::Error,
                message: "cannot find symbol: y".to_string(),
                kind: Some(ContextDiagnosticKind::Type),
            }],
            extra_files: Vec::new(),
            doc_comments: None,
            include_doc_comments: false,
            // Leaves some remaining tokens after the focal section, but not enough to include the
            // full Diagnostics header.
            token_budget: 7,
            privacy: PrivacyMode {
                anonymize_identifiers: false,
                include_file_paths: false,
                ..PrivacyMode::default()
            },
        };

        let built = builder.build(req);
        assert!(built.text.contains("## Focal code"));
        assert!(!built.text.contains("## Diagnostics"));
        assert!(built.truncated);
    }
}

# 13 - AI Augmentation

[← Back to Main Document](../AGENTS.md) | [Previous: Debugging Integration](12-debugging-integration.md)

## Overview

AI integration is a key differentiator for Nova. Unlike retrofitting AI onto existing architectures, Nova is designed from the ground up to leverage machine learning for enhanced intelligence.

---

## AI Integration Philosophy

```
┌─────────────────────────────────────────────────────────────────┐
│                    AI INTEGRATION PRINCIPLES                     │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  1. AUGMENT, DON'T REPLACE                                      │
│     AI enhances traditional analysis, doesn't substitute it.    │
│     Semantic analysis provides ground truth; AI adds insights.  │
│                                                                  │
│  2. GRACEFUL DEGRADATION                                         │
│     Nova must work fully without AI services.                   │
│     AI features are enhancements, not requirements.             │
│                                                                  │
│  3. PRIVACY-CONSCIOUS                                            │
│     Local models preferred where possible.                      │
│     Clear user control over data sent externally.               │
│     Code snippets anonymized when sent to services.             │
│     File paths excluded unless explicitly enabled.              │
│                                                                  │
│  4. PREDICTABLE LATENCY                                          │
│     AI operations should not block interactive features.        │
│     Async processing with fallback to non-AI results.           │
│                                                                  │
│  5. TRANSPARENT                                                  │
│     Clear indication when AI is involved.                       │
│     Explanations for AI-driven suggestions.                     │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## AI Feature Categories

### 1. Enhanced Code Completion

```
┌─────────────────────────────────────────────────────────────────┐
│                    AI COMPLETION                                 │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  TRADITIONAL COMPLETION                                         │
│  • Based on type system and scope                               │
│  • All valid completions, alphabetically sorted                 │
│                                                                  │
│  AI-ENHANCED COMPLETION                                         │
│  • Ranked by likelihood of selection                            │
│  • Context-aware (what you're trying to do)                     │
│  • Multi-token completions (method chains)                      │
│  • Pattern-based suggestions                                    │
│                                                                  │
│  EXAMPLE:                                                       │
│  List<String> names = people.stream().|                         │
│                                                                  │
│  Traditional: collect, count, distinct, filter, findAny...      │
│  AI-ranked:   filter, map, collect (based on context)           │
│  AI multi:    filter(p -> p.isActive()).map(Person::getName)    │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Implementation

```rust
pub struct AiCompletionProvider {
    /// Local ranking model
    local_model: CompletionRanker,
    
    /// Optional cloud model for advanced completions
    cloud_model: Option<CloudCompletionService>,
    
    /// Completion cache
    cache: LruCache<CompletionContext, Vec<ScoredCompletion>>,
}

impl AiCompletionProvider {
    pub async fn rank_completions(
        &self,
        ctx: &CompletionContext,
        items: Vec<CompletionItem>,
    ) -> Vec<CompletionItem> {
        // Build feature vector from context
        let features = self.extract_features(ctx);
        
        // Score each item with local model
        let mut scored: Vec<_> = items.into_iter()
            .map(|item| {
                let score = self.local_model.score(&features, &item);
                (item, score)
            })
            .collect();
        
        // Sort by score
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        
        // Optionally enhance top results with cloud model
        if let Some(cloud) = &self.cloud_model {
            if ctx.wants_multi_token {
                let enhanced = cloud.enhance_completions(ctx, &scored[..10]).await;
                scored.splice(0..0, enhanced);
            }
        }
        
        scored.into_iter().map(|(item, _)| item).collect()
    }
    
    fn extract_features(&self, ctx: &CompletionContext) -> FeatureVector {
        FeatureVector {
            // Contextual features
            receiver_type: ctx.receiver_type.clone(),
            expected_type: ctx.expected_type.clone(),
            in_method: ctx.enclosing_method.clone(),
            in_class: ctx.enclosing_class.clone(),
            
            // Positional features
            prefix_length: ctx.prefix.len(),
            line_context: ctx.line_text.clone(),
            
            // Historical features
            recent_completions: ctx.recent_selections.clone(),
            file_patterns: ctx.file_completion_patterns.clone(),
            
            // Semantic features
            visible_types: ctx.visible_types.clone(),
            import_suggestions: ctx.potential_imports.clone(),
        }
    }
}
```

### 2. Intelligent Code Generation

```rust
pub struct CodeGenerator {
    llm: LlmClient,
    context_builder: ContextBuilder,
}

impl CodeGenerator {
    /// Generate method implementation from signature and context
    pub async fn generate_method_body(
        &self,
        method: &MethodSignature,
        class_context: &ClassContext,
    ) -> Result<String> {
        // Build rich context
        let context = self.context_builder.build(ContextRequest {
            focal_method: method,
            class_info: class_context,
            include_related_methods: true,
            include_field_info: true,
            include_superclass_methods: true,
            max_tokens: 2000,
        });
        
        // Generate with LLM
        let prompt = format!(
            "Given the following Java class context:\n{}\n\n\
             Implement the method:\n{}\n\n\
             The implementation should:",
            context,
            format_method_signature(method),
        );
        
        let generated = self.llm.generate(&prompt).await?;
        
        // Validate generated code
        let validated = self.validate_and_fix(generated, method)?;
        
        Ok(validated)
    }
    
    /// Generate unit test for method
    pub async fn generate_test(
        &self,
        method: &Method,
        test_framework: TestFramework,
    ) -> Result<String> {
        let context = self.context_builder.build_for_testing(method);
        
        let prompt = format!(
            "Generate a {} test for the following method:\n{}\n\n\
             Include tests for:\n\
             - Normal cases\n\
             - Edge cases\n\
             - Error conditions",
            test_framework.name(),
            context,
        );
        
        self.llm.generate(&prompt).await
    }
}
```

### 3. Natural Language Code Search

```rust
pub struct SemanticSearch {
    /// Embedding model
    embedder: EmbeddingModel,
    
    /// Vector index
    index: VectorIndex,
}

impl SemanticSearch {
    /// Search code using natural language
    pub async fn search(&self, query: &str) -> Vec<SearchResult> {
        // Embed query
        let query_embedding = self.embedder.embed(query).await?;
        
        // Find similar code
        let candidates = self.index.search(&query_embedding, 50);
        
        // Re-rank with more context
        let results = self.rerank(query, candidates).await;
        
        results
    }
    
    /// Index codebase for search
    pub async fn index_project(&self, db: &dyn Database) {
        for file in db.project_files() {
            for method in db.methods_in_file(file) {
                // Create rich representation
                let text = self.create_searchable_text(db, method);
                
                // Generate embedding
                let embedding = self.embedder.embed(&text).await?;
                
                // Store in index
                self.index.insert(method.id, embedding, MethodMetadata {
                    name: method.name.clone(),
                    class: method.class.clone(),
                    file: file,
                    range: method.range,
                });
            }
        }
    }
    
    fn create_searchable_text(&self, db: &dyn Database, method: &Method) -> String {
        // Combine multiple representations
        format!(
            "{}\n{}\n{}\n{}",
            method.name,
            method.javadoc.unwrap_or_default(),
            format_signature(&method),
            summarize_body(&method.body),
        )
    }
}
```

### 4. Intelligent Error Explanation

```rust
pub struct ErrorExplainer {
    llm: LlmClient,
}

impl ErrorExplainer {
    /// Explain compiler error in plain language
    pub async fn explain_error(&self, error: &Diagnostic) -> ErrorExplanation {
        // Get context around error
        let context = self.get_error_context(error);
        
        let prompt = format!(
            "Explain this Java compiler error to a developer:\n\n\
             Error: {}\n\n\
             Code context:\n{}\n\n\
             Provide:\n\
             1. What the error means\n\
             2. Why it happened\n\
             3. How to fix it",
            error.message,
            context,
        );
        
        let explanation = self.llm.generate(&prompt).await?;
        
        // Extract structured explanation
        ErrorExplanation {
            summary: extract_summary(&explanation),
            cause: extract_cause(&explanation),
            fixes: extract_fixes(&explanation),
            examples: extract_examples(&explanation),
        }
    }
    
    /// Suggest fix for error
    pub async fn suggest_fix(&self, error: &Diagnostic) -> Vec<CodeFix> {
        // Use Nova's semantic info + AI to generate fixes
        let semantic_fixes = self.semantic_fixes(error);
        let ai_fixes = self.ai_generated_fixes(error).await;
        
        // Merge and deduplicate
        merge_fixes(semantic_fixes, ai_fixes)
    }
}
```

### 5. Code Review Assistant

```rust
pub struct CodeReviewer {
    llm: LlmClient,
    static_analyzer: StaticAnalyzer,
}

impl CodeReviewer {
    /// Review code changes
    pub async fn review_changes(&self, diff: &GitDiff) -> CodeReview {
        // Static analysis first
        let static_issues = self.static_analyzer.analyze(diff);
        
        // AI review for higher-level issues
        let ai_review = self.ai_review(diff).await?;
        
        CodeReview {
            issues: merge_issues(static_issues, ai_review.issues),
            suggestions: ai_review.suggestions,
            summary: ai_review.summary,
        }
    }
    
    async fn ai_review(&self, diff: &GitDiff) -> AiReviewResult {
        let prompt = format!(
            "Review this Java code change:\n\n{}\n\n\
             Consider:\n\
             - Correctness\n\
             - Performance implications\n\
             - Security concerns\n\
             - Code style and maintainability\n\
             - Missing error handling\n\
             - Test coverage",
            format_diff(diff),
        );
        
        let response = self.llm.generate(&prompt).await?;
        parse_review_response(&response)
    }
}
```

---

## Model Architecture

### Local Models

```
┌─────────────────────────────────────────────────────────────────┐
│                    LOCAL MODELS                                  │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  COMPLETION RANKING                                              │
│  • Small transformer or gradient boosting                       │
│  • ~10MB model size                                             │
│  • <1ms inference                                               │
│  • Trained on completion logs                                   │
│                                                                  │
│  EMBEDDING MODEL                                                 │
│  • Sentence transformer variant                                 │
│  • ~100MB model size                                            │
│  • ~10ms per embedding                                          │
│  • For semantic search                                          │
│                                                                  │
│  SIMPLE CODE MODEL                                               │
│  • Small language model (1-3B params)                           │
│  • ~2-6GB model size                                            │
│  • For simple completions and explanations                      │
│  • Runs on CPU (GPU optional, faster)                           │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Cloud Models

```
┌─────────────────────────────────────────────────────────────────┐
│                    CLOUD INTEGRATION                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  SUPPORTED PROVIDERS                                            │
│  • OpenAI (GPT-4, GPT-4-turbo)                                  │
│  • Anthropic (Claude)                                           │
│  • Google (Gemini)                                              │
│  • Azure OpenAI                                                 │
│  • Self-hosted (Ollama, vLLM)                                   │
│                                                                  │
│  USE CASES                                                      │
│  • Complex code generation                                      │
│  • Extended explanations                                        │
│  • Code review                                                  │
│  • Documentation generation                                     │
│                                                                  │
│  PRIVACY CONTROLS                                               │
│  • Opt-in per feature                                          │
│  • Code anonymization option                                    │
│  • Enterprise proxy support                                     │
│  • Usage logging and audit                                      │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Model Configuration

Nova uses a single source of truth for AI settings: `nova_config::AiConfig` (available as
`nova_config::NovaConfig::ai`). The same struct is used for both:

- Local augmentation (completion re-ranking, semantic search, etc.)
- LLM-backed actions (provider selection, privacy controls, audit logging)

Feature flags and latency budgets are configured under `ai.features` and `ai.timeouts`:

```toml
[ai]
enabled = true

[ai.features]
completion_ranking = true
semantic_search = true
multi_token_completion = true

[ai.timeouts]
completion_ranking_ms = 20
multi_token_completion_ms = 250
```

### Server-side overrides (environment variables)

Some editor integrations set environment variables when starting `nova-lsp` to provide **server-side
hard overrides** for AI behavior (for example: to disable multi-token completions without modifying
`nova.toml`).

These overrides are read at **process startup**, so changing them requires restarting the server.
For details (including `NOVA_AI_COMPLETIONS_MAX_ITEMS`) and how they affect the completion protocol
(`CompletionList.isIncomplete` and `nova/completion/more` polling), see `docs/protocol-extensions.md`
(`nova/completion/more`).

Common overrides:

- `NOVA_DISABLE_AI=1` — force-disable all AI features server-side.
- `NOVA_DISABLE_AI_COMPLETIONS=1` — force-disable multi-token completions server-side.
- `NOVA_AI_COMPLETIONS_MAX_ITEMS=<n>` — override the server’s AI multi-token completion max-items.
  `0` disables multi-token completions entirely (and the server clamps large values; see the
  protocol docs for details).

### Code-editing policy (patches / file edits)

Nova treats **patch-based code edits** (anything that applies edits to files) as higher risk than
explain-only AI actions.

In the LSP integration, AI code-editing features (for example: **"Generate method body with AI"**
and **"Generate tests with AI"**) work by asking the model for a **structured patch** (JSON patch or
unified diff) and applying it as an editor workspace edit. Because these operations must round-trip
exact source text, they are subject to the `allow_cloud_code_edits` + anonymization policy described
below.

Patch file paths are treated as **workspace-relative** (e.g. `src/Main.java`) and are resolved
against a `rootUri` derived from the workspace root (or, as a fallback, the document’s parent
directory). Patch safety rejects absolute paths, path traversal (`..`), and Windows-style backslashes
to ensure edits can’t escape the intended workspace root.

In particular, anonymizing identifiers is great for privacy, but it makes LLM-generated patches
impossible to apply reliably to the original source.

In cloud mode (`ai.privacy.local_only = false`), Nova will only allow patch-based code edits when
**all** of the following are true:

1. `ai.privacy.anonymize_identifiers = false` (or `ai.privacy.anonymize = false`)
2. `ai.privacy.allow_cloud_code_edits = true`
3. `ai.privacy.allow_code_edits_without_anonymization = true`

Nova refuses cloud code edits when identifier anonymization is enabled (the default in cloud mode),
because patches produced against anonymized code cannot be applied reliably to the original source.

To enable cloud code edits, you must **explicitly opt in** and **disable identifier anonymization**:

```toml
[ai.privacy]
local_only = false
anonymize_identifiers = false # `anonymize = false` is accepted as an alias
allow_cloud_code_edits = true
allow_code_edits_without_anonymization = true
```

Disabling identifier anonymization does **not** disable other privacy protections. In cloud mode,
Nova still defaults to redacting:

- suspicious string literals (`redact_sensitive_strings = true`)
- long numeric literals (`redact_numeric_literals = true`)
- comment bodies (`strip_or_redact_comments = true`)

These knobs can be overridden independently under `[ai.privacy]`.

Depending on the editor integration, these may be surfaced as settings prefixed with `nova.`
(for example: `nova.ai.privacy.allow_cloud_code_edits`).

Explain-only actions are always allowed regardless of these **code-edit gating** settings (but
`ai.privacy.excluded_paths` can still force Nova to omit file-backed code context from the prompt).

### Cloud multi-token completion policy (method-chain suggestions)

Nova's **multi-token completions** (suggesting short method chains / templates) build prompts that
include **identifier-heavy lists** like:

- `Available methods:` (often contains project-specific APIs)
- `Importable symbols:` (fully-qualified project class names)

When `ai.privacy.anonymize_identifiers=true` (the default in cloud mode), Nova **does not send**
these prompts to a cloud model. This avoids leaking raw project identifiers through these lists.

To enable cloud multi-token completions, you must **explicitly opt in** by disabling identifier
anonymization:

```toml
[ai.privacy]
local_only = false
anonymize_identifiers = false # required for cloud multi-token completions
```

Local-only mode (`ai.privacy.local_only=true`) is unaffected because code never leaves the machine.

---

### Excluding files from AI (`ai.privacy.excluded_paths`)

`ai.privacy.excluded_paths` is a list of glob patterns for files whose contents must **never** be
sent to an LLM provider (local or cloud).

**Path matching semantics:**

- Patterns are intended to be **workspace-relative**. Prefer `src/**`-style globs (or more specific
  ones like `src/secrets/**`) over absolute filesystem paths.
- The LSP layer typically works with **absolute** on-disk paths (decoded from `file://` URIs). Nova
  still allows workspace-relative globs to match those absolute paths (by also attempting to match
  each suffix of the absolute path), so a pattern like `src/**` will match an absolute LSP path like
  `/home/alice/project/src/Main.java`.

**Behavior:**

`excluded_paths` is enforced server-side in the LSP request handlers
(`crates/nova-lsp/src/stdio_ai.rs`) and again during prompt construction
(`crates/nova-ai/src/features.rs`). It applies even if a client supplies its own code snippets.

Nova’s behavior depends on the AI action:

1. **Explain-only actions** (for example: `nova/ai/explainError` and diff-based `codeReview`
   prompts) are **allowed**, even when the focal file matches `excluded_paths`.

   When the focal file is excluded, Nova builds a *diagnostic-only* prompt:

   - file-backed source text is not included (client-supplied `code` is ignored)
   - file path / range metadata is omitted to avoid leaking excluded paths
   - the prompt includes a placeholder such as `[code context omitted due to excluded_paths]`

2. **Patch-based code edits** (for example: `nova/ai/generateMethodBody` and
   `nova/ai/generateTests`) are **rejected** when the focal/target file matches `excluded_paths`
   (the server returns an error *before* calling the model).

   Other edit-like features that require file-backed prompts (such as multi-token completions) are
   also disabled for excluded files.

3. **Semantic search indexing** omits excluded files entirely (they are not embedded/indexed and
   therefore cannot be surfaced as related-code context).

4. **Extra context items**: when an AI request is otherwise allowed, any “extra files” /
   “related code” context items whose paths match `excluded_paths` are omitted from the prompt. If
   anything was omitted, Nova injects a synthetic placeholder snippet such as
   `[some context omitted due to excluded_paths]` so the model can tell context was intentionally
   removed.

---

## Context Building

```rust
/// Build optimal context for LLM queries
pub struct ContextBuilder {
    db: Arc<dyn Database>,
    max_tokens: usize,
}

impl ContextBuilder {
    pub fn build(&self, request: ContextRequest) -> String {
        let mut budget = self.max_tokens;
        let mut context = String::new();
        
        // Priority 1: Focal code (always include)
        let focal = self.format_focal_code(&request);
        context.push_str(&focal);
        budget -= count_tokens(&focal);
        
        // Priority 2: Direct dependencies
        let deps = self.format_dependencies(&request, budget / 3);
        context.push_str(&deps);
        budget -= count_tokens(&deps);
        
        // Priority 3: Related code by semantic similarity
        let related = self.find_related_code(&request, budget / 2);
        context.push_str(&related);
        budget -= count_tokens(&related);
        
        // Priority 4: Documentation and comments
        let docs = self.format_documentation(&request, budget);
        context.push_str(&docs);
        
        context
    }
    
    fn find_related_code(&self, request: &ContextRequest, budget: usize) -> String {
        // Use embeddings to find semantically similar code
        let focal_embedding = self.embed_code(&request.focal_code);
        
        let similar = self.db.semantic_search()
            .search_by_embedding(&focal_embedding, 10);
        
        // Select within budget
        let mut result = String::new();
        let mut used = 0;
        
        for item in similar {
            let formatted = self.format_code_snippet(&item);
            let tokens = count_tokens(&formatted);
            
            if used + tokens > budget {
                break;
            }
            
            result.push_str(&formatted);
            result.push('\n');
            used += tokens;
        }
        
        result
    }
}
```

---

## Privacy and Security

```rust
/// Code anonymization for privacy
pub struct CodeAnonymizer {
    /// Mapping of original to anonymized names
    name_map: HashMap<String, String>,
}

impl CodeAnonymizer {
    pub fn anonymize(&mut self, code: &str) -> String {
        let parsed = parse_java(code);
        let mut result = code.to_string();
        
        // Anonymize identifiers
        for ident in parsed.identifiers() {
            if self.should_anonymize(&ident) {
                let anon = self.get_or_create_anon_name(&ident.name);
                result = result.replace(&ident.name, &anon);
            }
        }
        
        // Anonymize string literals
        for string in parsed.string_literals() {
            if self.looks_like_sensitive(&string.value) {
                result = result.replace(&string.text, "\"[REDACTED]\"");
            }
        }
        
        result
    }
    
    fn should_anonymize(&self, ident: &Identifier) -> bool {
        // Keep standard library names
        if is_standard_library(&ident.name) {
            return false;
        }
        
        // Keep common patterns (get, set, is, etc.)
        if is_common_pattern(&ident.name) {
            return false;
        }
        
        true
    }
    
    fn get_or_create_anon_name(&mut self, original: &str) -> String {
        if let Some(anon) = self.name_map.get(original) {
            return anon.clone();
        }
        
        // Generate meaningful anonymous name
        let prefix = infer_type(original); // "method", "class", "field", etc.
        let anon = format!("{}_{}", prefix, self.name_map.len());
        self.name_map.insert(original.to_string(), anon.clone());
        anon
    }
}
```

---

## AI regression tests / evaluation

Nova's AI subsystems are intentionally heuristic-heavy (privacy sanitization, patch safety checks, and multi-token completion validation). To prevent regressions **without** requiring live model calls, we keep a deterministic evaluation suite that exercises these behaviors end-to-end using synthetic Java snippets and golden expectations.

- Tests live in `crates/nova-ai/tests/suite/ai_eval.rs` (included by `crates/nova-ai/tests/tests.rs`)
- They must not make any network calls (no providers, no HTTP)
- Run them (agent-safe) with:

```bash
bash scripts/cargo_agent.sh test --locked -p nova-ai --test tests suite::ai_eval
```

In normal local development / CI (outside the agent runner wrapper), the equivalent command is:

```bash
cargo test --locked -p nova-ai --test tests suite::ai_eval
```

The suite covers:
- privacy filtering (excluded paths, redaction/anonymization stability)
- patch parsing/application + safety limits (files/size/imports)
- multi-token completion validation + duplicate filtering

---

## Integration Points

```rust
impl NovaServer {
    /// Integrate AI into LSP handlers
    async fn completion_with_ai(
        &self,
        params: CompletionParams,
    ) -> Result<CompletionResponse> {
        // Get traditional completions
        let items = self.db.read().completions_at(params.file, params.position);
        
        // AI ranking (fast, local)
        let ranked = self.ai.rank_completions(&params, items).await;
        
        // Optional: AI multi-token completions (async, might use cloud)
        if self.config.ai.features.multi_token_completion {
            tokio::spawn(async move {
                let multi = self.ai.multi_token_completions(&params).await;
                self.send_additional_completions(multi);
            });
        }
        
        Ok(CompletionResponse::List(CompletionList {
            is_incomplete: true, // More coming
            items: ranked,
        }))
    }
    
    /// AI-powered code action
    async fn ai_code_actions(
        &self,
        params: CodeActionParams,
    ) -> Vec<CodeAction> {
        let mut actions = Vec::new();
        
        // Generate method body
        if let Some(method) = self.db.read().empty_method_at(params.range) {
            actions.push(CodeAction {
                title: "Generate method body with AI".into(),
                kind: Some(CodeActionKind::new("nova.ai.generate")),
                command: Some(command(
                    "nova.ai.generateMethodBody",
                    [GenerateMethodBodyArgs { /* method_signature, context, uri, range */ }],
                )),
                ..Default::default()
            });
        }
        
        // Explain error
        if let Some(diagnostic) = params.context.diagnostics.first() {
            actions.push(CodeAction {
                title: "Explain this error".into(),
                kind: Some(CodeActionKind::new("nova.explain")),
                command: Some(command(
                    "nova.ai.explainError",
                    [ExplainErrorArgs { /* diagnostic_message, code, uri, range */ }],
                )),
                ..Default::default()
            });
        }
        
        // Generate tests
        if let Some(method) = self.db.read().method_at(params.range.start) {
            actions.push(CodeAction {
                title: "Generate tests with AI".into(),
                kind: Some(CodeActionKind::new("nova.ai.tests")),
                command: Some(command(
                    "nova.ai.generateTests",
                    [GenerateTestsArgs { /* target, context, uri, range */ }],
                )),
                ..Default::default()
            });
        }
        
        actions
    }
}
```

---

## Next Steps

1. → [Testing Strategy](14-testing-strategy.md): Quality assurance
2. → [Testing Infrastructure](14-testing-infrastructure.md): How to run tests/CI and update fixtures
3. → [Work Breakdown](15-work-breakdown.md): Project organization

---

[← Previous: Debugging Integration](12-debugging-integration.md) | [Next: Testing Strategy →](14-testing-strategy.md) | [Testing Infrastructure](14-testing-infrastructure.md)

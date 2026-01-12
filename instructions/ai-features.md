# AI Features Workstream

> **⚠️ MANDATORY: Read and follow [AGENTS.md](../AGENTS.md) completely before proceeding.**
> **All rules in AGENTS.md apply at all times. This file adds workstream-specific guidance.**

---

## Scope

This workstream owns AI/ML integration - intelligent code assistance powered by machine learning:

| Crate | Purpose |
|-------|---------|
| `nova-ai` | AI infrastructure, model management, context building |
| `nova-ai-codegen` | Code generation from AI outputs |

---

## Key Documents

**Required reading:**
- [13 - AI Augmentation](../docs/13-ai-augmentation.md) - Architecture and features
- [Protocol extensions](../docs/protocol-extensions.md) - Source of truth for AI multi-token completion polling (`nova/completion/more`)

## Server-side AI overrides (environment variables)

Some editor integrations (notably VS Code) set environment variables when starting `nova-lsp` to
provide **server-side hard overrides** for AI behavior. These are read at process start (restart
required) and are useful as privacy/cost controls.

For multi-token completions, see the `nova/completion/more` notes in
[`docs/protocol-extensions.md`](../docs/protocol-extensions.md) (including
`NOVA_AI_COMPLETIONS_MAX_ITEMS`, where `0` disables multi-token completions).

---

## Architecture

### AI Integration Points

```
┌─────────────────────────────────────────────────────────────────┐
│                    AI Integration                                │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  IDE Features                                                   │
│  ├── Completion Ranking ──────→ AI Model                        │
│  ├── Code Generation ─────────→ AI Model                        │
│  ├── Error Explanation ───────→ AI Model                        │
│  ├── Test Generation ─────────→ AI Model                        │
│  └── Semantic Search ─────────→ Embeddings                      │
│                                                                  │
│  Models                                                         │
│  ├── Local (Ollama, llama.cpp) - Privacy-first                  │
│  └── Cloud (OpenAI, Anthropic) - Higher capability              │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Model Interface

```rust
#[async_trait]
pub trait AiModel: Send + Sync {
    /// Model identifier
    fn id(&self) -> &str;
    
    /// Generate completion
    async fn complete(&self, prompt: &Prompt) -> Result<Completion>;
    
    /// Generate embeddings
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
    
    /// Check if model supports capability
    fn supports(&self, capability: Capability) -> bool;
}

pub enum Capability {
    TextCompletion,
    CodeCompletion,
    Embedding,
    Chat,
    FunctionCalling,
}
```

---

## Features

### Completion Ranking

AI improves completion ordering:

```rust
pub fn rank_completions(
    items: &mut [CompletionItem],
    context: &CompletionContext,
    model: &dyn AiModel,
) {
    // Get AI scores for candidates
    let prompt = build_ranking_prompt(context, items);
    let scores = model.rank(prompt).await?;
    
    // Combine with static analysis scores
    for (item, ai_score) in items.iter_mut().zip(scores) {
        item.score = item.static_score * 0.3 + ai_score * 0.7;
    }
    
    items.sort_by(|a, b| b.score.cmp(&a.score));
}
```

### Code Generation

Generate code from natural language:

```rust
pub async fn generate_code(
    description: &str,
    context: &CodeContext,
    model: &dyn AiModel,
) -> Result<GeneratedCode> {
    let prompt = Prompt::builder()
        .system("You are a Java code generator...")
        .context(format_context(context))
        .user(description)
        .build();
    
    let completion = model.complete(&prompt).await?;
    
    // Parse and validate generated code
    let code = parse_code_block(&completion.text)?;
    validate_syntax(&code)?;
    
    Ok(GeneratedCode {
        code,
        explanation: completion.explanation,
    })
}
```

### Error Explanation

Explain compiler errors in natural language:

```rust
pub async fn explain_error(
    diagnostic: &Diagnostic,
    context: &ErrorContext,
    model: &dyn AiModel,
) -> Result<Explanation> {
    let prompt = Prompt::builder()
        .system("Explain this Java compiler error...")
        .context(format_error_context(context))
        .user(format!("Error: {}", diagnostic.message))
        .build();
    
    let completion = model.complete(&prompt).await?;
    
    Ok(Explanation {
        summary: completion.summary,
        detailed: completion.text,
        suggestions: parse_suggestions(&completion),
    })
}
```

### Test Generation

Generate tests for code:

```rust
pub async fn generate_tests(
    method: &MethodDecl,
    model: &dyn AiModel,
) -> Result<Vec<TestCase>> {
    let prompt = Prompt::builder()
        .system("Generate JUnit tests for this method...")
        .context(format_method_context(method))
        .build();
    
    let completion = model.complete(&prompt).await?;
    
    parse_test_cases(&completion.text)
}
```

### Semantic Search

Search code by meaning using embeddings:

```rust
pub struct SemanticIndex {
    embeddings: Vec<(FileId, Vec<f32>)>,
    model: Arc<dyn AiModel>,
}

impl SemanticIndex {
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        let query_embedding = self.model.embed(query).await?;
        
        let mut results: Vec<_> = self.embeddings
            .iter()
            .map(|(file, emb)| {
                let score = cosine_similarity(&query_embedding, emb);
                (file, score)
            })
            .collect();
        
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        results.truncate(10);
        
        Ok(results.into_iter().map(|(f, s)| SearchResult { file: *f, score: s }).collect())
    }
}
```

---

## Development Guidelines

### Context Building

Good context is critical for AI quality:

```rust
pub struct CodeContext {
    /// Current file content (with cursor position)
    pub current_file: String,
    /// Related files (imports, callers, etc.)
    pub related_files: Vec<(PathBuf, String)>,
    /// Project structure summary
    pub project_summary: String,
    /// Relevant documentation
    pub docs: Vec<String>,
}

fn build_context(file: FileId, offset: u32, workspace: &Workspace) -> CodeContext {
    // Get current file with cursor marker
    let current = workspace.file_content(file);
    let current_with_cursor = insert_cursor_marker(&current, offset);
    
    // Get related files (imports, type definitions)
    let imports = workspace.imports_for(file);
    let related = imports.iter()
        .map(|imp| (imp.path.clone(), workspace.file_content(imp.file)))
        .take(5)  // Limit context size
        .collect();
    
    CodeContext {
        current_file: current_with_cursor,
        related_files: related,
        project_summary: workspace.project_summary(),
        docs: workspace.relevant_docs(file, offset),
    }
}
```

### Privacy Controls

Users control what data is sent to AI:

```rust
pub struct PrivacySettings {
    /// Allow sending code to cloud models
    pub allow_cloud: bool,
    /// File patterns to exclude
    pub exclude_patterns: Vec<Glob>,
    /// Require local model only
    pub local_only: bool,
}

fn should_use_ai(file: &Path, settings: &PrivacySettings) -> bool {
    if settings.local_only && !model.is_local() {
        return false;
    }
    
    if settings.exclude_patterns.iter().any(|p| p.matches(file)) {
        return false;
    }
    
    true
}
```

### Model Selection

Choose appropriate model for task:

```rust
fn select_model(task: &AiTask, available: &[Arc<dyn AiModel>]) -> Arc<dyn AiModel> {
    match task {
        // Fast local model for ranking
        AiTask::CompletionRanking => find_local_model(available),
        // Powerful model for generation
        AiTask::CodeGeneration => find_capable_model(available, Capability::CodeCompletion),
        // Embedding model for search
        AiTask::SemanticSearch => find_embedding_model(available),
    }
}
```

---

## Testing

```bash
# AI core tests
bash scripts/cargo_agent.sh test --locked -p nova-ai --lib

# AI evaluation / regression suite (privacy filtering, patch safety, completion validation)
bash scripts/cargo_agent.sh test --locked -p nova-ai --test tests suite::ai_eval

# Code generation tests
bash scripts/cargo_agent.sh test --locked -p nova-ai-codegen --lib

# LSP unit tests for AI code actions / patch pipeline (privacy gating, excluded paths, patch safety)
# (filtered to just the AI code-action suite)
bash scripts/cargo_agent.sh test --locked -p nova-lsp --lib code_action::tests::
```

### Mock Models

Use mock models for testing:

```rust
struct MockModel {
    responses: HashMap<String, String>,
}

impl AiModel for MockModel {
    async fn complete(&self, prompt: &Prompt) -> Result<Completion> {
        let key = prompt.user_message();
        let text = self.responses.get(key).cloned().unwrap_or_default();
        Ok(Completion { text })
    }
}

#[test]
fn test_code_generation() {
    let model = MockModel::with_response(
        "Add two numbers",
        "```java\npublic int add(int a, int b) { return a + b; }\n```"
    );
    
    let result = generate_code("Add two numbers", &ctx, &model).await?;
    assert!(result.code.contains("return a + b"));
}
```

---

## Common Pitfalls

1. **Context too large** - Trim context to fit model limits
2. **Latency** - AI calls are slow; cache aggressively
3. **Hallucinations** - Validate AI outputs before using
4. **Cost** - Cloud models cost money; batch requests
5. **Privacy** - Never send sensitive code without consent

---

## Model Configuration

### Local Models (Ollama)

```toml
# nova.toml
[ai]
provider = "ollama"
model = "codellama:7b"
endpoint = "http://localhost:11434"
```

### Cloud Models (OpenAI)

```toml
# nova.toml
[ai]
provider = "openai"
model = "gpt-4"
# API key from environment: OPENAI_API_KEY
```

---

## Dependencies

**Upstream:** `nova-workspace`, `nova-ide` (context building)
**Downstream:** None (AI enhances other features)

---

## Note on GPU Requirements

**Nova does NOT require a GPU.** Local AI models run on CPU (slower but functional). GPU acceleration is optional for users who want faster local inference.

Development and testing should work on headless, GPU-less machines (see [13 - AI Augmentation](../docs/13-ai-augmentation.md)).

---

*Remember: Always follow [AGENTS.md](../AGENTS.md) rules. Use wrapper scripts. Scope your cargo commands.*

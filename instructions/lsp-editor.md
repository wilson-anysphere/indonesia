# LSP & Editor Integration Workstream

> **⚠️ MANDATORY: Read and follow [AGENTS.md](../AGENTS.md) completely before proceeding.**
> **All rules in AGENTS.md apply at all times. This file adds workstream-specific guidance.**

---

## Scope

This workstream owns the Language Server Protocol implementation and editor integrations:

| Crate/Directory | Purpose |
|-----------------|---------|
| `nova-lsp` | LSP server implementation |
| `nova-cli` | Command-line interface, server launcher |
| `nova-router` | Request routing and dispatching |
| `editors/vscode/` | VS Code extension |
| `editors/neovim/` | Neovim configuration |
| `editors/emacs/` | Emacs configuration |

---

## Key Documents

**Required reading:**
- [11 - Editor Integration](../docs/11-editor-integration.md) - Protocol and extension design
- [protocol-extensions.md](../docs/protocol-extensions.md) - Custom LSP extensions
  - For AI multi-token completions, see the `nova/completion/more` section, including the
    `NOVA_AI_COMPLETIONS_MAX_ITEMS` server startup override (0 disables; restart required).

**ADRs:**
- [ADR-0003: Protocol Frameworks](../docs/adr/0003-protocol-frameworks-lsp-dap.md)

---

## Architecture

### LSP Server

```
┌─────────────────────────────────────────────────────────────────┐
│                    LSP Server Architecture                       │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Editor ←──── JSON-RPC ────→ nova-lsp                           │
│                               │                                  │
│                               ├── Request Router                │
│                               │   ├── textDocument/completion   │
│                               │   ├── textDocument/definition   │
│                               │   ├── textDocument/references   │
│                               │   └── ...                       │
│                               │                                  │
│                               ├── Document Sync                 │
│                               │   ├── didOpen                   │
│                               │   ├── didChange                 │
│                               │   └── didClose                  │
│                               │                                  │
│                               └── Workspace ────→ nova-*        │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Request Handling

```rust
#[derive(Clone)]
pub struct LspServer {
    workspace: Arc<Workspace>,
    client: Client,
}

impl LspServer {
    async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        
        let file_id = self.workspace.file_id_for_uri(&uri)?;
        let offset = self.workspace.offset_for_position(file_id, position)?;
        
        let items = self.workspace.completions_at(file_id, offset);
        
        Ok(Some(CompletionResponse::Array(items)))
    }
}
```

---

## LSP Protocol

### Standard Features

| Category | Methods |
|----------|---------|
| **Lifecycle** | `initialize`, `shutdown`, `exit` |
| **Sync** | `textDocument/didOpen`, `didChange`, `didClose`, `didSave` |
| **Language** | `completion`, `hover`, `signatureHelp`, `definition`, `references`, `implementation`, `typeDefinition` |
| **Workspace** | `symbol`, `executeCommand`, `applyEdit` |
| **Diagnostics** | `publishDiagnostics` (server → client) |
| **Code Actions** | `codeAction`, `codeAction/resolve` |
| **Refactoring** | `rename`, `prepareRename` |

### Custom Extensions

Nova extends LSP with custom methods (prefix: `nova/`):

```typescript
// Request: nova/syntaxTree
interface SyntaxTreeParams {
    textDocument: TextDocumentIdentifier;
}
interface SyntaxTreeResult {
    tree: string;  // Debug representation
}

// Request: nova/runTest
interface RunTestParams {
    textDocument: TextDocumentIdentifier;
    position: Position;
}
```

See [protocol-extensions.md](../docs/protocol-extensions.md) for full list.

---

## Development Guidelines

### Adding LSP Methods

1. **Define types** - Request/response structures
2. **Add handler** - In `nova-lsp`
3. **Route request** - In router
4. **Test** - Unit test + integration test
5. **Document** - Update protocol docs

```rust
// 1. Define types
#[derive(Deserialize)]
struct MyRequestParams {
    text_document: TextDocumentIdentifier,
    // ...
}

#[derive(Serialize)]
struct MyResponse {
    // ...
}

// 2. Add handler
impl LspServer {
    async fn my_request(&self, params: MyRequestParams) -> Result<MyResponse> {
        // Implementation
    }
}

// 3. Route request
router.request::<MyRequest, _>(|server, params| {
    server.my_request(params)
});
```

### Document Synchronization

```rust
// Document opened
fn did_open(&self, params: DidOpenTextDocumentParams) {
    let uri = params.text_document.uri;
    let text = params.text_document.text;
    let version = params.text_document.version;
    
    self.workspace.open_document(uri, text, version);
}

// Document changed (incremental)
fn did_change(&self, params: DidChangeTextDocumentParams) {
    let uri = params.text_document.uri;
    let version = params.text_document.version;
    
    for change in params.content_changes {
        self.workspace.apply_change(uri, version, change);
    }
}
```

### Progress Reporting

```rust
// Long-running operations should report progress
async fn index_workspace(&self) {
    let token = self.client.create_work_done_progress().await?;
    
    token.begin("Indexing", Some("Starting...")).await?;
    
    for (i, file) in files.iter().enumerate() {
        token.report(Some(i * 100 / files.len()), Some(&file.name)).await?;
        self.index_file(file).await?;
    }
    
    token.end(Some("Done")).await?;
}
```

---

## VS Code Extension

Located in `editors/vscode/`:

```
editors/vscode/
├── package.json          # Extension manifest
├── src/
│   ├── extension.ts      # Entry point
│   ├── client.ts         # Language client setup
│   └── commands.ts       # Custom commands
├── syntaxes/             # TextMate grammars
└── language-configuration.json
```

### Building

```bash
cd editors/vscode
npm install
npm run compile
```

### Features

- Language client connecting to Nova LSP
- Custom commands (run test, show syntax tree, etc.)
- Debug adapter integration
- Configuration settings

---

## Other Editors

### Neovim
  
```lua
-- Minimal Neovim LSP setup for Nova (stdio) for Java.
--
-- This file is meant to be copied into your own Neovim config. It assumes you
-- have `neovim/nvim-lspconfig` installed and `nova` available on $PATH.
--
-- Copy to:
--   - Linux/macOS: ~/.config/nvim/init.lua
--   - Windows:    %LOCALAPPDATA%\\nvim\\init.lua

local ok, lspconfig = pcall(require, "lspconfig")
if not ok then
  vim.api.nvim_err_writeln("nova-lsp: missing dependency 'nvim-lspconfig'")
  return
end

local configs = require("lspconfig.configs")
local util = require("lspconfig.util")

-- `nvim-lspconfig` doesn't ship a built-in `nova-lsp` config, so define one if it
-- doesn't already exist.
if not configs.nova_lsp then
  configs.nova_lsp = {
    default_config = {
      cmd = { "nova", "lsp" },
      filetypes = { "java" },
      root_dir = util.root_pattern(
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
        -- Bazel workspace markers.
        "WORKSPACE",
        "WORKSPACE.bazel",
        "MODULE.bazel",
        ".git",
        ".nova"
      ),
    },
  }
end

local function organize_imports()
  vim.lsp.buf.code_action({
    context = { only = { "source.organizeImports" } },
  })
end

local function on_attach(_, bufnr)
  vim.api.nvim_buf_create_user_command(bufnr, "NovaOrganizeImports", organize_imports, {
    desc = "Organize imports (source.organizeImports)",
  })

  -- <leader>oi = "organize imports"
  vim.keymap.set("n", "<leader>oi", "<cmd>NovaOrganizeImports<CR>", {
    buffer = bufnr,
    desc = "Organize imports",
  })
end

lspconfig.nova_lsp.setup({
  on_attach = on_attach,
})
```
 
### Emacs
  
```elisp
;; editors/emacs/nova.el
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs
               '(java-mode . ("nova" "lsp"))))

(add-hook 'java-mode-hook #'eglot-ensure)
```

---

## Testing

```bash
# LSP server tests
bash scripts/cargo_agent.sh test --locked -p nova-lsp --lib

# Router tests
bash scripts/cargo_agent.sh test --locked -p nova-router --lib

# CLI tests
bash scripts/cargo_agent.sh test --locked -p nova-cli --lib
```

### Integration Tests

```rust
#[tokio::test]
async fn test_completion() {
    let server = TestServer::new().await;
    
    server.open_file("Test.java", "class Test { void foo() { this.| } }").await;
    
    let completions = server.completion("Test.java", Position { line: 0, character: 35 }).await;
    
    assert!(completions.iter().any(|c| c.label == "foo"));
}
```

---

## Common Pitfalls

1. **Blocking the event loop** - Use async/spawn for heavy work
2. **URI encoding** - Handle spaces, special characters
3. **Position encoding** - UTF-16 code units (LSP spec)
4. **Capability negotiation** - Check client capabilities
5. **Cancellation** - Handle `$/cancelRequest`

---

## Dependencies

**Upstream:** `nova-ide`, `nova-refactor`, `nova-workspace`
**Downstream:** Editor extensions

---

## Coordination

LSP changes may require:
- VS Code extension updates
- Documentation updates
- Protocol extension documentation

---

*Remember: Always follow [AGENTS.md](../AGENTS.md) rules. Use wrapper scripts. Scope your cargo commands.*

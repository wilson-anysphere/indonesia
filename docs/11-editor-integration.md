# 11 - Editor Integration

[← Back to Main Document](../AGENTS.md) | [Previous: Performance Engineering](10-performance-engineering.md)

## Overview

Nova communicates with editors through the Language Server Protocol (LSP). This document covers LSP implementation, custom extensions, and multi-editor support strategy.

**Implementation note:** Protocol stack decisions are captured in [ADR 0003](adr/0003-protocol-frameworks-lsp-dap.md). Some examples below use an `lsp-server`-style message loop; specific APIs in the current codebase may differ while the transport layer is still evolving.

---

## LSP Implementation

### Supported Features

```
┌─────────────────────────────────────────────────────────────────┐
│                    LSP FEATURE SUPPORT                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  TEXT SYNCHRONIZATION                                           │
│  ✓ Full document sync                                           │
│  ✓ Incremental sync                                             │
│  ✓ Will save / did save                                         │
│                                                                  │
│  LANGUAGE FEATURES                                               │
│  ✓ Hover                                                        │
│  ✓ Completion (+ resolve)                                       │
│  ✓ Signature help                                               │
│  ✓ Go to definition / declaration / type definition             │
│  ✓ Find references                                              │
│  ✓ Document highlight                                           │
│  ✓ Document symbol                                              │
│  ✓ Workspace symbol                                             │
│  ✓ Code action                                                  │
│  ✓ Code lens (+ resolve)                                        │
│  ✓ Document formatting / range formatting                       │
│  ✓ On-type formatting                                           │
│  ✓ Rename (+ prepare)                                           │
│  ✓ Folding range                                                │
│  ✓ Selection range                                              │
│  ✓ Semantic tokens (full + delta)                               │
│  ✓ Inlay hints                                                  │
│  ✓ Call hierarchy (incoming + outgoing)                         │
│  ✓ Type hierarchy                                               │
│                                                                  │
│  WORKSPACE FEATURES                                              │
│  ✓ Workspace folders                                            │
│  ✓ File operations (create/rename/delete)                       │
│  ✓ Configuration                                                │
│  ✓ Workspace edit                                               │
│                                                                  │
│  WINDOW FEATURES                                                 │
│  ✓ Show message / message request                               │
│  ✓ Show document                                                │
│  ✓ Progress                                                     │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Server Architecture

```rust
pub struct NovaServer {
    /// Core database
    db: Arc<RwLock<Database>>,
    
    /// LSP connection
    connection: Connection,
    
    /// Open documents
    documents: DashMap<Url, Document>,
    
    /// Configuration
    config: RwLock<NovaConfig>,
    
    /// Cancellation tokens for in-flight requests
    cancellations: DashMap<RequestId, CancellationToken>,
    
    /// Background task handle
    background: BackgroundScheduler,
}

impl NovaServer {
    pub async fn run(self) -> Result<()> {
        loop {
            select! {
                msg = self.connection.receiver.recv() => {
                    match msg? {
                        Message::Request(req) => {
                            self.handle_request(req).await?;
                        }
                        Message::Notification(not) => {
                            self.handle_notification(not).await?;
                        }
                        Message::Response(resp) => {
                            self.handle_response(resp).await?;
                        }
                    }
                }
                
                // Handle background task completion
                result = self.background.next() => {
                    self.handle_background_result(result).await?;
                }
            }
        }
    }
}
```

### Request Handling

```rust
impl NovaServer {
    async fn handle_request(&self, req: Request) -> Result<()> {
        // Create cancellation token
        let cancel = CancellationToken::new();
        self.cancellations.insert(req.id.clone(), cancel.clone());
        
        // Dispatch based on method
        let result = match req.method.as_str() {
            "textDocument/completion" => {
                self.completion(req.params, cancel).await
            }
            "textDocument/hover" => {
                self.hover(req.params, cancel).await
            }
            "textDocument/definition" => {
                self.definition(req.params, cancel).await
            }
            "textDocument/references" => {
                self.references(req.params, cancel).await
            }
            "textDocument/rename" => {
                self.rename(req.params, cancel).await
            }
            "textDocument/codeAction" => {
                self.code_action(req.params, cancel).await
            }
            // ... other methods
            _ => {
                Err(LspError::method_not_found())
            }
        };
        
        // Remove cancellation token
        self.cancellations.remove(&req.id);
        
        // Send response
        let response = match result {
            Ok(value) => Response::ok(req.id, value),
            Err(e) => Response::err(req.id, e),
        };
        
        self.connection.sender.send(Message::Response(response))?;
        
        Ok(())
    }
    
    async fn completion(
        &self,
        params: CompletionParams,
        cancel: CancellationToken,
    ) -> Result<CompletionResponse> {
        let file = self.uri_to_file(&params.text_document.uri)?;
        let position = params.position;
        
        // Get database snapshot (consistent view)
        let db = self.db.read().snapshot();
        
        // Check for cancellation
        if cancel.is_cancelled() {
            return Err(LspError::request_cancelled());
        }
        
        // Compute completions
        let items = db.completions_at(file, position.into());
        
        Ok(CompletionResponse::List(CompletionList {
            is_incomplete: items.len() >= MAX_COMPLETIONS,
            items,
        }))
    }
}
```

### Document Synchronization

```rust
impl NovaServer {
    async fn did_open(&self, params: DidOpenTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri;
        let content = params.text_document.text;
        let version = params.text_document.version;
        
        // Store document
        self.documents.insert(uri.clone(), Document {
            content: content.clone(),
            version,
        });
        
        // Update database
        {
            let mut db = self.db.write();
            let file = db.file_for_uri(&uri);
            db.set_file_content(file, content);
        }
        
        // Trigger diagnostics
        self.schedule_diagnostics(uri).await;
        
        Ok(())
    }
    
    async fn did_change(&self, params: DidChangeTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri;
        
        // Apply changes
        let mut doc = self.documents.get_mut(&uri)
            .ok_or(LspError::invalid_params("Document not open"))?;
        
        for change in params.content_changes {
            if let Some(range) = change.range {
                // Incremental change
                doc.apply_change(range, &change.text);
            } else {
                // Full content
                doc.content = change.text;
            }
        }
        
        doc.version = params.text_document.version;
        
        // Update database
        {
            let mut db = self.db.write();
            let file = db.file_for_uri(&uri);
            db.set_file_content(file, doc.content.clone());
        }
        
        // Trigger diagnostics (debounced)
        self.schedule_diagnostics_debounced(uri, Duration::from_millis(200)).await;
        
        Ok(())
    }
}
```

---

## Nova LSP Extensions

### Custom Methods
 
```
┌─────────────────────────────────────────────────────────────────┐
│                    NOVA LSP EXTENSIONS                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  PROJECT MANAGEMENT                                              │
│  • nova/projectConfiguration - Get project structure            │
│  • nova/reloadProject - Force project reload                    │
│  • nova/buildProject - Trigger build                            │
│                                                                  │
│  JAVA-SPECIFIC                                                   │
│  • nova/java/classpath - Get classpath info                     │
│  • nova/java/sourcePaths - Get source paths                     │
│  • nova/java/resolveMainClass - Find runnable classes           │
│  • nova/java/organizeImports - Organize imports                 │
│                                                                  │
│  FRAMEWORK SUPPORT                                               │
│  • nova/spring/beans - List Spring beans                        │
│  • nova/spring/endpoints - List REST endpoints                  │
│  • nova/spring/navigateToBean - Navigate to bean def            │
│                                                                  │
│  REFACTORING                                                     │
│  • nova/refactor/preview - Preview refactoring                  │
│  • nova/refactor/apply - Apply with options                     │
│                                                                  │
│  TESTING                                                         │
│  • nova/test/discover - Discover tests                          │
│  • nova/test/run - Run specific tests                           │
│                                                                  │
│  DEBUGGING                                                       │
│  • nova/debug/configurations - List debug configs               │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```
 
### Project configuration payload (language level included)

Version-aware analysis requires the client and server to agree on the effective Java language mode per module. `nova/projectConfiguration` should therefore include:
- detected/overridden **Java language level** (`major` + `preview`)
- where it came from (config override vs build tool)
- enough mapping information for per-file language level attribution

Example response shape (illustrative):

```json
{
  "modules": [
    {
      "id": ":app",
      "displayName": "app",
      "sourceRoots": ["file:///ws/app/src/main/java"],
      "classpath": ["file:///ws/.gradle/caches/.../guava.jar"],
      "languageLevel": { "major": 17, "preview": false },
      "languageLevelOrigin": "gradle"
    },
    {
      "id": ":experiments",
      "displayName": "experiments",
      "sourceRoots": ["file:///ws/experiments/src/main/java"],
      "classpath": [],
      "languageLevel": { "major": 21, "preview": true },
      "languageLevelOrigin": "nova-config"
    }
  ]
}
```

The server must store `module → JavaLanguageLevel` and derive `file → JavaLanguageLevel` for all parse/diagnostics queries. See: [16 - Java Language Levels and Feature Gating](16-java-language-levels.md).

### Dynamic updates

Language level can change while the server is running (e.g., `pom.xml` edited, Gradle toolchain updated, or `nova-config` override changed). Nova should handle this like any other incremental input change:
- rebuild the module configuration
- invalidate parse/semantic queries for files in affected modules
- re-publish diagnostics (and refresh semantic tokens/completions as needed)

Triggers:
- `workspace/didChangeConfiguration` (config overrides)
- file watching on build files (`pom.xml`, `build.gradle(.kts)`, `MODULE.bazel`/`BUILD`)
- explicit `nova/reloadProject`
 
### Extension Implementation
 
```rust
impl NovaServer {
    fn register_extensions(&self) {
        // Project configuration
        self.register_method("nova/projectConfiguration", |s, params| {
            s.project_configuration(params)
        });
        
        // Classpath
        self.register_method("nova/java/classpath", |s, params| {
            s.java_classpath(params)
        });
        
        // Spring beans
        self.register_method("nova/spring/beans", |s, params| {
            s.spring_beans(params)
        });
        
        // Refactoring preview
        self.register_method("nova/refactor/preview", |s, params| {
            s.refactor_preview(params)
        });
    }
    
    fn spring_beans(&self, params: SpringBeansParams) -> Result<SpringBeansResponse> {
        let db = self.db.read().snapshot();
        let project = db.project_for_uri(&params.uri)?;
        
        let beans: Vec<_> = db.spring_analyzer()
            .get_beans(project)
            .iter()
            .map(|b| SpringBeanInfo {
                name: b.name.clone(),
                bean_type: format_type(&b.bean_type),
                scope: b.scope.to_string(),
                profiles: b.profiles.clone(),
                location: Location {
                    uri: b.file.to_uri(),
                    range: b.range,
                },
            })
            .collect();
        
        Ok(SpringBeansResponse { beans })
    }
}
```

---

## Multi-Editor Support

### Editor-Specific Considerations

```
┌─────────────────────────────────────────────────────────────────┐
│                    EDITOR SUPPORT                                │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  VS CODE                                                        │
│  • Full LSP support                                             │
│  • Rich extension API for UI                                    │
│  • Nova extension provides:                                     │
│    - Project explorer integration                               │
│    - Debug launch configurations                                │
│    - Test runner integration                                    │
│    - Spring Boot dashboard                                      │
│                                                                  │
│  NEOVIM                                                         │
│  • Built-in LSP client (0.5+)                                   │
│  • nvim-lspconfig for easy setup                                │
│  • UI via telescope, nvim-cmp                                   │
│  • Nova provides: lua config template                           │
│                                                                  │
│  EMACS                                                          │
│  • lsp-mode or eglot                                            │
│  • Company for completion                                       │
│  • Nova provides: elisp configuration                           │
│                                                                  │
│  SUBLIME TEXT                                                    │
│  • LSP package                                                  │
│  • Nova provides: LSP settings template                         │
│                                                                  │
│  HELIX                                                          │
│  • Built-in LSP support                                         │
│  • languages.toml configuration                                 │
│                                                                  │
│  JetBrains IDEs                                                 │
│  • LSP plugin available                                         │
│  • May prefer native IntelliJ for full features                 │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### VS Code Extension

```typescript
// VS Code extension for Nova
export function activate(context: vscode.ExtensionContext) {
    // Start language server
    const serverOptions: ServerOptions = {
        command: 'nova-lsp',
        args: ['--stdio'],
    };
    
    const clientOptions: LanguageClientOptions = {
        documentSelector: [{ scheme: 'file', language: 'java' }],
        synchronize: {
            fileEvents: vscode.workspace.createFileSystemWatcher('**/*.java'),
        },
    };
    
    const client = new LanguageClient(
        'nova',
        'Nova Java Language Server',
        serverOptions,
        clientOptions
    );
    
    // Register custom commands
    context.subscriptions.push(
        vscode.commands.registerCommand('nova.organizeImports', () => {
            client.sendRequest('nova/java/organizeImports', {
                uri: vscode.window.activeTextEditor?.document.uri.toString(),
            });
        }),
        
        vscode.commands.registerCommand('nova.showBeans', async () => {
            const beans = await client.sendRequest('nova/spring/beans', {});
            showBeansView(beans);
        }),
    );
    
    // Spring Boot dashboard
    const springProvider = new SpringBootTreeDataProvider(client);
    vscode.window.registerTreeDataProvider('novaSpringBoot', springProvider);
    
    client.start();
}
```

---

## Progress and Status

### Progress Reporting

```rust
impl NovaServer {
    async fn report_progress<T>(
        &self,
        title: &str,
        task: impl Future<Output = T>,
    ) -> T {
        // Create progress token
        let token = ProgressToken::String(Uuid::new_v4().to_string());
        
        // Begin progress
        self.connection.send_notification::<WorkDoneProgressCreate>(
            WorkDoneProgressCreateParams { token: token.clone() }
        ).await?;
        
        self.connection.send_notification::<Progress>(
            ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(
                    WorkDoneProgress::Begin(WorkDoneProgressBegin {
                        title: title.into(),
                        cancellable: Some(true),
                        message: None,
                        percentage: None,
                    })
                ),
            }
        ).await?;
        
        // Run task
        let result = task.await;
        
        // End progress
        self.connection.send_notification::<Progress>(
            ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(
                    WorkDoneProgress::End(WorkDoneProgressEnd {
                        message: Some("Complete".into()),
                    })
                ),
            }
        ).await?;
        
        result
    }
    
    async fn index_project(&self) {
        self.report_progress("Indexing project", async {
            let files = self.db.read().project_files();
            let total = files.len();
            
            for (i, file) in files.iter().enumerate() {
                // Update progress
                self.update_progress(
                    &format!("Indexing: {}", file.name()),
                    (i * 100 / total) as u32,
                ).await;
                
                // Index file
                self.db.write().index_file(*file);
            }
        }).await;
    }
}
```

### Status Bar

```rust
/// Custom status messages
impl NovaServer {
    async fn send_status(&self, status: NovaStatus) {
        // Use custom notification for status
        self.connection.send_notification::<NovaStatusNotification>(
            NovaStatusParams {
                state: status.state,
                message: status.message,
                icon: status.icon,
            }
        ).await;
    }
    
    async fn set_indexing_status(&self, progress: f32) {
        self.send_status(NovaStatus {
            state: "indexing",
            message: format!("Indexing: {:.0}%", progress * 100.0),
            icon: "sync~spin",
        }).await;
    }
    
    async fn set_ready_status(&self) {
        self.send_status(NovaStatus {
            state: "ready",
            message: "Nova: Ready",
            icon: "check",
        }).await;
    }
    
    async fn set_error_status(&self, error: &str) {
        self.send_status(NovaStatus {
            state: "error",
            message: format!("Nova: {}", error),
            icon: "error",
        }).await;
    }
}
```

---

## Error Handling

Nova is designed to be resilient under editor workloads:

- The `nova-lsp` and `nova-dap` binaries wrap request handling in `catch_unwind` so a panic in one
  handler does not take down the entire process.
- Nova’s custom `nova/*` extension endpoints (e.g. build/test integration) run under a watchdog
  (`nova_scheduler::Watchdog`) with per-method deadlines. If a request panics or times out, Nova can
  temporarily enter **safe mode** to avoid repeatedly triggering the same failure.
- When safe mode is active, Nova keeps `nova/bugReport` available so clients can collect a
  diagnostic bundle.

For practical operational guidance (where logs go, how to generate bug report bundles, and how safe
mode behaves), see:

- [17 - Observability and Reliability](17-observability-and-reliability.md)

---

## Testing LSP

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use lsp_server::Message;
    
    #[tokio::test]
    async fn test_completion() {
        let (server, client) = create_test_pair();
        
        // Open document
        client.send(notification("textDocument/didOpen", DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: "file:///test/Main.java".into(),
                language_id: "java".into(),
                version: 1,
                text: "class Main { String s; void foo() { s.| } }".into(),
            },
        })).await;
        
        // Request completion
        let response = client.request("textDocument/completion", CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: "file:///test/Main.java".into(),
                },
                position: Position { line: 0, character: 40 },
            },
            ..Default::default()
        }).await;
        
        // Verify
        let completions: CompletionResponse = response.result.unwrap();
        let items = match completions {
            CompletionResponse::List(list) => list.items,
            _ => panic!("Expected list"),
        };
        
        assert!(items.iter().any(|i| i.label == "length"));
        assert!(items.iter().any(|i| i.label == "charAt"));
    }
}
```

---

## Next Steps

1. → [Debugging Integration](12-debugging-integration.md): DAP implementation
2. → [AI Augmentation](13-ai-augmentation.md): ML-powered features

---

[← Previous: Performance Engineering](10-performance-engineering.md) | [Next: Debugging Integration →](12-debugging-integration.md)

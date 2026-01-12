# 11 - Editor Integration

[← Back to Main Document](../AGENTS.md) | [Previous: Performance Engineering](10-performance-engineering.md)

## Overview

Nova communicates with editors through the Language Server Protocol (LSP). This document covers the `nova-lsp` stdio server implementation, Nova-specific extensions, and multi-editor support strategy.

**Launcher note:** Editors can invoke Nova's LSP server as `nova lsp` (recommended) instead of calling `nova-lsp` directly. The `nova lsp` subcommand is a thin stdio wrapper that locates and spawns the `nova-lsp` binary.

**Implementation note:** Protocol stack decisions are captured in [ADR 0003](adr/0003-protocol-frameworks-lsp-dap.md). The `nova-lsp` binary is currently **stdio-only** (JSON-RPC over stdin/stdout) and is implemented using [`lsp-server`](https://crates.io/crates/lsp-server). The authoritative capability advertisement lives in [`crates/nova-lsp/src/main.rs::initialize_result_json()`](../crates/nova-lsp/src/main.rs).

---

## LSP Implementation

### Supported Features

```
┌─────────────────────────────────────────────────────────────────┐
│                    LSP FEATURE SUPPORT                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Legend                                                         │
│  ✓ = implemented + advertised by `initialize`                    │
│  △ = implemented (but not advertised / no capability flag)       │
│  ○ = not yet implemented                                         │
│                                                                  │
│  LANGUAGE FEATURES                                               │
│  ✓ Completion (+ completionItem/resolve)                         │
│  ✓ Go to definition / declaration / type definition / impl      │
│  ✓ Diagnostics (pull model: textDocument/diagnostic)            │
│  ✓ Document symbol                                              │
│  ✓ Workspace symbol                                             │
│  ✓ Code action (+ codeAction/resolve)                            │
│  ✓ Code lens (+ codeLens/resolve)                                │
│  ✓ Formatting (document / range / on-type)                       │
│  ✓ Rename (+ prepareRename)                                      │
│  ✓ Semantic tokens (full + delta)                                │
│  ✓ Inlay hints                                                  │
│  ✓ Hover                                                        │
│  ✓ Signature help                                               │
│  ✓ Find references                                              │
│  ✓ Document highlight                                           │
│  ✓ Folding range                                                │
│  ✓ Selection range                                              │
│  ✓ Call hierarchy                                               │
│  ✓ Type hierarchy                                               │
│                                                                  │
│  TEXT SYNCHRONIZATION                                            │
│  ✓ didOpen / didChange (incremental) / didClose                  │
│  ✓ Will save / did save                                          │
│                                                                  │
│  WORKSPACE FEATURES                                              │
│  ✓ workspace/executeCommand                                      │
│  ✓ Workspace folders (workspace/didChangeWorkspaceFolders)        │
│  ✓ File operations (create/delete/rename)                        │
│  △ Configuration reload (workspace/didChangeConfiguration)        │
│                                                                  │
│  WINDOW FEATURES                                                 │
│  △ window/logMessage (used by AI features)                      │
│  △ $/progress (when a `workDoneToken` is supplied)              │
│  ○ window/showMessage / showMessageRequest / showDocument       │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

Notes:

- The server currently implements `$/cancelRequest` and uses request-scoped cancellation tokens internally.
- Workspace folders are supported, but the stdio server currently treats the first workspace folder as the active project root (best-effort multi-root).
- Diagnostics are provided via the LSP 3.17 **pull** model (`textDocument/diagnostic`). The server does not currently publish `textDocument/publishDiagnostics`.
- The stdio server requests standard file-operation notifications via `initializeResult.capabilities.workspace.fileOperations` and supports a fallback `nova/workspace/renamePath` notification for clients that cannot send `workspace/didRenameFiles` (see `protocol-extensions.md`). Editor clients should prefer sending the standard file-operation notifications through their LSP client library (e.g. `vscode-languageclient`'s `fileOperations` feature) rather than manually forwarding editor file events, to avoid duplicate notifications.
- `workspace/didChangeWatchedFiles` is handled, but the server does not dynamically register file watchers today; clients must configure watchers on their side if they want to send these notifications.
- OS file watching for the workspace engine (used by `nova` CLI / `nova-workspace`) is implemented in `nova-vfs` behind `watch-notify`. See [`file-watching.md`](file-watching.md) for the watcher layering and deterministic testing guidance.
- Some Nova commands/requests apply edits by sending the standard `workspace/applyEdit` request to the client (e.g. `nova/java/organizeImports`, `nova.safeDelete`). Clients must handle `workspace/applyEdit` for these to take effect.
- `textDocument/rename` (and some refactor-related edits) may include file operations (create/delete/rename) and therefore use `WorkspaceEdit.documentChanges` with resource operations instead of the legacy `WorkspaceEdit.changes` map. Editor clients must support applying `documentChanges` for these edits to work correctly.

### Server Architecture

```rust
use lsp_server::{Connection, Message};

fn main() -> std::io::Result<()> {
    // `nova-lsp` is currently stdio-only.
    let (connection, io_threads) = Connection::stdio();

    // Initialize handshake.
    let (init_id, _init_params) = connection.initialize_start()?;
    connection.initialize_finish(init_id, initialize_result_json())?;

    // Main stdio message loop.
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                // Dispatch by `req.method` (e.g. "textDocument/completion")
            }
            Message::Notification(not) => {
                // Handle notifications (e.g. didOpen/didChange/didClose)
            }
            Message::Response(_) => {
                // Best-effort: ignored today.
            }
        }
    }

    io_threads.join()?;
    Ok(())
}
```

### Request Handling

```rust
fn handle_request(method: &str, params: serde_json::Value) -> serde_json::Value {
    match method {
        "textDocument/completion" => handle_completion(params),
        "textDocument/hover" => handle_hover(params),
        "textDocument/signatureHelp" => handle_signature_help(params),
        "textDocument/references" => handle_references(params),
        "completionItem/resolve" => handle_completion_resolve(params),
        "textDocument/semanticTokens/full" => handle_semantic_tokens_full(params),
        "textDocument/semanticTokens/full/delta" => handle_semantic_tokens_full_delta(params),
        "textDocument/definition" => handle_definition(params),
        "textDocument/diagnostic" => handle_diagnostics(params),
        // ...and several more; see `crates/nova-lsp/src/main.rs::handle_request_json`
        _ => json!({
            "error": { "code": -32601, "message": format!("Method not found: {method}") }
        }),
    }
}
```

### Document Synchronization

```rust
fn handle_notification(method: &str, params: serde_json::Value) {
    match method {
        "textDocument/didOpen" => {
            // Store the full text in a VFS overlay.
        }
        "textDocument/didChange" => {
            // Apply incremental edits (`TextDocumentSyncKind::INCREMENTAL`).
        }
        "textDocument/willSave" => {
            // Best-effort: parsed, but typically no work is required.
        }
        "textDocument/didSave" => {
            // Best-effort: refresh from disk (or apply saved text if provided).
        }
        "textDocument/didClose" => {
            // Drop the overlay.
        }
        _ => {}
    }
}
```

---

## Nova LSP Extensions

### Custom Methods

Nova exposes a number of custom JSON-RPC requests under the `nova/*` namespace.

The stdio server advertises the supported custom request list in the `initialize` response under:

- `InitializeResult.capabilities.experimental.nova.requests`
- `InitializeResult.capabilities.experimental.nova.notifications`

For the authoritative list and exact JSON schemas, see:
[`protocol-extensions.md`](protocol-extensions.md) and `crates/nova-lsp/src/lib.rs`.

Notes for client authors:

- Always gate custom `nova/*` usage on the server-advertised method list
  (`initializeResult.capabilities.experimental.nova.requests`). Several endpoints are conditionally
  available depending on build features and runtime configuration.
- In particular, `nova/completion/more` is only advertised when `nova-lsp` is built with the `ai`
  feature (enabled by default in this repo).
- The `nova/ai/*` request family is advertised by default, but will return a JSON-RPC error if AI is
  not configured/enabled on the server (e.g. `"AI is not configured"`).
   
```
┌─────────────────────────────────────────────────────────────────┐
│                    NOVA LSP EXTENSIONS                           │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  TESTING                                                         │
│  • nova/test/discover                                            │
│  • nova/test/run                                                 │
│  • nova/test/debugConfiguration                                  │
│                                                                  │
│  PROJECT / BUILD / JAVA                                           │
│  • nova/projectConfiguration                                     │
│  • nova/projectModel                                             │
│  • nova/reloadProject                                            │
│  • nova/buildProject                                             │
│  • nova/java/classpath                                           │
│  • nova/java/sourcePaths                                         │
│  • nova/java/resolveMainClass                                    │
│  • nova/java/generatedSources                                    │
│  • nova/java/runAnnotationProcessing                             │
│  • nova/java/organizeImports                                     │
│                                                                  │
│  FRAMEWORKS                                                      │
│  • nova/web/endpoints                                            │
│  • nova/quarkus/endpoints (alias)                                │
│  • nova/micronaut/endpoints                                      │
│  • nova/micronaut/beans                                          │
│                                                                  │
│  DEBUGGING                                                       │
│  • nova/debug/configurations                                     │
│  • nova/debug/hotSwap                                            │
│                                                                  │
│  BUILD STATUS / DIAGNOSTICS                                       │
│  • nova/build/targetClasspath                                    │
│  • nova/build/status                                             │
│  • nova/build/diagnostics                                        │
│                                                                  │
│  RESILIENCE / OBSERVABILITY                                       │
│  • nova/bugReport                                                │
│  • nova/memoryStatus                                             │
│  • nova/metrics                                                  │
│  • nova/resetMetrics                                             │
│  • nova/safeModeStatus                                           │
│                                                                  │
│  REFACTOR (CUSTOM REQUESTS)                                       │
│  • nova/refactor/safeDelete                                      │
│  • nova/refactor/changeSignature                                 │
│  • nova/refactor/moveMethod                                      │
│  • nova/refactor/moveStaticMember                                │
│                                                                  │
│  AI (CUSTOM REQUESTS)                                             │
│  • nova/ai/explainError                                          │
│  • nova/ai/generateMethodBody                                    │
│  • nova/ai/generateTests                                         │
│  • nova/completion/more                                          │
│                                                                  │
│  SEMANTIC SEARCH                                                  │
│  • nova/semanticSearch/indexStatus                                │
│                                                                  │
│  EXTENSIONS (WASM)                                                │
│  • nova/extensions/status                                        │
│  • nova/extensions/navigation                                    │
│                                                                  │
│  CUSTOM NOTIFICATIONS (experimental.nova.notifications)           │
│  • nova/memoryStatusChanged (server → client)                    │
│  • nova/safeModeChanged (server → client)                        │
│  • nova/workspace/renamePath (client → server)                   │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```
  
### `nova/projectConfiguration` payload

The current stdio server implementation expects:

- request params: `{ "projectRoot": "<absolute path on disk>" }` (alias `root` is accepted)
- response fields (see [`crates/nova-lsp/src/extensions/project.rs`](../crates/nova-lsp/src/extensions/project.rs)):
  - `schemaVersion`
  - `workspaceRoot`
  - `buildSystem`
  - `java` (`source` + `target`)
  - `modules`, `sourceRoots`
  - `classpath`, `modulePath`
  - `outputDirs`
  - `dependencies`

```json
{
  "schemaVersion": 1,
  "workspaceRoot": "/ws",
  "buildSystem": "gradle",
  "java": { "source": 17, "target": 17 },
  "modules": [{ "name": ":app", "root": "/ws/app" }],
  "sourceRoots": [{ "kind": "main", "origin": "source", "path": "/ws/app/src/main/java" }],
  "classpath": [{ "kind": "jar", "path": "/ws/.gradle/caches/.../guava.jar" }],
  "modulePath": [],
  "outputDirs": [{ "kind": "main", "path": "/ws/app/build/classes/java/main" }],
  "dependencies": [{ "groupId": "com.google.guava", "artifactId": "guava", "version": "32.1.0-jre" }]
}
```

### Extension Implementation
  
```rust
match method {
    // Standard LSP requests...
    "textDocument/completion" => { /* ... */ }

    // A handful of stateful Nova requests are handled directly in the binary...
    nova_lsp::JAVA_ORGANIZE_IMPORTS_METHOD => { /* ... */ }

    // ...and most `nova/*` requests are dispatched through `nova-lsp`'s extension router.
    method if method.starts_with("nova/") => {
        nova_lsp::handle_custom_request_cancelable(method, params, cancel)?;
    }

    _ => { /* method not found */ }
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
│    - Frameworks dashboard ("Nova Frameworks" view in Explorer)  │
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

Templates shipped with this repo:

- VS Code: [`editors/vscode/README.md`](../editors/vscode/README.md)
- Neovim: [`editors/neovim/README.md`](../editors/neovim/README.md)
- Emacs: [`editors/emacs/README.md`](../editors/emacs/README.md)
- Sublime Text: [`editors/sublime/README.md`](../editors/sublime/README.md)
- Helix: [`editors/helix/README.md`](../editors/helix/README.md)

### VS Code Extension

Example / sketch of wiring up a **Frameworks dashboard** view (`novaFrameworks`) that queries
framework-introspection endpoints. The canonical method list + JSON schemas are in
[`protocol-extensions.md`](protocol-extensions.md); clients should treat "method not found" as capability gating
(older servers) and degrade gracefully (some Nova builds report unknown custom methods as `-32601` or `-32602` with an
“unknown (stateless) method” message).

When available, clients may also pre-gate features using the server-advertised capability list
`initializeResult.capabilities.experimental.nova.requests` (see `protocol-extensions.md`).

The real VS Code UX in this repo is an Explorer tree view (`novaFrameworks`, labeled “Nova Frameworks”) that:

- groups framework-derived navigation targets by **workspace folder** and **category**
- surfaces:
  - **Web endpoints** (via `nova/web/endpoints`, with `nova/quarkus/endpoints` as an alias)
  - **Micronaut endpoints** (via `nova/micronaut/endpoints`)
  - **Micronaut beans** (via `nova/micronaut/beans`)
- lets you click leaf items to jump to the source file (best-effort)
- exposes context menu actions (copy endpoint path, copy method+path, copy bean id/type, reveal in explorer). These are keyed off
  `TreeItem.contextValue` (e.g., `novaFrameworkEndpoint` for endpoints and `novaFrameworkBean` for beans).
- is refreshed on-demand via a view toolbar button / command (`nova.frameworks.refresh`)
- supports quick navigation via **Nova: Search Framework Items…** (`nova.frameworks.search`)
- shows contextual empty-state messages when no folder is open, the server isn't running, or discovery is unavailable

Discovery is intentionally manual because these requests run under a small watchdog time budget; repeatedly refreshing
could time out or trigger Nova safe mode.

Payload notes (see `protocol-extensions.md` for full schemas):

- `nova/web/endpoints` returns `file` (best-effort; may be `null`/missing) + **1-based** `line` (and `methods`), where
  `file` is often relative to `projectRoot`. Clients should still show the endpoint when `file` is unavailable, but
  disable navigation / show “location unavailable”.
- `nova/micronaut/endpoints` and `nova/micronaut/beans` include a `schemaVersion` field (currently `1`). Clients should
  validate it and reject unknown versions.
- Micronaut responses include `span.start` / `span.end` as **byte offsets** into UTF-8 source; clients may optionally
  translate that span into an editor selection.
```typescript
// VS Code extension for Nova (example / sketch).
//
// Note: `protocol-extensions.md` is the source of truth for supported `nova/*` methods and
// JSON schemas. Extensions should treat unknown methods as capability gating for older server builds,
// and degrade gracefully (some Nova builds report unknown custom methods as `-32601` or `-32602` with an
// “unknown (stateless) method” message).

type WebEndpoint = {
  path: string;
  methods: string[];
  // Best-effort relative path. May be `null`/missing when the server can't determine a source location.
  file?: string | null;
  line: number; // 1-based
};

type WebEndpointsResponse = { endpoints: WebEndpoint[] };

type FrameworkNode = { kind: 'web-endpoint'; projectRoot: string; endpoint: WebEndpoint };

type SearchPickItem = vscode.QuickPickItem & { uri?: vscode.Uri; range?: vscode.Range };

function isAbsolutePath(value: string): boolean {
  return value.startsWith('/') || /^[a-zA-Z]:[\\/]/.test(value) || value.startsWith('\\\\');
}

function uriFromProjectFile(projectRoot: string, file: string): vscode.Uri {
  // `file` may be absolute, a file:// URI, or a relative path.
  if (file.startsWith('file:')) return vscode.Uri.parse(file);
  if (isAbsolutePath(file)) return vscode.Uri.file(file);
  const segments = file.split(/[\\/]+/).filter(Boolean);
  return vscode.Uri.joinPath(vscode.Uri.file(projectRoot), ...segments);
}

async function sendOptionalRequest<R>(
  client: LanguageClient,
  method: string,
  params: unknown,
): Promise<R | undefined> {
  try {
    return await client.sendRequest(method, params);
  } catch (err: any) {
    // JSON-RPC method not found / capability gating.
    //
    // Note: some Nova server builds report unknown custom methods as `-32602` with an
    // "unknown (stateless) method" message (because everything is routed through one dispatcher).
    const message = typeof err?.message === 'string' ? err.message.toLowerCase() : '';
    if (err?.code === -32601) return undefined;
    if (err?.code === -32602 && message.includes('unknown (stateless) method')) return undefined;
    if (message.includes('method not found')) return undefined;
    throw err;
  }
}

async function pickWorkspaceFolder(): Promise<vscode.WorkspaceFolder | undefined> {
  const folders = vscode.workspace.workspaceFolders ?? [];
  if (folders.length === 0) {
    return undefined;
  }
  if (folders.length === 1) {
    return folders[0];
  }
  const picked = await vscode.window.showQuickPick(
    folders.map((folder) => ({ label: folder.name, description: folder.uri.fsPath, folder })),
    { placeHolder: 'Select workspace folder' },
  );
  return picked?.folder;
}

class FrameworkDashboardTreeDataProvider implements vscode.TreeDataProvider<FrameworkNode> {
  private readonly onDidChangeTreeDataEmitter = new vscode.EventEmitter<FrameworkNode | undefined>();
  readonly onDidChangeTreeData = this.onDidChangeTreeDataEmitter.event;

  constructor(private readonly client: LanguageClient) {}

  refresh(): void {
    this.onDidChangeTreeDataEmitter.fire(undefined);
  }

  getTreeItem(element: FrameworkNode): vscode.TreeItem {
    const { endpoint } = element;
    const methods = Array.isArray(endpoint.methods) ? endpoint.methods.filter((m) => typeof m === 'string' && m.length > 0) : [];
    const methodLabel = methods.length > 0 ? methods.join(', ') : 'ANY';
    const label = `${methodLabel} ${endpoint.path}`;

    const item = new vscode.TreeItem(label, vscode.TreeItemCollapsibleState.None);
    item.contextValue = 'novaFrameworkEndpoint';

    const file = typeof endpoint.file === 'string' ? endpoint.file : undefined;
    const line = typeof endpoint.line === 'number' ? endpoint.line : undefined;
    if (file && typeof line === 'number') {
      const uri = uriFromProjectFile(element.projectRoot, file);
      const range = new vscode.Range(new vscode.Position(Math.max(0, line - 1), 0), new vscode.Position(Math.max(0, line - 1), 0));
      item.command = { command: 'vscode.open', title: 'Open', arguments: [uri, { selection: range }] };
      item.tooltip = `${file}:${line}`;
    } else {
      item.tooltip = 'Source location unavailable';
    }
    return item;
  }

  async getChildren(element?: FrameworkNode): Promise<FrameworkNode[]> {
    if (element) return [];

    const workspaceFolders = vscode.workspace.workspaceFolders ?? [];
    const nodes: FrameworkNode[] = [];

    for (const workspaceFolder of workspaceFolders) {
      const projectRoot = workspaceFolder.uri.fsPath;

      // Best-effort: servers that don't implement these endpoints will throw "method not found".
      // Some clients prefer the alias for backwards compat.
      const web =
        (await sendOptionalRequest<WebEndpointsResponse>(this.client, 'nova/quarkus/endpoints', { projectRoot })) ??
        (await sendOptionalRequest<WebEndpointsResponse>(this.client, 'nova/web/endpoints', { projectRoot }));

      for (const endpoint of Array.isArray(web?.endpoints) ? web.endpoints : []) {
        nodes.push({ kind: 'web-endpoint', endpoint, projectRoot });
      }
    }

    return nodes;
  }
}

export function activate(context: vscode.ExtensionContext) {
    // Start language server
    const serverOptions: ServerOptions = {
        command: 'nova',
        args: ['lsp'],
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
        vscode.commands.registerCommand('nova.organizeImports', async () => {
            const uri = vscode.window.activeTextEditor?.document.uri.toString();
            if (!uri) return;
            await client.sendRequest('nova/java/organizeImports', { uri });
        }),
    );
    
    // Frameworks dashboard (Explorer: "Nova Frameworks" view, id: `novaFrameworks`)
    const frameworksProvider = new FrameworkDashboardTreeDataProvider(client);
    const frameworksView = vscode.window.createTreeView('novaFrameworks', { treeDataProvider: frameworksProvider, showCollapseAll: false });
    context.subscriptions.push(frameworksView);
    context.subscriptions.push(vscode.commands.registerCommand('nova.frameworks.refresh', () => frameworksProvider.refresh()));
    context.subscriptions.push(vscode.commands.registerCommand('nova.frameworks.search', async () => {
        const folder = await pickWorkspaceFolder();
        if (!folder) return;
        const projectRoot = folder.uri.fsPath;

        const web =
          (await sendOptionalRequest<WebEndpointsResponse>(client, 'nova/quarkus/endpoints', { projectRoot })) ??
          (await sendOptionalRequest<WebEndpointsResponse>(client, 'nova/web/endpoints', { projectRoot }));
        const micronautEndpoints = await sendOptionalRequest<any>(client, 'nova/micronaut/endpoints', { projectRoot });
        const micronautBeans = await sendOptionalRequest<any>(client, 'nova/micronaut/beans', { projectRoot });

        const items: SearchPickItem[] = [];
        for (const endpoint of Array.isArray(web?.endpoints) ? web.endpoints : []) {
          const file = typeof endpoint.file === 'string' ? endpoint.file : undefined;
          const line = typeof endpoint.line === 'number' ? endpoint.line : undefined;
          const methods = Array.isArray(endpoint.methods) ? endpoint.methods.filter((m) => typeof m === 'string') : [];
          const methodLabel = methods.length > 0 ? methods.join(', ') : 'ANY';
          const label = `${methodLabel} ${endpoint.path}`.trim();
          if (!file || typeof line !== 'number') continue;
          const uri = uriFromProjectFile(projectRoot, file);
          const range = new vscode.Range(new vscode.Position(Math.max(0, line - 1), 0), new vscode.Position(Math.max(0, line - 1), 0));
          items.push({ label, description: `${file}:${line}`, uri, range });
        }

        for (const endpoint of Array.isArray(micronautEndpoints?.endpoints) ? micronautEndpoints.endpoints : []) {
          const file = typeof endpoint?.handler?.file === 'string' ? endpoint.handler.file : undefined;
          if (!file) continue;
          const uri = uriFromProjectFile(projectRoot, file);
          // Micronaut locations provide UTF-8 byte spans; translating spans to VS Code ranges is optional.
          const label = `Micronaut ${endpoint.method ?? ''} ${endpoint.path ?? ''}`.trim();
          items.push({ label: label || 'Micronaut endpoint', description: file, uri });
        }

        for (const bean of Array.isArray(micronautBeans?.beans) ? micronautBeans.beans : []) {
          const file = typeof bean?.file === 'string' ? bean.file : undefined;
          if (!file) continue;
          const uri = uriFromProjectFile(projectRoot, file);
          const label = `Micronaut bean ${bean.name ?? ''}`.trim();
          items.push({ label: label || 'Micronaut bean', description: bean.ty ?? file, uri });
        }

        const picked = await vscode.window.showQuickPick(items, { placeHolder: 'Search framework items', matchOnDescription: true });
        if (!picked?.uri) return;
        const doc = await vscode.workspace.openTextDocument(picked.uri);
        await vscode.window.showTextDocument(doc, { selection: picked.range, preview: false });
    }));
    
    client.start();
}
```
 
---
 
## Progress and Status
 
### Progress Reporting
 
```rust
// `nova-lsp` uses `window/logMessage` for user-visible logs (especially for AI
// requests), and emits work-done progress via `$/progress` when the incoming
// request provides a `workDoneToken`.

out.send_notification(
    "$/progress",
    json!({
        "token": work_done_token,
        "value": {
            "kind": "begin",
            "title": "AI: Explain this error",
            "cancellable": false,
            "message": "",
        }
    }),
)?;
```
 
### Status Notifications (Nova-specific)
 
In addition to standard LSP notifications, `nova-lsp` emits two Nova-specific
notifications, advertised in `capabilities.experimental.nova.notifications`:

- `nova/memoryStatusChanged` (see `nova/memoryStatus`)
- `nova/safeModeChanged` (see `nova/safeModeStatus`)
 
---

## Error Handling

Nova is designed to be resilient under editor workloads:

- The `nova-lsp` and `nova-dap` binaries wrap request handling in `catch_unwind` so a panic in one
  handler does not take down the entire process.
- Nova’s custom `nova/*` extension endpoints (e.g. build/test integration) run under a watchdog
  (`nova_scheduler::Watchdog`) with per-method deadlines. If a request panics or times out, Nova can
  temporarily enter **safe mode** to avoid repeatedly triggering the same failure.
- When safe mode is active, Nova keeps `nova/bugReport`, `nova/metrics`, and `nova/resetMetrics`
  available so clients can collect a diagnostic bundle and inspect request-level metrics.

For practical operational guidance (where logs go, how to generate bug report bundles, and how safe
mode behaves), see:

- [17 - Observability and Reliability](17-observability-and-reliability.md)

---

## Testing LSP

```rust
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn stdio_initialize_shutdown() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_jsonrpc_message(&mut stdout);

    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "id": 2, "method": "shutdown" }));
    let _shutdown_resp = read_jsonrpc_message(&mut stdout);

    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

fn write_jsonrpc_message(writer: &mut impl Write, message: &serde_json::Value) {
    let bytes = serde_json::to_vec(message).expect("serialize");
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len()).expect("write header");
    writer.write_all(&bytes).expect("write body");
    writer.flush().expect("flush");
}

fn read_jsonrpc_message(reader: &mut impl BufRead) -> serde_json::Value {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).expect("read header line");
        assert!(bytes_read > 0, "unexpected EOF while reading headers");

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }

    let len = content_length.expect("Content-Length header");
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).expect("read body");
    serde_json::from_slice(&buf).expect("parse json")
}
```

---

## Next Steps

1. → [Debugging Integration](12-debugging-integration.md): DAP implementation
2. → [AI Augmentation](13-ai-augmentation.md): ML-powered features

---

[← Previous: Performance Engineering](10-performance-engineering.md) | [Next: Debugging Integration →](12-debugging-integration.md)

# Architecture

The documents under `docs/` describe Nova's intended design and implementation approach.

**Architecture Decision Records (ADRs)** are the *binding* decisions that keep implementation coherent across parallel work. If an ADR conflicts with a design sketch elsewhere, **the ADR wins** and the sketch should be updated.

For ADR authoring conventions, see: [`docs/adr/README.md`](adr/README.md).

## Technology stack (at a glance)

- Incremental query engine: Salsa via `ra_ap_salsa` (`ra_salsa`) ([ADR 0001](adr/0001-incremental-query-engine.md))
- Syntax trees: `rowan` red/green trees + typed AST wrappers ([ADR 0002](adr/0002-syntax-tree-rowan.md))
- Protocol transport:
  - LSP: `lsp-server` IO framing + explicit message loop ([ADR 0003](adr/0003-protocol-frameworks-lsp-dap.md))
  - DAP: Nova-owned message loop + codec ([ADR 0003](adr/0003-protocol-frameworks-lsp-dap.md))
- Concurrency: snapshot reads + single-writer updates; Tokio orchestration + Rayon for CPU work ([ADR 0004](adr/0004-concurrency-model.md))
- Persistence:
  - `rkyv` + validation for mmap-friendly indexes
  - `serde`/`bincode` for small caches and metadata
  ([ADR 0005](adr/0005-persistence-formats.md))
- Canonical identifiers: structured VFS paths + normalized URIs (`file`, `jar`/`jmod`, `nova`) ([ADR 0006](adr/0006-uri-normalization.md))
- Type identity: stable `ClassId` + project-level type environments ([ADR 0011](adr/0011-stable-classid-and-project-type-environments.md), [ADR 0012](adr/0012-classid-interning.md))
- Extension system: provider-based extensions (native + sandboxed WASM) ([ADR 0010](adr/0010-extension-system.md))
- Router↔worker remote RPC: CBOR-framed v3 protocol with negotiation, multiplexing, and chunking ([ADR 0009](adr/0009-remote-rpc-protocol.md))

## ADR index

1. [0001 — Incremental query engine (Salsa)](adr/0001-incremental-query-engine.md)
2. [0002 — Syntax trees (Rowan)](adr/0002-syntax-tree-rowan.md)
3. [0003 — LSP/DAP frameworks and JSON-RPC transport](adr/0003-protocol-frameworks-lsp-dap.md)
4. [0004 — Concurrency model (snapshots + single writer)](adr/0004-concurrency-model.md)
5. [0005 — Persistence formats and compatibility policy](adr/0005-persistence-formats.md)
6. [0006 — Path/URI normalization and virtual document schemes](adr/0006-uri-normalization.md)
7. [0007 — Crate boundaries and dependency policy](adr/0007-crate-boundaries-and-dependencies.md)
 8. [0008 — Distributed mode security (router↔worker)](adr/0008-distributed-mode-security.md)
 9. [0009 — Router↔worker remote RPC protocol (v3)](adr/0009-remote-rpc-protocol.md)
 10. [0010 — Extension system (native + WASM providers)](adr/0010-extension-system.md)
11. [0011 — Stable `ClassId` and project-level type environments](adr/0011-stable-classid-and-project-type-environments.md)
12. [0012 — `ClassId` stability and interning policy](adr/0012-classid-interning.md)

## Where to look in code

The ADRs are normative; these pointers are only meant to make it easy to find the current implementations.

- **Crate-by-crate ownership map (current reality):** [`architecture-map.md`](architecture-map.md)
- **Custom LSP methods (`nova/*`) spec:** [`protocol-extensions.md`](protocol-extensions.md)
- **File watching (watcher layering + testing guidance):** [`file-watching.md`](file-watching.md)
- **Testing & CI (how to run/update suites locally):** [`14-testing-infrastructure.md`](14-testing-infrastructure.md)
- **ADR 0001 (Salsa / incremental engine)**:
  - `crates/nova-db/src/salsa/mod.rs` — `ra_ap_salsa` query groups, snapshots, cancellation checkpoints
- **ADR 0002 (Rowan syntax trees)**:
  - `crates/nova-syntax/` — parser/lexer + `rowan` integration (`syntax_kind.rs`, `parser.rs`, `ast.rs`)
- **ADR 0003 (LSP/DAP transport)**:
  - `crates/nova-lsp/src/main.rs` — shipped LSP stdio server (`lsp_server::Connection::stdio()` + `initialize_start`/`initialize_finish`) with Nova-owned dispatch/cancellation
  - `crates/nova-lsp/src/lib.rs` — Nova-specific `nova/*` method constants + (stateless) request dispatch helpers (`handle_custom_request[_cancelable]`)
  - `crates/nova-lsp/src/codec.rs` — Content-Length framing helpers (used by unit tests/harnesses; the shipped binary uses `lsp-server`)
  - `crates/nova-dap/src/dap/codec.rs` — DAP `Content-Length` framing
- **ADR 0004 (Concurrency model)**:
  - `crates/nova-scheduler/` — Tokio + Rayon orchestration patterns and cancellation primitives
- **ADR 0005 (Persistence)**:
  - `crates/nova-storage/` — validated `rkyv` archives + mmap support
  - `crates/nova-index/src/persistence.rs` — index load/save built on `nova-storage`
  - `crates/nova-cache/` — small derived caches (currently `serde`/`bincode`)
- **ADR 0006 (URIs / document identity)**:
  - `crates/nova-core/src/path.rs` — `file:` URI <-> path conversion and normalization
  - `crates/nova-vfs/src/path.rs` and `crates/nova-vfs/src/archive.rs` — VFS path model (local + jar/jmod)
- **ADR 0007 (crate boundaries)**:
  - `Cargo.toml` workspace members + `crates/` tree
- **ADR 0008 (distributed mode security)**:
  - `crates/nova-router/src/tls.rs` and `crates/nova-worker/src/tls.rs` — TLS helpers (feature-gated today)
  - `crates/nova-router/src/lib.rs` and `crates/nova-worker/src/main.rs` — router/worker connection setup + auth token plumbing (MVP)
- **ADR 0009 (remote RPC protocol)**:
  - `crates/nova-router/src/lib.rs` and `crates/nova-worker/src/main.rs` — router/worker transport (nova remote RPC v3)
  - `crates/nova-remote-proto/src/v3.rs` — v3 CBOR envelope + payload schema (`WireFrame`, `RpcPayload`)
  - `crates/nova-remote-proto/src/legacy_v2.rs` — deprecated lockstep protocol kept for compatibility/tests
  - `crates/nova-remote-rpc/` — v3 negotiated transport/runtime used by router/worker (handshake, framing, multiplexing, chunking, optional compression)
- **ADR 0010 (extension system)**:
  - `crates/nova-ext/` — extension traits, registry, WASM ABI scaffolding
  - `crates/nova-ide/src/extensions.rs` — IDE integration and aggregation

## Current repo status vs ADRs

This repository contains working code **and** forward-looking design docs. Some subsystems are still scaffolding and may not yet match the ADR decisions. The intent is:

- ADRs describe the **target architecture** contributors should implement toward.
- Temporary implementations may exist to enable end-to-end demos and tests; those should be migrated as the architecture solidifies.

Notable “delta” areas to be aware of:

- **Incremental engine coverage (ADR 0001):**
  - Salsa is implemented in `crates/nova-db/src/salsa/` (see `mod.rs`), but many “shipping” features still bypass it:
    - `crates/nova-lsp/src/main.rs` tracks open documents in `ServerState::analysis: AnalysisState`, which wraps a `nova_vfs::Vfs<LocalFs>` overlay (plus small `HashMap` caches for file text/paths).
    - Refactorings use a Salsa-backed semantic snapshot (`nova_refactor::RefactorJavaDatabase`) and can run against open-document overlays extracted from the VFS (see `ServerState::refactor_snapshot` in `crates/nova-lsp/src/main.rs` and `crates/nova-lsp/src/refactor_workspace.rs`).
    - CLI indexing/diagnostics in `crates/nova-workspace/` are largely heuristic/regex-based.
- **Syntax tree usage (ADR 0002):**
  - `crates/nova-syntax` provides both a token-level green tree (`parse`) and a rowan-based parser (`parse_java`).
  - Several subsystems still use non-rowan parsing approaches (e.g. `crates/nova-framework-mapstruct/` uses Tree-sitter; `crates/nova-framework-web/` and `crates/nova-workspace/` use regex/text scans).
- **LSP transport framework (ADR 0003):**
  - The shipped `nova-lsp` binary uses `lsp-server` for the stdio transport (I/O threads, `Content-Length` framing, JSON-RPC parsing) and the `initialize` handshake (`lsp_server::Connection::stdio()` / `initialize_start` / `initialize_finish` in `crates/nova-lsp/src/main.rs`).
  - Request/notification handling is still Nova-owned: a custom router for `$/cancelRequest` + a manual `match` dispatch on method strings lives in `crates/nova-lsp/src/main.rs` (rather than a higher-level framework).
- **Persistence formats (ADR 0005):**
  - `rkyv` + validation is implemented in `crates/nova-storage/` and used by some persisted artifacts (e.g. dependency bundles in `crates/nova-deps-cache/`).
  - Some persistence remains serde/bincode-based (e.g. parts of classpath caching in `crates/nova-classpath/`), and not all editor-facing schemas are versioned yet.
- **URIs + VFS model (ADR 0006):**
  - `crates/nova-vfs/` has archive path types + overlay support, and the current `nova-lsp` stdio server uses a `nova_vfs::Vfs<LocalFs>` overlay for open documents (`AnalysisState` in `crates/nova-lsp/src/main.rs`).
  - Some LSP-facing features still assume `file:` URIs and/or legacy schemes:
    - `crates/nova-lsp/src/refactor_workspace.rs` requires `file://` URIs for project-root discovery.
    - JDK go-to-definition emits canonical ADR0006 `nova:///decompiled/...` URIs (see `goto_definition_jdk` in `crates/nova-lsp/src/main.rs`). Decompiled virtual documents are stored in `nova-vfs`'s bounded virtual document store (not injected into the open-document overlay), but legacy `nova-decompile:` handling still exists for compatibility (`crates/nova-lsp/src/decompile.rs` and `crates/nova-vfs/src/path.rs`).
- **Distributed mode (docs/16-distributed-mode.md):**
  - The router/worker stack (`crates/nova-router/`, `crates/nova-worker/`, `crates/nova-remote-proto/`) is now integrated into the shipped `nova-lsp` stdio server behind CLI flags:
    - `--distributed` enables local multi-process indexing/search.
    - `--distributed-worker-command <path>` overrides the `nova-worker` binary (see `parse_distributed_cli` in `crates/nova-lsp/src/main.rs`).
      - Default resolution prefers a sibling `nova-worker` next to the `nova-lsp` executable, otherwise falls back to `nova-worker` on `PATH` (`default_distributed_worker_command` in `crates/nova-lsp/src/main.rs`).
  - When enabled, `nova-lsp` starts a local IPC router after the `initialize` handshake (`ServerState::start_distributed_after_initialize` in `crates/nova-lsp/src/main.rs`), and the router spawns local `nova-worker` processes (one per shard/source-root).
  - Current scope is intentionally narrow/experimental:
    - `workspace/symbol` is served via the distributed router (`handle_workspace_symbol` in `crates/nova-lsp/src/main.rs`).
    - The frontend forwards best-effort file text updates to the router from `textDocument/didOpen`, `textDocument/didChange`, and `workspace/didChangeWatchedFiles` notifications (`handle_notification` in `crates/nova-lsp/src/main.rs`).
    - Most other LSP features still run in-process (the router/worker layer is not yet a general “semantic query RPC”).
    - See `crates/nova-lsp/tests/stdio_distributed_workspace_symbol.rs` for an end-to-end stdio integration test covering the distributed `workspace/symbol` flow.
- **Protocol extensions:**
  - Custom `nova/*` methods exist (implemented across `crates/nova-lsp/src/extensions/`, `crates/nova-lsp/src/hardening.rs`, and the `nova-lsp` binary) and are advertised via `initializeResult.capabilities.experimental.nova.{requests,notifications}` (see `initialize_result_json()` in `crates/nova-lsp/src/main.rs`).
  - Clients should still be defensive for older servers (or non-Nova servers) that don’t advertise these capabilities: use “optimistic call + method-not-found fallback” gating (see [`protocol-extensions.md`](protocol-extensions.md)).
  - Safe-mode is a shipped resilience feature for Nova-specific endpoints: timeouts/panics in watchdog-wrapped `nova/*` handlers can temporarily enable safe-mode, after which most `nova/*` requests return an error via `nova_lsp::hardening::guard_method` (`crates/nova-lsp/src/hardening.rs`). The binary surfaces state changes via `nova/safeModeChanged` (see `flush_safe_mode_notifications` in `crates/nova-lsp/src/main.rs`) and supports polling via `nova/safeModeStatus`.

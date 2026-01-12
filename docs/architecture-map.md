# Architecture map (docs ↔ code)

This document maps Nova’s **intended** architecture (see [`AGENTS.md`](../AGENTS.md) + the design docs/ADRs under
[`docs/`](./)) to the **current** Rust workspace in [`crates/`](../crates/).

It is meant to answer, concretely:

- “Where do I implement X?”
- “Which crate owns this responsibility today?”
- “Is this subsystem scaffolding, a prototype, or productionizing?”

For the stable spec of Nova’s custom `nova/*` LSP methods, see
[`protocol-extensions.md`](protocol-extensions.md).

For practical guidance on running tests locally, updating fixtures/snapshots, and understanding CI
gates, see [`14-testing-infrastructure.md`](14-testing-infrastructure.md).

## Maturity levels

- **scaffolding**: early skeletons; APIs and boundaries are still in flux and may not be wired into binaries.
- **prototype**: works end-to-end for at least one scenario, but is not yet aligned with all ADR targets.
- **productionizing**: used by `nova-lsp`/`nova`/`nova-dap` and has tests/fixtures; still evolving, but “real”.

## If you’re looking for…

- **LSP server + custom `nova/*` methods**: `crates/nova-lsp/` (`src/main.rs`, `src/lib.rs`, `src/extensions/*`)
- **DAP server / debugger UX**: `crates/nova-dap/` (+ `crates/nova-jdwp/`)
- **Project discovery (Maven/Gradle/Bazel)**: `crates/nova-project/` (+ `crates/nova-build/`, `crates/nova-build-bazel/`)
- **AI (completion ranking, semantic search, anonymization)**: `crates/nova-ai/` (wired into `nova-ide`/`nova-lsp`)
- **AI code edits / patch-based codegen**: `crates/nova-ai-codegen/` (structured patch parsing + safety + apply/format/validate)
- **Parsing / syntax trees**: `crates/nova-syntax/`
- **Incremental database (Salsa)**: `crates/nova-db/src/salsa/` (see `mod.rs`)
- **Indexing + persistence**: `crates/nova-index/`, `crates/nova-cache/`, `crates/nova-storage/`
- **Refactorings**: `crates/nova-refactor/` (editor-facing wiring currently lives in `crates/nova-lsp/`)
- **Framework support**: `crates/nova-framework-*` (Spring/Micronaut/JPA/Quarkus/MapStruct/etc)
- **Distributed mode**: `crates/nova-router/`, `crates/nova-worker/`, `crates/nova-remote-proto/`
- **Codegen / developer tasks**: `crates/xtask/` (`cargo run --locked -p xtask -- codegen`)
- **Repo invariants / layering / docs ↔ code checks**: `crates/nova-devtools/`, `scripts/check-repo-invariants.sh`, `crate-layers.toml`
- **File watching / watcher architecture**: `docs/file-watching.md` (see also `crates/nova-vfs/src/watch.rs`, `crates/nova-workspace/src/engine.rs`, `crates/nova-workspace/src/watch.rs`, `crates/nova-workspace/src/watch_roots.rs`)

---

## Crate-by-crate map (alphabetical)

### `nova-ai`
- **Purpose:** model-agnostic AI helpers (privacy/anonymization, context building, completion ranking, semantic search, optional cloud LLM client).
- **Key entry points:** `crates/nova-ai/src/lib.rs` (`AiService`, `CloudLlmClient`, `CloudLlmConfig`, `ContextBuilder`, `SemanticSearch`, `TrigramSemanticSearch`, `PrivacyMode`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - AI features are opt-in and mostly wired through the `nova-lsp` binary, not the incremental query engine.
  - Embeddings-backed semantic search is feature-gated (`embeddings`) and still experimental.

### `nova-ai-codegen`
- **Purpose:** AI “code edit/codegen” pipeline: parse a structured patch response, enforce safety policies, apply edits to a virtual workspace, format touched files, validate (syntax/type) diagnostics, and optionally attempt repair before returning a patch.
- **Key entry points:** `crates/nova-ai-codegen/src/lib.rs` (`generate_patch`, `CodeGenerationConfig`, `CodeGenerationResult`, `ValidationConfig`, `PatchSafetyConfig`, `PromptCompletionProvider`, `CodegenProgressReporter`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Primarily used by the `nova-lsp` AI endpoints today; not yet integrated into the Salsa DB / persistent workspace model.
  - Validation is best-effort (syntax/type diagnostics via `nova-ide`), not a full compile/test execution pipeline.
  - Safety rules are intentionally conservative (e.g. disallowing certain edits/imports) and may need tuning per editor/workflow.

### `nova-apt`
- **Purpose:** annotation-processing support (discovering generated source roots; triggering APT builds).
- **Key entry points:** `crates/nova-apt/src/lib.rs` (`AptManager`, `discover_generated_source_roots`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - No incremental/AP-aware compiler integration yet; currently shells out via `nova-build`.

### `nova-archive`
- **Purpose:** best-effort reading of JARs / exploded directories (used by metadata ingestion).
- **Key entry points:** `crates/nova-archive/src/lib.rs` (`Archive::read`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Not yet integrated with the VFS archive path model from ADR 0006 (`nova-vfs::ArchivePath`).

### `nova-bugreport`
- **Purpose:** crash recording + on-demand bug report bundle creation (logs/config/crashes/perf + system/env metadata, optional zip packaging).
- **Key entry points:** `crates/nova-bugreport/src/lib.rs` (`install_panic_hook`, `BugReportBuilder`, `create_bug_report_bundle`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Bundle schema is stable but currently Nova-specific; no cross-tool standardization.

### `nova-build`
- **Purpose:** Maven/Gradle build integration for classpaths + build diagnostics + background build orchestration.
- **Key entry points:** `crates/nova-build/src/lib.rs` (`BuildManager`, `BuildResult`, `Classpath`, `BuildOrchestrator`, `BuildRequest`, `BuildStatusSnapshot`, `BuildDiagnosticsSnapshot`).
- **LSP endpoints:** `crates/nova-lsp/src/extensions/build.rs` (`nova/buildProject`, `nova/java/classpath`, `nova/reloadProject`, `nova/build/targetClasspath`, `nova/build/status`, `nova/build/diagnostics`).
- **Docs:** [`gradle-build-integration.md`](gradle-build-integration.md) (Gradle snapshot handoff to `nova-project`)
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Background build state/diagnostics are surfaced via custom `nova/*` endpoints and are not yet wired into Nova’s main workspace/Salsa diagnostics pipeline or standard LSP progress notifications.
  - Build tool invocation is still opt-in (explicit `nova/*` requests) rather than a continuously running, editor-driven build/compile service.

### `nova-build-bazel`
- **Purpose:** Bazel integration (workspace discovery + `query`/`aquery` extraction + caching, optional BSP-backed build/diagnostics orchestration when enabled).
- **Key entry points:** `crates/nova-build-bazel/src/lib.rs` (`BazelWorkspace`, `JavaCompileInfo`), `crates/nova-build-bazel/src/orchestrator.rs` (feature `bsp`: `BazelBuildOrchestrator`, `BazelBspConfig`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - BSP support exists behind the `bsp` Cargo feature and is optional at runtime (configured via
    standard `.bsp/*.json` discovery or `NOVA_BSP_PROGRAM` / `NOVA_BSP_ARGS`); the default metadata
    path is still `query`/`aquery`.

### `nova-build-model`
- **Purpose:** build-system-agnostic “project model” types shared across build integrations (`nova-project`, `nova-build`, Bazel tooling), plus the object-safe build-system backend trait used for detection/loading.
- **Key entry points:** `crates/nova-build-model/src/lib.rs` (workspace model types, `ProjectConfig`, `Classpath`, `BuildSystemBackend`, `BuildSystemError`), `crates/nova-build-model/src/model.rs` (core model definitions, module/source root metadata), `crates/nova-build-model/src/package.rs` (`validate_package_name`, `infer_source_root`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - The model is still evolving and some consumers still use the older aggregated `ProjectConfig` surface rather than the normalized workspace model / per-module metadata.
  - Not yet integrated as the authoritative source-of-truth for Nova’s long-lived workspace/Salsa DB layer.

### `nova-cache`
- **Purpose:** per-project persistent cache directory management + cache packaging (`tar.zst`).
- **Key entry points:** `crates/nova-cache/src/lib.rs` (`CacheDir`, `AstArtifactCache`, `pack_cache_package`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Not all persisted artifacts are versioned/typed via `nova-storage` yet (some remain serde/bincode).

### `nova-classfile`
- **Purpose:** Java `.class` file parsing (constant pool, descriptors, signatures, stubs).
- **Key entry points:** `crates/nova-classfile/src/lib.rs` (`ClassFile::parse`, `ClassStub`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - None known (standalone parser; consumers still decide how to integrate into the semantic DB).

### `nova-classpath`
- **Purpose:** indexing of classpath entries (dirs/JARs/JMODs) into stub models + search indexes.
- **Key entry points:** `crates/nova-classpath/src/lib.rs` (`ClasspathIndex`, `ClasspathEntry`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Persistence uses a mix of custom formats and `bincode`; not fully aligned to ADR 0005 yet.

### `nova-cli`
- **Purpose:** `nova` CLI binary for CI smoke tests, indexing, diagnostics, cache management, perf tools.
- **Key entry points:** `crates/nova-cli/src/main.rs` (subcommands wired to `nova-workspace`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - CLI indexing is currently heuristic/regex-based (see `nova-workspace`), not Salsa-backed.

### `nova-config`
- **Purpose:** Nova config model + tracing/log buffering (for bug reports and structured logging).
- **Key entry points:** `crates/nova-config/src/lib.rs` (`NovaConfig`, `init_tracing_with_config`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Config loading is available, but many binaries currently rely on defaults/env vars.

### `nova-config-metadata`
- **Purpose:** Spring Boot configuration metadata ingestion (`spring-configuration-metadata.json`).
- **Key entry points:** `crates/nova-config-metadata/src/lib.rs` (`MetadataIndex`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Not yet backed by incremental/Salsa queries; currently consumed by `nova-ide` framework support
    (Spring config diagnostics/completions) via workspace-scoped caches.

### `nova-core`
- **Purpose:** dependency-minimized core types (names, ranges, edits, IDs, URI/path helpers).
- **Key entry points:** `crates/nova-core/src/lib.rs`, `crates/nova-core/src/path.rs`.
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - ADR 0006’s canonical URI model exists, but the `nova-lsp` stdio server mostly uses raw URI strings.

### `nova-dap`
- **Purpose:** Debug Adapter Protocol implementation + “debugger excellence” features (hot swap, stepping).
- **Key entry points:** `crates/nova-dap/src/lib.rs` (`server`, `hot_swap`, `wire_debugger`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - DAP transport is implemented, but editor integrations are still minimal and evolving.

### `nova-db`
- **Purpose:** database layer; contains both a small in-memory store and a Salsa-based query DB.
- **Key entry points:** `crates/nova-db/src/lib.rs` (`InMemoryFileStore`, `AnalysisDatabase`),
  `crates/nova-db/src/salsa/mod.rs` (`RootDatabase`, `Database` wrapper, snapshots).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Salsa exists, but many “shipped” paths still use ad-hoc in-memory DBs. Refactorings now use a Salsa-backed snapshot (`nova_refactor::RefactorJavaDatabase`), but other subsystems still bypass the query DB.

### `nova-decompile`
- **Purpose:** decompile `.class` to Java-like stub source as a navigation fallback.
- **Key entry points:** `crates/nova-decompile/src/lib.rs` (`decompile_classfile_cached`, `DECOMPILE_URI_SCHEME`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Decompile output isn’t yet integrated into a full virtual-document URI scheme for editors.

### `nova-deps-cache`
- **Purpose:** global dependency (JAR/JMOD) index cache stored as validated `rkyv` archives.
- **Key entry points:** `crates/nova-deps-cache/src/lib.rs` (`DependencyIndexStore`, `DependencyIndexBundle`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Cache format is stable, but consumers still mix different indexing/persistence strategies.

### `nova-devtools`
- **Purpose:** developer tooling for Nova’s Rust workspace (repo hygiene + architecture invariants: ADR 0007 layering, docs ↔ code checks, dependency graphing).
- **Key entry points:**
  - `crates/nova-devtools/src/check_deps.rs` (`check_deps::check`) — validates `cargo metadata` edges against `crate-layers.toml`.
  - `crates/nova-devtools/src/check_layers.rs` (`check_layers::check`) — validates `crate-layers.toml` coverage/consistency against workspace members.
  - `crates/nova-devtools/src/check_arch_map.rs` (`check_arch_map::check`) — validates `docs/architecture-map.md` coverage + quick-link path freshness.
  - `crates/nova-devtools/src/check_protocol_extensions.rs` (`check_protocol_extensions::check`) — validates `docs/protocol-extensions.md` against `nova-lsp` constants + VS Code client usage.
  - `crates/nova-devtools/src/graph.rs` (`graph::generate`) — emits a DOT/GraphViz dependency graph annotated by layer.
  - `crates/nova-devtools/src/main.rs` — CLI entrypoint (including `nova-devtools check-repo-invariants` meta command used by CI).
  - `crate-layers.toml` — policy + layer mapping configuration.
  - `scripts/check-deps.sh` — convenience wrapper for `check-deps`.
  - `scripts/check-repo-invariants.sh` — convenience wrapper to run the full suite (CI-equivalent).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Some “repo hygiene” checks still live outside `nova-devtools` (e.g. version sync scripts, actionlint); over time we should consolidate invariants here where it makes sense.

### `nova-ext`
- **Purpose:** extension/plugin abstractions (code actions, diagnostics, completions) + registry.
- **Key entry points:** `crates/nova-ext/src/lib.rs` (`ExtensionRegistry`, provider traits).
- **Docs:** [`docs/extensions/README.md`](extensions/README.md)
- **Maturity:** scaffolding
- **Known gaps vs intended docs:**
  - Not currently the primary framework/plugin system used by `nova-lsp` or `nova-workspace`.

### `nova-ext-abi`
- **Purpose:** standalone, wasm-friendly Nova extension ABI types (serde JSON wire structs) for guest plugin authors.
- **Key entry points:** `crates/nova-ext-abi/src/lib.rs` (`ABI_V1`), `crates/nova-ext-abi/src/v1/mod.rs` (ABI v1 structs + guest helpers).
- **Maturity:** scaffolding
- **Known gaps vs intended docs:**
  - ABI surface is still small and versioned only for v1; future v2 will require explicit evolution + compatibility policy.

### `nova-ext-wasm-example-todos`
- **Purpose:** minimal example Nova WASM extension that implements the diagnostics capability by flagging `TODO` occurrences.
- **Key entry points:** `examples/nova-ext-wasm-example-todos/src/lib.rs`, `examples/nova-ext-wasm-example-todos/bundle/nova-ext.toml`.
- **Maturity:** scaffolding
- **Known gaps vs intended docs:**
  - Example-only; not wired into `nova-lsp` by default (intended as a reference for external plugin authors).
  - Error reporting is intentionally minimal (best-effort JSON parsing + empty result on failure).

### `nova-flow`
- **Purpose:** flow analysis (CFG, definite assignment, null tracking) for Java method bodies.
- **Key entry points:** `crates/nova-flow/src/lib.rs` (`analyze`, `ControlFlowGraph`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Wired into the diagnostics pipeline (`nova-db` Salsa queries → `nova-ide` → `nova-lsp`) for reachability, definite assignment, and basic nullability diagnostics. Still best-effort and incomplete vs full Java semantics (exceptions, full HIR coverage).

### `nova-format`
- **Purpose:** best-effort Java formatter and formatting edit helpers.
- **Key entry points:** `crates/nova-format/src/lib.rs` (`edits_for_formatting`, `FormatPipeline`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Token-stream based formatter (not a full AST formatter yet).

### `nova-framework`
- **Purpose:** framework analyzer abstraction + registry (virtual members, diagnostics, completions hooks).
- **Key entry points:** `crates/nova-framework/src/lib.rs` (`FrameworkAnalyzer`, `AnalyzerRegistry`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Integration into the core semantic DB is still partial; analyzers mostly run as standalone passes.
  - IDE integration is currently split:
    - Most shipped framework diagnostics/completions live in `crates/nova-ide/src/framework_cache.rs`.
    - Registry-backed analyzers can run via a best-effort adapter (`crates/nova-ide/src/framework_db.rs`)
      and `FrameworkAnalyzerRegistryProvider` (`crates/nova-ide/src/extensions.rs`).
      `nova-ide`'s generic `IdeExtensions::<DB>::with_default_registry` builds the analyzer list via
      `nova-framework-builtins` and registers the provider for diagnostics/completions/navigation/inlay
      hints. The default provider is configured with `with_build_metadata_only()` so it only runs on
      projects with authoritative build metadata (Maven/Gradle/Bazel), avoiding duplicate results
      alongside the legacy `framework_cache` providers. (`FrameworkAnalyzerRegistryProvider::empty()`
      exists as a fast no-op option.)

### `nova-framework-builtins`
- **Purpose:** centralized construction/registration of Nova’s built-in `nova-framework-*` analyzers so downstream crates (IDE, LSP, etc.) don’t need to maintain their own lists.
- **Key entry points:** `crates/nova-framework-builtins/src/lib.rs` (`builtin_analyzers`, `register_builtin_analyzers`, `builtin_registry`).
- **Maturity:** scaffolding
- **Known gaps vs intended docs:**
  - Registers analyzers for Lombok/Dagger/MapStruct/Micronaut/Quarkus by default.
  - Spring/JPA analyzers are feature-gated (`spring`/`jpa`) to avoid pulling heavier dependencies unless needed.
  - Used by `nova-ide`'s generic `IdeExtensions::with_default_registry` to build the default
    `AnalyzerRegistry` (see `crates/nova-ide/src/extensions.rs`), but some call sites still build their
    own registries.

### `nova-framework-dagger`
- **Purpose:** best-effort Dagger DI graph extraction + diagnostics/navigation (text-based).
- **Key entry points:** `crates/nova-framework-dagger/src/lib.rs` (`DaggerAnalyzer`, `analyze_java_files`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Uses text scanning, not HIR/typed resolution. Diagnostics are surfaced via `nova-ide`
    (`crates/nova-ide/src/dagger_intel.rs`) and therefore show up in `nova-lsp`.

### `nova-framework-jpa`
- **Purpose:** JPA/Jakarta EE analysis + JPQL parsing/completions/diagnostics.
- **Key entry points:** `crates/nova-framework-jpa/src/lib.rs` (`JpaAnalyzer`, `analyze_java_sources`, `jpql_*`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Best-effort and mostly text-based (JPQL strings + simple entity model). Integrated into the IDE
    via `crates/nova-ide/src/jpa_intel.rs` (diagnostics + JPQL completions).

### `nova-framework-lombok`
- **Purpose:** Lombok “virtual member” synthesis for common annotations (getters/setters/builders/etc).
- **Key entry points:** `crates/nova-framework-lombok/src/lib.rs` (`LombokAnalyzer`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Virtual members are not yet integrated into a full type-checked HIR pipeline.

### `nova-framework-mapstruct`
- **Purpose:** MapStruct mapper discovery + navigation into generated sources (best-effort).
- **Key entry points:** `crates/nova-framework-mapstruct/src/lib.rs` (`analyze_workspace`, `goto_definition`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Uses Tree-sitter and filesystem probing rather than Nova’s parser/DB.

### `nova-framework-micronaut`
- **Purpose:** Micronaut DI/endpoints/config analysis (best-effort).
- **Key entry points:** `crates/nova-framework-micronaut/src/lib.rs` (`analyze_sources_with_config`, `Bean`, `Endpoint`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Exposed both via custom LSP endpoints (`nova/micronaut/*`) and via `nova-ide` framework support
    for diagnostics/completions; still best-effort and not semantically resolved.

### `nova-framework-parse`
- **Purpose:** shared Tree-sitter based parsing helpers for framework analyzers (node traversal, annotation parsing).
- **Key entry points:** `crates/nova-framework-parse/src/lib.rs` (`parse_java`, `visit_nodes`, `ParsedAnnotation`, `collect_annotations`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Duplicates parsing responsibilities with `nova-syntax`; framework analyzers are not yet unified on rowan-based AST types.

### `nova-framework-quarkus`
- **Purpose:** Quarkus CDI + endpoints + config helpers (best-effort).
- **Key entry points:** `crates/nova-framework-quarkus/src/lib.rs` (`analyze_java_sources`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Endpoint discovery delegates to `nova-framework-web`’s regex-based endpoint extractor (best-effort; not semantically resolved).

### `nova-framework-spring`
- **Purpose:** Spring “baseline IntelliJ” features (beans/DI diagnostics, config completions, navigation).
- **Key entry points:** `crates/nova-framework-spring/src/lib.rs` (`SpringAnalyzer`, `SpringWorkspaceIndex`, `diagnostics_for_config_file`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Framework model is still heuristic and not backed by incremental semantic queries.

### `nova-framework-web`
- **Purpose:** HTTP endpoint discovery across multiple Java web frameworks (JAX-RS, Spring MVC, Micronaut), used by `nova/web/endpoints`.
- **Key entry points:** `crates/nova-framework-web/src/lib.rs` (`extract_http_endpoints_in_dir`, `extract_http_endpoints_from_source`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Regex-based extraction; not tied to semantic resolution or framework models yet.

### `nova-fuzzy`
- **Purpose:** fuzzy matching primitives (trigram filtering + subsequence scoring).
- **Key entry points:** `crates/nova-fuzzy/src/lib.rs` (`TrigramIndex`, `fuzzy_match`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - None known; shared utility.

### `nova-hir`
- **Purpose:** HIR (high-level intermediate representation) scaffolding + per-file `ItemTree`.
- **Key entry points:** `crates/nova-hir/src/lib.rs` (`CompilationUnit`, `item_tree` module).
- **Maturity:** scaffolding
- **Known gaps vs intended docs:**
  - Not a full Java HIR yet; current HIR is intentionally minimal and mostly used for tests.

### `nova-ide`
- **Purpose:** IDE-facing semantic helpers (completions, hover, navigation, debug config discovery).
- **Key entry points:** `crates/nova-ide/src/lib.rs` (`completions`, `hover`, `Project`, `DebugConfiguration`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Many features run on in-memory snapshots rather than the Salsa DB described in `docs/04-*`.

### `nova-ids`
- **Purpose:** canonical, dependency-free strongly-typed IDs (`FileId`, `ProjectId`, `SymbolId`, etc).
- **Key entry points:** `crates/nova-ids/src/lib.rs`, re-exported from `crates/nova-core/src/id.rs`.
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Several crates still define their own ID newtypes (`nova-types`, `nova-db`); adoption is in progress.

### `nova-index`
- **Purpose:** in-memory indexes (symbols, annotations, classpath-derived indexes) + persistence helpers.
- **Key entry points:** `crates/nova-index/src/lib.rs` (`ProjectIndexes`, `SymbolSearchIndex`, `load_indexes`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Indexing is currently heuristic (regex-based) in `nova-workspace`, not a full semantic index.

### `nova-jdk`
- **Purpose:** JDK discovery + JMOD ingestion + standard-library symbol stubs.
- **Key entry points:** `crates/nova-jdk/src/lib.rs` (`JdkIndex`, `JdkInstallation`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Used mostly as a “good enough” index for resolution; not yet integrated with module graphs everywhere.

### `nova-jdwp`
- **Purpose:** Java Debug Wire Protocol client façade (used by `nova-dap` and hot swap).
- **Key entry points:** `crates/nova-jdwp/src/lib.rs` (`TcpJdwpClient`, `JdwpClient` trait).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Implements only a subset of JDWP; many inspection APIs return `NotImplemented`.

### `nova-lsp`
- **Purpose:** LSP integration crate + `nova-lsp` binary.
- **Key entry points:** `crates/nova-lsp/src/main.rs` (stdio LSP server using `lsp-server` framing + a Nova-owned dispatch loop),
  `crates/nova-lsp/src/lib.rs` (custom method constants + dispatch helpers),
  `crates/nova-lsp/src/extensions/*` (custom `nova/*` endpoints).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - ADR 0003 mentions an optional TCP transport; `nova-lsp` currently only supports stdio (`lsp_server::Connection::stdio()`) and uses `lsp-server` for framing + the initialize handshake (`initialize_start/finish`) in `crates/nova-lsp/src/main.rs`.
  - `nova-lsp` has an **experimental** local distributed mode behind `--distributed` / `--distributed-worker-command` (see `crates/nova-lsp/src/main.rs`). Today this primarily powers `workspace/symbol` via `nova-router` + `nova-worker`, and is not enabled by default.
  - Request/notification dispatch is still a Nova-owned manual router (`match` over `method` strings), not `tower-lsp`; `$/cancelRequest` is handled explicitly via `message_router` + `nova_lsp::RequestCancellation`.
  - Custom `nova/*` method support is advertised via `initializeResult.capabilities.experimental.nova.{requests,notifications}` (clients should still handle older servers that omit this).
  - Request cancellation is routed (`$/cancelRequest` → request-scoped `CancellationToken`), but many handlers still only check cancellation at coarse boundaries; long-running work may not stop promptly.
  - The server loop is intentionally simple/mostly synchronous today (requests are handled serially), and does not yet have a general async scheduling model for isolating expensive work.
  - `crates/nova-lsp/src/codec.rs` is now test-only helper code for reading/writing `Content-Length` framed JSON-RPC messages, not the production transport.

### `nova-memory`
- **Purpose:** memory budgeting + accounting + cooperative eviction (used by `nova-lsp` telemetry endpoints).
- **Key entry points:** `crates/nova-memory/src/lib.rs` (`MemoryManager`, `MemoryReport`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Only some components report usage today; accounting is opt-in and approximate by design.

### `nova-metrics`
- **Purpose:** lightweight runtime metrics (counters + latency histograms) for Nova servers.
- **Key entry points:** `crates/nova-metrics/src/lib.rs` (`MetricsRegistry`, `MetricsSnapshot`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - In-memory only; currently exported via the custom LSP endpoints `nova/metrics` + `nova/resetMetrics` (no Prometheus/OpenTelemetry integration yet).

### `nova-modules`
- **Purpose:** JPMS (Java Platform Module System) model (`ModuleGraph`, readability checks).
- **Key entry points:** `crates/nova-modules/src/lib.rs` (`ModuleGraph`, `ModuleInfo`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Not yet used as the authoritative module model by `nova-project` / resolution.

### `nova-perf`
- **Purpose:** benchmark/performance report utilities (criterion parsing + regression comparison).
- **Key entry points:** `crates/nova-perf/src/lib.rs` (`load_criterion_directory`, `compare_runs`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Focused on CI regressions; not yet integrated with runtime telemetry aggregation.

### `nova-process`
- **Purpose:** safe helpers for spawning external commands with bounded output capture (avoids OOM from chatty build tools) and optional timeouts.
- **Key entry points:** `crates/nova-process/src/lib.rs` (`run_command`, `run_command_checked`, `RunOptions`, `CommandFailure`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - No async API yet (uses threads to drain stdout/stderr); callers that need cancellation must layer it externally.

### `nova-project`
- **Purpose:** workspace discovery and project configuration (source roots, classpath, Java levels), used by `nova-lsp` project metadata endpoints including `nova/projectConfiguration` and the normalized `nova/projectModel`.
- **Key entry points:** `crates/nova-project/src/discover.rs` (`load_project`, `load_workspace_model`, `LoadOptions`), `crates/nova-project/src/build_systems.rs` (`default_build_systems`, build backend implementations), plus the shared model types in `crates/nova-build-model/` (re-exported by `nova-project`).
- **Docs:** [`gradle-build-integration.md`](gradle-build-integration.md) (Gradle discovery + `nova-build` snapshot reader)
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Module graph construction is still limited; JPMS support is partial and evolving.

### `nova-properties`
- **Purpose:** range-preserving `.properties` parser (for framework config support).
- **Key entry points:** `crates/nova-properties/src/lib.rs` (`parse`, `PropertiesFile`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Intentionally partial parser (not full `.properties` spec).

### `nova-refactor`
- **Purpose:** refactoring engine (safe delete, move, change signature, extract, inline, record conversion, etc).
- **Key entry points:** `crates/nova-refactor/src/lib.rs` (`rename`, `organize_imports`, `WorkspaceEdit`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Refactorings run on the canonical syntax/HIR pipeline and a Salsa-backed semantic snapshot (`RefactorJavaDatabase`). Multi-file refactorings in `nova-lsp` apply open-document overlays, but integration with a long-lived workspace DB is still evolving.

### `nova-remote-proto`
- **Purpose:** on-the-wire message types for distributed mode (router ↔ worker RPC).
- **Key entry points:**
  - `crates/nova-remote-proto/src/v3.rs` — v3 CBOR envelope + payload schema (`WireFrame`, `RpcPayload`, `Request`, `Response`, `Notification`)
  - `crates/nova-remote-proto/src/legacy_v2.rs` — deprecated lockstep protocol kept for compatibility/tests
  - `crates/nova-remote-proto/src/lib.rs` — shared hard limits + helpers
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Distributed mode is still experimental, but is now editor-facing via `nova-lsp --distributed` (local IPC router + spawned `nova-worker`); today the editor-facing surface area is intentionally narrow (primarily `workspace/symbol`).
  - v3 is the current router↔worker wire protocol; schema evolution is expected within minor versions.

### `nova-remote-rpc`
- **Purpose:** async negotiated RPC transport for distributed mode (v3 handshake, request/response framing, optional zstd compression).
- **Key entry points:** `crates/nova-remote-rpc/src/lib.rs` (`RpcConnection`, `RpcRole`, `RouterConfig`, `WorkerConfig`, `RpcError`, `RpcTransportError`, `RequestId`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Distributed mode is still experimental; `nova-lsp` uses it behind `--distributed` (local IPC router + spawned `nova-worker`) primarily for `workspace/symbol` + best-effort file update propagation.
  - No application-level keepalive/heartbeat yet; idle connections rely on the transport/deployment.

### `nova-resolve`
- **Purpose:** name resolution + scope building (currently based on simplified `nova-hir`).
- **Key entry points:** `crates/nova-resolve/src/lib.rs` (`Resolver`, `build_scopes`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Resolution operates on simplified HIR, not the rowan AST + full semantic model described in `docs/06-*`.

### `nova-router`
- **Purpose:** distributed “query router” prototype (sharding + worker coordination + symbol aggregation).
- **Key entry points:** `crates/nova-router/src/lib.rs` (`QueryRouter`, `DistributedRouterConfig`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Distributed mode is integrated into the shipped `nova-lsp` stdio server behind CLI flags (`--distributed`, `--distributed-worker-command`), but the editor-facing surface area is still intentionally narrow/experimental (see `docs/16-distributed-mode.md`).
  - No general per-query fanout yet; the router mostly serves queries from an aggregated symbol index instead of routing arbitrary semantic queries to multiple shards/workers on demand.

### `nova-scheduler`
- **Purpose:** concurrency helpers (scheduler pools, cancellation tokens, watchdog timeouts, debouncers).
- **Key entry points:** `crates/nova-scheduler/src/lib.rs` (`Scheduler`, `CancellationToken`, `Watchdog`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Cancellation is cooperative; many current handlers don’t yet poll tokens.

### `nova-storage`
- **Purpose:** validated, mmap-friendly `rkyv` storage backend (schema+version headers, compression).
- **Key entry points:** `crates/nova-storage/src/lib.rs` (`PersistedArchive`, `write_archive_atomic`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - None known; this is the primary “big artifact” persistence layer per ADR 0005.

### `nova-stream-debug`
- **Purpose:** stream pipeline analysis + debugger-adjacent helpers for Java streams.
- **Key entry points:** `crates/nova-stream-debug/src/lib.rs` (`analyze_stream_expression`, `StreamChain`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Not yet surfaced as a stable editor feature; primarily used by DAP experiments.

### `nova-syntax`
- **Purpose:** parsing and syntax trees (token-level green tree + rowan-based `parse_java`).
- **Key entry points:** `crates/nova-syntax/src/lib.rs` (`parse`, `parse_java`, `SyntaxNode`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - `parse()` is still token-level; the full Java grammar/typed AST wrappers are still under construction.

### `nova-test-utils`
- **Purpose:** shared test helpers (fixture loading, marker extraction, javac differential harness).
- **Key entry points:** `crates/nova-test-utils/src/lib.rs`.
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Test-only crate; not part of shipped architecture.

### `nova-testing`
- **Purpose:** Java test discovery + execution with a **versioned** JSON schema for editor integrations.
- **Key entry points:** `crates/nova-testing/src/lib.rs` (`discover_tests`, `run_tests`),
  `crates/nova-testing/src/schema.rs` (request/response types).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Limited to JUnit 4/5 today; richer framework support is planned.

### `nova-types`
- **Purpose:** shared diagnostic/completion types + best-effort Java type system primitives (`Type`).
- **Key entry points:** `crates/nova-types/src/lib.rs` (`Type`, `Diagnostic`, `CompletionItem`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Not a full JLS implementation; designed for IDE-grade approximations.

### `nova-types-bridge`
- **Purpose:** bridges external stubs (e.g. parsed classfiles) into `nova-types`’ type system (`TypeStore`).
- **Key entry points:** `crates/nova-types-bridge/src/lib.rs` (`ExternalTypeLoader`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Still a best-effort adapter; higher semantic layers still need a project-level/shared `TypeStore` (or equivalent) for stable identities across Salsa queries (see [ADR 0011](adr/0011-stable-classid-and-project-type-environments.md) and [ADR 0012](adr/0012-classid-interning.md)).

### `nova-types-signature`
- **Purpose:** best-effort translation from JVM descriptors + generic signature ASTs (`nova-classfile`) into Nova’s type model (`nova-types::Type`).
- **Key entry points:** `crates/nova-types-signature/src/lib.rs` (`SignatureTranslator`, `TypeVarScope`, `ty_from_type_sig`, `class_sig_from_classfile`, `method_sig_from_classfile`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Nested/inner class type arguments are flattened into a single argument list (Nova’s `Type::Class` does not model owner-type generics yet).

### `nova-vfs`
- **Purpose:** virtual filesystem layer (file IDs, overlays, archive paths, file-watching). Includes feature-gated OS watcher integration (Notify-backed; keeps the `notify` dependency inside `nova-vfs`), recursive/non-recursive watch modes, and move/rename normalization.
- **Key entry points:** `crates/nova-vfs/src/lib.rs` (`Vfs`, `OpenDocuments`, `VfsPath`), `crates/nova-vfs/src/watch.rs` (`FileWatcher`, `WatchMode`, `WatchEvent`).
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - `nova-lsp`’s stdio server uses `nova_vfs::Vfs<LocalFs>` as its primary file store + open-document overlay (see `AnalysisState` in `crates/nova-lsp/src/main.rs`); decompiled virtual documents are stored in `nova-vfs`'s bounded virtual document store.
  - The richer path model (`VfsPath::{Archive,Decompiled}` / `ArchivePath`) exists, but adoption across the wider codebase is still partial; making `VfsPath`/ADR 0006 canonical URIs the end-to-end representation for *all* “virtual documents” (archives, decompiled sources, generated files) is still in progress.

### `nova-worker`
- **Purpose:** `nova-worker` binary for distributed mode (connects to `nova-router` and builds shard indexes).
- **Key entry points:** `crates/nova-worker/src/main.rs`.
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - Distributed mode is now wired into the shipped `nova-lsp` stdio server behind CLI flags (`--distributed`, `--distributed-worker-command`), but the editor-facing surface area is still intentionally narrow/experimental (primarily `workspace/symbol` + best-effort file update propagation; see `docs/16-distributed-mode.md`).
  - v3 is the current router↔worker protocol; schema evolution is expected within minor versions.

### `nova-workspace`
- **Purpose:** library-first workspace engine used by the `nova` CLI (indexing, diagnostics, cache mgmt, events). Workspace watching is built on `nova-vfs::FileWatcher` and refreshes its watch paths dynamically after project reload (when source roots change).
- **Key entry points:** `crates/nova-workspace/src/lib.rs` (`Workspace::open`, `Workspace::index_and_write_cache`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Indexing/diagnostics are still largely heuristic (regex + per-file parsing), not yet Salsa query-based.

### `nova-yaml`
- **Purpose:** minimal, range-preserving YAML parser (targeted at Spring/Micronaut config files).
- **Key entry points:** `crates/nova-yaml/src/lib.rs` (`parse`, `YamlDocument`).
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Intentionally partial YAML implementation (not YAML 1.2-complete).

### `xtask`
- **Purpose:** developer tooling tasks invoked via `cargo run --locked -p xtask -- ...` (currently: codegen).
- **Key entry points:** `crates/xtask/src/lib.rs` (`main`, `codegen`, `generate_ast`),
  `crates/xtask/src/main.rs`.
- **Maturity:** productionizing
- **Known gaps vs intended docs:**
  - Only implements `cargo xtask codegen` today (generates `nova-syntax` AST bindings from `java.syntax`).

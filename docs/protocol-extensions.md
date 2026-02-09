# Protocol extensions (`nova/*` LSP methods)

Nova extends LSP with a small set of **custom JSON-RPC methods** under the `nova/*` namespace. This
document is the stable reference for those methods so editor clients do **not** need to read Rust
code to interoperate.

Source of truth for method names:

- `crates/nova-lsp/src/lib.rs` (string constants like `TEST_DISCOVER_METHOD`)
- `editors/vscode/src/*.ts` (client usage for a subset)

This document is validated in CI by `nova-devtools check-protocol-extensions` to ensure it stays in
sync with both server + client code.

> Note: Nova also uses standard LSP requests (e.g. `textDocument/formatting`) and standard command /
> code action wiring. Those are intentionally *not* covered here **except** for a small set of
> `workspace/executeCommand` command IDs that have important cross-editor interoperability
> requirements (notably when the server drives `workspace/applyEdit`).

## Capability gating (how clients detect support)

`nova-lsp` advertises its custom protocol surface in `initializeResult.capabilities.experimental`:

- `experimental.nova.requests`: array of supported `nova/*` request method strings
- `experimental.nova.notifications`: array of supported `nova/*` notification method strings

Clients should still gate features defensively because older Nova versions may omit this list (or
may not include newly-added methods). Use one or more of:

1. **Optimistic call + graceful fallback**: send the request and treat JSON-RPC `-32601` “Method
   not found” **or** `-32602` with an “unknown … method” message as “server doesn’t support this
   extension”. (The current `nova-lsp` stdio server routes all `nova/*` requests through a single
   dispatcher, so unsupported `nova/*` methods often show up as `-32602`.)
2. **Version gating**: use `initializeResult.serverInfo` (`name`/`version`) and require a minimum
   Nova version for features that are known to exist after a cutoff.
3. **Schema gating**: for endpoints that return `schemaVersion`, clients must validate it and
   reject unknown major versions.

The VS Code extension uses (1) broadly for `nova/*` requests:

- Feature-level gating using `initializeResult.capabilities.experimental.nova` (see
  `editors/vscode/src/novaCapabilities.ts`)
- Optimistic call + graceful method-not-found handling in `sendNovaRequest` (see
  `editors/vscode/src/extension.ts`)

AI multi-token completions (`nova/completion/more`) also uses an optimistic call + graceful fallback
loop in `editors/vscode/src/aiCompletionMore.ts`.

## Common error behavior (timeouts, safe-mode, cancellation)

### JSON-RPC error codes

Nova uses standard JSON-RPC/LSP error codes:

- `-32601` — method not found (treat as “unsupported extension”)
- `-32602` — invalid params (schema mismatch). Note: the current `nova-lsp` stdio server also
  returns `-32602` for **unknown `nova/*` methods** (because it attempts to dispatch all `nova/*`
  through `nova_lsp::handle_custom_request()`).
- `-32603` — internal error
- `-32800` — request cancelled

### Watchdog timeouts + safe-mode

Most `nova/*` requests dispatched through `nova_lsp::handle_custom_request()` are wrapped in a
watchdog (see `crates/nova-lsp/src/hardening.rs`):

- If the handler **exceeds its per-method time budget**, the request fails with `-32603`.
- If the handler **panics**, the request fails with `-32603`.
- Some watchdog failures may temporarily put the server into **safe-mode**.

When in safe-mode, **all methods dispatched through** `nova_lsp::handle_custom_request()` **except**
`nova/bugReport`, `nova/metrics`, `nova/resetMetrics`, and `nova/safeModeStatus` fail with `-32603`
and a message like:

> “Nova is running in safe-mode … Only `nova/bugReport`, `nova/metrics`, `nova/resetMetrics`, and `nova/safeModeStatus` are available for now.”

Note: safe-mode enforcement is implemented by `nova_lsp::hardening::guard_method()` and is enforced
by both `nova_lsp::handle_custom_request()` **and** the `nova-lsp` stdio server for stateful
endpoints implemented directly in `crates/nova-lsp/src/main.rs`.

Safe-mode windows:

- Panic: ~60s
- Watchdog timeout (selected methods): ~30s

### Cancellation

Nova’s watchdog has a cancellation mechanism (via `nova-scheduler`), but most current handlers are
synchronous and **do not yet poll cancellation tokens**. Clients should treat cancellation as
best-effort:

- If the server honours cancellation, the request fails with LSP `-32800` (“RequestCancelled”).
- Otherwise, the request will complete normally or hit its timeout budget.

## Method catalog

Unless stated otherwise:

- Requests use `params` as a JSON object (not positional).
- Positions/ranges follow LSP conventions: 0-based line and UTF-16 `character` offsets.
- Field casing is as defined by the Rust `serde` types. Most endpoints use `camelCase`; notable
  exceptions are called out below.

---

## Testing (`nova-testing`)

### `nova/test/discover`

- **Kind:** request
- **Stability:** stable
- **Rust types:** `crates/nova-testing/src/schema.rs` (`TestDiscoverRequest`, `TestDiscoverResponse`)
- **Handler:** `crates/nova-lsp/src/extensions/test.rs::handle_discover`
- **Time budget:** 30s (no safe-mode on timeout)

#### Request params

```json
{
  "projectRoot": "/absolute/path/to/workspace"
}
```

#### Response

```json
{
  "schemaVersion": 1,
  "tests": [
    {
      "id": "com.example.MyTest",
      "label": "MyTest",
      "kind": "class",
      "framework": "junit5",
      "path": "src/test/java/com/example/MyTest.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
      "children": []
    }
  ]
}
```

#### Errors

- `-32602` if params do not match the schema.
- `-32603` for internal failures (IO, parsing, tool errors).

---

### `nova/test/run`

- **Kind:** request
- **Stability:** stable
- **Rust types:** `crates/nova-testing/src/schema.rs` (`TestRunRequest`, `TestRunResponse`)
- **Handler:** `crates/nova-lsp/src/extensions/test.rs::handle_run`
- **Time budget:** 300s (no safe-mode on timeout)

#### Request params

```json
{
  "projectRoot": "/absolute/path/to/workspace",
  "buildTool": "auto",
  "tests": ["com.example.MyTest#adds"]
}
```

`buildTool` is one of: `"auto" | "maven" | "gradle"`.

#### Response

```json
{
  "schemaVersion": 1,
  "tool": "maven",
  "success": true,
  "exitCode": 0,
  "stdout": "",
  "stderr": "",
  "tests": [
    { "id": "com.example.MyTest#adds", "status": "passed", "durationMs": 4 }
  ],
  "summary": { "total": 1, "passed": 1, "failed": 0, "skipped": 0 }
}
```

---

### `nova/test/debugConfiguration`

- **Kind:** request
- **Stability:** stable
- **Rust types:** `crates/nova-testing/src/schema.rs` (`TestDebugRequest`, `TestDebugResponse`, `DebugConfiguration`)
- **Handler:** `crates/nova-lsp/src/extensions/test.rs::handle_debug_configuration`
- **Time budget:** 30s (no safe-mode on timeout)

#### Request params

```json
{
  "projectRoot": "/absolute/path/to/workspace",
  "buildTool": "auto",
  "test": "com.example.MyTest#adds"
}
```

#### Response

```json
{
  "schemaVersion": 1,
  "tool": "maven",
  "configuration": {
    "schemaVersion": 1,
    "name": "Debug com.example.MyTest#adds",
    "cwd": "/absolute/path/to/workspace",
    "command": "mvn",
    "args": ["-Dmaven.surefire.debug", "-Dtest=com.example.MyTest#adds", "test"],
    "env": {}
  }
}
```

---

## Build integration (`nova-build`, `nova-project`)

### `nova/buildProject`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/build.rs` (`NovaProjectParams`, `NovaBuildProjectResponse`)
- **Handler:** `crates/nova-lsp/src/extensions/build.rs::handle_build_project`
- **Time budget:** 120s (no safe-mode on timeout)

#### Request params

```json
{
  "projectRoot": "/absolute/path/to/workspace",
  "buildTool": "auto",
  "module": null,
  "projectPath": null,
  "target": null
}
```

Notes:

- `projectRoot` also accepts the legacy alias `root`.
- `buildTool` also accepts the legacy alias `kind`.
- For Maven multi-module projects, `module` is a path relative to `projectRoot`.
- For Gradle, `projectPath` is the Gradle path (e.g. `":app"`).
- For Bazel workspaces, `target` is required and should be a Bazel label (e.g. `"//app:lib"`).

#### Response

```json
{
  "schemaVersion": 1,
  "buildId": 123,
  "status": "queued",
  "diagnostics": [
    {
      "file": "/absolute/path/to/Foo.java",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
      "severity": "error",
      "message": "…",
      "source": "maven"
    }
  ]
}
```

Notes:

- `buildProject` enqueues a background build and returns immediately.
- `diagnostics` contains the **last known** diagnostics. Clients should poll `nova/build/status` and
  `nova/build/diagnostics` to observe the build completion and retrieve updated diagnostics.
- For Bazel workspaces, builds are executed via BSP; Nova resolves the BSP launcher from standard
  `.bsp/*.json` config files (when present) and/or `NOVA_BSP_PROGRAM` / `NOVA_BSP_ARGS`.

---

### `nova/java/classpath`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/build.rs` (`NovaProjectParams`, `NovaClasspathResponse`)
- **Handler:** `crates/nova-lsp/src/extensions/build.rs::handle_java_classpath`
- **Time budget:** 60s (no safe-mode on timeout)

#### Request params

Same as `nova/buildProject` (`NovaProjectParams`).

#### Response

```json
{
  "classpath": ["/path/to/dependency.jar", "/path/to/target/classes"],
  "modulePath": [],
  "sourceRoots": ["src/main/java"],
  "generatedSourceRoots": [],
  "languageLevel": { "major": 17, "preview": false },
  "outputDirs": { "main": ["/path/to/target/classes"], "test": ["/path/to/target/test-classes"] }
}
```

Notes:

- The response is **backwards compatible** with early clients that only expect `classpath`.
- New fields are always present; when Nova cannot determine a value, it falls back to an empty
  list / default language level.

---

### `nova/projectConfiguration`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/project.rs` (`ProjectConfigurationParams`, `ProjectConfigurationResponse`)
- **Handler:** `crates/nova-lsp/src/extensions/project.rs::handle_project_configuration`
- **Time budget:** 60s (no safe-mode on timeout)

This endpoint returns a **single snapshot** of Nova’s inferred project configuration for a
workspace root: build system kind, Java language level, source roots, classpath/module-path,
output directories, and a best-effort dependency list.

#### Request params

```json
{ "projectRoot": "/absolute/path/to/workspace" }
```

`projectRoot` also accepts the legacy alias `root`.

#### Response

```json
{
  "schemaVersion": 1,
  "workspaceRoot": "/absolute/path/to/workspace",
  "buildSystem": "maven",
  "java": { "source": 17, "target": 17 },
  "modules": [{ "name": "app", "root": "/absolute/path/to/workspace" }],
  "sourceRoots": [
    { "kind": "main", "origin": "source", "path": "/absolute/path/to/workspace/src/main/java" }
  ],
  "classpath": [{ "kind": "jar", "path": "/path/to/dependency.jar" }],
  "modulePath": [],
  "outputDirs": [{ "kind": "main", "path": "/absolute/path/to/workspace/target/classes" }],
  "dependencies": [{ "groupId": "org.junit.jupiter", "artifactId": "junit-jupiter", "scope": "test" }]
}
```

Notes:

- `buildSystem` is one of: `"maven" | "gradle" | "bazel" | "simple"`.
- `java` is a **workspace-level** language level (not per-module). For multi-module workspaces, Nova
  reports a conservative value by taking the **maximum** `source`/`target` across modules and
  enabling preview if any module enables it.
- Most paths are returned as **absolute filesystem paths** (Nova canonicalizes the workspace root).
- `dependencies` is best-effort and may be empty, especially for Gradle/Bazel projects.

#### Errors

- `-32602` if `projectRoot` is missing/empty or params do not match the schema.
- `-32603` for internal failures (filesystem errors, build tool integration failures).

---

### `nova/java/sourcePaths`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/java.rs` (`JavaSourcePathsParams`, `JavaSourcePathsResponse`)
- **Handler:** `crates/nova-lsp/src/extensions/java.rs::handle_source_paths`
- **Time budget:** 30s (no safe-mode on timeout)

This is a convenience endpoint that returns the workspace’s Java source roots (including generated
roots when known).

#### Request params

```json
{ "projectRoot": "/absolute/path/to/workspace" }
```

`projectRoot` also accepts the legacy alias `root`.

#### Response

```json
{
  "schemaVersion": 1,
  "roots": [
    { "kind": "main", "origin": "source", "path": "/absolute/path/to/workspace/src/main/java" },
    { "kind": "test", "origin": "source", "path": "/absolute/path/to/workspace/src/test/java" }
  ]
}
```

This is equivalent to `nova/projectConfiguration.sourceRoots` (subset).

#### Errors

- `-32602` if `projectRoot` is missing/empty or params do not match the schema.
- `-32603` for internal failures while loading the project configuration.

---

### `nova/java/resolveMainClass`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/java.rs` (`ResolveMainClassParams`, `ResolveMainClassResponse`, `ResolvedJavaClass`)
- **Handler:** `crates/nova-lsp/src/extensions/java.rs::handle_resolve_main_class`
- **Time budget:** 60s (no safe-mode on timeout)

Discover runnable entry points (“main classes”) or Spring Boot application classes.

#### Request params

Either provide `projectRoot` (scan the project) **or** a `uri` (only inspect that file):

```json
{ "projectRoot": "/absolute/path/to/workspace", "includeTests": false }
```

```json
{ "uri": "file:///absolute/path/to/Foo.java", "includeTests": true }
```

Notes:

- `projectRoot` also accepts the legacy alias `root`.
- `uri` must be a `file://` URI.

#### Response

```json
{
  "schemaVersion": 1,
  "classes": [
    {
      "qualifiedName": "com.example.Main",
      "simpleName": "Main",
      "path": "/absolute/path/to/workspace/src/main/java/com/example/Main.java",
      "hasMain": true,
      "isTest": false,
      "isSpringBootApp": false
    }
  ]
}
```

Filtering behavior:

- When `includeTests` is `false` (default), the server returns:
  - classes with `hasMain = true`, and
  - classes with `isSpringBootApp = true`.
- When `includeTests` is `true`, the server also returns test classes (`isTest = true`).

The returned list is sorted by `qualifiedName`, then `path`, for determinism.

#### Errors

- `-32602` if neither `projectRoot` nor `uri` is provided, or if `uri` is not a valid `file://` URI.
- `-32603` for IO errors reading the file(s) or internal failures during discovery.

---

### `nova/reloadProject`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/build.rs` (`NovaProjectParams`)
- **Handler:** `crates/nova-lsp/src/extensions/build.rs::handle_reload_project`
- **Time budget:** 60s (no safe-mode on timeout)

#### Request params

Same as `nova/buildProject` (`NovaProjectParams`).

#### Response

`null` (JSON-RPC result `null`).

---

### `nova/build/targetClasspath`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/build.rs` (`TargetClasspathParams`, `TargetClasspathResult`)
- **Handler:** `crates/nova-lsp/src/extensions/build.rs::handle_target_classpath`
- **Time budget:** 60s (no safe-mode on timeout)

#### Request params

```json
{
  "projectRoot": "/absolute/path/to/workspace",
  "target": null
}
```

Notes:

- `projectRoot` also accepts the legacy alias `root`.
- For **Bazel** workspaces, `target` is required and should be a Bazel label (e.g. `//app:lib`).
- For **Maven** workspaces, `target` may be used to select a module directory relative to
  `projectRoot` (e.g. `"module-a"`). `"."` / empty means “workspace” (no module selection).
- For **Gradle** workspaces, `target` may be used to select a Gradle project path (e.g. `":app"`).
  `":"` / empty means “workspace” (no project selection). Nova accepts project paths without the
  leading `:` and will normalize them.

#### Response

```json
{
  "projectRoot": "/absolute/path/to/workspace",
  "target": "//app:lib",
  "classpath": ["/path/to/dependency.jar"],
  "modulePath": [],
  "sourceRoots": ["src/main/java"],
  "source": "17",
  "targetVersion": "17",
  "release": null,
  "outputDir": null,
  "enablePreview": false
}
```

---

### `nova/build/fileClasspath`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/build.rs` (`FileClasspathParams`, `TargetClasspathResult`)
- **Handler:** `crates/nova-lsp/src/extensions/build.rs::handle_file_classpath`
- **Time budget:** 60s (no safe-mode on timeout)

This endpoint is the **file-based** variant of `nova/build/targetClasspath` for Bazel projects. It
uses Bazel file → owning-target resolution to return compilation flags only for the currently opened
file (on-demand), without requiring clients to know a Bazel target upfront.

#### Request params

```json
{
  "projectRoot": "/absolute/path/to/workspace",
  "uri": "file:///absolute/path/to/workspace/java/Hello.java",
  "runTarget": null
}
```

Notes:

- `projectRoot` also accepts the legacy alias `root`.
- `projectRoot` may be the Bazel workspace root itself or any path under it; the server will
  normalize it to the detected workspace root.
- `uri` must be a `file://` URI.
- `runTarget` is optional; when provided, resolution is restricted to the transitive closure of that
  Bazel target (`deps(runTarget)`).

#### Response

On success, the response is a `TargetClasspathResult` object (same shape as `nova/build/targetClasspath`).
If Nova cannot resolve compile info for the file (outside workspace / not in a Bazel package / no owning
`java_*` target), the result is `null`.

---

### `nova/build/status`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/build.rs` (`BuildStatusParams`, `BuildStatusResult`)
- **Handler:** `crates/nova-lsp/src/extensions/build.rs::handle_build_status`
- **Time budget:** 5s (**timeout may enter safe-mode**)

#### Request params

```json
{ "projectRoot": "/absolute/path/to/workspace" }
```

`projectRoot` also accepts the legacy alias `root`.

#### Response

```json
{
  "schemaVersion": 1,
  "status": "idle",
  "lastError": null
}
```

Status values are `snake_case`:

`"idle" | "building" | "failed"`.

Semantics:

- `idle` when no build tool invocation is currently running and the last invocation succeeded (or none ran).
- `building` while any build/classpath/build-tool command is in-flight for the workspace.
- `failed` when the last build tool invocation for the workspace failed.

---

### `nova/build/diagnostics`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/build.rs` (`BuildDiagnosticsParams`, `BuildDiagnosticsResult`)
- **Handler:** `crates/nova-lsp/src/extensions/build.rs::handle_build_diagnostics`
- **Time budget:** 120s (no safe-mode on timeout)

#### Request params

```json
{
  "projectRoot": "/absolute/path/to/workspace",
  "target": null
}
```

#### Response

```json
{
  "schemaVersion": 1,
  "target": null,
  "status": "idle",
  "buildId": null,
  "diagnostics": [],
  "source": null,
  "error": null
}
```

Notes:

- For Bazel projects diagnostics are sourced via BSP when configured (via standard `.bsp/*.json` or
  `NOVA_BSP_PROGRAM` / `NOVA_BSP_ARGS`).
- `status` indicates the state of the background build task (if one has been enqueued).

---

### `nova/projectModel`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/build.rs` (`ProjectModelParams`, `ProjectModelResult`,
  `ProjectModelUnit`, `JavaLanguageLevel`)
- **Handler:** `crates/nova-lsp/src/extensions/build.rs::handle_project_model`
- **Time budget:** 120s (no safe-mode on timeout)

This endpoint returns a **normalized “project model”** for the workspace so editor clients can build
their own internal module/target graph without having to re-implement Maven/Gradle/Bazel discovery.

#### Request params

```json
{ "projectRoot": "/absolute/path/to/workspace" }
```

`projectRoot` also accepts the legacy alias `root`.

#### Response

```json
{
  "projectRoot": "/absolute/path/to/workspace",
  "units": [
    {
      "kind": "maven",
      "module": ".",
      "compileClasspath": ["/path/to/dependency.jar"],
      "modulePath": [],
      "sourceRoots": ["src/main/java"],
      "languageLevel": { "source": "17", "target": "17", "release": null }
    }
  ]
}
```

`units` is a list of `ProjectModelUnit` objects keyed by the `kind` discriminator:

- `"maven"`: `{ module, compileClasspath, modulePath, sourceRoots, languageLevel }`
- `"gradle"`: `{ projectPath, compileClasspath, modulePath, sourceRoots, languageLevel }`
- `"bazel"`: `{ target, compileClasspath, modulePath, sourceRoots, languageLevel }`
- `"simple"`: `{ module, compileClasspath, modulePath, sourceRoots, languageLevel }`

#### Errors

- `-32602` if `projectRoot` is missing/empty.
- `-32603` for build tool invocation failures, Bazel query issues, or filesystem errors.

---

## Annotation processing / generated sources (`nova-apt`)

### `nova/java/generatedSources`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/apt.rs` (`NovaGeneratedSourcesParams`, `GeneratedSourcesResponse`)
- **Handler:** `crates/nova-lsp/src/extensions/apt.rs::handle_generated_sources`
- **Time budget:** 60s (no safe-mode on timeout)

#### Request params

```json
{
  "projectRoot": "/absolute/path/to/workspace",
  "module": null,
  "projectPath": null,
  "target": null
}
```

`projectRoot` also accepts the legacy alias `root`.

Notes:

- `module` (Maven): module path relative to `projectRoot` (e.g. `"module-a"`). `"."` / empty means
  “workspace” (no filtering; include all modules).
- `projectPath` (Gradle): Gradle project path (e.g. `":app"`). `":"` / empty means “workspace” (no
  filtering; include all modules). Nova accepts project paths without the leading `:` and will
  normalize them.
- `target` (Bazel): Bazel label (e.g. `"//app:lib"`). Currently accepted for symmetry but does not
  change behavior for Bazel workspaces (Nova reports generated roots at the workspace/module level).

#### Response

```json
{
  "enabled": true,
  "modules": [
    {
      "moduleName": "app",
      "moduleRoot": "/absolute/path/to/workspace",
      "roots": [
        { "kind": "main", "path": "/absolute/path/to/workspace/target/generated-sources/annotations", "freshness": "fresh" }
      ]
    }
  ]
}
```

---

### `nova/java/runAnnotationProcessing`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/apt.rs` (`RunAnnotationProcessingResponse`)
- **Handler:** `crates/nova-lsp/src/extensions/apt.rs::handle_run_annotation_processing`
- **Time budget:** 300s (no safe-mode on timeout)

#### Request params

Same as `nova/java/generatedSources`.

#### Response

```json
{
  "progress": ["Running annotation processing", "Invoking build tool", "Build finished", "done"],
  "progressEvents": [
    { "kind": "begin", "message": "Running annotation processing" },
    { "kind": "report", "message": "Invoking build tool" },
    { "kind": "report", "message": "Build finished" },
    { "kind": "end", "message": "done" }
  ],
  "diagnostics": [],
  "moduleDiagnostics": [],
  "generatedSources": { "enabled": true, "modules": [] }
}
```

Notes:

- `progressEvents` and `moduleDiagnostics` are newer structured fields. They are always present but
  may be empty when the underlying build tool does not provide sufficient metadata.

---

## Framework introspection endpoints

### `nova/web/endpoints`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/web.rs` (`WebEndpointsRequest`, `WebEndpointsResponse`)
- **Handler:** `crates/nova-lsp/src/extensions/web.rs::handle_endpoints`
- **Time budget:** 2s (**timeout may enter safe-mode**)

#### Request params

```json
{ "projectRoot": "/absolute/path/to/workspace" }
```

#### Response

```json
{
  "endpoints": [
    { "path": "/api/hello", "methods": ["GET"], "file": "src/main/java/com/example/Hello.java", "line": 42 },
    { "path": "/api/health", "methods": ["GET"], "file": null, "line": 1 }
  ]
}
```

Notes:

- `line` is **1-based** (matches `nova-framework-web`).
- `file` is a best-effort relative path when `projectRoot` is provided, but it may be `null` (or
  missing) when the extractor cannot determine a source location.
  - Current `nova-lsp` behavior: the `file` field is present and set to `null` when unavailable.
    Clients should still treat a missing `file` field as “unavailable” for forward compatibility.
  - Clients should still display the endpoint, but disable navigation (or show “location
    unavailable”) when `file` is unavailable.

---

### `nova/quarkus/endpoints` (alias)

- **Kind:** request
- **Stability:** experimental
- **Definition:** `crates/nova-lsp/src/lib.rs::QUARKUS_ENDPOINTS_METHOD`
- **Behavior:** identical to `nova/web/endpoints` (same handler and payloads).

---

### `nova/micronaut/endpoints`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/micronaut.rs` (`MicronautRequest`, `MicronautEndpointsResponse`)
- **Handler:** `crates/nova-lsp/src/extensions/micronaut.rs::handle_endpoints`
- **Time budget:** 2s (**timeout may enter safe-mode**)

#### Request params

```json
{ "projectRoot": "/absolute/path/to/workspace" }
```

#### Response

```json
{
  "schemaVersion": 1,
  "endpoints": [
    {
      "method": "GET",
      "path": "/hello",
      "handler": {
        "file": "src/main/java/com/example/HelloController.java",
        "span": { "start": 123, "end": 140 },
        "className": "com.example.HelloController",
        "methodName": "hello"
      }
    }
  ]
}
```

Notes:

- `handler.file` is a best-effort path relative to `projectRoot` (when provided). It may contain
  platform path separators (e.g. `\` on Windows).
- `handler.span.start` / `handler.span.end` are **byte offsets** into the UTF-8 source file.

---

### `nova/micronaut/beans`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/extensions/micronaut.rs` (`MicronautBeansResponse`)
- **Handler:** `crates/nova-lsp/src/extensions/micronaut.rs::handle_beans`
- **Time budget:** 2s (**timeout may enter safe-mode**)

#### Request params

Same as `nova/micronaut/endpoints`.

#### Response

```json
{
  "schemaVersion": 1,
  "beans": [
    {
      "id": "bean:com.example.Foo",
      "name": "foo",
      "ty": "com.example.Foo",
      "kind": "class",
      "qualifiers": [],
      "file": "src/main/java/com/example/Foo.java",
      "span": { "start": 10, "end": 20 }
    }
  ]
}
```

Notes:

- `beans[].kind` is one of: `"class" | "factoryMethod"`.
- `beans[].qualifiers` is a list of string-encoded qualifiers:
  - `Named(<name>)` for `@Named` qualifiers (e.g. `"Named(primary)"`).
  - A raw qualifier annotation name (e.g. `"MyQualifier"`).
- `beans[].file` is a best-effort path relative to `projectRoot` (when provided). It may contain
  platform path separators (e.g. `\` on Windows).
- `beans[].span.start` / `beans[].span.end` are **byte offsets** into the UTF-8 source file.

---

## Debugger-excellence endpoints

### `nova/debug/configurations`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-ide/src/project.rs` (`DebugConfiguration`)
- **Handler:** `crates/nova-lsp/src/extensions/debug.rs::handle_debug_configurations`
- **Time budget:** 30s (no safe-mode on timeout)

#### Request params

```json
{ "projectRoot": "/absolute/path/to/workspace" }
```

`projectRoot` also accepts the legacy alias `root`.

#### Response

JSON array of VS Code-style debug configurations:

```json
[
  {
    "name": "Run Main",
    "type": "java",
    "request": "launch",
    "mainClass": "com.example.Main",
    "args": [],
    "vmArgs": [],
    "projectName": "my-workspace",
    "springBoot": false
  }
]
```

---

### `nova/debug/hotSwap`

- **Kind:** request
- **Stability:** experimental
- **Rust types:**
  - Request: `crates/nova-lsp/src/extensions/debug.rs` (`HotSwapRequestParams`)
  - Response: `crates/nova-dap/src/hot_swap.rs` (`HotSwapResult`)
- **Handler:** `crates/nova-lsp/src/extensions/debug.rs::handle_hot_swap`
- **Time budget:** 120s (no safe-mode on timeout)

#### Request params

```json
{
  "projectRoot": "/absolute/path/to/workspace",
  "changedFiles": ["src/main/java/com/example/Foo.java"],
  "host": "localhost",
  "port": 5005
}
```

Notes:

- `changedFiles` entries may be absolute or relative paths; relative paths are resolved against `projectRoot`.
- `host` is optional; default is `127.0.0.1`. May be an IP address *or* hostname (for example `localhost`).
- A single `.java` file can compile to **multiple JVM classes** (e.g. the primary class plus nested / anonymous classes like `Foo$Inner` or `Foo$1`). For each `changedFiles` entry, the server may redefine **multiple** classes and then **aggregate** the outcome into a single per-file result entry.
- Implementations may choose to **skip unloaded classes** (recommended): if a compiled class is not currently loaded in the target VM, it is ignored/skipped rather than treated as an error. In that case, `status: "success"` still means “all attempted (loaded) class redefinitions succeeded”.

#### Response

```json
{
  "results": [
    { "file": "/absolute/path/to/workspace/src/main/java/com/example/Foo.java", "status": "success" }
  ]
}
```

`status` values are `snake_case`:

`"success" | "compile_error" | "schema_change" | "redefinition_error"`.

---

## AI augmentation endpoints

These endpoints are currently implemented in the `nova-lsp` **binary**
(`crates/nova-lsp/src/main.rs`) and require AI to be configured (see env vars in `main.rs` like
`NOVA_AI_PROVIDER`).

`nova/ai/explainError` is an explain-only operation that returns text. `nova/ai/generateMethodBody`
and `nova/ai/generateTests` are **patch-based code-editing operations** that apply a `WorkspaceEdit`
via `workspace/applyEdit` and return `null` (on success).

All AI requests accept an optional `workDoneToken` (standard LSP work-done progress token). When
present, the server emits `$/progress` notifications for user-visible progress.

#### Privacy: `ai.privacy.excluded_paths`

`ai.privacy.excluded_paths` is a server-side allow/deny list for **file-backed** AI context.
Behavior depends on the operation:

- **Explain-only** requests (e.g. `nova/ai/explainError`) are still accepted for excluded files, but
  the server omits file-backed source text and file path metadata from the prompt (it will ignore
  any client-supplied `code` snippet for excluded files).
- **Patch-based code edits** (e.g. `nova/ai/generateMethodBody`, `nova/ai/generateTests`) are
  rejected when the target file is excluded.
- Any additional context snippets whose paths match `excluded_paths` (semantic-search related code,
  “extra files”) are omitted and replaced with an omission placeholder.

See `crates/nova-lsp/src/stdio_ai.rs` (request-level enforcement) and `crates/nova-ai/src/features.rs`
(prompt sanitization).

### `nova/ai/explainError`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-ide/src/ai.rs` (`ExplainErrorArgs`)

#### Request params

```json
{
  "workDoneToken": "optional",
  "diagnostic_message": "cannot find symbol",
  "code": "optional snippet",
  "uri": "file:///…",
  "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 10 } }
}
```

Note the field names inside args are currently **snake_case** (`diagnostic_message`) because the
Rust type does not use `rename_all = "camelCase"`.

#### Response

JSON string (the explanation).

#### Errors

- `-32600` if AI is not configured.
- `-32603` for model/provider failures.

---

### `nova/ai/generateMethodBody`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-ide/src/ai.rs` (`GenerateMethodBodyArgs`)

#### Request params

```json
{
  "workDoneToken": "optional",
  "method_signature": "public int add(int a, int b)",
  "context": "optional surrounding code",
  "uri": "file:///…",
  "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 10 } }
}
```

Notes:

- `uri` and `range` are required for patch-based edits.
- Field names inside args are currently **snake_case** (`method_signature`) because the Rust type
  does not use `rename_all = "camelCase"`.
- The server expects `range` to include both `{` and `}` of the target method, and the selected
  method body must be empty.

#### Response

`null` (JSON-RPC result `null`).

#### Side effects

On success, the server sends a `workspace/applyEdit` request (label: `"AI: Generate method body"`)
containing a standard LSP `WorkspaceEdit`.

#### Errors

- `-32600` if AI is not configured, or if the target file is blocked by `ai.privacy.excluded_paths`.
- `-32602` for invalid params (e.g. missing `uri`/`range`).
- `-32603` for internal failures (model/provider errors, patch parsing/validation failures) **or**
  when blocked by privacy policy (cloud code-edit policy enforcement).
- `-32800` if the request is cancelled.

---

### `nova/ai/generateTests`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-ide/src/ai.rs` (`GenerateTestsArgs`)

#### Request params

```json
{
  "workDoneToken": "optional",
  "target": "public int add(int a, int b)",
  "context": "optional surrounding code",
  "uri": "file:///…",
  "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 10 } }
}
```

Notes:

- `uri` and `range` are required for patch-based edits.
- Field names inside args are currently **snake_case** because the Rust type does not use
  `rename_all = "camelCase"`.
- The server attempts (best-effort) to generate/update a test file under `src/test/java/` based on
  the selected source file’s package and class name. If that derivation fails, it falls back to
  inserting the generated tests into the current file at `range`.

#### Response

`null` (JSON-RPC result `null`).

#### Side effects

On success, the server sends a `workspace/applyEdit` request (label: `"AI: Generate tests"`)
containing a standard LSP `WorkspaceEdit`.

#### Errors

Same as `nova/ai/generateMethodBody`.

---

## Semantic search endpoints

### `nova/semanticSearch/indexStatus`

- **Kind:** request
- **Stability:** experimental
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server)

This endpoint exposes the state of Nova’s background **workspace semantic-search indexing**.

It is primarily useful for clients/tests that want to wait until indexing has completed before
issuing AI requests that benefit from semantic search context.

#### Request params

No params are required; clients should send `{}` or omit params.

#### Response

```json
{
  "currentRunId": 1,
  "completedRunId": 1,
  "done": true,
  "indexedFiles": 42,
  "indexedBytes": 12345
}
```

Field semantics:

- `currentRunId` (number): the id of the most recent indexing run. `0` means no workspace indexing
  run has been started (for example: semantic search disabled, missing workspace root, AI runtime
  not available, or the server is in safe-mode).
- `completedRunId` (number): the id of the most recently completed indexing run.
- `done` (boolean): `true` when `currentRunId != 0` and `currentRunId == completedRunId`.
- `indexedFiles` (number): number of files indexed so far in the current run.
- `indexedBytes` (number): number of bytes indexed so far in the current run.

Notes:

- Workspace semantic-search indexing is best-effort and is only started when semantic search is
  enabled in the Nova config (e.g. `ai.enabled=true`, `ai.features.semantic_search=true`), the
  server has a valid workspace root (`initialize.rootUri`), the AI runtime is available, and the
  server is not in safe-mode.

#### Safe-mode

This request is guarded by `nova_lsp::hardening::guard_method()` and fails with `-32603` while the
server is in safe-mode.

---

### `nova/semanticSearch/search`

- **Kind:** request
- **Stability:** experimental
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server)

Query Nova’s in-process semantic-search index (populated via open-document indexing and, when
available, best-effort workspace indexing).

#### Request params

```json
{ "query": "zebraToken", "limit": 10 }
```

Fields:

- `query` (string, required): search query.
- `limit` (number, optional): maximum number of results to return (server clamps internally).

#### Response

```json
{
  "results": [
    {
      "path": "src/UsesZebra.java",
      "kind": "file",
      "score": 1.23,
      "snippet": "class UsesZebra { String token = \"zebraToken\"; }"
    }
  ]
}
```

Notes:

- `path` is workspace-relative (with forward slashes) when the server has a workspace root
  (`initialize.rootUri`) and the result file is under it; otherwise it is an absolute path string.
- If semantic search is disabled in config (`ai.enabled=false` or `ai.features.semantic_search=false`),
  the server returns `{ "results": [] }`.

#### Safe-mode

This request is guarded by `nova_lsp::hardening::guard_method()` and fails with `-32603` while the
server is in safe-mode.

---

## Performance / observability endpoints

### `nova/memoryStatus`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/lib.rs::MemoryStatusResponse`, `crates/nova-memory/src/report.rs::MemoryReport`
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server)

#### Request params

No params are required; clients should send `{}` or omit params.

#### Response

```json
{
  "report": {
    "budget": { "total": 4294967296, "categories": { "query_cache": 0, "syntax_trees": 0, "indexes": 0, "type_info": 0, "other": 0 } },
    "usage": { "query_cache": 0, "syntax_trees": 0, "indexes": 0, "type_info": 0, "other": 0 },
    "rss_bytes": 123456789,
    "pressure": "low",
    "degraded": { "skip_expensive_diagnostics": false, "completion_candidate_cap": 200, "background_indexing": "full" }
  }
}
```

Notes:

- This payload uses **snake_case** for many nested fields (it is a direct `serde` encoding of `nova-memory` types).
- `rss_bytes` is best-effort process RSS (currently populated on Linux; otherwise omitted/`null`).
- Historical note: some older Nova builds accepted `nova/metrics` as an alias for this endpoint. That
  name is now used for request metrics; clients should always call `nova/memoryStatus`.

---

### `nova/memoryStatusChanged`

- **Kind:** notification
- **Stability:** experimental
- **Rust types:** same payload as `nova/memoryStatus`
- **Implemented in:** `crates/nova-lsp/src/main.rs`

#### Notification params

Same as the `nova/memoryStatus` response object.

---

### `nova/metrics`

Per-method runtime request metrics for debugging and bug reports.

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-metrics/src/lib.rs` (`MetricsSnapshot`)
- **Handler:** `crates/nova-lsp/src/lib.rs` (`METRICS_METHOD`)

#### Request params

No params are required; clients should send `{}` or omit params.

#### Response

```json
{
  "totals": {
    "requestCount": 12,
    "errorCount": 1,
    "timeoutCount": 0,
    "panicCount": 0,
    "latencyUs": { "p50Us": 120, "p95Us": 900, "maxUs": 1200 }
  },
  "methods": {
    "initialize": {
      "requestCount": 1,
      "errorCount": 0,
      "timeoutCount": 0,
      "panicCount": 0,
      "latencyUs": { "p50Us": 500, "p95Us": 500, "maxUs": 500 }
    }
  }
}
```

Notes:

- Latencies are reported in **microseconds** (`*_Us`).
- This endpoint is allowed in safe-mode.

---

### `nova/resetMetrics`

Reset the runtime metrics registry (useful when capturing a focused reproduction).

- **Kind:** request
- **Stability:** experimental
- **Handler:** `crates/nova-lsp/src/lib.rs` (`RESET_METRICS_METHOD`)

#### Request params

No params are required; clients should send `{}` or omit params.

#### Response

```json
{ "ok": true }
```

---

## Workspace / file operation notifications

### `nova/workspace/renamePath`

- **Kind:** notification
- **Stability:** experimental
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server)

Fallback notification for clients that cannot (or do not) send the standard LSP
`workspace/didRenameFiles` notification.

Clients should prefer `workspace/didRenameFiles` whenever possible; `nova-lsp` requests the standard
file-operation notifications via `initializeResult.capabilities.workspace.fileOperations`.

#### Notification params

```json
{
  "from": "file:///absolute/path/to/Old.java",
  "to": "file:///absolute/path/to/New.java"
}
```

Semantics:

- Updates Nova’s internal VFS/caches to treat `from` as renamed to `to`.
- Preserves Nova’s internal `FileId` for the file across the rename, so cached analysis state and
  semantic-search indexing can be updated in-place.
- Removes `from` from the semantic-search index and updates the semantic-search path key to `to`.
- If `to` is not currently open in the editor, Nova refreshes the new path from disk.
- If `to` is open, Nova treats the rename as a pure path move (the in-memory overlay remains the
  source of truth).

---

## Resilience endpoints

### `nova/bugReport`

- **Kind:** request
- **Stability:** stable (intended as the “escape hatch” when other requests are failing)
- **Rust types:** `crates/nova-lsp/src/hardening.rs` (`BugReportParams`), `crates/nova-bugreport/`
- **Handler:** `crates/nova-lsp/src/hardening.rs::handle_bug_report`

#### Request params

Params are optional; send `null` or omit params to accept defaults.

```json
{
  "maxLogLines": 500,
  "reproduction": "optional free-form text"
}
```

#### Response

```json
{ "path": "/tmp/nova-bugreport-…/", "archivePath": "/tmp/nova-bugreport-….zip" }
```

This is always available, even while the server is in safe-mode.

Notes:

- `archivePath` may be `null` if archive creation is disabled or fails. Nova will still emit the
  on-disk directory at `path`.

---

### `nova/safeModeStatus`

The VS Code extension calls this at startup to determine whether the server is currently in safe
mode (`editors/vscode/src/extension.ts`).

- **Kind:** request
- **Stability:** experimental
- **Implemented in:** `crates/nova-lsp/src/lib.rs` (`SAFE_MODE_STATUS_METHOD`)

#### Request params

No params are required; clients should send `{}` or omit params.

#### Response

```json
{ "schemaVersion": 1, "enabled": true, "reason": "panic" }
```

`reason` is optional and, if present, should be one of:

- `"panic"`
- `"watchdog_timeout"`

Compatibility note: older servers may return a bare boolean `true | false`.

#### Errors

- `-32603` for internal errors.

---

### `nova/safeModeChanged`

The VS Code extension registers this notification to update UI state when safe-mode changes
(`editors/vscode/src/extension.ts`).

- **Kind:** notification
- **Stability:** experimental
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server only)

#### Notification params

Same object as the `nova/safeModeStatus` response.

---

## Extension system endpoints (`nova-ext`)

### `nova/extensions/status`

- **Kind:** request
- **Stability:** experimental
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server)

This endpoint returns the current extension system configuration + runtime status: which extension
bundles were loaded, any load/register errors, and per-provider runtime stats.

#### Request params

```json
{
  "schemaVersion": 1
}
```

- `schemaVersion` is optional; when present, it must be `1`.
- Clients may also omit `params` or send `null`.

#### Response

```json
{
  "schemaVersion": 1,
  "enabled": true,
  "wasmPaths": ["/absolute/path/to/extensions"],
  "allow": null,
  "deny": [],
  "loadedExtensions": [],
  "loadErrors": [],
  "registerErrors": [],
  "stats": {
    "diagnostic": {},
    "completion": {},
    "codeAction": {},
    "navigation": {},
    "inlayHint": {}
  }
}
```

`loadedExtensions` entries:

```json
{
  "id": "com.example.my-extension",
  "version": "1.2.3",
  "dir": "/absolute/path/to/extension",
  "name": "My extension",
  "description": "optional",
  "authors": ["optional"],
  "homepage": "optional",
  "license": "optional",
  "abiVersion": 1,
  "capabilities": ["completion", "navigation"]
}
```

`stats.*` values are objects keyed by provider id, with values like:

```json
{
  "callsTotal": 0,
  "timeoutsTotal": 0,
  "panicsTotal": 0,
  "invalidResponsesTotal": 0,
  "skippedTotal": 0,
  "circuitOpenedTotal": 0,
  "consecutiveFailures": 0,
  "circuitOpen": false,
  "lastError": null,
  "lastDurationMs": null
}
```

#### Errors

- `-32602` for invalid params.
- `-32602` when `schemaVersion` is present but unsupported:
  `"unsupported schemaVersion <version> (expected 1)"`.

#### Safe-mode

This request is guarded by `nova_lsp::hardening::guard_method()` and fails with `-32603` while the
server is in safe-mode.

---

### `nova/extensions/navigation`

- **Kind:** request
- **Stability:** experimental
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server)

This endpoint invokes any registered extension navigation providers for a single document and
returns a list of navigation targets (usually within the same file).

#### Request params

```json
{
  "schemaVersion": 1,
  "textDocument": { "uri": "file:///absolute/path/to/Foo.java" }
}
```

- `schemaVersion` is optional; when present, it must be `1`.
- `textDocument.uri` is required.

#### Response

```json
{
  "schemaVersion": 1,
  "targets": [
    {
      "label": "My navigation target",
      "uri": "file:///absolute/path/to/Foo.java",
      "fileId": 1,
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 10 } },
      "span": { "start": 0, "end": 10 }
    }
  ]
}
```

Notes:

- `targets` may be empty.
- `range` follows standard LSP conventions (0-based line and UTF-16 `character` offsets) and may be
  `null` when the target does not include a span.
- `span` is a UTF-8 byte-offset range `{start,end}` into the document text and may be `null`.

#### Errors

- `-32602` for invalid params (e.g. missing `textDocument.uri`).
- `-32603` when `schemaVersion` is present but unsupported:
  `"unsupported schemaVersion <version> (expected 1)"`.

#### Safe-mode

This request is guarded by `nova_lsp::hardening::guard_method()` and fails with `-32603` while the
server is in safe-mode.

## Experimental / client-specific methods

### `nova/completion/more`

This is the “poll for async AI completions” endpoint used by the VS Code extension.

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/requests.rs` (`MoreCompletionsParams`, `MoreCompletionsResult`)
- **Client usage:** `editors/vscode/src/aiCompletionMore.ts`

Notes:

- The stdio server only spawns background AI completions when `ai.enabled = true` and
  `ai.features.multi_token_completion = true` in `nova.toml`, and multi-token completions are not
  disabled by server-side env var overrides (see below).
- `NOVA_AI_COMPLETIONS_MAX_ITEMS=<n>` overrides the server’s **AI multi-token completion max-items**
  setting (how many AI completion items may be surfaced for a single completion context).
  - `0` is treated as a **hard disable** of multi-token completions: the server does not spawn any
    background AI completion tasks and the initial LSP `CompletionList.isIncomplete` is `false`.
  - Values are clamped to a reasonable maximum (currently `32`).
  - Empty / invalid values are ignored (the server falls back to config/default behavior).
  - This override is read at process start; a server restart is required for changes to take effect.
  - This override does **not** enable multi-token completions by itself; it only caps (or disables)
    multi-token completions when they are otherwise enabled by `nova.toml` and not disabled by other
    env var overrides.
  - This only affects **async multi-token** AI completions; standard (non-AI) completions returned
    from `textDocument/completion` are unaffected.
  - When enabled, this value influences both:
    - how many suggestions the server asks the AI provider to generate, and
    - the final number of AI completion items returned (items are validated/deduped and then
      truncated to the max).
  - VS Code note: the Nova VS Code extension surfaces `nova.aiCompletions.maxItems` by setting this
    env var when starting `nova-lsp` and prompts for a server restart when it changes.
- Other server-side env var overrides that can disable AI completions entirely:
  - `NOVA_DISABLE_AI=1` disables all AI features.
  - `NOVA_DISABLE_AI_COMPLETIONS=1` disables multi-token completions.
- Clients should gate polling on `CompletionList.isIncomplete = true`; when multi-token completions
  are disabled, the server returns `isIncomplete = false` and `nova/completion/more` will return an
  empty result.

#### Request params (note: snake_case)

```json
{ "context_id": "123" }
```

#### Response (note: snake_case)

```json
{
  "items": [/* standard LSP CompletionItem objects */],
  "is_incomplete": false
}
```

Clients obtain `context_id` from the `data` field attached to completion items (best-effort). The
VS Code extension expects:

```json
{ "nova": { "completion_context_id": "123" } }
```

#### Errors

- Clients should treat `-32601` (method not found) **or** `-32602` (“unknown … method”) as “AI
  completions not supported”.

---

### `nova/refactor/changeSignature` (experimental)

- **Kind:** request
- **Stability:** experimental
- **Rust types:** request plan is `crates/nova-refactor/src/change_signature.rs` (`ChangeSignature`,
  `ParameterOperation`, `HierarchyPropagation`)
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server; see `nova_lsp::CHANGE_SIGNATURE_METHOD`)

This method name exists as a constant (`crates/nova-lsp/src/refactor.rs::CHANGE_SIGNATURE_METHOD`)
and is handled by the `nova-lsp` stdio server in a best-effort mode (it currently builds a sketch
index from open documents only).

The LSP layer also exposes a helper (`crates/nova-lsp/src/refactor.rs::change_signature_workspace_edit`)
for converting the refactoring engine output into an LSP `WorkspaceEdit` with correct UTF-16
positions.

#### Request params (note: snake_case)

```json
{
  "target": 42,
  "new_name": "renamedMethod",
  "parameters": [{ "Existing": { "old_index": 0, "new_name": "value", "new_type": null } }],
  "new_return_type": null,
  "new_throws": null,
  "propagate_hierarchy": "Both"
}
```

#### Response

The response is a standard LSP `WorkspaceEdit`.

Note: the refactoring engine produces Nova's canonical `nova_refactor::WorkspaceEdit` (byte-offset
text edits). The LSP layer should convert it using `nova_refactor::workspace_edit_to_lsp`, which
maps offsets to UTF-16 LSP positions.

Notes:

- Today, the stdio server builds a best-effort `nova-index::Index` from **open documents** only, so
  clients should ensure relevant files are opened/synchronized before calling this endpoint.

#### Errors

- `-32602` if params do not match the schema.
- `-32603` for refactoring conflicts or internal failures.

---

### `nova/refactor/moveMethod`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/refactor.rs` (`MoveMethodParams`), engine lives in
  `crates/nova-refactor/src/move_member.rs`
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server; see `nova_lsp::MOVE_METHOD_METHOD`)

Move an **instance method** from one Java class to another and update usages.

#### Request params

`MoveMethodParams` (camelCase):
`{ fromClass: string, methodName: string, toClass: string }`

```json
{
  "fromClass": "com.example.A",
  "methodName": "foo",
  "toClass": "com.example.B"
}
```

#### Response

The response is a standard LSP `WorkspaceEdit`.

Notes:

- The current stdio server implementation operates on an in-memory workspace built from **open
  documents only** (it calls `open_document_files(state)` before running the refactor). Clients must
  ensure the relevant files are opened/synchronized first.

#### Errors

- `-32602` if params do not match the schema, or if required files/symbols cannot be found in the
  open-document workspace snapshot.
- `-32603` for internal errors (safe-mode, watchdog timeout, panic, serialization failures).

---

### `nova/refactor/moveStaticMember`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/refactor.rs` (`MoveStaticMemberParams`), engine lives in
  `crates/nova-refactor/src/move_member.rs`
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server; see `nova_lsp::MOVE_STATIC_MEMBER_METHOD`)

Move a **static member** (method/field) from one Java class to another and update usages.

#### Request params

`MoveStaticMemberParams` (camelCase):
`{ fromClass: string, memberName: string, toClass: string }`

```json
{
  "fromClass": "com.example.A",
  "memberName": "CONST",
  "toClass": "com.example.B"
}
```

#### Response

The response is a standard LSP `WorkspaceEdit`.

Notes:

- The current stdio server implementation operates on an in-memory workspace built from **open
  documents only** (it calls `open_document_files(state)` before running the refactor). Clients must
  ensure the relevant files are opened/synchronized first.

#### Errors

- `-32602` if params do not match the schema, or if required files/symbols cannot be found in the
  open-document workspace snapshot.
- `-32603` for internal errors (safe-mode, watchdog timeout, panic, serialization failures).

---

### `nova/refactor/safeDelete`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/refactor.rs` (`SafeDeleteParams`, `SafeDeleteResult`, `SafeDeleteTargetParam`)
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server; see `nova_lsp::SAFE_DELETE_METHOD`)

This endpoint runs Safe Delete against a target symbol. In `"safe"` mode, the server may return a
preview payload when usages exist. In `"deleteAnyway"` mode, the server applies the deletion
regardless.

#### Request params

```json
{
  "target": 123,
  "mode": "safe"
}
```

- `mode` is one of: `"safe" | "deleteAnyway"`.
- `target` may be either:
  - a raw symbol id (JSON number), or
  - a tagged object: `{ "type": "symbol", "id": 123 }`.

#### Response

The response is **either**:

1) A preview payload (custom tagged object):

```json
{
  "type": "nova/refactor/preview",
  "report": { /* see nova_refactor::SafeDeleteReport */ }
}
```

2) A standard LSP `WorkspaceEdit` object (when the delete is applied).

Notes:

- Today, the stdio server builds a best-effort `nova-index::Index` from **open documents** only, so
  clients should ensure relevant files are opened/synchronized before calling this endpoint.
- `nova_refactor::SafeDeleteReport` stores ranges as `nova_index::TextRange` values:
  `start`/`end` are **byte offsets** into the UTF-8 source file (not LSP UTF-16 positions).
- The stdio server also exposes Safe Delete via `workspace/executeCommand` (`nova.safeDelete`),
  using the same argument shape as `SafeDeleteParams` and returning the same `SafeDeleteResult`.
  When the command returns a `WorkspaceEdit`, the server also sends a `workspace/applyEdit`
  request (label: `"Safe delete"`) to apply it immediately.
- `textDocument/codeAction` may return a Safe Delete code action with:
  - an inline `edit` (when Safe Delete is immediately applicable), or
  - `data` containing a `nova/refactor/preview` payload and a `command` (`nova.safeDelete`) that
    re-runs Safe Delete. Clients can show a preview using `data.report`, then confirm by calling
    `nova/refactor/safeDelete` (or `nova.safeDelete`) with `mode: "deleteAnyway"`.

#### Errors

- `-32602` for invalid params / missing target.
- `-32603` for internal errors while computing or converting the edit.

---

### `nova/java/organizeImports`

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/main.rs` (`JavaOrganizeImportsRequestParams`, `JavaOrganizeImportsResponse`)
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server; see `nova_lsp::JAVA_ORGANIZE_IMPORTS_METHOD`)

This endpoint is used by the VS Code extension to organize imports in the active document.

#### Request params

```json
{ "uri": "file:///absolute/path/to/Foo.java" }
```

#### Response

```json
{
  "applied": true,
  "edit": { /* standard LSP WorkspaceEdit */ }
}
```

If no edits are needed, the server returns:

```json
{ "applied": false }
```

#### Side effects

When `applied` is `true`, the server also sends a `workspace/applyEdit` request to the client to
apply the edit immediately (label: `"Organize imports"`). Clients should support `workspace/applyEdit`
to use this endpoint reliably.

#### Notes

- Prefer the standard LSP code action kind `source.organizeImports` when possible; `nova-lsp` also
  implements it via `textDocument/codeAction` (see `crates/nova-lsp/src/main.rs::organize_imports_code_action`).

#### Errors

- `-32602` for invalid params / unknown document.
- `-32603` for internal errors (refactoring engine failures, serialization).

---

### AI code actions via `workspace/executeCommand`

Nova’s AI code actions are surfaced to clients as standard LSP `workspace/executeCommand` commands
(emitted by `textDocument/codeAction` in `crates/nova-lsp/src/main.rs`; argument types are defined in
`crates/nova-ide/src/ai.rs`).

The server advertises these command IDs via the standard LSP capability:
`initializeResult.capabilities.executeCommandProvider.commands`.

These commands include:

- **Explain-only actions** (returning text)
- **Patch-based code edits** (applied via `workspace/applyEdit`)

When patch edits are allowed by privacy policy, the server **sends** a `workspace/applyEdit` request
containing a `WorkspaceEdit` (similar to Safe Delete / Organize Imports). Explain-only actions do
not apply edits and instead return a JSON string result.

Like other AI endpoints, these commands accept an optional `workDoneToken` (standard LSP work-done
progress token). When present, the server emits `$/progress` notifications for user-visible progress.

Clients must support `workspace/applyEdit` to use these commands reliably: the `workspace/executeCommand`
response does **not** return the edit payload.

Compatibility note: older Nova builds may return a JSON string result (a generated snippet) instead
of applying an edit via `workspace/applyEdit`. Clients should gracefully handle both behaviors when
targeting multiple Nova versions.

#### `nova.ai.explainError`

- **Kind:** `workspace/executeCommand` command
- **Rust types:** `crates/nova-ide/src/ai.rs` (`ExplainErrorArgs`)

##### ExecuteCommand params

The first (and only) entry in `arguments` is an `ExplainErrorArgs` object:

```json
{
  "command": "nova.ai.explainError",
  "arguments": [
    {
      "diagnostic_message": "cannot find symbol",
      "code": "optional snippet",
      "uri": "file:///…",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 10 } }
    }
  ],
  "workDoneToken": "optional"
}
```

Note: the argument object uses **snake_case** field names (e.g. `diagnostic_message`) because the
Rust type does not use `rename_all = "camelCase"`.

##### Response

JSON string (the explanation).

##### Errors

- `-32600` if AI is not configured.
- `-32603` for model/provider failures.

##### Notes

- This is an explain-only action; it does not apply edits.
- When the target file is blocked by `ai.privacy.excluded_paths`, the server still accepts the
  request but omits file-backed code context from the prompt (it will ignore any client-supplied
  `code` snippet).

#### `nova.ai.generateMethodBody`

- **Kind:** `workspace/executeCommand` command
- **Rust types:** `crates/nova-ide/src/ai.rs` (`GenerateMethodBodyArgs`)

##### ExecuteCommand params

The first (and only) entry in `arguments` is a `GenerateMethodBodyArgs` object:

```json
{
  "command": "nova.ai.generateMethodBody",
  "arguments": [
    {
      "method_signature": "public int add(int a, int b)",
      "context": "optional surrounding code",
      "uri": "file:///…",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 10 } }
    }
  ],
  "workDoneToken": "optional"
}
```

Note: the argument object uses **snake_case** field names (e.g. `method_signature`) because the Rust
type does not use `rename_all = "camelCase"`.

`GenerateMethodBodyArgs` fields:

- `method_signature` (string, required) — method signature including modifiers/return type/name.
- `context` (string, optional) — best-effort surrounding context (class/members/etc).
- `uri` (string, required) — document URI (typically a `file://` URI).
- `range` (object, required) — best-effort range covering the selected snippet (0-based line and
  UTF-16 `character` offsets), matching LSP conventions:
  `{ start: { line, character }, end: { line, character } }`.

Range semantics (server-enforced):

- The server expects `range` to include both `{` and `}` of the target method.
- The selected method body must be empty; otherwise the server rejects the request with `-32602`
  (message: “selected method body is not empty; select an empty method”).

##### Response

`null` (JSON-RPC result `null`). The server applies the edit via `workspace/applyEdit` as a side
effect (see below).

##### Side effects

The server sends a `workspace/applyEdit` request (label: `"AI: Generate method body"`) containing a
standard LSP `WorkspaceEdit`.

##### Errors

- `-32600` if AI is not configured, or if the target file is blocked by `ai.privacy.excluded_paths`.
- `-32602` for invalid params (e.g. missing `uri`/`range`).
- `-32603` for internal failures (model/provider errors, patch parsing/validation failures) **or**
  when blocked by privacy policy (cloud code-edit policy enforcement).
- `-32800` if the request is cancelled.

##### Privacy gating (code edits)

Patch-based AI code edits are gated by `ai.privacy`:

- **Local-only mode**: `ai.privacy.local_only=true` allows patch edits.
- **Cloud mode**: `ai.privacy.local_only=false` requires **all** of:
  - `ai.privacy.allow_cloud_code_edits=true`
  - `ai.privacy.allow_code_edits_without_anonymization=true`
  - `ai.privacy.anonymize_identifiers=false`

If these conditions are not met, the server will not apply patch edits.

---

#### `nova.ai.generateTests`

- **Kind:** `workspace/executeCommand` command
- **Rust types:** `crates/nova-ide/src/ai.rs` (`GenerateTestsArgs`)

##### ExecuteCommand params

The first (and only) entry in `arguments` is a `GenerateTestsArgs` object:

```json
{
  "command": "nova.ai.generateTests",
  "arguments": [
    {
      "target": "public int add(int a, int b)",
      "context": "optional surrounding code",
      "uri": "file:///…",
      "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 10 } }
    }
  ],
  "workDoneToken": "optional"
}
```

`GenerateTestsArgs` fields:

- `target` (string, required) — description of the test target (method or class signature).
- `context` (string, optional) — best-effort surrounding context.
- `uri` (string, required) — document URI (typically a `file://` URI).
- `range` (object, required) — best-effort range covering the selected snippet (0-based line and
  UTF-16 `character` offsets), matching LSP conventions:
  `{ start: { line, character }, end: { line, character } }`.

Notes:

- The server attempts (best-effort) to generate/update a test file under `src/test/java/` based on
  the selected source file’s package and class name.
- If the server cannot derive a test file path, it falls back to inserting the generated tests into
  the current file at `range`.

##### Response

`null` (JSON-RPC result `null`). The server applies the edit via `workspace/applyEdit` as a side
effect (see below).

##### Side effects

The server sends a `workspace/applyEdit` request (label: `"AI: Generate tests"`) containing a
standard LSP `WorkspaceEdit`. This edit may include creating or updating a test file (best-effort:
under `src/test/java/`).

##### Errors

Same as `nova.ai.generateMethodBody`.

---

## Internal (debug/test-only)

### `nova/internal/interruptibleWork`

- **Kind:** request
- **Stability:** internal (debug-only)
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server; debug builds only)

This request is used by `crates/nova-lsp/tests/suite/salsa_cancellation.rs` to validate that
`$/cancelRequest` triggers Salsa cancellation.

#### Request params

```json
{ "steps": 1000 }
```

- `steps` (u32, required) — number of loop iterations to perform.

#### Response

```json
{ "value": 0 }
```

#### Errors

- `-32602` for invalid params.
- `-32800` when cancelled.

#### Notes

- When the handler begins, the server emits a `nova/internal/interruptibleWorkStarted` notification
  containing the request id.
- Release builds do not implement this endpoint.

---

### `nova/internal/interruptibleWorkStarted`

- **Kind:** notification
- **Stability:** internal (debug-only)
- **Implemented in:** `crates/nova-lsp/src/main.rs` (stdio server; debug builds only)

Emitted when the server begins handling `nova/internal/interruptibleWork`. This allows tests to
send `$/cancelRequest` after the request has entered the handler.

#### Notification params

```json
{ "id": 2 }
```

- `id` is the JSON-RPC request id for the in-flight `nova/internal/interruptibleWork` request.

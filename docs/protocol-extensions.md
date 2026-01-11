# Protocol extensions (`nova/*` LSP methods)

Nova extends LSP with a small set of **custom JSON-RPC methods** under the `nova/*` namespace. This
document is the stable reference for those methods so editor clients do **not** need to read Rust
code to interoperate.

Source of truth for method names:

- `crates/nova-lsp/src/lib.rs` (string constants like `TEST_DISCOVER_METHOD`)
- `editors/vscode/src/*.ts` (client usage for a subset)

> Note: Nova also uses standard LSP requests (e.g. `textDocument/formatting`) and standard command /
> code action wiring. Those are intentionally *not* covered here.

## Capability gating (how clients detect support)

Today, `nova-lsp` does **not** advertise a structured list of custom methods in
`initializeResult.capabilities.experimental`. As a result, clients should gate features using one
or more of:

1. **Optimistic call + graceful fallback**: send the request and treat JSON-RPC `-32601` “Method
   not found” **or** `-32602` with an “unknown … method” message as “server doesn’t support this
   extension”. (The current `nova-lsp` stdio server routes all `nova/*` requests through a single
   dispatcher, so unsupported `nova/*` methods often show up as `-32602`.)
2. **Version gating**: use `initializeResult.serverInfo` (`name`/`version`) and require a minimum
   Nova version for features that are known to exist after a cutoff.
3. **Schema gating**: for endpoints that return `schemaVersion`, clients must validate it and
   reject unknown major versions.

The VS Code extension uses (1) for `nova/completion/more` (see
`editors/vscode/src/aiCompletionMore.ts`).

## Common error behavior (timeouts, safe-mode, cancellation)

### JSON-RPC error codes

Nova uses standard JSON-RPC/LSP error codes:

- `-32601` — method not found (treat as “unsupported extension”)
- `-32602` — invalid params (schema mismatch). Note: the current `nova-lsp` stdio server also
  returns `-32602` for **unknown `nova/*` methods** (because it attempts to dispatch all `nova/*`
  through `nova_lsp::handle_custom_request()`).
- `-32603` — internal error

### Watchdog timeouts + safe-mode

Most `nova/*` requests dispatched through `nova_lsp::handle_custom_request()` are wrapped in a
watchdog (see `crates/nova-lsp/src/hardening.rs`):

- If the handler **exceeds its per-method time budget**, the request fails with `-32603`.
- If the handler **panics**, the request fails with `-32603`.
- Some watchdog failures may temporarily put the server into **safe-mode**.

When in safe-mode, **all methods dispatched through** `nova_lsp::handle_custom_request()` **except**
`nova/bugReport`, `nova/metrics`, and `nova/resetMetrics` fail with `-32603` and a message like:

> “Nova is running in safe-mode … Only `nova/bugReport`, `nova/metrics`, and `nova/resetMetrics` are available for now.”

Note: safe-mode enforcement is currently implemented by `nova_lsp::hardening::guard_method()` and
is typically enforced by `nova_lsp::handle_custom_request()`, but endpoints handled directly by the
stdio server must call it explicitly. Some endpoints (e.g. `nova/memoryStatus`,
`nova/java/organizeImports`) currently bypass that guard and may still succeed during safe-mode.

Safe-mode windows:

- Panic: ~60s
- Watchdog timeout (selected methods): ~30s

### Cancellation

Nova’s watchdog has a cancellation mechanism (via `nova-scheduler`), but most current handlers are
synchronous and **do not yet poll cancellation tokens**. Clients should treat cancellation as
best-effort:

- If the server honours cancellation, the request may fail with `-32603` and a message like
  “`{method}` was cancelled”.
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
  "projectPath": null
}
```

Notes:

- `projectRoot` also accepts the legacy alias `root`.
- `buildTool` also accepts the legacy alias `kind`.
- For Maven multi-module projects, `module` is a path relative to `projectRoot`.
- For Gradle, `projectPath` is the Gradle path (e.g. `":app"`).

#### Response

```json
{
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
{ "status": "idle" }
```

Status values are `snake_case`: `"idle" | "building" | "failed"`.

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
  "target": null,
  "diagnostics": [],
  "source": null
}
```

Notes:

- For Bazel projects the endpoint returns diagnostics sourced via BSP when configured (see `NOVA_BSP_PROGRAM`).

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
    { "path": "/api/hello", "methods": ["GET"], "file": "src/main/java/com/example/Hello.java", "line": 42 }
  ]
}
```

Notes:

- `line` is **1-based** (matches `nova-framework-web`).
- `file` is a best-effort relative path when `projectRoot` is provided.

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

`span.start` / `span.end` are **byte offsets** into the UTF-8 source file.

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
  "host": "127.0.0.1",
  "port": 5005
}
```

Notes:

- `changedFiles` entries may be absolute or relative paths; relative paths are resolved against `projectRoot`.
- `host` is optional; default is `127.0.0.1`.

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

All AI requests accept an optional `workDoneToken` (standard LSP work-done progress token). When
present, the server emits `$/progress` notifications for user-visible progress.

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

#### Response

JSON string (the generated method body snippet).

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

#### Response

JSON string (the generated tests snippet).

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
    "pressure": "low",
    "degraded": { "skip_expensive_diagnostics": false, "completion_candidate_cap": 200, "background_indexing": "full" }
  }
}
```

Notes:

- This payload uses **snake_case** for many nested fields (it is a direct `serde` encoding of `nova-memory` types).
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
{
  "path": "/tmp/nova-bugreport-…/",
  "archivePath": "/tmp/nova-bugreport-….zip"
}
```

This is always available, even while the server is in safe-mode.

Notes:

- `archivePath` may be `null` if archive creation is disabled or fails (Nova will still emit the on-disk directory at `path`).

---

### `nova/safeModeStatus` (currently not implemented)

The VS Code extension attempts to call this at startup to determine whether the server is currently
in safe-mode (`editors/vscode/src/extension.ts`). The shipped `nova-lsp` server does **not**
implement it yet; clients should infer safe-mode by observing the `-32603` safe-mode error message.

- **Kind:** request
- **Stability:** experimental

#### Request params

No params are required; clients should send `{}` or omit params.

#### Response (proposed)

```json
{ "schemaVersion": 1, "enabled": true, "reason": "panic" }
```

`reason` is optional and, if present, should be one of:

- `"panic"`
- `"watchdog_timeout"`

Compatibility note: clients may encounter older servers that return a bare boolean `true | false`.

#### Errors

- `-32601` or `-32602` if the server does not support this endpoint.
- `-32603` for internal errors.

---

### `nova/safeModeChanged` (currently not implemented)

The VS Code extension registers this notification to update UI state when safe-mode changes
(`editors/vscode/src/extension.ts`). The shipped `nova-lsp` server does **not** emit it yet.

- **Kind:** notification
- **Stability:** experimental

#### Notification params (proposed)

Same object as the `nova/safeModeStatus` response.

---

## Experimental / client-specific methods

### `nova/completion/more`

This is the “poll for async AI completions” endpoint used by the VS Code extension.

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/requests.rs` (`MoreCompletionsParams`, `MoreCompletionsResult`)
- **Client usage:** `editors/vscode/src/aiCompletionMore.ts`

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

### `nova/refactor/moveMethod` (reserved; not implemented)

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/refactor.rs` (`MoveMethodParams`), engine lives in
  `crates/nova-refactor/src/move_member.rs`

#### Request params

```json
{
  "fromClass": "com.example.A",
  "methodName": "foo",
  "toClass": "com.example.B"
}
```

#### Response

If/when implemented, the expected response is a standard LSP `WorkspaceEdit`.

#### Errors

- Today: the request fails with `-32602` (“unknown (stateless) method: nova/refactor/moveMethod”).

---

### `nova/refactor/moveStaticMember` (reserved; not implemented)

- **Kind:** request
- **Stability:** experimental
- **Rust types:** `crates/nova-lsp/src/refactor.rs` (`MoveStaticMemberParams`), engine lives in
  `crates/nova-refactor/src/move_member.rs`

#### Request params

```json
{
  "fromClass": "com.example.A",
  "memberName": "CONST",
  "toClass": "com.example.B"
}
```

#### Response

If/when implemented, the expected response is a standard LSP `WorkspaceEdit`.

#### Errors

- Today: the request fails with `-32602` (“unknown (stateless) method: nova/refactor/moveStaticMember”).

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

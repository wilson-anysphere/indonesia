# Gradle build integration (`nova-build` ↔ `nova-project`)

Nova supports Gradle workspaces through **two complementary paths**:

- **Heuristic discovery (`nova-project`)**: fast, zero-build-tool startup that infers a reasonable
  project model by parsing Gradle files + (best-effort) locating jars in the local Gradle cache.
- **Build-tool execution (`nova-build`)**: explicit Gradle invocation to extract *resolved* Java
  compilation inputs (source roots, output dirs, classpaths, language level) directly from Gradle.

The bridge between the two is a **workspace-local snapshot file** written by `nova-build` and
consumed by `nova-project`:

> `.nova/queries/gradle.json`

This file is intentionally *not* a global cache: it lives in the workspace so it can be updated by
running Gradle in that workspace, then read later without invoking Gradle again.

The relative on-disk path and glob are defined in `nova-build-model`:

- `nova_build_model::GRADLE_SNAPSHOT_REL_PATH`
- `nova_build_model::GRADLE_SNAPSHOT_GLOB`

---

## Typical end-to-end flow (discovery → classpath → reload)

1. **Open workspace**: `nova-project` loads a Gradle project model heuristically (fast, no Gradle
   invocation).
2. **Build integration runs** (explicitly, or via a host opting in): `nova-build` executes Gradle to
   extract a resolved `JavaCompileConfig` (classpath/source roots/output dirs).
3. **Snapshot handoff**: `nova-build` writes `.nova/queries/gradle.json` (schema + fingerprint).
4. **Reload trigger**: file watching / reload logic treats `.nova/queries/gradle.json` as a build
   change and reloads the project model.
5. **Snapshot consumption**: `nova-project` validates `schemaVersion` and `buildFingerprint`, then
   uses the snapshot to populate classpath/source roots/output dirs more accurately than heuristics.

This pattern keeps “open folder” cheap while still allowing richer, build-tool-derived metadata to
be reused without re-running Gradle on every reload.

---

## Why both heuristic parsing and build-tool execution?

Running Gradle is the only reliable way to obtain:

- the **resolved compile/test classpath** (including transitive dependencies),
- Gradle’s **variant/attribute selection** results,
- accurate **`sourceSets` roots** and **output directories**,
- Java language levels implied by toolchains / compiler args.

But Gradle execution is also:

- relatively expensive (process startup + configuration phase),
- sensitive to the developer environment (JDK, Gradle distribution, network/caches),
- something Nova generally avoids doing implicitly during “open folder”.

So Nova starts with a best-effort model (`nova-project`) and can be **upgraded** by running the
build tool (`nova-build`) and persisting the results for later reloads.

---

## Snapshot contract: `.nova/queries/gradle.json`

### Writer

`nova-build` writes the snapshot as a **best-effort side effect** of Gradle queries, primarily:

- `GradleBuild::projects(...)` populates `projects` (Gradle project path → `projectDir`)
- `GradleBuild::java_compile_config(..., project_path=Some(":app"), ...)` populates
  `javaCompileConfigs[":app"]` with resolved compilation inputs
- `GradleBuild::java_compile_configs_all(...)` populates `projects` *and* `javaCompileConfigs` for
  all subprojects in a **single** Gradle invocation (via `printNovaAllJavaCompileConfigs`)

Under the hood, `nova-build`:

- writes a temporary Gradle **init script** that registers Nova helper tasks (see
  `crates/nova-build/src/gradle.rs:write_init_script`):
  - `printNovaJavaCompileConfig`
  - `printNovaAllJavaCompileConfigs`
  - `printNovaProjects`
  - `printNovaAnnotationProcessing`
- runs Gradle with `--init-script <temp>` (plus `--no-daemon --console=plain -q`) and parses JSON
  blocks printed between sentinel markers like `NOVA_JSON_BEGIN` / `NOVA_JSON_END`.
- selects the Gradle executable in a wrapper-first way:
  - prefers `./gradlew` / `gradlew.bat` when present (`GradleConfig.prefer_wrapper = true`)
  - falls back to invoking `gradle` (or a configured path) when no wrapper is present
  - on Unix, will run `sh ./gradlew` if `gradlew` exists but is not marked executable (common in
    Windows-originated checkouts)

Implementation: `crates/nova-build/src/gradle.rs` (`update_gradle_snapshot*` helpers).

Schema definition (shared by writer + reader): `crates/nova-build-model/src/gradle_snapshot.rs`
(`GradleSnapshotFile`, `GRADLE_SNAPSHOT_SCHEMA_VERSION`, `GRADLE_SNAPSHOT_REL_PATH`).

The snapshot is written **atomically** (write to a unique temp file in the same directory, then
rename over the destination) to avoid leaving partially-written JSON on disk.

Note: `nova-build` also caches many Gradle query results in its own build cache directory. When a
query is served purely from cache (no Gradle process spawned), the snapshot may not be updated.

### Reader

`nova-project` attempts to load the snapshot during Gradle project discovery. If it can’t load or
validate it, it falls back to heuristic parsing.

Implementation: `crates/nova-project/src/gradle.rs` (`load_gradle_snapshot`).

### Schema versioning

The snapshot is versioned by a top-level `schemaVersion` field.

To keep `nova-build` and `nova-project` in sync, the schema types/constants live in `nova-build-model`:

- `crates/nova-build-model/src/gradle_snapshot.rs`:
  - `GRADLE_SNAPSHOT_SCHEMA_VERSION`
  - `GRADLE_SNAPSHOT_REL_PATH`

Both sides import these from `nova_build_model`:

- writer: `crates/nova-build/src/gradle.rs` (`update_gradle_snapshot*`)
- reader: `crates/nova-project/src/gradle.rs` (`load_gradle_snapshot`)

When the schema changes, bump the constant in `nova-build-model` and update the shared serde structs
in the same module. Both `nova-build` (writer) and `nova-project` (reader) consume the shared
definition.

Compatibility policy is intentionally simple:

- if the schema version doesn’t match, treat the snapshot as **absent** (no migration/upgrade).
- `nova-build` will also reset/overwrite the snapshot when it detects a schema mismatch.

### Validation: `buildFingerprint`

The snapshot includes a `buildFingerprint` (hex SHA-256) so `nova-project` can reject stale output.

`nova-project` loads the snapshot only when:

- `schemaVersion` matches, **and**
- `buildFingerprint` matches the current fingerprint computed from Gradle build inputs.

If validation fails, the snapshot is ignored and Nova falls back to heuristics.

`nova-build` also treats a fingerprint mismatch as “stale snapshot”: it will reset the in-memory
snapshot object and write a fresh file for the new fingerprint the next time it updates any fields.

### Build fingerprint inputs (shared)

`buildFingerprint` is computed by hashing the **relative path** and **file contents** of a set of
Gradle build inputs (with `NUL` separators). The file set is discovered by walking the workspace
and **skipping** these directories:

- `.git/`
- `.gradle/`
- `build/`
- `bazel-*/` (only when a top-level entry under the workspace root, e.g. `bazel-out/`)
- `node_modules/`
- `target/`
- `.nova/`
- `.idea/`

Included inputs (current implementation, shared via `nova-build-model`):

- `build.gradle*` (e.g. `build.gradle`, `build.gradle.kts`)
- `settings.gradle*` (e.g. `settings.gradle`, `settings.gradle.kts`)
- any `*.gradle` / `*.gradle.kts` file (script plugins like `apply from: "deps.gradle"`)
- `gradle.properties`
- Gradle dependency lockfiles:
  - `gradle.lockfile` (at any depth)
  - `*.lockfile` under any `dependency-locks/` directory (e.g. `gradle/dependency-locks/compileClasspath.lockfile`)
- Gradle version catalogs:
  - `libs.versions.toml` (at any depth; commonly `gradle/libs.versions.toml`, but some builds reference a root-level catalog)
  - custom catalogs under `gradle/*.versions.toml` (e.g. `gradle/foo.versions.toml`)
- `gradlew` / `gradlew.bat` (only when located at the workspace root)
- `gradle/wrapper/gradle-wrapper.properties`
- `gradle/wrapper/gradle-wrapper.jar`

Implementation references:

- canonical (shared by writer + reader): `crates/nova-build-model/src/build_files.rs`
  (`collect_gradle_build_files`, `BuildFileFingerprint`)
- writer entry point: `crates/nova-build/src/gradle.rs` (`gradle_build_fingerprint`)
- reader entry point: `crates/nova-project/src/gradle.rs` (`gradle_build_fingerprint`)

Because the fingerprinting logic is shared via `nova-build-model`, `nova-build` (writer) and
`nova-project` (reader) stay aligned by construction. If you change what counts as a “Gradle build
input”, existing snapshots will be treated as stale until they are regenerated.

If you change what counts as a “Gradle build input”, also consider whether file watching should
treat the same paths as **build changes** (so the workspace reloads when those inputs change). In
practice this may involve updating:

- `crates/nova-workspace/src/watch.rs:is_build_file` (build vs source change categorization)
- `crates/nova-project/src/discover.rs:is_build_file` (reload triggers when callers provide changed
  files)

### Data model (schema v1)

Top-level fields:

- `schemaVersion`: integer
- `buildFingerprint`: string (hex digest)
- `projects`: `[{ path, projectDir }]`
- `javaCompileConfigs`: object mapping `":projectPath"` → compile config

Each `javaCompileConfigs[":path"]` contains:

- `projectDir`
- `compileClasspath`, `testClasspath`, `modulePath`
- `mainSourceRoots`, `testSourceRoots`
- `mainOutputDir`, `testOutputDir`
- `source`, `target`, `release`, `enablePreview`

`projectDir` and other paths may be absolute or workspace-relative; `nova-project` resolves relative
paths against the workspace root.

This snapshot currently covers **only** the minimal project-directory mapping + Java compile
configuration needed for discovery/classpath construction. Other build-tool outputs (e.g. build
diagnostics, annotation processing details) are tracked separately (typically via `nova-build`’s
cache/orchestrator APIs) and are **not** part of this file yet.

### Special case: Gradle `buildSrc/`

Gradle treats `buildSrc/` as a special build: it is automatically compiled and put on the
buildscript classpath, but it is **not** a normal subproject and does **not** appear in
`settings.gradle(.kts)`.

To make `buildSrc` sources navigable without invoking Gradle, `nova-project` includes `buildSrc/` as
an additional module when it exists and contains Java sources under conventional layouts
(e.g. `buildSrc/src/main/java`).

Nova represents `buildSrc` using a stable synthetic Gradle project path:

- project path: `:__buildSrc` (chosen to avoid collisions with real Gradle project paths)
- module id: `gradle::__buildSrc`
- module root: `<workspace>/buildSrc`

This synthetic path may also appear in `.nova/queries/gradle.json`:

- if `javaCompileConfigs[":__buildSrc"]` exists, `nova-project` consumes it (source roots/output
  dirs/classpaths) just like any other Gradle module,
- otherwise, `nova-project` falls back to heuristics for `buildSrc` (e.g. `src/*/java` source roots,
  `build/classes/java/{main,test}` output dirs).

Example (abridged):

```json
{
  "schemaVersion": 1,
  "buildFingerprint": "0123abcd...",
  "projects": [
    { "path": ":", "projectDir": "/path/to/workspace" },
    { "path": ":app", "projectDir": "/path/to/workspace/app" }
  ],
  "javaCompileConfigs": {
    ":app": {
      "projectDir": "/path/to/workspace/app",
      "compileClasspath": [
        "/path/to/workspace/app/build/classes/java/main",
        "/home/me/.gradle/caches/.../some-dep.jar"
      ],
      "testClasspath": [
        "/path/to/workspace/app/build/classes/java/test",
        "/path/to/workspace/app/build/classes/java/main"
      ],
      "modulePath": [],
      "mainSourceRoots": ["/path/to/workspace/app/src/main/java"],
      "testSourceRoots": ["/path/to/workspace/app/src/test/java"],
      "mainOutputDir": "/path/to/workspace/app/build/classes/java/main",
      "testOutputDir": "/path/to/workspace/app/build/classes/java/test",
      "source": "17",
      "target": "17",
      "release": "21",
      "enablePreview": false
    }
  }
}
```

---

## Heuristic mode vs snapshot mode (`nova-project`)

### Heuristic mode: what you get (and don’t)

Without a snapshot, `nova-project`:

- parses `settings.gradle(.kts)` to infer modules,
- uses conventional source/output layouts (plus generated roots from `nova.toml` / APT snapshots),
- extracts some dependency coordinates via regex and tries to locate jars in the local Gradle cache,
  but **does not** run Gradle.

Known limitations (by design):

- no transitive dependency resolution,
- no Gradle variant/attribute selection,
- no plugin-applied dependency injection (BOMs, dependency substitution, platform constraints, etc),
- jar lookup usually requires an explicit version *and* that jar already exists in the local cache.

### Snapshot mode: what becomes available

With a valid `.nova/queries/gradle.json`, `nova-project` can use:

- resolved compile/test **classpath entries** from Gradle (including transitive deps),
- `sourceSets`-derived **source roots** and **output dirs**,
- project directory mappings for Gradle subprojects (`:path` → `projectDir`),
- Java level and preview flags derived from Gradle config/toolchains.

If the snapshot is **partial** (e.g. missing `javaCompileConfigs` for some subprojects), those
subprojects fall back to heuristic defaults, but any available `projects` mapping can still improve
module root resolution.

---

## Gradle cache lookup configuration (heuristic mode)

Heuristic jar resolution uses the local Gradle cache under `gradle_user_home`:

- programmatic: `nova_project::LoadOptions.gradle_user_home`
- environment: `GRADLE_USER_HOME`
- fallback: `$HOME/.gradle` (or `%USERPROFILE%\\.gradle` on Windows when `HOME` is unset)

See:

- `crates/nova-project/src/discover.rs` (`LoadOptions`)
- `crates/nova-project/src/gradle.rs` (`default_gradle_user_home`)

---

## Reload behavior

`nova-project` reads `.nova/queries/gradle.json` **only on project load/reload**.

After `nova-build` updates the snapshot, callers must trigger a project reload to pick up the new
classpath/source roots (e.g. by restarting, or by having file watching treat
`.nova/queries/gradle.json` as a “build change” that invalidates the current project model).

Today this is already wired in the reload heuristics:

- `nova-project`: `crates/nova-project/src/discover.rs:is_build_file` treats `.nova/queries/gradle.json`
  as a Gradle “build file” so `reload_project()` reloads configuration when it changes.
- `nova-workspace`: `crates/nova-workspace/src/watch.rs:is_build_file` classifies the snapshot as a
  build change so watchers can trigger a reload when it updates.

---

## How the snapshot is generated in practice

The snapshot is written as a side effect of *some* Gradle queries (not every Gradle invocation).
In particular:

- `GradleBuild::projects(...)` updates the `projects` mapping, and
- per-project / batch compile-config extraction updates `javaCompileConfigs`.

Note: calling `BuildManager::java_compile_config_gradle(..., project_path=None)` for a **single**
project build currently executes Gradle but does **not** necessarily write the snapshot (unless it
uses the batch multi-project path internally).

Common call sites include:

- **LSP build/classpath endpoints**: `crates/nova-lsp/src/extensions/build.rs`
  - `handle_java_classpath` (classpath + source roots + language level) calls
    `BuildManager::java_compile_config_gradle`, which executes Gradle and may write/update the
    snapshot.
  - `handle_build_project` triggers Gradle compilation tasks (`compileJava` / `compileTestJava`),
    which may also execute Gradle (but this path is about diagnostics/build, not snapshot reading).
- **Workspace reload build integration**: `crates/nova-workspace/src/engine.rs` reloads the project
  and may call `BuildManager::java_compile_config_gradle(workspace_root, None)` to refresh the
  workspace classpath during reloads.

### Timeouts and execution budgets

Gradle execution is time-bounded by whichever `CommandRunner` is in use:

- `nova-lsp` typically uses a request-scoped time budget (e.g. 60s for `handle_java_classpath`) via
  `DeadlineCommandRunner` in `crates/nova-lsp/src/extensions/mod.rs`.
- `nova-workspace` uses its configured build runner (`WorkspaceEngineConfig.build_runner`); in
  non-test builds this defaults to `nova_build::DefaultCommandRunner` (15 minute timeout).

---

## Troubleshooting

### Snapshot exists but is ignored by `nova-project`

`nova-project` will ignore `.nova/queries/gradle.json` when:

- `schemaVersion` doesn’t match `nova_build_model::GRADLE_SNAPSHOT_SCHEMA_VERSION`, or
- `buildFingerprint` doesn’t match the current fingerprint of Gradle build inputs, or
- the JSON can’t be parsed.

In all of these cases, Nova falls back to heuristic discovery.

Quick recovery steps:

- delete `.nova/queries/gradle.json` and reload (forces heuristic mode until the next successful
  snapshot write), or
- re-run Gradle extraction via `nova-build` to regenerate a fresh snapshot.

### Snapshot never appears on disk

Snapshot writing is **best-effort**. Call sites intentionally ignore I/O errors when writing
`.nova/queries/gradle.json`.

If the file is missing after running Gradle integration:

- ensure the workspace is writable (Nova needs to create `.nova/queries/`),
- ensure you invoked a query that writes the snapshot (`GradleBuild::projects`, per-project
  `java_compile_config(..., project_path=Some(":app"))`, or the batch `java_compile_configs_all`).
  Note: some queries can be served from `nova-build`’s cache and may not spawn Gradle; snapshot
  updates are tied to executing Gradle.

### Snapshot seems “stale” even though build files didn’t change

`buildFingerprint` is based on **build inputs**, not on Gradle cache contents. If dependency
resolution changes without build file edits (e.g. dynamic versions, refreshed caches), the snapshot
may still validate and be used, but can be outdated. Re-run Gradle extraction to refresh it.

---

## Tests (writer/reader contract)

The snapshot handoff has cross-crate tests that exercise both sides of the contract:

- schema/types: `crates/nova-build-model/tests/gradle_snapshot.rs` (locks down JSON field names and
  defaulting behavior for forward compatibility)
- writer: `crates/nova-build/tests/suite/gradle_snapshot.rs` (asserts `.nova/queries/gradle.json` is
  created and contains expected fields after Gradle config extraction)
- reader: `crates/nova-project/tests/suite/gradle_snapshot.rs` (asserts `nova-project` consumes a
  valid snapshot and uses it to override module roots/classpaths/source roots)

When evolving the schema, update these tests alongside `GRADLE_SNAPSHOT_SCHEMA_VERSION` (defined in
`nova-build-model`).

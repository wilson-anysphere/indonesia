# File watching (Nova)

This repository has **two layers** involved in responding to filesystem changes:

1. **`nova-vfs`** owns the *watcher abstraction* and any *OS integration* (e.g. the `notify` backend,
   behind a feature flag such as `watch-notify`).
2. **`nova-workspace`** consumes `nova_vfs::FileWatcher` to drive project reloads and incremental
   re-indexing, but does **not** talk to `notify` directly.

This split keeps platform-specific watcher behavior isolated, and makes higher-level components
testable without involving real OS watcher timing.

## Event model and normalization

Watcher backends are normalized into a small set of operations:

- **Created**
- **Modified**
- **Deleted**
- **Moved** (rename/move)
- **Rescan** (not a file change): indicates the watcher dropped events and consumers should rescan watched paths/roots.

Rename detection is *heuristic* on many platforms. Some backends emit a rename as two separate
events (`from` then `to`) and may reorder or coalesce them. `nova-vfs` is responsible for pairing
these into a single logical **Moved** operation when possible; when pairing fails, consumers may
observe a fallback representation (e.g. delete+create, or just modified).

`Rescan` events can occur under sustained filesystem churn when watcher queues overflow. When a
consumer receives a rescan signal, it should fall back to walking/relisting the relevant
paths/roots and rebuilding its view of the filesystem (treat watcher events as a hint, not an
authoritative source of truth).

### Backpressure / overflow

The Notify-backed watcher implementation uses **bounded internal queues** to avoid unbounded memory
growth under event storms (e.g. `git checkout`, branch switches, build output churn). When either
queue overflows, the watcher drops events and emits `WatchEvent::Rescan`.

Queue sizes can be tuned via environment variables:

- `NOVA_WATCH_NOTIFY_RAW_QUEUE_CAPACITY` (notify callback → drain thread)
- `NOVA_WATCH_NOTIFY_EVENTS_QUEUE_CAPACITY` (drain thread → consumer)

Values must be positive integers. Empty/`0` values fall back to built-in defaults.

Downstream of the watcher, `nova-workspace` also uses bounded channels for its own internal
watcher/driver pipeline and for the external workspace event stream returned by
`Workspace::subscribe()`. If a subscriber does not keep up, workspace events may be dropped.

In addition, `nova-workspace` may treat certain watcher conditions as a **rescan trigger** and fall
back to a full project reload, for example:

- when it receives `WatchEvent::Rescan` (backend overflow/backpressure),
- when its own internal watcher batching pipeline overflows, and
- when the OS watcher reports a **directory-level** operation (directory rename/move/delete), which
  is difficult to map safely into per-file VFS operations.

## Watch paths and modes (`WatchMode`)

`nova_vfs::FileWatcher` is defined in terms of watching **paths**, not just “workspace roots”.

- Directory paths can be watched:
  - **Recursively** (`WatchMode::Recursive`) to include all descendants.
  - **Non-recursively** (`WatchMode::NonRecursive`) to watch only the directory itself.
- File paths are effectively always watched **non-recursively**.

`WatchMode` is owned by `nova-vfs` so higher layers can express “recursive vs non-recursive” without
depending on `notify`’s backend-specific enums.

Convenience: `nova_vfs::FileWatcher` also exposes `watch_root(root)` / `unwatch_root(root)` helpers
for the common case of recursively watching a directory root. These are equivalent to
`watch_path(root, WatchMode::Recursive)` / `unwatch_path(root)`.

## Dynamic watch paths (workspace reloads)

In `nova-workspace`, the set of watched **paths** (directory roots + their `WatchMode`) can change
after a project reload. For example:

- Maven/Gradle discovery may refine `source_roots`.
- Generated source roots may appear/disappear depending on build configuration and APT output.
- Build-system module roots (`ProjectConfig.modules[*].root`) may live outside the workspace root
  (e.g. Maven `<modules>` entries like `../common`). These module roots are included as watcher
  roots so build file changes still trigger project reloads.
- Some build integrations write workspace-local **snapshot files** under `.nova/` (e.g.
  `.nova/queries/gradle.json` for Gradle classpath/source roots, or
  `.nova/apt-cache/generated-roots.json` for APT generated-source roots). When these snapshots
  change, treat them as **build changes** so the workspace reloads and picks up the new
  configuration.
- If the resolved Nova config file lives outside the workspace root, the workspace will watch it
  **non-recursively** to avoid accidentally watching huge trees (e.g. `$HOME`).

To handle this, the workspace reconciles its desired watch paths (directory roots + their
`WatchMode`) against the active watcher and updates them dynamically. Paths that do not exist yet
are retried later (instead of failing permanently), which keeps “generated sources not created yet”
from breaking file watching.

Implementation references:

- `crates/nova-workspace/src/engine.rs` (`compute_watch_roots`)
- `crates/nova-workspace/src/watch.rs` (normalization + build-vs-source categorization)
- `crates/nova-workspace/src/watch_roots.rs` (`WatchRootManager`)

## Optional build-tool invocation during workspace load/reload

By default, Nova does **not** invoke external build tools during workspace load/reload. This keeps
startup fast and avoids spawning subprocesses unexpectedly, but it can result in a best-effort
classpath/source root model in some Maven/Gradle workspaces.

To allow Nova to invoke Maven/Gradle during workspace load/reload (to compute accurate classpaths
and source roots), opt in via `nova.toml`:

```toml
[build]
enabled = true
timeout_ms = 30000

# Optional per-tool toggles (only apply when build.enabled = true)
[build.maven]
enabled = true

[build.gradle]
enabled = true
```

Tradeoffs:

- Enabling build integration may be **slow** (build tools can take seconds to minutes).
- It may spawn external processes and can download/update dependency caches.
- `timeout_ms` exists to keep workspace loading time-bounded.

## Feature flags

`nova-vfs` keeps OS watcher dependencies behind feature flags:

- `watch-notify`: enables a Notify-backed watcher implementation inside `nova-vfs`.

Guidance:

- **Binaries / integration crates** that need to watch the real filesystem (e.g. `nova-workspace`,
  which is used by the `nova` CLI) should enable `nova-vfs/watch-notify`.
- Lower-level crates should depend only on the `nova-vfs` abstraction and avoid depending on
  `notify` directly.

## Adding a new watcher implementation

If you need another OS backend (or a specialized watcher), implement it inside `nova-vfs` and keep
it **feature-gated** so `nova-vfs` can remain lightweight by default.

Guidelines:

- Put the OS integration in `crates/nova-vfs/` (typically under `src/watch.rs` or a submodule) and
  expose it from `nova-vfs` behind a Cargo feature (e.g. `watch-foo`).
- Add the backend crate as an **optional dependency** of `nova-vfs`, and wire it to the feature via
  `watch-foo = ["dep:foo"]` in `crates/nova-vfs/Cargo.toml`.
- Normalize backend-specific events into `nova_vfs::FileChange` (Created/Modified/Deleted/Moved).
  Prefer keeping rename/move pairing logic in `nova-vfs` so consumers stay portable.
- Normalize paths using `VfsPath::local(...)` rather than passing raw `PathBuf`s through the system
  (this centralizes path normalization rules in one place).

## Testing: keep watcher tests deterministic

Avoid tests that:

- start a real OS watcher (`notify`),
- write to disk, and
- `sleep(...)` hoping an event arrives.

They are inherently flaky across platforms and CI environments (different watcher backends, OS load,
coalescing, and timing).

Note: some Nova tests (especially in `nova-workspace`) may spawn background threads (scheduler pools,
debouncers, etc.). In very resource-constrained environments you may see errors like `failed to
spawn thread` / `Resource temporarily unavailable`. If that happens, try rerunning with a single
test thread (e.g. `cargo test --locked -p nova-workspace --lib -- --test-threads=1`) to reduce peak
thread usage.

Prefer one of these deterministic approaches:

### 1) Inject a manual watcher (`ManualFileWatcher`)

When a component accepts a `nova_vfs::FileWatcher`, tests can pass a deterministic in-memory
implementation (`ManualFileWatcher`) and explicitly enqueue events. This also lets tests assert
which paths were registered via `watch_path(.., WatchMode::...)` without involving the OS.

Note: `ManualFileWatcher` uses a **bounded** internal queue and `push(...)` is non-blocking. If a
test enqueues too many events without draining the receiver, `push` will return an
`io::ErrorKind::WouldBlock` error.

If the watcher itself is moved into another thread (e.g. a background watcher driver), create a
`ManualFileWatcherHandle` via `watcher.handle()` and use the handle to inject events after the move.

Conceptually:

```rust
use nova_vfs::{FileChange, FileWatcher, ManualFileWatcher, VfsPath, WatchEvent};
use std::path::PathBuf;
use std::time::Duration;

// ManualFileWatcher implements FileWatcher and exposes a way to inject events.
let watcher = ManualFileWatcher::default();
// If you move `watcher` into another thread/component under test, keep this handle around to inject
// events from the test thread.
let handle = watcher.handle();

handle
    .push(WatchEvent::Changes {
        changes: vec![FileChange::Created {
            path: VfsPath::local(PathBuf::from("/tmp/Main.java")),
        }],
    })
    .unwrap();

// WatchMessage = io::Result<WatchEvent>, so errors travel on the same channel.
let msg = watcher
    .receiver()
    .recv_timeout(Duration::from_secs(1))
    .unwrap()
    .unwrap();
match msg {
    WatchEvent::Changes { changes } => assert_eq!(changes.len(), 1),
    WatchEvent::Rescan => panic!("unexpected rescan event in test"),
}
```

You can also assert which paths the component tried to watch:

```rust
use nova_vfs::{FileWatcher, ManualFileWatcher, WatchMode};
use std::path::{Path, PathBuf};

let mut watcher = ManualFileWatcher::default();
watcher
    .watch_path(Path::new("/project"), WatchMode::Recursive)
    .unwrap();

assert_eq!(
    watcher.watch_calls(),
    &[(PathBuf::from("/project"), WatchMode::Recursive)]
);
```

### 2) Bypass the watcher and call "apply events" APIs directly

For higher-level workspace behavior, many tests can skip the watcher entirely and call the
"apply filesystem events" entrypoint directly (this is typically what the watcher thread *would*
do after normalization/debouncing).

This style is fast, deterministic, and avoids platform-specific edge cases unrelated to the logic
under test.

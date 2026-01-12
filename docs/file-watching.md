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

Rename detection is *heuristic* on many platforms. Some backends emit a rename as two separate
events (`from` then `to`) and may reorder or coalesce them. `nova-vfs` is responsible for pairing
these into a single logical **Moved** operation when possible; when pairing fails, consumers may
observe a fallback representation (e.g. delete+create, or just modified).

## Dynamic watch roots (workspace reloads)

In `nova-workspace`, the set of watch roots can change after a project reload. For example:

- Maven/Gradle discovery may refine `source_roots`.
- Generated source roots may appear/disappear depending on build configuration and APT output.

To handle this, the workspace reconciles its desired roots against the active watcher and updates
them dynamically. Roots that do not exist yet are retried later (instead of failing permanently),
which keeps “generated sources not created yet” from breaking file watching.

Implementation reference: `crates/nova-workspace/src/watch_roots.rs` (`WatchRootManager`).

## Feature flags

`nova-vfs` keeps OS watcher dependencies behind feature flags:

- `watch-notify`: enables a Notify-backed watcher implementation inside `nova-vfs`.

Guidance:

- **Binaries / integration crates** that need to watch the real filesystem (e.g. `nova-lsp`,
  `nova-cli`, `nova-workspace`) should enable `nova-vfs/watch-notify`.
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

Prefer one of these deterministic approaches:

### 1) Inject a manual watcher (`ManualFileWatcher`)

When a component accepts a `nova_vfs::FileWatcher`, tests can pass a deterministic in-memory
implementation (often called `ManualFileWatcher`) and explicitly enqueue events.

Conceptually:

```rust
use nova_vfs::{FileWatcher, ManualFileWatcher, WatchEvent};
use std::time::Duration;

// ManualFileWatcher implements FileWatcher and exposes a way to inject events.
let watcher = ManualFileWatcher::default();

watcher.push(WatchEvent { changes: vec![/* ... */] }).unwrap();

// WatchMessage = io::Result<WatchEvent>, so errors travel on the same channel.
let msg = watcher
    .receiver()
    .recv_timeout(Duration::from_secs(1))
    .unwrap()
    .unwrap();
assert_eq!(msg.changes.len(), 1);
```

### 2) Bypass the watcher and call "apply events" APIs directly

For higher-level workspace behavior, many tests can skip the watcher entirely and call the
"apply filesystem events" entrypoint directly (this is typically what the watcher thread *would*
do after normalization/debouncing).

This style is fast, deterministic, and avoids platform-specific edge cases unrelated to the logic
under test.

//! File watching.
//!
//! This module defines [`FileWatcher`], Nova's cross-platform file-watching abstraction.
//!
//! # Ownership / layering
//!
//! `nova-vfs` intentionally owns *all* operating-system integration for file watching.
//! Higher layers (like `nova-workspace` and the LSP/DAP binaries) depend only on the
//! [`FileWatcher`] trait and the stable [`WatchEvent`] / [`crate::change::FileChange`] model.
//!
//! In particular:
//!
//! - OS backends (currently a Notify-based implementation) live in `nova-vfs` behind the
//!   `watch-notify` feature. This keeps `notify` and platform-specific watcher dependencies out of
//!   the default build.
//!   - This feature should be enabled by binaries / integration crates that actually need OS file
//!     watching (e.g. `nova-workspace`, which is used by the `nova` CLI), not by low-level library
//!     crates.
//!   - If you add another backend, keep it in `nova-vfs` and feature-gate it similarly (optional
//!     dependency + `watch-*` feature), so other crates don't take on extra OS-specific deps.
//! - Recursion semantics are represented by [`WatchMode`] (recursive vs non-recursive), which is
//!   also owned by `nova-vfs` so downstream crates don't need to depend on backend-specific enums
//!   like `notify::RecursiveMode`.
//! - Move/rename normalization lives here (pairing `from`/`to` where possible) so consumers don't
//!   need to implement per-platform rename heuristics.
//!
//! # Event delivery
//!
//! Most OS watchers are *push-based* internally (a background thread invokes a callback when the OS
//! reports a change). `nova-vfs` exposes these changes as an event stream (`crossbeam_channel`)
//! returned by [`FileWatcher::receiver`].
//!
//! Watchers can surface errors asynchronously; these are delivered on the same stream (see
//! [`WatchMessage`]).
//!
//! This design keeps the watcher boundary "library friendly": consumers can integrate file
//! watching into their own event loops without forcing a particular async runtime.
//!
//! # Semantics
//!
//! `nova-vfs` normalizes backend-specific events into a small set of high-level operations (see
//! [`crate::change::FileChange`]):
//!
//! - **Created**
//! - **Modified**
//! - **Deleted**
//! - **Moved**
//! - **Rescan** (not a file change; indicates the watcher dropped events and consumers should rescan)
//!
//! Backends are allowed to be *lossy* and the OS can legitimately coalesce/reorder events; this is
//! unavoidable in practice. The goal is to provide a stable "best effort" stream that higher
//! layers can batch/debounce.
//!
//! If a backend drops events due to overflow/backpressure, it should emit [`WatchEvent::Rescan`] so
//! consumers can fall back to a full rescan of watched paths/roots.
//!
//! ## Backpressure / overflow (Notify backend)
//!
//! The Notify-backed watcher implementation (feature `watch-notify`) uses bounded internal queues to
//! avoid unbounded memory growth under event storms (e.g. `git checkout`, branch switches, build
//! output churn). If these queues overflow, the watcher drops events and emits
//! [`WatchEvent::Rescan`].
//!
//! Queue sizes can be tuned via environment variables:
//!
//! - `NOVA_WATCH_NOTIFY_RAW_QUEUE_CAPACITY` (notify callback → drain thread)
//! - `NOVA_WATCH_NOTIFY_EVENTS_QUEUE_CAPACITY` (drain thread → consumer)
//!
//! See `docs/file-watching.md` for details and testing guidance.
//!
//! ## Rename pairing and limitations
//!
//! Many watcher backends do not provide an atomic rename event. Instead, they may emit two separate
//! events ("rename from" and "rename to"), sometimes out-of-order or split across frames.
//!
//! `nova-vfs` attempts to pair these into a single logical **Moved** change by buffering a bounded
//! set of pending "from" paths and matching them against subsequent "to" paths within a small time
//! window.
//!
//! Limitations:
//!
//! - Pairing is heuristic: under heavy churn, "from"/"to" events can be reordered and the best we
//!   can do is fall back to interpreting them as creates/deletes/modifies.
//! - Some platforms report "atomic save" as a rename + create; consumers should treat `Moved` and
//!   `Modified` as hints and always re-read file contents when needed.
//! - Cross-root moves (e.g. between watched paths/roots) may not be pairable, depending on the backend.
//!
//! # Testing
//!
//! Avoid tests that rely on real OS watcher timing (sleeping and hoping the watcher fires). They
//! tend to be flaky on CI and across platforms.
//!
//! Instead, prefer a deterministic injected watcher (see [`ManualFileWatcher`]) or direct calls
//! into higher-level "apply events" APIs. See `docs/file-watching.md` for guidance.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use crossbeam_channel as channel;

use crate::change::FileChange;
use crate::path::VfsPath;

/// An event produced by a file watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// One or more normalized file changes.
    ///
    /// Backends may batch multiple changes together to reduce overhead.
    Changes { changes: Vec<FileChange> },
    /// Indicates the watcher dropped events due to overflow/backpressure and downstream consumers
    /// should rescan watched paths/roots.
    ///
    /// This commonly corresponds to `notify::event::Flag::Rescan`.
    Rescan,
}

impl WatchEvent {
    /// Returns the normalized file changes contained in this event (if any).
    ///
    /// - For [`WatchEvent::Changes`], this is the underlying batch of changes.
    /// - For [`WatchEvent::Rescan`], this is an empty slice (callers should rescan watched paths/roots).
    pub fn changes(&self) -> &[FileChange] {
        match self {
            WatchEvent::Changes { changes } => changes,
            WatchEvent::Rescan => &[],
        }
    }

    /// Returns every VFS path touched by this watch event.
    ///
    /// For moves this includes both `from` and `to`.
    pub fn paths(&self) -> impl Iterator<Item = &VfsPath> {
        self.changes().iter().flat_map(|change| change.paths())
    }

    /// Returns every local filesystem path touched by this watch event.
    pub fn local_paths(&self) -> impl Iterator<Item = &Path> {
        self.paths().filter_map(|path| path.as_local_path())
    }
}

/// Controls whether a directory watch should recurse into subdirectories.
///
/// This enum is owned by `nova-vfs` so downstream crates don't need to depend on a specific
/// filesystem watcher backend (e.g. `notify`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WatchMode {
    /// Watch the given directory and all descendants.
    Recursive,
    /// Only watch the given path itself (for directories, this does **not** include descendants).
    NonRecursive,
}

/// Message type delivered by a [`FileWatcher`].
///
/// OS watcher backends may surface errors asynchronously; these are delivered as `Err(io::Error)`
/// values via the same event stream.
pub type WatchMessage = io::Result<WatchEvent>;

/// Event-driven watcher abstraction.
///
/// Consumers are expected to:
///
/// 1. Register paths to watch with [`watch_path`](FileWatcher::watch_path) (or the convenience
///    [`watch_root`](FileWatcher::watch_root) helper for recursive directory roots).
/// 2. Consume events from [`receiver`](FileWatcher::receiver).
///
/// Notes:
///
/// - Watchers are allowed to coalesce events.
/// - Consumers should treat events as *hints* and consult the filesystem/VFS for the authoritative
///   state.
pub trait FileWatcher: Send {
    /// Begin watching `path`.
    ///
    /// `path` may be either a directory path or a file path. For file paths, `mode` is treated as
    /// [`WatchMode::NonRecursive`] since recursion is not meaningful.
    fn watch_path(&mut self, path: &Path, mode: WatchMode) -> io::Result<()>;

    /// Stop watching `path`.
    fn unwatch_path(&mut self, path: &Path) -> io::Result<()>;

    /// Convenience wrapper for watching a directory root recursively.
    ///
    /// This is equivalent to `watch_path(root, WatchMode::Recursive)`.
    fn watch_root(&mut self, root: &Path) -> io::Result<()> {
        self.watch_path(root, WatchMode::Recursive)
    }

    /// Convenience wrapper for unwatching a directory root.
    ///
    /// This is equivalent to `unwatch_path(root)`.
    fn unwatch_root(&mut self, root: &Path) -> io::Result<()> {
        self.unwatch_path(root)
    }

    /// Returns the receiver used to consume watcher events.
    fn receiver(&self) -> &channel::Receiver<WatchMessage>;

    /// Retrieves all currently pending events, if any.
    ///
    /// This is a convenience wrapper over [`FileWatcher::receiver`] that drains any available
    /// messages without blocking.
    fn poll(&mut self) -> io::Result<Vec<WatchEvent>> {
        let mut out = Vec::new();
        for msg in self.receiver().try_iter() {
            match msg {
                Ok(event) => out.push(event),
                Err(err) => return Err(err),
            }
        }
        Ok(out)
    }
}

impl<W: ?Sized + FileWatcher> FileWatcher for Box<W> {
    fn watch_path(&mut self, path: &Path, mode: WatchMode) -> io::Result<()> {
        self.as_mut().watch_path(path, mode)
    }

    fn unwatch_path(&mut self, path: &Path) -> io::Result<()> {
        self.as_mut().unwatch_path(path)
    }

    fn receiver(&self) -> &channel::Receiver<WatchMessage> {
        self.as_ref().receiver()
    }
}

/// Deterministic watcher implementation for tests.
///
/// This watcher does not interact with the OS. Callers inject events manually via
/// [`ManualFileWatcher::push`] or [`ManualFileWatcherHandle`].
///
/// Event delivery uses a bounded in-memory queue to avoid unbounded memory growth in tests.
/// Injection APIs (`push`, `push_error`) are non-blocking and return `io::ErrorKind::WouldBlock` if
/// the queue is full (tests should drain the receiver if they enqueue many events).
#[derive(Debug)]
pub struct ManualFileWatcher {
    tx: channel::Sender<WatchMessage>,
    rx: channel::Receiver<WatchMessage>,
    watch_calls: Vec<(PathBuf, WatchMode)>,
    unwatch_calls: Vec<PathBuf>,
    watched: HashMap<PathBuf, WatchMode>,
}

/// Cloneable handle for injecting events into a [`ManualFileWatcher`] after it has been moved into
/// another thread (e.g. a workspace watcher driver).
#[derive(Debug, Clone)]
pub struct ManualFileWatcherHandle {
    tx: channel::Sender<WatchMessage>,
}

const MANUAL_WATCH_QUEUE_CAPACITY: usize = 1024;

impl ManualFileWatcherHandle {
    /// Inject a synthetic watcher event.
    pub fn push(&self, event: WatchEvent) -> io::Result<()> {
        match self.tx.try_send(Ok(event)) {
            Ok(()) => Ok(()),
            Err(channel::TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "watch queue is full",
            )),
            Err(channel::TrySendError::Disconnected(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "watch receiver dropped",
            )),
        }
    }

    /// Inject an asynchronous watcher error.
    pub fn push_error(&self, error: io::Error) -> io::Result<()> {
        match self.tx.try_send(Err(error)) {
            Ok(()) => Ok(()),
            Err(channel::TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "watch queue is full",
            )),
            Err(channel::TrySendError::Disconnected(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "watch receiver dropped",
            )),
        }
    }
}

impl Default for ManualFileWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ManualFileWatcher {
    pub fn new() -> Self {
        let (tx, rx) = channel::bounded(MANUAL_WATCH_QUEUE_CAPACITY);
        Self {
            tx,
            rx,
            watch_calls: Vec::new(),
            unwatch_calls: Vec::new(),
            watched: HashMap::new(),
        }
    }

    /// Returns a cloneable handle that can be used to inject events even after the watcher has been
    /// moved into another thread.
    pub fn handle(&self) -> ManualFileWatcherHandle {
        ManualFileWatcherHandle {
            tx: self.tx.clone(),
        }
    }

    /// Inject a synthetic watcher event.
    pub fn push(&self, event: WatchEvent) -> io::Result<()> {
        self.handle().push(event)
    }

    /// Inject an asynchronous watcher error.
    pub fn push_error(&self, error: io::Error) -> io::Result<()> {
        self.handle().push_error(error)
    }

    /// Paths passed to [`FileWatcher::watch_path`] (in call order).
    pub fn watch_calls(&self) -> &[(PathBuf, WatchMode)] {
        &self.watch_calls
    }

    /// Paths passed to [`FileWatcher::unwatch_path`] (in call order).
    pub fn unwatch_calls(&self) -> &[PathBuf] {
        &self.unwatch_calls
    }

    /// Returns the set of currently watched roots/paths (sorted for determinism).
    pub fn watched_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = self.watched.keys().cloned().collect();
        roots.sort();
        roots
    }

    /// Returns the currently watched paths and their modes (sorted for determinism).
    pub fn watched_paths(&self) -> Vec<(PathBuf, WatchMode)> {
        let mut out: Vec<(PathBuf, WatchMode)> =
            self.watched.iter().map(|(p, m)| (p.clone(), *m)).collect();
        out.sort_by(|(a, _), (b, _)| a.cmp(b));
        out
    }
}

impl FileWatcher for ManualFileWatcher {
    fn watch_path(&mut self, path: &Path, mode: WatchMode) -> io::Result<()> {
        let path = path.to_path_buf();
        self.watch_calls.push((path.clone(), mode));
        // Mirror `NotifyFileWatcher` semantics: once a path is watched recursively, do not silently
        // downgrade it to non-recursive on subsequent calls.
        let mode = match self.watched.get(&path) {
            Some(existing) if *existing == WatchMode::Recursive || mode == WatchMode::Recursive => {
                WatchMode::Recursive
            }
            Some(_) => WatchMode::NonRecursive,
            None => mode,
        };
        self.watched.insert(path, mode);
        Ok(())
    }

    fn unwatch_path(&mut self, path: &Path) -> io::Result<()> {
        let path = path.to_path_buf();
        self.unwatch_calls.push(path.clone());
        self.watched.remove(&path);
        Ok(())
    }

    fn receiver(&self) -> &channel::Receiver<WatchMessage> {
        &self.rx
    }
}

#[cfg(any(test, feature = "watch-notify"))]
mod notify_impl {
    use super::*;

    use crate::path::VfsPath;
    use notify::EventKind;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    #[cfg(feature = "watch-notify")]
    use notify::{RecursiveMode, Watcher};
    #[cfg(feature = "watch-notify")]
    use std::collections::HashMap;

    fn notify_event_requests_rescan(event: &notify::Event) -> bool {
        // `notify` signals dropped events / overflows by marking the event with `Flag::Rescan`.
        // Some backends also emit a path-less `EventKind::Other`.
        matches!(event.attrs.flag(), Some(notify::event::Flag::Rescan))
            || (matches!(event.kind, EventKind::Other) && event.paths.is_empty())
    }

    #[derive(Debug, Default)]
    pub struct EventNormalizer {
        pending_renames: VecDeque<(Instant, VfsPath)>,
    }

    impl EventNormalizer {
        pub const MAX_AGE: Duration = Duration::from_secs(2);
        pub const MAX_PENDING_RENAMES: usize = 512;

        pub fn new() -> Self {
            Self {
                pending_renames: VecDeque::new(),
            }
        }

        pub fn push(&mut self, event: notify::Event, now: Instant) -> Vec<FileChange> {
            let mut out = self.gc_pending(now);

            use notify::event::{ModifyKind, RenameMode};

            match event.kind {
                EventKind::Create(_) => {
                    out.extend(event.paths.into_iter().map(|path| FileChange::Created {
                        path: VfsPath::local(path),
                    }))
                }
                EventKind::Remove(_) => {
                    out.extend(event.paths.into_iter().map(|path| FileChange::Deleted {
                        path: VfsPath::local(path),
                    }))
                }
                EventKind::Modify(ModifyKind::Data(_))
                | EventKind::Modify(ModifyKind::Metadata(_))
                | EventKind::Modify(ModifyKind::Other)
                | EventKind::Modify(ModifyKind::Any) => {
                    out.extend(event.paths.into_iter().map(|path| FileChange::Modified {
                        path: VfsPath::local(path),
                    }))
                }
                EventKind::Modify(ModifyKind::Name(rename_mode)) => match rename_mode {
                    RenameMode::Both => out.extend(paths_to_moves(event.paths)),
                    RenameMode::From => {
                        for path in event.paths {
                            self.pending_renames.push_back((now, VfsPath::local(path)));
                        }
                        // Enforce queue bounds immediately so we don't silently drop a rename-from.
                        out.extend(self.gc_pending(now));
                    }
                    RenameMode::To => {
                        for to in event.paths {
                            let to = VfsPath::local(to);
                            if let Some((_, from)) = self.pending_renames.pop_front() {
                                out.push(FileChange::Moved { from, to });
                            } else {
                                out.push(FileChange::Created { path: to });
                            }
                        }
                    }
                    // Unknown rename variants: treat as modified.
                    RenameMode::Any | RenameMode::Other => {
                        out.extend(event.paths.into_iter().map(|path| FileChange::Modified {
                            path: VfsPath::local(path),
                        }))
                    }
                },
                // Some backends report a rename as a "modify" without further detail.
                _ => out.extend(event.paths.into_iter().map(|path| FileChange::Modified {
                    path: VfsPath::local(path),
                })),
            }

            out
        }

        /// Flushes internal state for expired/evicted rename-from events.
        pub fn flush(&mut self, now: Instant) -> Vec<FileChange> {
            self.gc_pending(now)
        }

        /// Flushes **all** pending rename-from events, treating them as deletions.
        ///
        /// This is used when shutting down the watcher (or when the notify callback disconnects)
        /// to ensure we never silently drop unmatched `RenameMode::From` events.
        pub fn flush_all_pending_renames_as_deleted(&mut self) -> Vec<FileChange> {
            let mut out = Vec::new();
            while let Some((_, path)) = self.pending_renames.pop_front() {
                out.push(FileChange::Deleted { path });
            }
            out
        }

        fn gc_pending(&mut self, now: Instant) -> Vec<FileChange> {
            let mut out = Vec::new();

            while let Some((t, _)) = self.pending_renames.front() {
                if now.saturating_duration_since(*t) <= Self::MAX_AGE {
                    break;
                }
                if let Some((_, path)) = self.pending_renames.pop_front() {
                    out.push(FileChange::Deleted { path });
                }
            }

            // Bound memory for rename storms.
            while self.pending_renames.len() > Self::MAX_PENDING_RENAMES {
                if let Some((_, path)) = self.pending_renames.pop_front() {
                    out.push(FileChange::Deleted { path });
                }
            }

            out
        }
    }

    fn paths_to_moves(paths: Vec<PathBuf>) -> Vec<FileChange> {
        let mut out = Vec::new();
        let mut it = paths.into_iter().map(VfsPath::local);
        while let Some(from) = it.next() {
            let Some(to) = it.next() else {
                out.push(FileChange::Modified { path: from });
                break;
            };
            out.push(FileChange::Moved { from, to });
        }
        out
    }

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[cfg(feature = "watch-notify")]
    const RAW_QUEUE_CAPACITY: usize = 4096;
    #[cfg(feature = "watch-notify")]
    const EVENTS_QUEUE_CAPACITY: usize = 1024;
    const OVERFLOW_RETRY_INTERVAL: Duration = Duration::from_millis(50);
    #[cfg(feature = "watch-notify")]
    const ENV_RAW_QUEUE_CAPACITY: &str = "NOVA_WATCH_NOTIFY_RAW_QUEUE_CAPACITY";
    #[cfg(feature = "watch-notify")]
    const ENV_EVENTS_QUEUE_CAPACITY: &str = "NOVA_WATCH_NOTIFY_EVENTS_QUEUE_CAPACITY";

    fn notify_error_to_io(err: notify::Error) -> io::Error {
        io::Error::other(err)
    }

    #[cfg(feature = "watch-notify")]
    fn queue_capacity_from_env(var: &str) -> io::Result<Option<usize>> {
        let raw = match std::env::var(var) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(err) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("failed to read env var {var}: {err}"),
                ))
            }
        };

        let raw = raw.trim();
        if raw.is_empty() || raw == "0" {
            return Ok(None);
        }

        let parsed = raw.parse::<usize>().map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid {var}={raw:?}: {err}"),
            )
        })?;

        // Avoid pathological configs that attempt to allocate enormous bounded channels.
        const MAX_CAPACITY: usize = 1_000_000;
        Ok(Some(parsed.clamp(1, MAX_CAPACITY)))
    }

    fn try_send_or_overflow<T>(tx: &channel::Sender<T>, overflowed: &AtomicBool, msg: T) {
        match tx.try_send(msg) {
            Ok(()) => {}
            Err(channel::TrySendError::Full(_)) => {
                overflowed.store(true, Ordering::Release);
            }
            Err(channel::TrySendError::Disconnected(_)) => {
                // The watcher is shutting down; dropping the message is fine.
            }
        }
    }

    fn run_notify_drain_loop(
        raw_rx: channel::Receiver<notify::Result<notify::Event>>,
        events_tx: channel::Sender<WatchMessage>,
        stop_rx: channel::Receiver<()>,
        overflowed: Arc<AtomicBool>,
    ) {
        let mut normalizer = EventNormalizer::new();

        loop {
            // If we've overflowed either the raw queue (notify callback) or the downstream queue,
            // the only safe recovery strategy is a full rescan.
            if overflowed.load(Ordering::Acquire) {
                normalizer = EventNormalizer::new();
                while raw_rx.try_recv().is_ok() {}

                match events_tx.try_send(Ok(WatchEvent::Rescan)) {
                    Ok(()) => {
                        overflowed.store(false, Ordering::Release);
                    }
                    Err(channel::TrySendError::Full(_)) => {
                        // Keep the flag set so we retry once consumers catch up.
                        overflowed.store(true, Ordering::Release);
                    }
                    Err(channel::TrySendError::Disconnected(_)) => break,
                }
            }

            let tick = if overflowed.load(Ordering::Acquire) {
                channel::after(OVERFLOW_RETRY_INTERVAL)
            } else {
                match normalizer.pending_renames.front() {
                    Some((started_at, _)) => {
                        let now = Instant::now();
                        let deadline = *started_at + EventNormalizer::MAX_AGE;
                        let timeout = deadline.saturating_duration_since(now);
                        channel::after(timeout)
                    }
                    None => channel::after(Duration::from_secs(3600)),
                }
            };

            channel::select! {
                recv(stop_rx) -> _ => {
                    // Flush any pending rename-froms so they aren't silently dropped when
                    // shutting down the watcher.
                    let changes = normalizer.flush_all_pending_renames_as_deleted();
                    if !changes.is_empty() {
                        let _ = events_tx.try_send(Ok(WatchEvent::Changes { changes }));
                    }
                    break;
                },
                recv(raw_rx) -> msg => {
                    let Ok(res) = msg else {
                        // The notify callback is gone (usually because the watcher is shutting down
                        // unexpectedly). Ensure we don't silently drop any pending rename-froms.
                        let changes = normalizer.flush_all_pending_renames_as_deleted();
                        if !changes.is_empty() {
                            let _ = events_tx.try_send(Ok(WatchEvent::Changes { changes }));
                        }
                        break;
                    };
                    match res {
                        Ok(event) => {
                            if notify_event_requests_rescan(&event) {
                                // `notify` signals dropped/coalesced events using a special rescan
                                // marker. The only safe recovery is a full rescan, so reuse the
                                // existing overflow recovery path.
                                overflowed.store(true, Ordering::Release);
                                continue;
                            }
                            let now = Instant::now();
                            let changes = normalizer.push(event, now);
                            if !changes.is_empty() {
                                if let Err(err) = events_tx.try_send(Ok(WatchEvent::Changes { changes })) {
                                    if matches!(err, channel::TrySendError::Full(_)) {
                                        overflowed.store(true, Ordering::Release);
                                    } else {
                                        break;
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            // Forward the error, but also request a rescan: many notify backends use
                            // errors to signal lost events.
                            overflowed.store(true, Ordering::Release);
                            if let Err(err) = events_tx.try_send(Err(notify_error_to_io(err))) {
                                if matches!(err, channel::TrySendError::Full(_)) {
                                    overflowed.store(true, Ordering::Release);
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                }
                recv(tick) -> _ => {
                    let changes = normalizer.flush(Instant::now());
                    if !changes.is_empty() {
                        if let Err(err) = events_tx.try_send(Ok(WatchEvent::Changes { changes })) {
                            if matches!(err, channel::TrySendError::Full(_)) {
                                overflowed.store(true, Ordering::Release);
                            } else {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(feature = "watch-notify")]
    #[derive(Debug, Clone, Copy)]
    struct ActualWatch {
        mode: WatchMode,
        ref_count: usize,
    }

    #[cfg(feature = "watch-notify")]
    pub struct NotifyFileWatcher {
        watcher: notify::RecommendedWatcher,
        events_rx: channel::Receiver<WatchMessage>,
        stop_tx: channel::Sender<()>,
        thread: Option<std::thread::JoinHandle<()>>,
        requested_paths: HashMap<PathBuf, PathBuf>,
        actual_watches: HashMap<PathBuf, ActualWatch>,

        #[cfg(test)]
        raw_tx_for_tests: channel::Sender<notify::Result<notify::Event>>,
        #[cfg(test)]
        overflowed_for_tests: Arc<AtomicBool>,
        #[cfg(test)]
        start_tx_for_tests: channel::Sender<()>,
    }

    #[cfg(feature = "watch-notify")]
    impl NotifyFileWatcher {
        pub fn new() -> io::Result<Self> {
            let raw_queue_capacity =
                queue_capacity_from_env(ENV_RAW_QUEUE_CAPACITY)?.unwrap_or(RAW_QUEUE_CAPACITY);
            let events_queue_capacity = queue_capacity_from_env(ENV_EVENTS_QUEUE_CAPACITY)?
                .unwrap_or(EVENTS_QUEUE_CAPACITY);
            Self::new_with_capacities(raw_queue_capacity, events_queue_capacity)
        }

        #[cfg(test)]
        fn new_with_capacities_for_tests(
            raw_queue_capacity: usize,
            events_queue_capacity: usize,
        ) -> io::Result<Self> {
            Self::new_with_capacities_impl(raw_queue_capacity, events_queue_capacity, false)
        }

        #[cfg(test)]
        fn start_for_tests(&self) {
            // Best-effort: if the watcher is already started this will either do nothing or error.
            let _ = self.start_tx_for_tests.try_send(());
        }

        #[cfg(test)]
        fn inject_raw_event_for_tests(&self, event: notify::Result<notify::Event>) {
            try_send_or_overflow(
                &self.raw_tx_for_tests,
                self.overflowed_for_tests.as_ref(),
                event,
            );
        }

        #[cfg(not(test))]
        fn new_with_capacities(
            raw_queue_capacity: usize,
            events_queue_capacity: usize,
        ) -> io::Result<Self> {
            let (raw_tx, raw_rx) =
                channel::bounded::<notify::Result<notify::Event>>(raw_queue_capacity);
            let (events_tx, events_rx) = channel::bounded::<WatchMessage>(events_queue_capacity);
            let (stop_tx, stop_rx) = channel::bounded::<()>(0);

            let overflowed = Arc::new(AtomicBool::new(false));

            let raw_tx_cb = raw_tx.clone();
            let overflowed_cb = Arc::clone(&overflowed);
            let watcher = notify::recommended_watcher(move |res| {
                try_send_or_overflow(&raw_tx_cb, overflowed_cb.as_ref(), res);
            })
            .map_err(notify_error_to_io)?;

            let thread_overflowed = Arc::clone(&overflowed);
            let thread = std::thread::spawn(move || {
                run_notify_drain_loop(raw_rx, events_tx, stop_rx, thread_overflowed)
            });

            Ok(Self {
                watcher,
                events_rx,
                stop_tx,
                thread: Some(thread),
                requested_paths: HashMap::new(),
                actual_watches: HashMap::new(),
            })
        }

        #[cfg(test)]
        fn new_with_capacities(
            raw_queue_capacity: usize,
            events_queue_capacity: usize,
        ) -> io::Result<Self> {
            Self::new_with_capacities_impl(raw_queue_capacity, events_queue_capacity, true)
        }

        #[cfg(test)]
        fn new_with_capacities_impl(
            raw_queue_capacity: usize,
            events_queue_capacity: usize,
            auto_start: bool,
        ) -> io::Result<Self> {
            let (raw_tx, raw_rx) =
                channel::bounded::<notify::Result<notify::Event>>(raw_queue_capacity);
            let (events_tx, events_rx) = channel::bounded::<WatchMessage>(events_queue_capacity);
            let (stop_tx, stop_rx) = channel::bounded::<()>(0);
            let (start_tx, start_rx) = channel::bounded::<()>(1);

            if auto_start {
                let _ = start_tx.try_send(());
            }

            let overflowed = Arc::new(AtomicBool::new(false));

            let raw_tx_cb = raw_tx.clone();
            let overflowed_cb = Arc::clone(&overflowed);
            let watcher = notify::recommended_watcher(move |res| {
                try_send_or_overflow(&raw_tx_cb, overflowed_cb.as_ref(), res);
            })
            .map_err(notify_error_to_io)?;

            let stop_rx_for_start = stop_rx.clone();
            let thread_overflowed = Arc::clone(&overflowed);
            let thread = std::thread::spawn(move || {
                // Deterministic tests sometimes need to overflow the raw queue before we start
                // draining it, so allow the thread to block on a "start" signal. `Drop` still works
                // because we also listen for `stop`.
                channel::select! {
                    recv(start_rx) -> _ => {},
                    recv(stop_rx_for_start) -> _ => return,
                }
                run_notify_drain_loop(raw_rx, events_tx, stop_rx, thread_overflowed)
            });

            Ok(Self {
                watcher,
                events_rx,
                stop_tx,
                thread: Some(thread),
                requested_paths: HashMap::new(),
                actual_watches: HashMap::new(),
                raw_tx_for_tests: raw_tx,
                overflowed_for_tests: overflowed,
                start_tx_for_tests: start_tx,
            })
        }

        fn ensure_actual_watch(&mut self, path: &Path, mode: WatchMode) -> io::Result<()> {
            let desired = match self.actual_watches.get(path) {
                Some(existing) => combine_modes(existing.mode, mode),
                None => mode,
            };

            match self.actual_watches.get_mut(path) {
                None => {
                    self.watcher
                        .watch(path, to_recursive_mode(desired))
                        .map_err(notify_error_to_io)?;
                    self.actual_watches.insert(
                        path.to_path_buf(),
                        ActualWatch {
                            mode: desired,
                            ref_count: 0,
                        },
                    );
                }
                Some(existing) => {
                    if existing.mode != desired {
                        // Upgrade the mode (e.g. NonRecursive -> Recursive).
                        self.watcher.unwatch(path).map_err(notify_error_to_io)?;
                        self.watcher
                            .watch(path, to_recursive_mode(desired))
                            .map_err(notify_error_to_io)?;
                        existing.mode = desired;
                    }
                }
            }

            Ok(())
        }

        fn add_requested_watch(
            &mut self,
            requested: PathBuf,
            actual: PathBuf,
            mode: WatchMode,
        ) -> io::Result<()> {
            if let Some(existing_actual) = self.requested_paths.get(&requested) {
                if existing_actual == &actual {
                    // Idempotent watch call; ensure we've upgraded modes if needed.
                    self.ensure_actual_watch(&actual, mode)?;
                    return Ok(());
                }
                self.remove_requested_watch(&requested)?;
            }

            self.ensure_actual_watch(&actual, mode)?;

            let entry = self
                .actual_watches
                .get_mut(&actual)
                .expect("ensure_actual_watch inserted entry");
            entry.mode = combine_modes(entry.mode, mode);
            entry.ref_count = entry.ref_count.saturating_add(1);

            self.requested_paths.insert(requested, actual);
            Ok(())
        }

        fn remove_requested_watch(&mut self, requested: &Path) -> io::Result<()> {
            let Some(actual) = self.requested_paths.remove(requested) else {
                return Ok(());
            };

            let Some(mut watch) = self.actual_watches.remove(&actual) else {
                return Ok(());
            };

            watch.ref_count = watch.ref_count.saturating_sub(1);
            if watch.ref_count > 0 {
                self.actual_watches.insert(actual, watch);
                return Ok(());
            }

            self.watcher.unwatch(&actual).map_err(notify_error_to_io)?;
            Ok(())
        }
    }

    #[cfg(feature = "watch-notify")]
    #[track_caller]
    fn join_notify_thread_best_effort(thread: std::thread::JoinHandle<()>, reason: &'static str) {
        static JOIN_PANIC_LOGGED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

        if let Err(panic) = thread.join() {
            if JOIN_PANIC_LOGGED.set(()).is_ok() {
                let loc = std::panic::Location::caller();
                let message = panic
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(|s| s.as_str()))
                    .unwrap_or("<non-string panic>");

                tracing::debug!(
                    target = "nova.vfs",
                    reason,
                    file = loc.file(),
                    line = loc.line(),
                    column = loc.column(),
                    panic = %message,
                    "file watcher drain thread panicked (best effort join)"
                );
            }
        }
    }

    #[cfg(feature = "watch-notify")]
    impl Drop for NotifyFileWatcher {
        fn drop(&mut self) {
            let _ = self.stop_tx.send(());
            if let Some(thread) = self.thread.take() {
                join_notify_thread_best_effort(thread, "vfs.notify_watcher.drop");
            }
        }
    }

    #[cfg(feature = "watch-notify")]
    impl FileWatcher for NotifyFileWatcher {
        fn watch_path(&mut self, path: &Path, mode: WatchMode) -> io::Result<()> {
            // When the caller requests a recursive watch, treat this as a directory watch even if
            // the directory doesn't exist yet (this allows higher layers to retry NotFound errors
            // deterministically for generated roots).
            let meta = std::fs::metadata(path);
            let is_dir = matches!(meta, Ok(ref meta) if meta.is_dir());
            let is_file = matches!(meta, Ok(ref meta) if meta.is_file());

            if is_dir || (mode == WatchMode::Recursive && !is_file) {
                return self.add_requested_watch(path.to_path_buf(), path.to_path_buf(), mode);
            }

            // Treat as a file watch (or a non-existent path the caller wants to treat as a file).
            // Recursion is not meaningful for file paths, so we clamp to NonRecursive.
            let file_mode = WatchMode::NonRecursive;
            let requested = path.to_path_buf();

            match self.add_requested_watch(requested.clone(), requested.clone(), file_mode) {
                Ok(()) => Ok(()),
                Err(_err) => {
                    let parent = path.parent().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent")
                    })?;
                    self.add_requested_watch(
                        requested,
                        parent.to_path_buf(),
                        WatchMode::NonRecursive,
                    )
                }
            }
        }

        fn unwatch_path(&mut self, path: &Path) -> io::Result<()> {
            self.remove_requested_watch(path)
        }

        fn receiver(&self) -> &channel::Receiver<WatchMessage> {
            &self.events_rx
        }
    }

    #[cfg(feature = "watch-notify")]
    fn combine_modes(a: WatchMode, b: WatchMode) -> WatchMode {
        if a == WatchMode::Recursive || b == WatchMode::Recursive {
            WatchMode::Recursive
        } else {
            WatchMode::NonRecursive
        }
    }

    #[cfg(feature = "watch-notify")]
    fn to_recursive_mode(mode: WatchMode) -> RecursiveMode {
        match mode {
            WatchMode::Recursive => RecursiveMode::Recursive,
            WatchMode::NonRecursive => RecursiveMode::NonRecursive,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        use notify::event::{ModifyKind, RenameMode};

        #[cfg(feature = "watch-notify")]
        #[derive(Debug)]
        struct EnvVarGuard {
            key: &'static str,
            prev: Option<std::ffi::OsString>,
        }

        #[cfg(feature = "watch-notify")]
        impl EnvVarGuard {
            fn new(key: &'static str) -> Self {
                let prev = std::env::var_os(key);
                Self { key, prev }
            }
        }

        #[cfg(feature = "watch-notify")]
        impl Drop for EnvVarGuard {
            fn drop(&mut self) {
                if let Some(prev) = &self.prev {
                    std::env::set_var(self.key, prev);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }

        #[cfg(feature = "watch-notify")]
        fn env_lock() -> std::sync::MutexGuard<'static, ()> {
            use std::sync::{Mutex, OnceLock};

            static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
            ENV_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(|err| {
                    let loc = std::panic::Location::caller();
                    tracing::error!(
                        target = "nova.vfs",
                        file = loc.file(),
                        line = loc.line(),
                        column = loc.column(),
                        error = %err,
                        "mutex poisoned; continuing with recovered guard"
                    );
                    err.into_inner()
                })
        }

        #[cfg(feature = "watch-notify")]
        #[test]
        fn emits_rescan_when_raw_queue_overflows_via_notify_watcher() {
            use notify::EventKind;

            // Use an extremely small raw queue to deterministically trigger overflow.
            let watcher = NotifyFileWatcher::new_with_capacities_for_tests(1, 16)
                .expect("failed to construct NotifyFileWatcher");

            let event = notify::Event {
                kind: EventKind::Modify(ModifyKind::Any),
                paths: vec![PathBuf::from("/tmp/A.java")],
                attrs: Default::default(),
            };

            // Fill the raw queue, then overflow it before the drain loop starts.
            watcher.inject_raw_event_for_tests(Ok(event.clone()));
            watcher.inject_raw_event_for_tests(Ok(event));
            assert!(watcher.overflowed_for_tests.load(Ordering::Acquire));

            watcher.start_for_tests();

            let msg = watcher
                .receiver()
                .recv_timeout(Duration::from_secs(1))
                .expect("expected watcher message")
                .expect("expected ok watcher event");
            assert_eq!(msg, WatchEvent::Rescan);
        }

        #[test]
        fn emits_rescan_when_raw_queue_overflows() {
            use notify::EventKind;

            let (raw_tx, raw_rx) = channel::bounded::<notify::Result<notify::Event>>(1);
            let (events_tx, events_rx) = channel::bounded::<WatchMessage>(16);
            let (stop_tx, stop_rx) = channel::bounded::<()>(0);
            let overflowed = Arc::new(AtomicBool::new(false));

            let event = notify::Event {
                kind: EventKind::Modify(ModifyKind::Any),
                paths: vec![PathBuf::from("/tmp/A.java")],
                attrs: Default::default(),
            };

            // Fill the raw queue, then overflow it. No background thread is running yet, so this is
            // deterministic.
            try_send_or_overflow(&raw_tx, overflowed.as_ref(), Ok(event.clone()));
            try_send_or_overflow(&raw_tx, overflowed.as_ref(), Ok(event));
            assert!(overflowed.load(Ordering::Acquire));

            let overflowed_for_thread = Arc::clone(&overflowed);
            let thread = std::thread::spawn(move || {
                run_notify_drain_loop(raw_rx, events_tx, stop_rx, overflowed_for_thread);
            });

            let msg = events_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("expected watcher message")
                .expect("expected ok watcher event");
            assert_eq!(msg, WatchEvent::Rescan);

            let _ = stop_tx.send(());
            thread
                .join()
                .expect("notify drain thread panicked in overflow test");
        }

        #[test]
        fn emits_rescan_when_events_queue_overflows() {
            use notify::EventKind;

            // Use a tiny downstream queue so we can deterministically force an overflow without any
            // OS timing assumptions.
            //
            // Use a synchronous raw channel so `send(...)` doesn't complete until the drain loop has
            // actually received the event.
            let (raw_tx, raw_rx) = channel::bounded::<notify::Result<notify::Event>>(0);
            let (events_tx, events_rx) = channel::bounded::<WatchMessage>(1);
            let (stop_tx, stop_rx) = channel::bounded::<()>(0);
            let overflowed = Arc::new(AtomicBool::new(false));

            // Pre-fill the downstream queue so the drain loop's first attempt to deliver a `Changes`
            // event experiences backpressure.
            let placeholder_change = FileChange::Modified {
                path: VfsPath::local("/tmp/placeholder.java"),
            };
            events_tx
                .send(Ok(WatchEvent::Changes {
                    changes: vec![placeholder_change.clone()],
                }))
                .unwrap();

            let overflowed_for_thread = Arc::clone(&overflowed);
            let thread = std::thread::spawn(move || {
                run_notify_drain_loop(raw_rx, events_tx, stop_rx, overflowed_for_thread);
            });
            let make_event = |path: &str| notify::Event {
                kind: EventKind::Modify(ModifyKind::Any),
                paths: vec![PathBuf::from(path)],
                attrs: Default::default(),
            };

            // This change should overflow the downstream queue, triggering a Rescan request.
            raw_tx.send(Ok(make_event("/tmp/A.java"))).unwrap();

            // Wait until the drain loop has observed the queue overflow.
            //
            // This avoids a race where we drain the placeholder message too early, allowing the
            // subsequent `Changes` send to succeed.
            let started_at = Instant::now();
            while !overflowed.load(Ordering::Acquire) {
                if started_at.elapsed() > Duration::from_secs(1) {
                    panic!("expected overflow flag to be set");
                }
                std::thread::yield_now();
            }
            assert!(overflowed.load(Ordering::Acquire));

            // Ensure the drain loop had a chance to observe the full downstream queue before we
            // start receiving messages. Without this, the consumer may read the first `Changes`
            // message quickly enough that the second send succeeds, making the test flaky.
            let deadline = Instant::now() + Duration::from_secs(1);
            while !overflowed.load(Ordering::Acquire) {
                assert!(
                    Instant::now() <= deadline,
                    "expected watcher to mark downstream queue as overflowed"
                );
                std::thread::yield_now();
            }

            let msg = events_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("expected watcher message")
                .expect("expected ok watcher event");
            assert_eq!(
                msg,
                WatchEvent::Changes {
                    changes: vec![placeholder_change],
                }
            );

            // Wake the drain loop without generating additional Changes so it can retry emitting a
            // Rescan immediately (without waiting for the retry tick).
            raw_tx
                .send(Ok(notify::Event {
                    kind: EventKind::Modify(ModifyKind::Any),
                    paths: Vec::new(),
                    attrs: Default::default(),
                }))
                .unwrap();

            let msg = events_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("expected watcher message")
                .expect("expected ok watcher event");
            assert_eq!(msg, WatchEvent::Rescan);

            let _ = stop_tx.send(());
            thread
                .join()
                .expect("notify drain thread panicked in rescan-on-overflow test");
        }

        #[test]
        fn emits_deleted_on_stop_when_rename_from_is_pending() {
            let (raw_tx, raw_rx) = channel::bounded::<notify::Result<notify::Event>>(16);
            let (events_tx, events_rx) = channel::bounded::<WatchMessage>(16);
            let (stop_tx, stop_rx) = channel::bounded::<()>(0);
            let overflowed = Arc::new(AtomicBool::new(false));

            let from = PathBuf::from("/tmp/pending.java");
            let ev_from = notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                paths: vec![from.clone()],
                attrs: Default::default(),
            };
            // Ensure the drain loop processes the rename-from before we signal stop by following it
            // with an event that produces output.
            let ev_modify = notify::Event {
                kind: EventKind::Modify(ModifyKind::Any),
                paths: vec![PathBuf::from("/tmp/other.java")],
                attrs: Default::default(),
            };

            raw_tx.send(Ok(ev_from)).unwrap();
            raw_tx.send(Ok(ev_modify)).unwrap();

            let overflowed_for_thread = Arc::clone(&overflowed);
            let thread = std::thread::spawn(move || {
                run_notify_drain_loop(raw_rx, events_tx, stop_rx, overflowed_for_thread);
            });

            // Consume the modify event output so we know the rename-from has been enqueued.
            let msg = events_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("expected watcher message")
                .expect("expected ok watcher event");
            assert!(matches!(msg, WatchEvent::Changes { .. }));

            // Stop the watcher and expect it to flush the pending rename-from as a delete.
            stop_tx.send(()).unwrap();

            let msg = events_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("expected watcher message")
                .expect("expected ok watcher event");
            assert_eq!(
                msg,
                WatchEvent::Changes {
                    changes: vec![FileChange::Deleted {
                        path: VfsPath::local(from),
                    }]
                }
            );

            thread
                .join()
                .expect("notify drain thread panicked in pending-rename flush test");
        }

        #[test]
        fn emits_deleted_when_notify_channel_disconnects_with_pending_rename_from() {
            let (raw_tx, raw_rx) = channel::bounded::<notify::Result<notify::Event>>(16);
            let (events_tx, events_rx) = channel::bounded::<WatchMessage>(16);
            let (_stop_tx, stop_rx) = channel::bounded::<()>(0);
            let overflowed = Arc::new(AtomicBool::new(false));

            let from = PathBuf::from("/tmp/pending_disconnect.java");
            let ev_from = notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                paths: vec![from.clone()],
                attrs: Default::default(),
            };
            let ev_modify = notify::Event {
                kind: EventKind::Modify(ModifyKind::Any),
                paths: vec![PathBuf::from("/tmp/other_disconnect.java")],
                attrs: Default::default(),
            };

            raw_tx.send(Ok(ev_from)).unwrap();
            raw_tx.send(Ok(ev_modify)).unwrap();
            drop(raw_tx);

            let overflowed_for_thread = Arc::clone(&overflowed);
            let thread = std::thread::spawn(move || {
                run_notify_drain_loop(raw_rx, events_tx, stop_rx, overflowed_for_thread);
            });

            // First message: modify output.
            let msg = events_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("expected watcher message")
                .expect("expected ok watcher event");
            assert!(matches!(msg, WatchEvent::Changes { .. }));

            // Second message: forced flush on disconnect should surface the pending rename-from as a
            // delete.
            let msg = events_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("expected watcher message")
                .expect("expected ok watcher event");
            assert_eq!(
                msg,
                WatchEvent::Changes {
                    changes: vec![FileChange::Deleted {
                        path: VfsPath::local(from),
                    }]
                }
            );

            thread
                .join()
                .expect("notify drain thread panicked in disconnect flush test");
        }

        #[test]
        fn rescan_flag_yields_rescan_event() {
            let (raw_tx, raw_rx) = channel::bounded::<notify::Result<notify::Event>>(16);
            let (events_tx, events_rx) = channel::bounded::<WatchMessage>(16);
            let (stop_tx, stop_rx) = channel::bounded::<()>(0);
            let overflowed = Arc::new(AtomicBool::new(false));

            let mut attrs = notify::event::EventAttributes::default();
            attrs.set_flag(notify::event::Flag::Rescan);

            let ev = notify::Event {
                kind: EventKind::Other,
                paths: Vec::new(),
                attrs,
            };

            raw_tx.send(Ok(ev)).unwrap();

            let overflowed_for_thread = Arc::clone(&overflowed);
            let thread = std::thread::spawn(move || {
                run_notify_drain_loop(raw_rx, events_tx, stop_rx, overflowed_for_thread);
            });

            let msg = events_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("expected watcher message")
                .expect("expected ok watcher event");
            assert_eq!(msg, WatchEvent::Rescan);

            let _ = stop_tx.send(());
            thread
                .join()
                .expect("notify drain thread panicked in rescan-flag test");
        }

        #[test]
        fn emits_deleted_when_rename_from_expires() {
            let mut normalizer = EventNormalizer::new();
            let t0 = Instant::now();
            let tmp = tempfile::tempdir().unwrap();

            let from = tmp.path().join("A.java");
            let ev_from = notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                paths: vec![from.clone()],
                attrs: Default::default(),
            };

            assert_eq!(normalizer.push(ev_from, t0), Vec::new());

            let t1 = t0 + EventNormalizer::MAX_AGE + Duration::from_millis(1);
            assert_eq!(
                normalizer.flush(t1),
                vec![FileChange::Deleted {
                    path: VfsPath::local(from)
                }]
            );
        }

        #[test]
        fn rename_to_after_rename_from_expires_yields_deleted_then_created() {
            let mut normalizer = EventNormalizer::new();
            let t0 = Instant::now();

            let from = PathBuf::from("/tmp/A.java");
            let to = PathBuf::from("/tmp/B.java");

            let ev_from = notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                paths: vec![from.clone()],
                attrs: Default::default(),
            };
            let ev_to = notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                paths: vec![to.clone()],
                attrs: Default::default(),
            };

            assert_eq!(normalizer.push(ev_from, t0), Vec::new());

            let t1 = t0 + EventNormalizer::MAX_AGE + Duration::from_millis(1);
            assert_eq!(
                normalizer.push(ev_to, t1),
                vec![
                    FileChange::Deleted {
                        path: VfsPath::local(from),
                    },
                    FileChange::Created {
                        path: VfsPath::local(to),
                    },
                ]
            );
        }

        #[test]
        fn normalize_rename_from_to_into_move_without_extra_deleted() {
            let mut normalizer = EventNormalizer::new();
            let t0 = Instant::now();

            let from = PathBuf::from("/tmp/A.java");
            let to = PathBuf::from("/tmp/B.java");

            let ev_from = notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                paths: vec![from.clone()],
                attrs: Default::default(),
            };
            let ev_to = notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                paths: vec![to.clone()],
                attrs: Default::default(),
            };

            assert_eq!(normalizer.push(ev_from, t0), Vec::new());
            assert_eq!(
                normalizer.push(ev_to, t0),
                vec![FileChange::Moved {
                    from: VfsPath::local(from),
                    to: VfsPath::local(to)
                }]
            );

            // Ensure the matched rename doesn't later show up as a delete.
            let t1 = t0 + EventNormalizer::MAX_AGE + Duration::from_millis(1);
            assert_eq!(normalizer.flush(t1), Vec::new());
        }

        #[test]
        fn normalize_create_remove_and_modify() {
            let mut normalizer = EventNormalizer::new();
            let t0 = Instant::now();
            let p = PathBuf::from("/tmp/A.java");

            let create = notify::Event {
                kind: EventKind::Create(notify::event::CreateKind::File),
                paths: vec![p.clone()],
                attrs: Default::default(),
            };
            let remove = notify::Event {
                kind: EventKind::Remove(notify::event::RemoveKind::File),
                paths: vec![p.clone()],
                attrs: Default::default(),
            };
            let modify = notify::Event {
                kind: EventKind::Modify(ModifyKind::Any),
                paths: vec![p.clone()],
                attrs: Default::default(),
            };

            assert_eq!(
                normalizer.push(create, t0),
                vec![FileChange::Created {
                    path: VfsPath::local(p.clone())
                }]
            );
            assert_eq!(
                normalizer.push(modify, t0),
                vec![FileChange::Modified {
                    path: VfsPath::local(p.clone())
                }]
            );
            assert_eq!(
                normalizer.push(remove, t0),
                vec![FileChange::Deleted {
                    path: VfsPath::local(p)
                }]
            );
        }

        #[test]
        fn normalize_unmatched_rename_to_is_treated_as_create() {
            let mut normalizer = EventNormalizer::new();
            let t0 = Instant::now();

            let to = PathBuf::from("/tmp/B.java");
            let ev_to = notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                paths: vec![to.clone()],
                attrs: Default::default(),
            };

            assert_eq!(
                normalizer.push(ev_to, t0),
                vec![FileChange::Created {
                    path: VfsPath::local(to)
                }]
            );
        }

        #[test]
        fn normalize_rename_both_with_leftover_path_is_treated_as_modified() {
            let mut normalizer = EventNormalizer::new();
            let t0 = Instant::now();

            let from = PathBuf::from("/tmp/A.java");
            let to = PathBuf::from("/tmp/B.java");
            let leftover = PathBuf::from("/tmp/leftover.java");
            let ev = notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                paths: vec![from.clone(), to.clone(), leftover.clone()],
                attrs: Default::default(),
            };

            assert_eq!(
                normalizer.push(ev, t0),
                vec![
                    FileChange::Moved {
                        from: VfsPath::local(from),
                        to: VfsPath::local(to),
                    },
                    FileChange::Modified {
                        path: VfsPath::local(leftover),
                    }
                ]
            );
        }

        #[test]
        fn evicted_rename_from_emits_deleted() {
            let mut normalizer = EventNormalizer::new();
            let t0 = Instant::now();
            let tmp = tempfile::tempdir().unwrap();

            let mut paths = Vec::with_capacity(EventNormalizer::MAX_PENDING_RENAMES + 1);
            for idx in 0..(EventNormalizer::MAX_PENDING_RENAMES + 1) {
                paths.push(tmp.path().join(format!("File{idx}.java")));
            }
            let first = paths[0].clone();

            let ev_from = notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                paths,
                attrs: Default::default(),
            };

            assert_eq!(
                normalizer.push(ev_from, t0),
                vec![FileChange::Deleted {
                    path: VfsPath::local(first)
                }]
            );
        }

        #[test]
        fn paths_are_normalized_with_vfs_path_rules() {
            let mut normalizer = EventNormalizer::new();
            let t0 = Instant::now();

            let normalized = PathBuf::from("/tmp/A.java");
            let with_dotdot = PathBuf::from("/tmp/x/../A.java");
            let ev = notify::Event {
                kind: EventKind::Create(notify::event::CreateKind::File),
                paths: vec![with_dotdot],
                attrs: Default::default(),
            };

            assert_eq!(
                normalizer.push(ev, t0),
                vec![FileChange::Created {
                    path: VfsPath::local(normalized),
                }]
            );
        }

        #[cfg(feature = "watch-notify")]
        #[test]
        fn queue_capacity_from_env_returns_none_when_unset() {
            let _lock = env_lock();
            const VAR: &str = "NOVA_VFS_TEST_QUEUE_CAPACITY_UNSET";
            let _guard = EnvVarGuard::new(VAR);
            std::env::remove_var(VAR);
            assert_eq!(queue_capacity_from_env(VAR).unwrap(), None);
        }

        #[cfg(feature = "watch-notify")]
        #[test]
        fn queue_capacity_from_env_treats_empty_and_zero_as_none() {
            let _lock = env_lock();
            const VAR: &str = "NOVA_VFS_TEST_QUEUE_CAPACITY_EMPTY";
            let _guard = EnvVarGuard::new(VAR);

            std::env::set_var(VAR, "");
            assert_eq!(queue_capacity_from_env(VAR).unwrap(), None);

            std::env::set_var(VAR, "  ");
            assert_eq!(queue_capacity_from_env(VAR).unwrap(), None);

            std::env::set_var(VAR, "0");
            assert_eq!(queue_capacity_from_env(VAR).unwrap(), None);

            std::env::set_var(VAR, " 0 ");
            assert_eq!(queue_capacity_from_env(VAR).unwrap(), None);
        }

        #[cfg(feature = "watch-notify")]
        #[test]
        fn queue_capacity_from_env_parses_and_clamps_values() {
            let _lock = env_lock();
            const VAR: &str = "NOVA_VFS_TEST_QUEUE_CAPACITY_PARSE";
            let _guard = EnvVarGuard::new(VAR);

            std::env::set_var(VAR, "123");
            assert_eq!(queue_capacity_from_env(VAR).unwrap(), Some(123));

            // Guard against pathological configs that would allocate enormous queues.
            std::env::set_var(VAR, "999999999");
            assert_eq!(queue_capacity_from_env(VAR).unwrap(), Some(1_000_000));
        }

        #[cfg(feature = "watch-notify")]
        #[test]
        fn queue_capacity_from_env_rejects_invalid_values() {
            let _lock = env_lock();
            const VAR: &str = "NOVA_VFS_TEST_QUEUE_CAPACITY_INVALID";
            let _guard = EnvVarGuard::new(VAR);

            std::env::set_var(VAR, "nope");
            let err = queue_capacity_from_env(VAR).expect_err("expected invalid input error");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        }
    }
}

#[cfg(feature = "watch-notify")]
pub use notify_impl::{EventNormalizer, NotifyFileWatcher};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::VfsPath;
    use std::io;
    use std::time::Duration;

    #[test]
    fn watch_event_paths_is_empty_for_rescan() {
        let event = WatchEvent::Rescan;
        let paths: Vec<VfsPath> = event.paths().cloned().collect();
        assert!(paths.is_empty());
    }

    #[test]
    fn watch_event_paths_yields_all_paths_for_changes() {
        let a = VfsPath::local("/tmp/A.java");
        let b = VfsPath::local("/tmp/B.java");

        let event = WatchEvent::Changes {
            changes: vec![
                FileChange::Created { path: a.clone() },
                FileChange::Moved {
                    from: a.clone(),
                    to: b.clone(),
                },
            ],
        };

        let paths: Vec<VfsPath> = event.paths().cloned().collect();
        assert_eq!(paths, vec![a.clone(), a, b]);
    }

    #[test]
    fn manual_watcher_tracks_roots_and_delivers_events() {
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("a");
        let root_b = tmp.path().join("b");

        let mut watcher = ManualFileWatcher::new();
        watcher.watch_path(&root_a, WatchMode::Recursive).unwrap();
        watcher
            .watch_path(&root_b, WatchMode::NonRecursive)
            .unwrap();
        watcher.unwatch_path(&root_a).unwrap();

        assert_eq!(
            watcher.watch_calls(),
            &[
                (root_a.clone(), WatchMode::Recursive),
                (root_b.clone(), WatchMode::NonRecursive)
            ]
        );
        assert_eq!(watcher.unwatch_calls(), std::slice::from_ref(&root_a));
        assert_eq!(watcher.watched_roots(), vec![root_b.clone()]);

        let path = VfsPath::local(root_b.join("Main.java"));
        watcher
            .push(WatchEvent::Changes {
                changes: vec![FileChange::Created { path: path.clone() }],
            })
            .unwrap();

        let msg = watcher
            .receiver()
            .recv_timeout(Duration::from_secs(1))
            .expect("watch event")
            .expect("ok event");

        assert_eq!(
            msg,
            WatchEvent::Changes {
                changes: vec![FileChange::Created { path }]
            }
        );
    }

    #[test]
    fn manual_watcher_combines_watch_modes_for_repeated_watches() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let mut watcher = ManualFileWatcher::new();

        watcher.watch_path(&root, WatchMode::NonRecursive).unwrap();
        watcher.watch_path(&root, WatchMode::Recursive).unwrap();
        assert_eq!(
            watcher.watched_paths(),
            vec![(root.clone(), WatchMode::Recursive)]
        );

        // Do not downgrade an existing recursive watch.
        watcher.watch_path(&root, WatchMode::NonRecursive).unwrap();
        assert_eq!(watcher.watched_paths(), vec![(root, WatchMode::Recursive)]);
    }

    #[test]
    fn manual_watcher_delivers_errors() {
        let watcher = ManualFileWatcher::new();

        watcher.push_error(io::Error::other("boom")).unwrap();

        let err = watcher
            .receiver()
            .recv_timeout(Duration::from_secs(1))
            .expect("watch error")
            .expect_err("expected error");

        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert_eq!(err.to_string(), "boom");
    }

    #[test]
    fn manual_watcher_handle_can_inject_events() {
        let watcher = ManualFileWatcher::new();
        let handle = watcher.handle();

        let path = VfsPath::local("/tmp/Main.java");
        handle
            .push(WatchEvent::Changes {
                changes: vec![FileChange::Created { path: path.clone() }],
            })
            .unwrap();

        let msg = watcher
            .receiver()
            .recv_timeout(Duration::from_secs(1))
            .expect("watch event")
            .expect("ok event");

        assert_eq!(
            msg,
            WatchEvent::Changes {
                changes: vec![FileChange::Created { path }]
            }
        );
    }

    #[test]
    fn manual_watcher_queue_is_bounded() {
        let watcher = ManualFileWatcher::new();

        // Fill the bounded queue without draining it.
        for _ in 0..MANUAL_WATCH_QUEUE_CAPACITY {
            watcher.push(WatchEvent::Rescan).unwrap();
        }

        // The next push should fail instead of allowing unbounded growth.
        let err = watcher
            .push(WatchEvent::Rescan)
            .expect_err("expected ManualFileWatcher queue to be full");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

        // Drain a message, then ensure we can push again.
        watcher
            .receiver()
            .recv_timeout(Duration::from_secs(1))
            .expect("watch event")
            .expect("ok event");
        watcher.push(WatchEvent::Rescan).unwrap();
    }
}

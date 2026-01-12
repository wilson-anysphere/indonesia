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
//!     watching (e.g. `nova-lsp`, `nova-cli`, `nova-workspace`), not by low-level library crates.
//!   - If you add another backend, keep it in `nova-vfs` and feature-gate it similarly (optional
//!     dependency + `watch-*` feature), so other crates don't take on extra OS-specific deps.
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
//!
//! Backends are allowed to be *lossy* and the OS can legitimately coalesce/reorder events; this is
//! unavoidable in practice. The goal is to provide a stable "best effort" stream that higher
//! layers can batch/debounce.
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
//! - Cross-root moves (e.g. between watched roots) may not be pairable, depending on the backend.
//!
//! # Testing
//!
//! Avoid tests that rely on real OS watcher timing (sleeping and hoping the watcher fires). They
//! tend to be flaky on CI and across platforms.
//!
//! Instead, prefer a deterministic injected watcher (see [`ManualFileWatcher`]) or direct calls
//! into higher-level "apply events" APIs. See `docs/file-watching.md` for guidance.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use crossbeam_channel as channel;

use crate::change::FileChange;

/// An event produced by a file watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
    /// One or more normalized file changes.
    ///
    /// Backends may batch multiple changes together to reduce overhead.
    pub changes: Vec<FileChange>,
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
/// 1. Register roots to watch with [`watch_root`](FileWatcher::watch_root).
/// 2. Consume events from [`receiver`](FileWatcher::receiver).
///
/// Notes:
///
/// - Watchers are allowed to coalesce events.
/// - Consumers should treat events as *hints* and consult the filesystem/VFS for the authoritative
///   state.
pub trait FileWatcher: Send {
    /// Begin watching `root` recursively.
    fn watch_root(&mut self, root: &Path) -> io::Result<()>;

    /// Stop watching `root`.
    fn unwatch_root(&mut self, root: &Path) -> io::Result<()>;

    /// Returns the receiver used to consume watcher events.
    fn receiver(&self) -> &channel::Receiver<WatchMessage>;
}

/// Deterministic watcher implementation for tests.
///
/// This watcher does not interact with the OS. Callers inject events manually via
/// [`ManualFileWatcher::push`].
#[derive(Debug)]
pub struct ManualFileWatcher {
    tx: channel::Sender<WatchMessage>,
    rx: channel::Receiver<WatchMessage>,
    watch_calls: Vec<PathBuf>,
    unwatch_calls: Vec<PathBuf>,
    watched: HashSet<PathBuf>,
}

impl Default for ManualFileWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ManualFileWatcher {
    pub fn new() -> Self {
        let (tx, rx) = channel::unbounded();
        Self {
            tx,
            rx,
            watch_calls: Vec::new(),
            unwatch_calls: Vec::new(),
            watched: HashSet::new(),
        }
    }

    /// Inject a synthetic watcher event.
    pub fn push(&self, event: WatchEvent) -> io::Result<()> {
        self.tx
            .send(Ok(event))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "watch receiver dropped"))
    }

    /// Inject an asynchronous watcher error.
    pub fn push_error(&self, error: io::Error) -> io::Result<()> {
        self.tx
            .send(Err(error))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "watch receiver dropped"))
    }

    /// Paths passed to [`FileWatcher::watch_root`] (in call order).
    pub fn watch_calls(&self) -> &[PathBuf] {
        &self.watch_calls
    }

    /// Paths passed to [`FileWatcher::unwatch_root`] (in call order).
    pub fn unwatch_calls(&self) -> &[PathBuf] {
        &self.unwatch_calls
    }

    /// Returns the set of currently watched roots (sorted for determinism).
    pub fn watched_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = self.watched.iter().cloned().collect();
        roots.sort();
        roots
    }
}

impl FileWatcher for ManualFileWatcher {
    fn watch_root(&mut self, root: &Path) -> io::Result<()> {
        let root = root.to_path_buf();
        self.watch_calls.push(root.clone());
        self.watched.insert(root);
        Ok(())
    }

    fn unwatch_root(&mut self, root: &Path) -> io::Result<()> {
        let root = root.to_path_buf();
        self.unwatch_calls.push(root.clone());
        self.watched.remove(&root);
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

    #[derive(Debug, Default)]
    struct EventNormalizer {
        pending_renames: VecDeque<(Instant, VfsPath)>,
    }

    impl EventNormalizer {
        const MAX_AGE: Duration = Duration::from_secs(2);
        const MAX_PENDING_RENAMES: usize = 512;

        fn new() -> Self {
            Self {
                pending_renames: VecDeque::new(),
            }
        }

        fn push(&mut self, event: notify::Event, now: Instant) -> Vec<FileChange> {
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
        fn flush(&mut self, now: Instant) -> Vec<FileChange> {
            self.gc_pending(now)
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

    #[cfg(feature = "watch-notify")]
    fn notify_error_to_io(err: notify::Error) -> io::Error {
        io::Error::other(err)
    }

    #[cfg(feature = "watch-notify")]
    pub struct NotifyFileWatcher {
        watcher: notify::RecommendedWatcher,
        events_rx: channel::Receiver<WatchMessage>,
        stop_tx: channel::Sender<()>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    #[cfg(feature = "watch-notify")]
    impl NotifyFileWatcher {
        pub fn new() -> io::Result<Self> {
            let (raw_tx, raw_rx) = channel::unbounded::<notify::Result<notify::Event>>();
            let (events_tx, events_rx) = channel::unbounded::<WatchMessage>();
            let (stop_tx, stop_rx) = channel::bounded::<()>(0);

            let watcher = notify::recommended_watcher(move |res| {
                let _ = raw_tx.send(res);
            })
            .map_err(notify_error_to_io)?;

            let thread = std::thread::spawn(move || {
                let mut normalizer = EventNormalizer::new();
                loop {
                    let tick = match normalizer.pending_renames.front() {
                        Some((started_at, _)) => {
                            let now = Instant::now();
                            let deadline = *started_at + EventNormalizer::MAX_AGE;
                            let timeout = deadline.saturating_duration_since(now);
                            channel::after(timeout)
                        }
                        None => channel::after(Duration::from_secs(3600)),
                    };

                    channel::select! {
                        recv(stop_rx) -> _ => {
                            // Flush any pending rename-froms so they aren't silently dropped when
                            // shutting down the watcher.
                            let changes = normalizer.flush(Instant::now());
                            if !changes.is_empty() {
                                let _ = events_tx.send(Ok(WatchEvent { changes }));
                            }
                            break;
                        },
                        recv(raw_rx) -> msg => {
                            let Ok(res) = msg else { break };
                            match res {
                                Ok(event) => {
                                    let now = Instant::now();
                                    let changes = normalizer.push(event, now);
                                    if !changes.is_empty() {
                                        let _ = events_tx.send(Ok(WatchEvent { changes }));
                                    }
                                }
                                Err(err) => {
                                    let _ = events_tx.send(Err(notify_error_to_io(err)));
                                }
                            }
                        }
                        recv(tick) -> _ => {
                            let changes = normalizer.flush(Instant::now());
                            if !changes.is_empty() {
                                let _ = events_tx.send(Ok(WatchEvent { changes }));
                            }
                        }
                    }
                }
            });

            Ok(Self {
                watcher,
                events_rx,
                stop_tx,
                thread: Some(thread),
            })
        }
    }

    #[cfg(feature = "watch-notify")]
    impl Drop for NotifyFileWatcher {
        fn drop(&mut self) {
            let _ = self.stop_tx.send(());
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[cfg(feature = "watch-notify")]
    impl FileWatcher for NotifyFileWatcher {
        fn watch_root(&mut self, root: &Path) -> io::Result<()> {
            use notify::Watcher;
            self.watcher
                .watch(root, notify::RecursiveMode::Recursive)
                .map_err(notify_error_to_io)
        }

        fn unwatch_root(&mut self, root: &Path) -> io::Result<()> {
            use notify::Watcher;
            self.watcher.unwatch(root).map_err(notify_error_to_io)
        }

        fn receiver(&self) -> &channel::Receiver<WatchMessage> {
            &self.events_rx
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        use notify::event::{ModifyKind, RenameMode};

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
    }
}

#[cfg(feature = "watch-notify")]
pub use notify_impl::NotifyFileWatcher;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::VfsPath;
    use std::time::Duration;

    #[test]
    fn manual_watcher_tracks_roots_and_delivers_events() {
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("a");
        let root_b = tmp.path().join("b");

        let mut watcher = ManualFileWatcher::new();
        watcher.watch_root(&root_a).unwrap();
        watcher.watch_root(&root_b).unwrap();
        watcher.unwatch_root(&root_a).unwrap();

        assert_eq!(watcher.watch_calls(), &[root_a.clone(), root_b.clone()]);
        assert_eq!(watcher.unwatch_calls(), std::slice::from_ref(&root_a));
        assert_eq!(watcher.watched_roots(), vec![root_b.clone()]);

        let path = VfsPath::local(root_b.join("Main.java"));
        watcher
            .push(WatchEvent {
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
            WatchEvent {
                changes: vec![FileChange::Created { path }]
            }
        );
    }
}

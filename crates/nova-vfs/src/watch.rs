use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use crossbeam_channel as channel;

use crate::change::FileChange;

/// An event produced by a file watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
    pub changes: Vec<FileChange>,
}

/// Message type delivered by a [`FileWatcher`].
///
/// OS watcher backends may surface errors asynchronously; these are delivered as `Err(io::Error)`
/// values via the same event stream.
pub type WatchMessage = io::Result<WatchEvent>;

/// Event-driven watcher abstraction.
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
        fn new() -> Self {
            Self {
                pending_renames: VecDeque::new(),
            }
        }

        fn push(&mut self, event: notify::Event, now: Instant) -> Vec<FileChange> {
            self.gc_pending(now);

            use notify::event::{ModifyKind, RenameMode};

            match event.kind {
                EventKind::Create(_) => event
                    .paths
                    .into_iter()
                    .map(|path| FileChange::Created {
                        path: VfsPath::local(path),
                    })
                    .collect(),
                EventKind::Remove(_) => event
                    .paths
                    .into_iter()
                    .map(|path| FileChange::Deleted {
                        path: VfsPath::local(path),
                    })
                    .collect(),
                EventKind::Modify(ModifyKind::Data(_))
                | EventKind::Modify(ModifyKind::Metadata(_))
                | EventKind::Modify(ModifyKind::Other)
                | EventKind::Modify(ModifyKind::Any) => event
                    .paths
                    .into_iter()
                    .map(|path| FileChange::Modified {
                        path: VfsPath::local(path),
                    })
                    .collect(),
                EventKind::Modify(ModifyKind::Name(rename_mode)) => match rename_mode {
                    RenameMode::Both => paths_to_moves(event.paths),
                    RenameMode::From => {
                        for path in event.paths {
                            self.pending_renames.push_back((now, VfsPath::local(path)));
                        }
                        Vec::new()
                    }
                    RenameMode::To => {
                        let mut out = Vec::new();
                        for to in event.paths {
                            let to = VfsPath::local(to);
                            if let Some((_, from)) = self.pending_renames.pop_front() {
                                out.push(FileChange::Moved { from, to });
                            } else {
                                out.push(FileChange::Created { path: to });
                            }
                        }
                        out
                    }
                    // Unknown rename variants: treat as modified.
                    RenameMode::Any | RenameMode::Other => event
                        .paths
                        .into_iter()
                        .map(|path| FileChange::Modified {
                            path: VfsPath::local(path),
                        })
                        .collect(),
                },
                // Some backends report a rename as a "modify" without further detail.
                _ => event
                    .paths
                    .into_iter()
                    .map(|path| FileChange::Modified {
                        path: VfsPath::local(path),
                    })
                    .collect(),
            }
        }

        fn gc_pending(&mut self, now: Instant) {
            const MAX_AGE: Duration = Duration::from_secs(2);
            while let Some((t, _)) = self.pending_renames.front() {
                if now.duration_since(*t) <= MAX_AGE {
                    break;
                }
                self.pending_renames.pop_front();
            }

            // Bound memory for rename storms.
            while self.pending_renames.len() > 512 {
                self.pending_renames.pop_front();
            }
        }
    }

    fn paths_to_moves(paths: Vec<PathBuf>) -> Vec<FileChange> {
        let mut out = Vec::new();
        let mut it = paths.into_iter().map(VfsPath::local);
        loop {
            let Some(from) = it.next() else { break };
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
        io::Error::new(io::ErrorKind::Other, err)
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
                    channel::select! {
                        recv(stop_rx) -> _ => break,
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
        fn normalize_rename_from_to_into_move() {
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

            assert!(normalizer.push(ev_from, t0).is_empty());
            assert_eq!(
                normalizer.push(ev_to, t0),
                vec![FileChange::Moved {
                    from: VfsPath::local(from),
                    to: VfsPath::local(to)
                }]
            );
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
        fn unmatched_rename_from_is_garbage_collected() {
            let mut normalizer = EventNormalizer::new();
            let t0 = Instant::now();

            let from = PathBuf::from("/tmp/A.java");
            let to = PathBuf::from("/tmp/B.java");

            let ev_from = notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                paths: vec![from],
                attrs: Default::default(),
            };
            assert!(normalizer.push(ev_from, t0).is_empty());

            // Force GC to discard the pending rename-from.
            let t1 = t0 + Duration::from_secs(3);
            let ev_to = notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                paths: vec![to.clone()],
                attrs: Default::default(),
            };
            assert_eq!(
                normalizer.push(ev_to, t1),
                vec![FileChange::Created {
                    path: VfsPath::local(to)
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
        assert_eq!(watcher.unwatch_calls(), &[root_a.clone()]);
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

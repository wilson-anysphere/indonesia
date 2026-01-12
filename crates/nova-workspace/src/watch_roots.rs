//! Watch-root management for workspace file watching.
//!
//! `nova-workspace` builds file watching on `nova_vfs::FileWatcher`, which exposes a minimal API:
//! callers add/remove roots to watch (`watch_root` / `unwatch_root`) and then consume events from the
//! watcher's receiver.
//!
//! In Nova, the set of roots that should be watched can change over time:
//!
//! - Project reloads can discover new `source_roots` / `generated_source_roots`.
//! - Generated roots may not exist yet when watching starts (e.g. build outputs).
//!
//! [`WatchRootManager`] reconciles the *desired* set of roots with the currently watched set,
//! handling adds/removes deterministically and retrying roots that are temporarily missing.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nova_vfs::FileWatcher;

pub(crate) trait RootWatcher {
    fn watch_root(&mut self, root: &Path) -> io::Result<()>;
    fn unwatch_root(&mut self, root: &Path) -> io::Result<()>;
}

impl<T> RootWatcher for T
where
    T: FileWatcher,
{
    fn watch_root(&mut self, root: &Path) -> io::Result<()> {
        FileWatcher::watch_root(self, root)
    }

    fn unwatch_root(&mut self, root: &Path) -> io::Result<()> {
        FileWatcher::unwatch_root(self, root)
    }
}

#[derive(Debug)]
pub(crate) enum WatchRootError {
    WatchFailed { root: PathBuf, error: io::Error },
    UnwatchFailed { root: PathBuf, error: io::Error },
}

#[derive(Debug)]
pub(crate) struct WatchRootManager {
    retry_interval: Duration,
    desired_roots: HashSet<PathBuf>,
    watched_roots: HashSet<PathBuf>,
    pending_roots: HashMap<PathBuf, Instant>,
}

impl WatchRootManager {
    pub(crate) fn new(retry_interval: Duration) -> Self {
        Self {
            retry_interval,
            desired_roots: HashSet::new(),
            watched_roots: HashSet::new(),
            pending_roots: HashMap::new(),
        }
    }

    pub(crate) fn watched_roots(&self) -> &HashSet<PathBuf> {
        &self.watched_roots
    }

    pub(crate) fn pending_roots(&self) -> &HashMap<PathBuf, Instant> {
        &self.pending_roots
    }

    pub(crate) fn set_desired_roots<W: RootWatcher>(
        &mut self,
        desired: HashSet<PathBuf>,
        now: Instant,
        watcher: &mut W,
    ) -> Vec<WatchRootError> {
        let mut out = Vec::new();

        let mut removed: Vec<PathBuf> = self
            .desired_roots
            .difference(&desired)
            .cloned()
            .collect();
        removed.sort();

        let mut added: Vec<PathBuf> = desired.difference(&self.desired_roots).cloned().collect();
        added.sort();

        self.desired_roots = desired;

        for root in removed {
            self.pending_roots.remove(&root);
            if self.watched_roots.remove(&root) {
                if let Err(err) = watcher.unwatch_root(&root) {
                    out.push(WatchRootError::UnwatchFailed { root, error: err });
                }
            }
        }

        for root in added {
            self.try_watch_root(&root, now, watcher, &mut out);
        }

        out
    }

    pub(crate) fn retry_pending<W: RootWatcher>(
        &mut self,
        now: Instant,
        watcher: &mut W,
    ) -> Vec<WatchRootError> {
        let mut out = Vec::new();

        let mut due: Vec<PathBuf> = self
            .pending_roots
            .iter()
            .filter_map(|(root, deadline)| (*deadline <= now).then(|| root.clone()))
            .collect();
        due.sort();

        for root in due {
            // Skip if the root is no longer desired.
            if !self.desired_roots.contains(&root) {
                self.pending_roots.remove(&root);
                continue;
            }

            self.pending_roots.remove(&root);
            self.try_watch_root(&root, now, watcher, &mut out);
        }

        out
    }

    fn try_watch_root<W: RootWatcher>(
        &mut self,
        root: &Path,
        now: Instant,
        watcher: &mut W,
        errors: &mut Vec<WatchRootError>,
    ) {
        match watcher.watch_root(root) {
            Ok(()) => {
                self.watched_roots.insert(root.to_path_buf());
                self.pending_roots.remove(root);
            }
            Err(err) => {
                if should_retry_watch_error(root, &err) {
                    self.pending_roots
                        .insert(root.to_path_buf(), now + self.retry_interval);
                } else {
                    errors.push(WatchRootError::WatchFailed {
                        root: root.to_path_buf(),
                        error: err,
                    });
                }
            }
        }
    }
}

fn should_retry_watch_error(root: &Path, err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::NotFound {
        return true;
    }
    let meta = std::fs::metadata(root);
    matches!(meta, Err(meta_err) if meta_err.kind() == io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct StubWatcher {
        fail_not_found: HashSet<PathBuf>,
        watch_calls: Vec<PathBuf>,
        unwatch_calls: Vec<PathBuf>,
    }

    impl RootWatcher for StubWatcher {
        fn watch_root(&mut self, root: &Path) -> io::Result<()> {
            let root = root.to_path_buf();
            self.watch_calls.push(root.clone());
            if self.fail_not_found.contains(&root) {
                return Err(io::Error::new(io::ErrorKind::NotFound, "missing"));
            }
            Ok(())
        }

        fn unwatch_root(&mut self, root: &Path) -> io::Result<()> {
            self.unwatch_calls.push(root.to_path_buf());
            Ok(())
        }
    }

    #[test]
    fn retries_missing_roots_until_they_can_be_watched() {
        let retry_interval = Duration::from_secs(1);
        let mut manager = WatchRootManager::new(retry_interval);
        let mut watcher = StubWatcher::default();

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("generated-src");
        watcher.fail_not_found.insert(root.clone());

        let t0 = Instant::now();
        let desired: HashSet<PathBuf> = [root.clone()].into_iter().collect();
        manager.set_desired_roots(desired, t0, &mut watcher);

        assert_eq!(watcher.watch_calls.len(), 1);
        assert!(manager.pending_roots().contains_key(&root));
        assert!(!manager.watched_roots().contains(&root));

        // Not due yet.
        manager.retry_pending(t0 + Duration::from_millis(500), &mut watcher);
        assert_eq!(watcher.watch_calls.len(), 1);

        watcher.fail_not_found.remove(&root);
        manager.retry_pending(t0 + retry_interval, &mut watcher);

        assert_eq!(watcher.watch_calls.len(), 2);
        assert!(!manager.pending_roots().contains_key(&root));
        assert!(manager.watched_roots().contains(&root));
    }

    #[test]
    fn removed_roots_are_not_retried() {
        let retry_interval = Duration::from_secs(1);
        let mut manager = WatchRootManager::new(retry_interval);
        let mut watcher = StubWatcher::default();

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("generated-src");
        watcher.fail_not_found.insert(root.clone());

        let t0 = Instant::now();
        let desired: HashSet<PathBuf> = [root.clone()].into_iter().collect();
        manager.set_desired_roots(desired, t0, &mut watcher);
        assert_eq!(watcher.watch_calls.len(), 1);
        assert!(manager.pending_roots().contains_key(&root));

        // Config refresh removes the root.
        manager.set_desired_roots(HashSet::new(), t0, &mut watcher);
        assert!(!manager.pending_roots().contains_key(&root));

        watcher.fail_not_found.remove(&root);
        manager.retry_pending(t0 + retry_interval, &mut watcher);
        assert_eq!(watcher.watch_calls.len(), 1);
    }

    #[test]
    fn removed_watched_roots_are_unwatched() {
        let retry_interval = Duration::from_secs(1);
        let mut manager = WatchRootManager::new(retry_interval);
        let mut watcher = StubWatcher::default();

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("external-src");

        let t0 = Instant::now();
        let desired: HashSet<PathBuf> = [root.clone()].into_iter().collect();
        manager.set_desired_roots(desired, t0, &mut watcher);
        assert!(manager.watched_roots().contains(&root));

        manager.set_desired_roots(HashSet::new(), t0, &mut watcher);
        assert!(!manager.watched_roots().contains(&root));
        assert_eq!(watcher.unwatch_calls, vec![root]);
    }
}

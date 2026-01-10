use std::io;

use crate::change::FileChange;

/// An event produced by a file watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
    pub changes: Vec<FileChange>,
}

/// Minimal watcher abstraction to allow plugging in a real implementation later (e.g. `notify`).
pub trait FileWatcher: Send {
    /// Begin watching `root` recursively.
    fn watch_root(&mut self, root: &std::path::Path) -> io::Result<()>;

    /// Stop watching `root`.
    fn unwatch_root(&mut self, root: &std::path::Path) -> io::Result<()>;

    /// Retrieves pending events, if any.
    fn poll(&mut self) -> io::Result<Vec<WatchEvent>>;
}


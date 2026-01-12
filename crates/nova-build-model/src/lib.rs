use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClasspathEntryKind {
    Directory,
    Jar,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClasspathEntry {
    pub kind: ClasspathEntryKind,
    pub path: PathBuf,
}

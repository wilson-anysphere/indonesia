use nova_core::FileId;
use nova_types::{ClassId, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeAction {
    pub title: String,
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Symbol {
    File(FileId),
    Class(ClassId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationTarget {
    pub file: FileId,
    pub span: Option<Span>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHint {
    pub span: Option<Span>,
    pub label: String,
}

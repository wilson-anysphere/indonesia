use nova_syntax::{SyntaxKind, SyntaxNode, TextRange};
use nova_types::Span;
use std::collections::HashMap;
use std::fmt;

/// Identifies a syntax node *within a single file*.
///
/// `AstId`s are assigned by [`AstIdMap`] in a deterministic preorder walk of the
/// rowan syntax tree, filtered down to nodes relevant to HIR lowering (type
/// declarations, members, and blocks).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AstId(u32);

impl AstId {
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn to_raw(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for AstId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AstId({})", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AstPtr {
    pub kind: SyntaxKind,
    pub range: TextRange,
}

impl AstPtr {
    #[must_use]
    pub fn span(self) -> Span {
        text_range_to_span(self.range)
    }
}

/// A bidirectional mapping between [`AstId`] and rowan syntax nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstIdMap {
    nodes: Vec<AstPtr>,
    by_ptr: HashMap<AstPtr, AstId>,
}

impl AstIdMap {
    #[must_use]
    pub fn new(root: &SyntaxNode) -> Self {
        let mut nodes = Vec::new();
        let mut by_ptr = HashMap::new();

        for node in root.descendants() {
            if !is_relevant_node(&node) {
                continue;
            }

            let kind = node.kind();
            let range = text_range_of(&node);
            let ptr = AstPtr { kind, range };
            let id = AstId::new(nodes.len() as u32);
            nodes.push(ptr);
            by_ptr.insert(ptr, id);
        }

        Self { nodes, by_ptr }
    }

    #[must_use]
    pub fn ast_id(&self, node: &SyntaxNode) -> Option<AstId> {
        if !is_relevant_node(node) {
            return None;
        }
        let kind = node.kind();
        let range = text_range_of(node);
        self.by_ptr.get(&AstPtr { kind, range }).copied()
    }

    #[must_use]
    pub fn ast_id_for_ptr(&self, kind: SyntaxKind, range: TextRange) -> Option<AstId> {
        self.by_ptr.get(&AstPtr { kind, range }).copied()
    }

    #[must_use]
    pub fn ptr(&self, id: AstId) -> Option<AstPtr> {
        self.nodes.get(id.to_raw() as usize).copied()
    }

    #[must_use]
    pub fn range(&self, id: AstId) -> Option<TextRange> {
        self.ptr(id).map(|ptr| ptr.range)
    }

    #[must_use]
    pub fn span(&self, id: AstId) -> Option<Span> {
        self.range(id).map(text_range_to_span)
    }
}

fn is_relevant_node(node: &SyntaxNode) -> bool {
    match node.kind() {
        SyntaxKind::Parameter => node.parent().is_some_and(|parent| {
            parent.kind() == SyntaxKind::ParameterList
                && parent
                    .parent()
                    .is_some_and(|grandparent| grandparent.kind() == SyntaxKind::RecordDeclaration)
        }),
        kind => matches!(
            kind,
            SyntaxKind::ClassDeclaration
                | SyntaxKind::InterfaceDeclaration
                | SyntaxKind::EnumDeclaration
                | SyntaxKind::RecordDeclaration
                | SyntaxKind::AnnotationTypeDeclaration
                | SyntaxKind::FieldDeclaration
                | SyntaxKind::EnumConstant
                | SyntaxKind::VariableDeclarator
                | SyntaxKind::MethodDeclaration
                | SyntaxKind::ConstructorDeclaration
                | SyntaxKind::CompactConstructorDeclaration
                | SyntaxKind::InitializerBlock
                | SyntaxKind::Block
                | SyntaxKind::ModuleDeclaration
        ),
    }
}

fn text_range_of(node: &SyntaxNode) -> TextRange {
    let range = node.text_range();
    TextRange {
        start: range.start().into(),
        end: range.end().into(),
    }
}

#[must_use]
pub fn text_range_to_span(range: TextRange) -> Span {
    Span::new(range.start as usize, range.end as usize)
}

#[must_use]
pub fn span_to_text_range(span: Span) -> TextRange {
    let start = u32::try_from(span.start).expect("span start does not fit in u32");
    let end = u32::try_from(span.end).expect("span end does not fit in u32");
    TextRange { start, end }
}

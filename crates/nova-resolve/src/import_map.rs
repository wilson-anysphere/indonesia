use nova_core::{Name, QualifiedName};
use nova_hir::item_tree;
use nova_types::Span;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportMap {
    pub type_single: Vec<TypeSingleImport>,
    pub type_star: Vec<TypeStarImport>,
    pub static_single: Vec<StaticSingleImport>,
    pub static_star: Vec<StaticStarImport>,
}

impl ImportMap {
    #[must_use]
    pub fn from_item_tree(tree: &item_tree::ItemTree) -> Self {
        let mut out = ImportMap::default();

        for import in &tree.imports {
            if import.path.trim().is_empty() {
                continue;
            }

            match (import.is_static, import.is_star) {
                (false, false) => {
                    let path = QualifiedName::from_dotted(&import.path);
                    let imported = path
                        .last()
                        .cloned()
                        .unwrap_or_else(|| Name::from(import.path.as_str()));
                    out.type_single.push(TypeSingleImport {
                        path,
                        imported,
                        range: import.range,
                    });
                }
                (false, true) => {
                    out.type_star.push(TypeStarImport {
                        path: QualifiedName::from_dotted(&import.path),
                        range: import.range,
                    });
                }
                (true, false) => {
                    let path = QualifiedName::from_dotted(&import.path);
                    let segments = path.segments();
                    let Some((member, ty_segments)) = segments.split_last() else {
                        continue;
                    };
                    if ty_segments.is_empty() {
                        continue;
                    }

                    let ty = QualifiedName::from_dotted(
                        &ty_segments
                            .iter()
                            .map(|n| n.as_str())
                            .collect::<Vec<_>>()
                            .join("."),
                    );
                    let member = member.clone();

                    out.static_single.push(StaticSingleImport {
                        ty,
                        member: member.clone(),
                        imported: member,
                        range: import.range,
                    });
                }
                (true, true) => {
                    out.static_star.push(StaticStarImport {
                        ty: QualifiedName::from_dotted(&import.path),
                        range: import.range,
                    });
                }
            }
        }

        out
    }

    /// Best-effort estimate of heap memory usage of this import map in bytes.
    ///
    /// This is intended for cheap, deterministic memory accounting (e.g. Nova's
    /// `nova-memory` budgets). The heuristic is not exact; it intentionally
    /// prioritizes stability over precision.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        fn name_bytes(name: &Name) -> u64 {
            name.as_str().len() as u64
        }

        fn qualified_bytes(path: &QualifiedName) -> u64 {
            let mut bytes = (path.segments().len() as u64).saturating_mul(size_of::<Name>() as u64);
            for seg in path.segments() {
                bytes = bytes.saturating_add(name_bytes(seg));
            }
            bytes
        }

        let mut bytes = 0u64;

        bytes = bytes.saturating_add(
            (self.type_single.capacity() as u64)
                .saturating_mul(size_of::<TypeSingleImport>() as u64),
        );
        for import in &self.type_single {
            bytes = bytes.saturating_add(qualified_bytes(&import.path));
            bytes = bytes.saturating_add(name_bytes(&import.imported));
        }

        bytes = bytes.saturating_add(
            (self.type_star.capacity() as u64).saturating_mul(size_of::<TypeStarImport>() as u64),
        );
        for import in &self.type_star {
            bytes = bytes.saturating_add(qualified_bytes(&import.path));
        }

        bytes = bytes.saturating_add(
            (self.static_single.capacity() as u64)
                .saturating_mul(size_of::<StaticSingleImport>() as u64),
        );
        for import in &self.static_single {
            bytes = bytes.saturating_add(qualified_bytes(&import.ty));
            bytes = bytes.saturating_add(name_bytes(&import.member));
            bytes = bytes.saturating_add(name_bytes(&import.imported));
        }

        bytes = bytes.saturating_add(
            (self.static_star.capacity() as u64)
                .saturating_mul(size_of::<StaticStarImport>() as u64),
        );
        for import in &self.static_star {
            bytes = bytes.saturating_add(qualified_bytes(&import.ty));
        }

        bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeSingleImport {
    pub path: QualifiedName,
    pub imported: Name,
    pub range: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeStarImport {
    /// `import X.*;` where `X` is a `PackageOrTypeName` (JLS 7.5.2).
    ///
    /// `X` can refer to either:
    /// - a package (`import java.util.*;`), or
    /// - a type (`import java.util.Map.*;`), in which case member types can be imported.
    ///
    /// The resolver decides which interpretation applies by consulting the available type
    /// indices and package set.
    pub path: QualifiedName,
    pub range: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticSingleImport {
    pub ty: QualifiedName,
    pub member: Name,
    pub imported: Name,
    pub range: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticStarImport {
    pub ty: QualifiedName,
    pub range: Span,
}

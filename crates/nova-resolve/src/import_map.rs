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

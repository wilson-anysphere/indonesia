use crate::ast_id::AstIdMap;
use crate::hir::Body;
use crate::ids::{ConstructorId, InitializerId, MethodId};
use crate::item_tree::ItemTree;
use crate::lowering::{lower_body, lower_item_tree};
use nova_vfs::FileId;
use std::sync::Arc;

/// Minimal database interface required for the HIR queries.
///
/// The default method implementations in this trait provide a simple, direct
/// lowering pipeline driven off file text. Query engines (like Salsa) can
/// override these methods to provide stable, incremental results without
/// reparsing.
pub trait HirDatabase {
    fn file_text(&self, file: FileId) -> Arc<str>;

    /// Build the file-level [`ItemTree`].
    ///
    /// Item trees are designed to be stable: the same source text produces the
    /// same `ItemTree` structure, enabling early-cutoff in incremental queries.
    #[must_use]
    fn hir_item_tree(&self, file: FileId) -> Arc<ItemTree> {
        let text = self.file_text(file);
        let parse_java = nova_syntax::parse_java(&text);
        let ast_id_map = AstIdMap::new(&parse_java.syntax());
        let parse = nova_syntax::java::parse(&text);
        Arc::new(lower_item_tree(
            file,
            parse.compilation_unit(),
            &parse_java,
            &ast_id_map,
        ))
    }

    /// Lower the body of a method into HIR.
    #[must_use]
    fn hir_body(&self, method: MethodId) -> Arc<Body> {
        let tree = self.hir_item_tree(method.file);
        let method_data = tree.method(method);
        let Some(body_id) = method_data.body else {
            return Arc::new(Body::empty(method_data.range));
        };

        let text = self.file_text(method.file);
        let parse_java = nova_syntax::parse_java(&text);
        let ast_id_map = AstIdMap::new(&parse_java.syntax());
        let Some(body_range) = ast_id_map.span(body_id) else {
            return Arc::new(Body::empty(method_data.range));
        };

        let Some(block_text) = text.get(body_range.start..body_range.end) else {
            return Arc::new(Body::empty(method_data.range));
        };

        let block = nova_syntax::java::parse_block(block_text, body_range.start);
        Arc::new(lower_body(&block))
    }

    /// Lower the body of a constructor into HIR.
    #[must_use]
    fn hir_constructor_body(&self, constructor: ConstructorId) -> Arc<Body> {
        let tree = self.hir_item_tree(constructor.file);
        let data = tree.constructor(constructor);
        let Some(body_id) = data.body else {
            return Arc::new(Body::empty(data.range));
        };
        let text = self.file_text(constructor.file);
        let parse_java = nova_syntax::parse_java(&text);
        let ast_id_map = AstIdMap::new(&parse_java.syntax());
        let Some(body_range) = ast_id_map.span(body_id) else {
            return Arc::new(Body::empty(data.range));
        };

        let Some(block_text) = text.get(body_range.start..body_range.end) else {
            return Arc::new(Body::empty(data.range));
        };

        let block = nova_syntax::java::parse_block(block_text, body_range.start);
        Arc::new(lower_body(&block))
    }

    /// Lower a class or instance initializer block into HIR.
    #[must_use]
    fn hir_initializer_body(&self, initializer: InitializerId) -> Arc<Body> {
        let tree = self.hir_item_tree(initializer.file);
        let data = tree.initializer(initializer);
        let Some(body_id) = data.body else {
            return Arc::new(Body::empty(data.range));
        };
        let text = self.file_text(initializer.file);
        let parse_java = nova_syntax::parse_java(&text);
        let ast_id_map = AstIdMap::new(&parse_java.syntax());
        let Some(body_range) = ast_id_map.span(body_id) else {
            return Arc::new(Body::empty(data.range));
        };

        let Some(block_text) = text.get(body_range.start..body_range.end) else {
            return Arc::new(Body::empty(data.range));
        };

        let block = nova_syntax::java::parse_block(block_text, body_range.start);
        Arc::new(lower_body(&block))
    }
}

/// Build the file-level [`ItemTree`].
///
/// Item trees are designed to be stable: the same source text produces the
/// same `ItemTree` structure, enabling early-cutoff in incremental queries.
#[must_use]
pub fn item_tree(db: &dyn HirDatabase, file: FileId) -> Arc<ItemTree> {
    db.hir_item_tree(file)
}

/// Lower the body of a method into HIR.
#[must_use]
pub fn body(db: &dyn HirDatabase, method: MethodId) -> Arc<Body> {
    db.hir_body(method)
}

/// Lower the body of a constructor into HIR.
#[must_use]
pub fn constructor_body(db: &dyn HirDatabase, constructor: ConstructorId) -> Arc<Body> {
    db.hir_constructor_body(constructor)
}

/// Lower a class or instance initializer block into HIR.
#[must_use]
pub fn initializer_body(db: &dyn HirDatabase, initializer: InitializerId) -> Arc<Body> {
    db.hir_initializer_body(initializer)
}

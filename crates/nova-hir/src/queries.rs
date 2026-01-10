use crate::hir::Body;
use crate::ids::{ConstructorId, InitializerId, MethodId};
use crate::item_tree::ItemTree;
use crate::lowering::{lower_body, lower_item_tree, slice_range};
use nova_vfs::FileId;
use std::sync::Arc;

/// Minimal database interface required for the HIR queries.
///
/// `nova-db` will eventually provide an incremental query system on top of this
/// API.
pub trait HirDatabase {
    fn file_text(&self, file: FileId) -> Arc<str>;
}

/// Build the file-level [`ItemTree`].
///
/// Item trees are designed to be stable: the same source text produces the
/// same `ItemTree` structure, enabling early-cutoff in incremental queries.
#[must_use]
pub fn item_tree(db: &dyn HirDatabase, file: FileId) -> Arc<ItemTree> {
    let text = db.file_text(file);
    let parse = nova_syntax::java::parse(&text);
    Arc::new(lower_item_tree(file, parse.compilation_unit()))
}

/// Lower the body of a method into HIR.
#[must_use]
pub fn body(db: &dyn HirDatabase, method: MethodId) -> Arc<Body> {
    let tree = item_tree(db, method.file);
    let method_data = tree.method(method);
    let Some(body_range) = method_data.body_range else {
        return Arc::new(Body::empty(method_data.range));
    };

    let text = db.file_text(method.file);
    let block_text = slice_range(&text, body_range);
    let block = nova_syntax::java::parse_block(block_text, body_range.start);
    Arc::new(lower_body(&block))
}

/// Lower the body of a constructor into HIR.
#[must_use]
pub fn constructor_body(db: &dyn HirDatabase, constructor: ConstructorId) -> Arc<Body> {
    let tree = item_tree(db, constructor.file);
    let data = tree.constructor(constructor);
    let body_range = data.body_range;
    let text = db.file_text(constructor.file);
    let block_text = slice_range(&text, body_range);
    let block = nova_syntax::java::parse_block(block_text, body_range.start);
    Arc::new(lower_body(&block))
}

/// Lower a class or instance initializer block into HIR.
#[must_use]
pub fn initializer_body(db: &dyn HirDatabase, initializer: InitializerId) -> Arc<Body> {
    let tree = item_tree(db, initializer.file);
    let data = tree.initializer(initializer);
    let body_range = data.body_range;
    let text = db.file_text(initializer.file);
    let block_text = slice_range(&text, body_range);
    let block = nova_syntax::java::parse_block(block_text, body_range.start);
    Arc::new(lower_body(&block))
}

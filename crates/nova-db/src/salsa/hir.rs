use std::sync::Arc;
use std::time::Instant;

use nova_hir::{
    ast_id::AstIdMap,
    hir::Body as HirBody,
    ids::{ConstructorId, InitializerId, MethodId},
    item_tree::{Item as HirItem, ItemTree as HirItemTree, Member as HirMember},
    lowering::{lower_body_with, lower_item_tree_with},
};

use crate::FileId;

use super::cancellation as cancel;
use super::stats::HasQueryStats;
use super::syntax::NovaSyntax;
use super::{TrackedSalsaBodyMemo, TrackedSalsaMemo};
use nova_resolve::ids::DefWithBodyId;

#[ra_salsa::query_group(NovaHirStorage)]
pub trait NovaHir: NovaSyntax + HasQueryStats {
    /// Parse a file using the lightweight syntax layer used by semantic lowering.
    fn java_parse(&self, file: FileId) -> Arc<nova_syntax::java::Parse>;

    /// Stable mapping between syntax nodes and per-file [`nova_hir::ast_id::AstId`]s.
    fn hir_ast_id_map(&self, file: FileId) -> Arc<AstIdMap>;

    /// File-level item tree lowered into Nova's stable semantic substrate.
    fn hir_item_tree(&self, file: FileId) -> Arc<HirItemTree>;

    /// Lower the body of a method into HIR.
    fn hir_body(&self, method: MethodId) -> Arc<HirBody>;

    /// Lower the body of a constructor into HIR.
    fn hir_constructor_body(&self, constructor: ConstructorId) -> Arc<HirBody>;

    /// Lower the body of a class/instance initializer block into HIR.
    fn hir_initializer_body(&self, initializer: InitializerId) -> Arc<HirBody>;

    /// Derived query used by tests to validate early-cutoff behavior.
    fn hir_symbol_names(&self, file: FileId) -> Arc<Vec<String>>;
}

fn java_parse(db: &dyn NovaHir, file: FileId) -> Arc<nova_syntax::java::Parse> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "java_parse", ?file).entered();

    cancel::check_cancelled(db);

    let parse_java = db.parse_java(file);
    let root = parse_java.syntax();
    // Avoid re-reading `file_content` just to compute `text.len()`: the parsed
    // syntax tree's range is always file-relative (`0..text_len`).
    let text_len = u32::from(root.text_range().end()) as usize;
    let parsed = nova_syntax::java::parse_with_syntax(&root, text_len);
    let result = Arc::new(parsed);
    db.record_salsa_memo_bytes(file, TrackedSalsaMemo::JavaParse, text_len as u64);
    db.record_query_stat("java_parse", start.elapsed());
    result
}

fn hir_ast_id_map(db: &dyn NovaHir, file: FileId) -> Arc<AstIdMap> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "hir_ast_id_map", ?file).entered();

    cancel::check_cancelled(db);

    let parse_java = db.parse_java(file);
    let syntax = parse_java.syntax();
    let map = AstIdMap::new(&syntax);
    let result = Arc::new(map);
    db.record_salsa_memo_bytes(
        file,
        TrackedSalsaMemo::HirAstIdMap,
        result.estimated_bytes(),
    );
    db.record_query_stat("hir_ast_id_map", start.elapsed());
    result
}

fn hir_item_tree(db: &dyn NovaHir, file: FileId) -> Arc<HirItemTree> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "hir_item_tree", ?file).entered();

    cancel::check_cancelled(db);

    let parse = db.java_parse(file);
    let parse_java = db.parse_java(file);
    let ast_id_map = db.hir_ast_id_map(file);
    let mut steps: u32 = 0;
    let mut check_cancelled = || {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);
    };
    let tree = lower_item_tree_with(
        file,
        parse.compilation_unit(),
        parse_java.as_ref(),
        ast_id_map.as_ref(),
        &mut check_cancelled,
    );

    let result = Arc::new(tree);
    // NOTE: This is a best-effort estimate intended for memory pressure heuristics. HIR item trees
    // can be significantly larger than the raw file text due to storing identifiers, spans, and
    // nested item data, so we apply a small multiplier.
    let approx_bytes = if db.file_exists(file) {
        let text = db.file_content(file);
        (text.len() as u64).saturating_mul(2)
    } else {
        0
    };
    db.record_salsa_memo_bytes(file, TrackedSalsaMemo::HirItemTree, approx_bytes);
    db.record_query_stat("hir_item_tree", start.elapsed());
    result
}

fn hir_body(db: &dyn NovaHir, method: MethodId) -> Arc<HirBody> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "hir_body", ?method).entered();

    cancel::check_cancelled(db);

    let owner = DefWithBodyId::Method(method);

    let tree = db.hir_item_tree(method.file);
    let method_data = tree.method(method);
    let Some(body) = method_data.body else {
        let result = Arc::new(HirBody::empty(method_data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::HirBody, 0);
        db.record_query_stat("hir_body", start.elapsed());
        return result;
    };

    let ast_id_map = db.hir_ast_id_map(method.file);
    let Some(body_range) = ast_id_map.span(body) else {
        let result = Arc::new(HirBody::empty(method_data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::HirBody, 0);
        db.record_query_stat("hir_body", start.elapsed());
        return result;
    };

    let text = if db.file_exists(method.file) {
        db.file_content(method.file)
    } else {
        Arc::new(String::new())
    };

    let Some(block_text) = text.get(body_range.start..body_range.end) else {
        let result = Arc::new(HirBody::empty(method_data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::HirBody, 0);
        db.record_query_stat("hir_body", start.elapsed());
        return result;
    };

    let block = nova_syntax::java::parse_block(block_text, body_range.start);
    let mut steps: u32 = 0;
    let mut check_cancelled = || {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);
    };
    let body = lower_body_with(&block, &mut check_cancelled);

    let result = Arc::new(body);
    // NOTE: Best-effort heuristic for query-cache accounting. Body lowering can allocate
    // significantly more than the raw text (HIR nodes, spans, etc), so apply a small
    // multiplier.
    let approx_bytes = (block_text.len() as u64).saturating_mul(2);
    db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::HirBody, approx_bytes);
    db.record_query_stat("hir_body", start.elapsed());
    result
}

fn hir_constructor_body(db: &dyn NovaHir, constructor: ConstructorId) -> Arc<HirBody> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "hir_constructor_body", ?constructor).entered();

    cancel::check_cancelled(db);

    let owner = DefWithBodyId::Constructor(constructor);

    let tree = db.hir_item_tree(constructor.file);
    let data = tree.constructor(constructor);
    let Some(body_id) = data.body else {
        let result = Arc::new(HirBody::empty(data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::HirBody, 0);
        db.record_query_stat("hir_constructor_body", start.elapsed());
        return result;
    };
    let ast_id_map = db.hir_ast_id_map(constructor.file);
    let Some(body_range) = ast_id_map.span(body_id) else {
        let result = Arc::new(HirBody::empty(data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::HirBody, 0);
        db.record_query_stat("hir_constructor_body", start.elapsed());
        return result;
    };

    let text = if db.file_exists(constructor.file) {
        db.file_content(constructor.file)
    } else {
        Arc::new(String::new())
    };

    let Some(block_text) = text.get(body_range.start..body_range.end) else {
        let result = Arc::new(HirBody::empty(data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::HirBody, 0);
        db.record_query_stat("hir_constructor_body", start.elapsed());
        return result;
    };

    let block = nova_syntax::java::parse_block(block_text, body_range.start);
    let mut steps: u32 = 0;
    let mut check_cancelled = || {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);
    };
    let body = lower_body_with(&block, &mut check_cancelled);

    let result = Arc::new(body);
    let approx_bytes = (block_text.len() as u64).saturating_mul(2);
    db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::HirBody, approx_bytes);
    db.record_query_stat("hir_constructor_body", start.elapsed());
    result
}

fn hir_initializer_body(db: &dyn NovaHir, initializer: InitializerId) -> Arc<HirBody> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "hir_initializer_body", ?initializer).entered();

    cancel::check_cancelled(db);

    let owner = DefWithBodyId::Initializer(initializer);

    let tree = db.hir_item_tree(initializer.file);
    let data = tree.initializer(initializer);
    let Some(body_id) = data.body else {
        let result = Arc::new(HirBody::empty(data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::HirBody, 0);
        db.record_query_stat("hir_initializer_body", start.elapsed());
        return result;
    };
    let ast_id_map = db.hir_ast_id_map(initializer.file);
    let Some(body_range) = ast_id_map.span(body_id) else {
        let result = Arc::new(HirBody::empty(data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::HirBody, 0);
        db.record_query_stat("hir_initializer_body", start.elapsed());
        return result;
    };

    let text = if db.file_exists(initializer.file) {
        db.file_content(initializer.file)
    } else {
        Arc::new(String::new())
    };

    let Some(block_text) = text.get(body_range.start..body_range.end) else {
        let result = Arc::new(HirBody::empty(data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::HirBody, 0);
        db.record_query_stat("hir_initializer_body", start.elapsed());
        return result;
    };

    let block = nova_syntax::java::parse_block(block_text, body_range.start);
    let mut steps: u32 = 0;
    let mut check_cancelled = || {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);
    };
    let body = lower_body_with(&block, &mut check_cancelled);

    let result = Arc::new(body);
    let approx_bytes = (block_text.len() as u64).saturating_mul(2);
    db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::HirBody, approx_bytes);
    db.record_query_stat("hir_initializer_body", start.elapsed());
    result
}

fn hir_symbol_names(db: &dyn NovaHir, file: FileId) -> Arc<Vec<String>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "hir_symbol_names", ?file).entered();

    cancel::check_cancelled(db);

    let tree = db.hir_item_tree(file);
    let mut names = Vec::new();
    for (i, item) in tree.items.iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, i as u32, 16);
        collect_hir_item_names(db, &tree, *item, &mut names);
    }

    let result = Arc::new(names);
    db.record_query_stat("hir_symbol_names", start.elapsed());
    result
}

fn collect_hir_item_names(
    db: &dyn NovaHir,
    tree: &HirItemTree,
    item: HirItem,
    names: &mut Vec<String>,
) {
    cancel::check_cancelled(db);
    match item {
        HirItem::Class(id) => {
            let data = tree.class(id);
            names.push(data.name.clone());
            collect_hir_member_names(db, tree, &data.members, names);
        }
        HirItem::Interface(id) => {
            let data = tree.interface(id);
            names.push(data.name.clone());
            collect_hir_member_names(db, tree, &data.members, names);
        }
        HirItem::Enum(id) => {
            let data = tree.enum_(id);
            names.push(data.name.clone());
            collect_hir_member_names(db, tree, &data.members, names);
        }
        HirItem::Record(id) => {
            let data = tree.record(id);
            names.push(data.name.clone());
            collect_hir_member_names(db, tree, &data.members, names);
        }
        HirItem::Annotation(id) => {
            let data = tree.annotation(id);
            names.push(data.name.clone());
            collect_hir_member_names(db, tree, &data.members, names);
        }
    }
}

fn collect_hir_member_names(
    db: &dyn NovaHir,
    tree: &HirItemTree,
    members: &[HirMember],
    names: &mut Vec<String>,
) {
    for (i, member) in members.iter().enumerate() {
        cancel::checkpoint_cancelled_every(db, i as u32, 16);
        match member {
            HirMember::Field(id) => names.push(tree.field(*id).name.clone()),
            HirMember::Method(id) => names.push(tree.method(*id).name.clone()),
            HirMember::Constructor(id) => names.push(tree.constructor(*id).name.clone()),
            HirMember::Initializer(_) => {}
            HirMember::Type(item) => collect_hir_item_names(db, tree, *item, names),
        }
    }
}

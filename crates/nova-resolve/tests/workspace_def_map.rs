use nova_core::FileId;
use nova_hir::ast_id::AstIdMap;
use nova_hir::lowering::lower_item_tree;
use nova_modules::ModuleName;
use nova_resolve::{DefMap, WorkspaceDefMap};

fn def_map_from_source(file: FileId, source: &str) -> DefMap {
    let parse = nova_syntax::java::parse(source);
    let rowan_parse = nova_syntax::parse_java(source);
    let ast_id_map = AstIdMap::new(&rowan_parse.syntax());
    let tree = lower_item_tree(file, parse.compilation_unit(), &rowan_parse, &ast_id_map);
    DefMap::from_item_tree(file, &tree)
}

#[test]
fn iter_type_names_returns_all_inserted_types() {
    let file_a = FileId::from_raw(0);
    let file_b = FileId::from_raw(1);

    let def_a = def_map_from_source(file_a, "package p; class A { class Inner {} }");
    let def_b = def_map_from_source(file_b, "package p; class B {}");

    let mut workspace = WorkspaceDefMap::default();
    workspace.extend_from_def_map_with_module(&def_a, ModuleName::new("m"));
    workspace.extend_from_def_map(&def_b);

    let names: Vec<&str> = workspace.iter_type_names().map(|n| n.as_str()).collect();
    assert_eq!(names, vec!["p.A", "p.A$Inner", "p.B"]);
}

#[test]
fn iter_type_names_is_sorted_and_deterministic() {
    let file_a = FileId::from_raw(0);
    let file_b = FileId::from_raw(1);

    let def_a = def_map_from_source(file_a, "package p; class A { class Inner {} }");
    let def_b = def_map_from_source(file_b, "package p; class B {}");

    let mut workspace_1 = WorkspaceDefMap::default();
    workspace_1.extend_from_def_map(&def_b);
    workspace_1.extend_from_def_map(&def_a);

    let mut workspace_2 = WorkspaceDefMap::default();
    workspace_2.extend_from_def_map(&def_a);
    workspace_2.extend_from_def_map(&def_b);

    let names_1: Vec<String> = workspace_1
        .iter_type_names()
        .map(|n| n.as_str().to_string())
        .collect();
    let names_2: Vec<String> = workspace_2
        .iter_type_names()
        .map(|n| n.as_str().to_string())
        .collect();

    assert_eq!(names_1, names_2);

    let mut sorted = names_1.clone();
    sorted.sort();
    assert_eq!(names_1, sorted);
}


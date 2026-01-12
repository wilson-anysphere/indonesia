use nova_core::{FileId, Name, PackageName, QualifiedName};
use nova_hir::ast_id::AstIdMap;
use nova_hir::lowering::lower_item_tree;
use nova_resolve::{DefMap, Import};

#[test]
fn def_map_records_imports_and_binary_names() {
    let source = r#"
package p;

import java.util.List;
import java.util.*;
import static java.lang.Math.max;
import static java.lang.Math.*;

class Outer {
    class Inner {
        class Deep {}
    }
}
"#;

    let file = FileId::from_raw(0);
    let parse = nova_syntax::java::parse(source);
    let rowan_parse = nova_syntax::parse_java(source);
    let ast_id_map = AstIdMap::new(&rowan_parse.syntax());
    let tree = lower_item_tree(file, parse.compilation_unit(), &rowan_parse, &ast_id_map);

    let def_map = DefMap::from_item_tree(file, &tree);
    assert_eq!(def_map.package(), Some(&PackageName::from_dotted("p")));

    assert!(def_map.imports().contains(&Import::TypeSingle {
        ty: QualifiedName::from_dotted("java.util.List"),
    }));
    assert!(def_map.imports().contains(&Import::TypeStar {
        package: PackageName::from_dotted("java.util"),
    }));
    assert!(def_map.imports().contains(&Import::StaticSingle {
        ty: QualifiedName::from_dotted("java.lang.Math"),
        member: Name::from("max"),
    }));
    assert!(def_map.imports().contains(&Import::StaticStar {
        ty: QualifiedName::from_dotted("java.lang.Math"),
    }));

    let outer = def_map
        .lookup_top_level(&Name::from("Outer"))
        .expect("Outer");
    assert_eq!(def_map.binary_name(outer).unwrap().as_str(), "p.Outer");

    let inner = def_map
        .lookup_nested(outer, &Name::from("Inner"))
        .expect("Inner");
    assert_eq!(
        def_map.binary_name(inner).unwrap().as_str(),
        "p.Outer$Inner"
    );

    let deep = def_map
        .lookup_nested(inner, &Name::from("Deep"))
        .expect("Deep");
    assert_eq!(
        def_map.binary_name(deep).unwrap().as_str(),
        "p.Outer$Inner$Deep"
    );
}

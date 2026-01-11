use std::path::PathBuf;

use nova_classpath::{ClasspathEntry, ClasspathIndex};
use nova_types::{
    resolve_method_call, CallKind, ChainTypeProvider, ClassKind, MethodCall, MethodResolution,
    PrimitiveType, Type, TypeEnv, TypeStore, TypeStoreLoader,
};

use pretty_assertions::assert_eq;

#[test]
fn type_store_loader_bridge_from_classpath_index() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let jdk_jmod = manifest_dir.join("../nova-jdk/testdata/fake-jdk/jmods/java.base.jmod");
    let dep_jar = manifest_dir.join("../nova-classpath/testdata/dep.jar");

    let jdk_index =
        ClasspathIndex::build_with_deps_store(&[ClasspathEntry::Jmod(jdk_jmod)], None, None, None)
            .unwrap();
    let dep_index =
        ClasspathIndex::build_with_deps_store(&[ClasspathEntry::Jar(dep_jar)], None, None, None)
            .unwrap();

    let provider = ChainTypeProvider::new(vec![&dep_index, &jdk_index]);

    let mut store = TypeStore::default();
    let mut loader = TypeStoreLoader::new(&mut store, &provider);
    loader.bootstrap_well_known().unwrap();

    let (object_id, string_id) = {
        let wk = loader.store().well_known();
        (wk.object, wk.string)
    };

    // --- Load java.util.List --------------------------------------------------
    let list_id = loader.ensure_class("java.util.List").unwrap();
    let list_type_param = {
        let list_def = loader.store().class(list_id).unwrap();
        assert_eq!(list_def.kind, ClassKind::Interface);
        assert_eq!(list_def.type_params.len(), 1);
        list_def.type_params[0]
    };

    {
        let e_def = loader.store().type_param(list_type_param).unwrap();
        assert_eq!(e_def.name, "E");
        assert_eq!(e_def.upper_bounds, vec![Type::class(object_id, vec![])]);
    }

    {
        let list_def = loader.store().class(list_id).unwrap();
        let get = list_def
            .methods
            .iter()
            .find(|m| m.name == "get" && m.params.len() == 1)
            .expect("java.util.List.get(int)");
        assert_eq!(get.params[0], Type::Primitive(PrimitiveType::Int));
        assert_eq!(get.return_type, Type::TypeVar(list_type_param));
    }

    // --- Generic substitution + resolve_method_call --------------------------
    let list_string = Type::class(list_id, vec![Type::class(string_id, vec![])]);
    let call = MethodCall {
        receiver: list_string,
        call_kind: CallKind::Instance,
        name: "get",
        args: vec![Type::int()],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let MethodResolution::Found(resolved) = resolve_method_call(loader.store_mut(), &call) else {
        panic!("expected method resolution success for List<String>.get(int)");
    };
    assert_eq!(resolved.return_type, Type::class(string_id, vec![]));

    // --- Load com.example.dep.Foo --------------------------------------------
    let foo_id = loader.ensure_class("com.example.dep.Foo").unwrap();
    {
        let foo_def = loader.store().class(foo_id).unwrap();

        let strings = foo_def
            .methods
            .iter()
            .find(|m| m.name == "strings" && m.params.is_empty())
            .expect("com.example.dep.Foo.strings()");
        assert_eq!(
            strings.return_type,
            Type::class(list_id, vec![Type::class(string_id, vec![])])
        );

        let id_method = foo_def
            .methods
            .iter()
            .find(|m| m.name == "id" && m.params.len() == 1)
            .expect("com.example.dep.Foo.id(T)");
        assert_eq!(id_method.type_params.len(), 1);
        let t = id_method.type_params[0];
        assert_eq!(id_method.params, vec![Type::TypeVar(t)]);
        assert_eq!(id_method.return_type, Type::TypeVar(t));
    }

    // --- Optional: method inference sanity check -----------------------------
    let call = MethodCall {
        receiver: Type::class(foo_id, vec![]),
        call_kind: CallKind::Instance,
        name: "id",
        args: vec![Type::class(string_id, vec![])],
        expected_return: None,
        explicit_type_args: vec![],
    };
    let MethodResolution::Found(resolved) = resolve_method_call(loader.store_mut(), &call) else {
        panic!("expected method resolution success for Foo.id(String)");
    };
    assert_eq!(resolved.return_type, Type::class(string_id, vec![]));
}

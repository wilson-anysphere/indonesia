use nova_framework_spring::{
    analyze_java_sources, is_spring_applicable, BeanKind, InjectionKind, SPRING_AMBIGUOUS_BEAN,
    SPRING_NO_BEAN,
};
use nova_project::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, Dependency, JavaConfig, ProjectConfig,
};
use pretty_assertions::assert_eq;
use std::path::PathBuf;

#[test]
fn bean_and_injection_resolves() {
    let foo = r#"
        import org.springframework.stereotype.Component;

        @Component
        class Foo {
        }
    "#;
    let bar = r#"
        import org.springframework.stereotype.Component;
        import org.springframework.beans.factory.annotation.Autowired;

        @Component
        class Bar {
            @Autowired
            Foo foo;
        }
    "#;

    let analysis = analyze_java_sources(&[foo, bar]);
    assert!(
        analysis.diagnostics.is_empty(),
        "unexpected diagnostics: {:#?}",
        analysis.diagnostics
    );

    let inj_idx = analysis
        .model
        .injections
        .iter()
        .position(|i| i.owner_class == "Bar" && i.ty == "Foo")
        .expect("missing Foo injection");
    assert_eq!(
        analysis.model.injections[inj_idx].kind,
        InjectionKind::Field
    );

    let candidates = &analysis.model.injection_candidates[inj_idx];
    assert_eq!(candidates.len(), 1);
    let bean = &analysis.model.beans[candidates[0]];
    assert_eq!(bean.kind, BeanKind::Component);
    assert_eq!(bean.ty, "Foo");
}

#[test]
fn no_bean_diagnostic_triggers() {
    let bar = r#"
        import org.springframework.stereotype.Component;
        import org.springframework.beans.factory.annotation.Autowired;

        @Component
        class Bar {
            @Autowired
            Missing missing;
        }
    "#;

    let analysis = analyze_java_sources(&[bar]);
    assert_eq!(analysis.diagnostics.len(), 1);
    assert_eq!(analysis.diagnostics[0].diagnostic.code, SPRING_NO_BEAN);
    assert!(analysis.diagnostics[0]
        .diagnostic
        .message
        .contains("Missing"));
}

#[test]
fn ambiguous_bean_diagnostic_triggers() {
    let impl1 = r#"
        import org.springframework.stereotype.Component;

        @Component
        class FooImpl1 implements Foo {
        }
    "#;
    let impl2 = r#"
        import org.springframework.stereotype.Component;

        @Component
        class FooImpl2 implements Foo {
        }
    "#;
    let bar = r#"
        import org.springframework.stereotype.Component;
        import org.springframework.beans.factory.annotation.Autowired;

        @Component
        class Bar {
            @Autowired
            Foo foo;
        }
    "#;

    let analysis = analyze_java_sources(&[impl1, impl2, bar]);
    assert_eq!(analysis.diagnostics.len(), 1);
    assert_eq!(
        analysis.diagnostics[0].diagnostic.code,
        SPRING_AMBIGUOUS_BEAN
    );

    let inj_idx = analysis
        .model
        .injections
        .iter()
        .position(|i| i.owner_class == "Bar" && i.ty == "Foo")
        .expect("missing Foo injection");
    assert_eq!(analysis.model.injection_candidates[inj_idx].len(), 2);
}

#[test]
fn applicability_detects_spring_via_classpath_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let marker = tmp
        .path()
        .join("org/springframework/context/ApplicationContext.class");
    std::fs::create_dir_all(marker.parent().unwrap()).expect("mkdir");
    std::fs::write(&marker, b"").expect("write marker");

    let config = ProjectConfig {
        workspace_root: PathBuf::from(tmp.path()),
        build_system: BuildSystem::Simple,
        java: JavaConfig::default(),
        modules: vec![],
        jpms_modules: vec![],
        source_roots: vec![],
        module_path: vec![],
        classpath: vec![ClasspathEntry {
            kind: ClasspathEntryKind::Directory,
            path: PathBuf::from(tmp.path()),
        }],
        output_dirs: vec![],
        dependencies: vec![Dependency {
            group_id: "com.example".to_string(),
            artifact_id: "demo".to_string(),
            version: None,
            scope: None,
            classifier: None,
            type_: None,
        }],
        workspace_model: None,
    };

    assert!(is_spring_applicable(&config));
}

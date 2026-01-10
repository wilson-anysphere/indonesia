use nova_framework_micronaut::analyze_java_sources;
use nova_types::Severity;
use pretty_assertions::assert_eq;

#[test]
fn injection_resolution_by_type() {
    let bar = r#"
        import io.micronaut.context.annotation.Singleton;

        @Singleton
        class Bar {}
    "#;

    let foo = r#"
        import io.micronaut.context.annotation.Singleton;
        import jakarta.inject.Inject;

        @Singleton
        class Foo {
            @Inject Bar bar;
        }
    "#;

    let analysis = analyze_java_sources(&[bar, foo]);

    assert!(
        analysis.diagnostics.is_empty(),
        "unexpected diagnostics: {:#?}",
        analysis.diagnostics
    );

    let foo_bean = analysis
        .beans
        .iter()
        .find(|b| b.ty == "Foo")
        .expect("Foo bean missing");
    let bar_bean = analysis
        .beans
        .iter()
        .find(|b| b.ty == "Bar")
        .expect("Bar bean missing");

    let res = analysis
        .injection_resolutions
        .iter()
        .find(|r| r.requesting_bean_id == foo_bean.id && r.injection_point.label == "bar")
        .expect("resolution missing");

    assert_eq!(res.candidates, vec![bar_bean.id.clone()]);
}

#[test]
fn endpoint_discovery_controller_and_methods() {
    let src = r#"
        import io.micronaut.http.annotation.Controller;
        import io.micronaut.http.annotation.Get;
        import io.micronaut.http.annotation.Post;

        @Controller("/hello")
        class HelloController {
            @Get("/world")
            String world() { return "ok"; }

            @Post("/")
            void create() {}
        }
    "#;

    let analysis = analyze_java_sources(&[src]);

    assert_eq!(analysis.endpoints.len(), 2);
    assert!(analysis.endpoints.iter().any(|e| {
        e.method == "GET"
            && e.path == "/hello/world"
            && e.handler.class_name == "HelloController"
            && e.handler.method_name == "world"
    }));
    assert!(analysis.endpoints.iter().any(|e| {
        e.method == "POST"
            && e.path == "/hello"
            && e.handler.class_name == "HelloController"
            && e.handler.method_name == "create"
    }));
}

#[test]
fn diagnostic_no_bean_candidate() {
    let src = r#"
        import io.micronaut.context.annotation.Singleton;
        import jakarta.inject.Inject;

        @Singleton
        class Foo {
            @Inject Bar bar;
        }
    "#;

    let analysis = analyze_java_sources(&[src]);

    assert_eq!(analysis.diagnostics.len(), 1);
    assert_eq!(analysis.diagnostics[0].code, "MICRONAUT_NO_BEAN");
    assert_eq!(analysis.diagnostics[0].severity, Severity::Error);
}

#[test]
fn diagnostic_ambiguous_beans() {
    let service = r#"interface Service {}"#;

    let a = r#"
        import io.micronaut.context.annotation.Singleton;

        @Singleton
        class A implements Service {}
    "#;

    let b = r#"
        import io.micronaut.context.annotation.Singleton;

        @Singleton
        class B implements Service {}
    "#;

    let foo = r#"
        import io.micronaut.context.annotation.Singleton;
        import jakarta.inject.Inject;

        @Singleton
        class Foo {
            @Inject Service service;
        }
    "#;

    let analysis = analyze_java_sources(&[service, a, b, foo]);

    assert_eq!(analysis.diagnostics.len(), 1);
    assert_eq!(analysis.diagnostics[0].code, "MICRONAUT_AMBIGUOUS_BEAN");
}

#[test]
fn diagnostic_circular_dependency() {
    let a = r#"
        import io.micronaut.context.annotation.Singleton;
        import jakarta.inject.Inject;

        @Singleton
        class A {
            @Inject B b;
        }
    "#;

    let b = r#"
        import io.micronaut.context.annotation.Singleton;
        import jakarta.inject.Inject;

        @Singleton
        class B {
            @Inject A a;
        }
    "#;

    let analysis = analyze_java_sources(&[a, b]);

    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.code == "MICRONAUT_CIRCULAR_DEPENDENCY" && d.severity == Severity::Warning),
        "expected circular dependency warning, got: {:#?}",
        analysis.diagnostics
    );
}


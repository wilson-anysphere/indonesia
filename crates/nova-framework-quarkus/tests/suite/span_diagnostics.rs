use nova_framework_quarkus::{
    analyze_java_sources_with_spans, CDI_AMBIGUOUS_CODE, CDI_UNSATISFIED_CODE,
};

#[test]
fn unsatisfied_dependency_diagnostic_has_span() {
    let src = r#"
        import jakarta.enterprise.context.ApplicationScoped;
        import jakarta.inject.Inject;

        @ApplicationScoped
        public class ServiceA {
          @Inject ServiceB missing;
        }
    "#;

    let analysis = analyze_java_sources_with_spans(&[src]);
    let diag = analysis
        .diagnostics
        .iter()
        .find(|d| d.diagnostic.code == CDI_UNSATISFIED_CODE)
        .expect("expected unsatisfied dependency diagnostic");

    assert_eq!(diag.source, 0);
    let span = diag.diagnostic.span.expect("expected diagnostic span");
    assert_eq!(&src[span.start..span.end], "missing");
}

#[test]
fn ambiguous_dependency_diagnostic_has_span() {
    let iface = r#"
        public interface GreetingService {
          String greet();
        }
    "#;
    let english = r#"
        import jakarta.enterprise.context.ApplicationScoped;

        @ApplicationScoped
        public class EnglishGreetingService implements GreetingService {
          public String greet() { return "hello"; }
        }
    "#;
    let spanish = r#"
        import jakarta.enterprise.context.ApplicationScoped;

        @ApplicationScoped
        public class SpanishGreetingService implements GreetingService {
          public String greet() { return "hola"; }
        }
    "#;
    let greeter = r#"
        import jakarta.enterprise.context.ApplicationScoped;
        import jakarta.inject.Inject;

        @ApplicationScoped
        public class Greeter {
          @Inject GreetingService service;
        }
    "#;

    let analysis = analyze_java_sources_with_spans(&[iface, english, spanish, greeter]);
    let diag = analysis
        .diagnostics
        .iter()
        .find(|d| d.diagnostic.code == CDI_AMBIGUOUS_CODE)
        .expect("expected ambiguous dependency diagnostic");

    assert_eq!(diag.source, 3);
    let span = diag.diagnostic.span.expect("expected diagnostic span");
    assert_eq!(&greeter[span.start..span.end], "service");
}

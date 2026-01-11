use nova_framework_quarkus::{
    analyze_java_sources, CDI_AMBIGUOUS_CODE, CDI_CIRCULAR_CODE, CDI_UNSATISFIED_CODE,
};
use pretty_assertions::assert_eq;

#[test]
fn cdi_reports_unsatisfied_dependency() {
    let src = r#"
        import jakarta.enterprise.context.ApplicationScoped;
        import jakarta.inject.Inject;

        @ApplicationScoped
        public class ServiceA {
          @Inject ServiceB missing;
        }
    "#;

    let analysis = analyze_java_sources(&[src]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.code == CDI_UNSATISFIED_CODE),
        "expected {CDI_UNSATISFIED_CODE} diagnostic, got: {:#?}",
        analysis.diagnostics
    );
}

#[test]
fn cdi_reports_ambiguous_dependency() {
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

    let analysis = analyze_java_sources(&[iface, english, spanish, greeter]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.code == CDI_AMBIGUOUS_CODE),
        "expected {CDI_AMBIGUOUS_CODE} diagnostic, got: {:#?}",
        analysis.diagnostics
    );
}

#[test]
fn cdi_reports_circular_dependency_best_effort() {
    let a = r#"
        import jakarta.enterprise.context.ApplicationScoped;
        import jakarta.inject.Inject;

        @ApplicationScoped
        public class A {
          @Inject B b;
        }
    "#;
    let b = r#"
        import jakarta.enterprise.context.ApplicationScoped;
        import jakarta.inject.Inject;

        @ApplicationScoped
        public class B {
          @Inject A a;
        }
    "#;

    let analysis = analyze_java_sources(&[a, b]);
    assert!(
        analysis
            .diagnostics
            .iter()
            .any(|d| d.code == CDI_CIRCULAR_CODE),
        "expected {CDI_CIRCULAR_CODE} diagnostic, got: {:#?}",
        analysis.diagnostics
    );
}

#[test]
fn discovers_jaxrs_endpoints() {
    let src = r#"
        import jakarta.ws.rs.GET;
        import jakarta.ws.rs.POST;
        import jakarta.ws.rs.Path;

        @Path("/hello")
        public class HelloResource {
          @GET
          public String hello() { return "hello"; }

          @POST
          @Path("/create")
          public void create() {}
        }
    "#;

    let mut endpoints = analyze_java_sources(&[src]).endpoints;
    endpoints.sort_by(|a, b| a.path.cmp(&b.path));

    assert_eq!(endpoints.len(), 2, "endpoints: {:#?}", endpoints);
    assert_eq!(endpoints[0].path, "/hello");
    assert_eq!(endpoints[0].methods, vec!["GET"]);
    assert_eq!(endpoints[1].path, "/hello/create");
    assert_eq!(endpoints[1].methods, vec!["POST"]);
}

#[test]
fn discovers_spring_mvc_endpoints() {
    let src = r#"
        import org.springframework.web.bind.annotation.GetMapping;
        import org.springframework.web.bind.annotation.RequestMapping;
        import org.springframework.web.bind.annotation.RestController;

        @RestController
        @RequestMapping("/api")
        public class HelloController {
          @GetMapping("/hello")
          public String hello() { return "hello"; }
        }
    "#;

    let endpoints = analyze_java_sources(&[src]).endpoints;
    assert_eq!(endpoints.len(), 1, "endpoints: {:#?}", endpoints);
    assert_eq!(endpoints[0].path, "/api/hello");
    assert_eq!(endpoints[0].methods, vec!["GET"]);
}

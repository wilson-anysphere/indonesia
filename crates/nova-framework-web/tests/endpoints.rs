use std::path::PathBuf;

use nova_framework_web::{
    extract_jaxrs_endpoints, extract_micronaut_endpoints, extract_spring_mvc_endpoints,
};

#[test]
fn jaxrs_baseline_extraction() {
    let src = r#"import jakarta.ws.rs.GET;
import jakarta.ws.rs.Path;

@Path("/api")
public class HelloResource {
    @GET
    @Path("hello")
    public String hello() {
        return "hi";
    }
}
"#;

    let endpoints = extract_jaxrs_endpoints(&[src]);
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].path, "/api/hello");
    assert_eq!(endpoints[0].methods, vec!["GET".to_string()]);
    assert_eq!(endpoints[0].handler.file, None);
    assert_eq!(endpoints[0].handler.line, 8);
}

#[test]
fn spring_mvc_extraction_class_and_method_mappings() {
    let src = r#"import org.springframework.web.bind.annotation.*;

@RestController
@RequestMapping("/api")
public class UserController {
    @GetMapping("/users")
    public String list() { return ""; }

    @RequestMapping(method = RequestMethod.POST, path = "/users")
    public String create() { return ""; }
}
"#;

    let endpoints =
        extract_spring_mvc_endpoints(&[(src, Some(PathBuf::from("UserController.java")))]);
    assert_eq!(endpoints.len(), 2);

    assert_eq!(endpoints[0].path, "/api/users");
    assert_eq!(endpoints[0].methods, vec!["GET".to_string()]);
    assert_eq!(
        endpoints[0].handler.file,
        Some(PathBuf::from("UserController.java"))
    );
    assert_eq!(endpoints[0].handler.line, 7);

    assert_eq!(endpoints[1].path, "/api/users");
    assert_eq!(endpoints[1].methods, vec!["POST".to_string()]);
    assert_eq!(
        endpoints[1].handler.file,
        Some(PathBuf::from("UserController.java"))
    );
    assert_eq!(endpoints[1].handler.line, 10);
}

#[test]
fn spring_mvc_extraction_supports_allman_brace_style() {
    let src = r#"import org.springframework.web.bind.annotation.*;

@RestController
@RequestMapping("/api")
public class UserController
{
    @GetMapping("/users")
    public String list() { return ""; }
}
"#;

    let endpoints = extract_spring_mvc_endpoints(&[(src, Some(PathBuf::from("UserController.java")))]);
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].path, "/api/users");
    assert_eq!(endpoints[0].methods, vec!["GET".to_string()]);
    assert_eq!(endpoints[0].handler.file, Some(PathBuf::from("UserController.java")));
    assert_eq!(endpoints[0].handler.line, 8);
}

#[test]
fn spring_mvc_extraction_handles_generic_return_types_and_slash_normalization() {
    let src = r#"import org.springframework.web.bind.annotation.*;
import java.util.Map;

@RestController
@RequestMapping("/api/")
public class UserController {
    @GetMapping("/users")
    public Map<String, Object> list() { return null; }
}
"#;

    let endpoints = extract_spring_mvc_endpoints(&[(src, Some(PathBuf::from("UserController.java")))]);
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].path, "/api/users");
    assert_eq!(endpoints[0].methods, vec!["GET".to_string()]);
    assert_eq!(endpoints[0].handler.file, Some(PathBuf::from("UserController.java")));
    assert_eq!(endpoints[0].handler.line, 8);
}

#[test]
fn micronaut_extraction_controller_and_method_mappings() {
    let src = r#"import io.micronaut.http.annotation.*;

@Controller("/api")
public class HelloController {
    @Get("/hello")
    public String hello() { return "hi"; }

    @Post("/hello")
    public String create() { return "ok"; }
}
"#;

    let endpoints =
        extract_micronaut_endpoints(&[(src, Some(PathBuf::from("HelloController.java")))]);
    assert_eq!(endpoints.len(), 2);

    assert_eq!(endpoints[0].path, "/api/hello");
    assert_eq!(endpoints[0].methods, vec!["GET".to_string()]);
    assert_eq!(
        endpoints[0].handler.file,
        Some(PathBuf::from("HelloController.java"))
    );
    assert_eq!(endpoints[0].handler.line, 6);

    assert_eq!(endpoints[1].path, "/api/hello");
    assert_eq!(endpoints[1].methods, vec!["POST".to_string()]);
    assert_eq!(
        endpoints[1].handler.file,
        Some(PathBuf::from("HelloController.java"))
    );
    assert_eq!(endpoints[1].handler.line, 9);
}

#[test]
fn micronaut_extraction_supports_allman_brace_style() {
    let src = r#"import io.micronaut.http.annotation.*;

@Controller("/api")
public class HelloController
{
    @Get("/hello")
    public String hello() { return "hi"; }
}
"#;

    let endpoints =
        extract_micronaut_endpoints(&[(src, Some(PathBuf::from("HelloController.java")))]);
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].path, "/api/hello");
    assert_eq!(endpoints[0].methods, vec!["GET".to_string()]);
    assert_eq!(endpoints[0].handler.file, Some(PathBuf::from("HelloController.java")));
    assert_eq!(endpoints[0].handler.line, 7);
}

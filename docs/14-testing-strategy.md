# 14 - Testing Strategy

[← Back to Main Document](../AGENTS.md) | [Previous: AI Augmentation](13-ai-augmentation.md)

## Overview

A language server is only as good as its correctness. Nova requires comprehensive testing to ensure reliability across the vast complexity of Java and its ecosystem.

---

## Testing Philosophy

```
┌─────────────────────────────────────────────────────────────────┐
│                    TESTING PHILOSOPHY                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  1. TEST AT EVERY LEVEL                                         │
│     • Unit tests for individual components                      │
│     • Integration tests for component interactions              │
│     • End-to-end tests for user scenarios                       │
│                                                                  │
│  2. SPECIFICATION COMPLIANCE                                     │
│     • Java Language Specification (JLS) compliance tests        │
│     • LSP specification compliance tests                        │
│     • Framework-specific behavior tests                         │
│                                                                  │
│  3. REGRESSION PREVENTION                                        │
│     • Every bug fix includes a test                             │
│     • Property-based testing for edge cases                     │
│     • Fuzz testing for robustness                               │
│                                                                  │
│  4. PERFORMANCE TESTING                                          │
│     • Benchmark suite for critical paths                        │
│     • Continuous performance monitoring                         │
│     • No performance regression allowed                         │
│                                                                  │
│  5. REAL-WORLD VALIDATION                                        │
│     • Tests against real open-source projects                   │
│     • Comparison tests against javac/IntelliJ                   │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Test Categories

### Unit Tests

```rust
#[cfg(test)]
mod lexer_tests {
    use super::*;
    
    #[test]
    fn test_lex_identifier() {
        let lexer = Lexer::new("hello_world");
        let token = lexer.next_token();
        assert_eq!(token.kind, SyntaxKind::Identifier);
        assert_eq!(token.text, "hello_world");
    }
    
    #[test]
    fn test_lex_keywords() {
        let keywords = vec![
            ("class", SyntaxKind::ClassKw),
            ("public", SyntaxKind::PublicKw),
            ("static", SyntaxKind::StaticKw),
            ("void", SyntaxKind::VoidKw),
        ];
        
        for (text, expected) in keywords {
            let lexer = Lexer::new(text);
            let token = lexer.next_token();
            assert_eq!(token.kind, expected, "Failed for: {}", text);
        }
    }
    
    #[test]
    fn test_lex_string_with_escapes() {
        let lexer = Lexer::new(r#""hello\nworld""#);
        let token = lexer.next_token();
        assert_eq!(token.kind, SyntaxKind::StringLiteral);
    }
    
    #[test]
    fn test_lex_text_block() {
        let lexer = Lexer::new(r#""""
            multi
            line
            """"#);
        let token = lexer.next_token();
        assert_eq!(token.kind, SyntaxKind::TextBlock);
    }
}

#[cfg(test)]
mod parser_tests {
    #[test]
    fn test_parse_class_declaration() {
        let source = "public class Foo extends Bar implements Baz { }";
        let parse = parse_java(source);
        
        assert!(parse.errors.is_empty());
        
        let class = parse.tree.first_class().unwrap();
        assert_eq!(class.name(), "Foo");
        assert!(class.modifiers().any(|m| m.is_public()));
        assert_eq!(class.extends().unwrap().name(), "Bar");
        assert!(class.implements().any(|i| i.name() == "Baz"));
    }
    
    #[test]
    fn test_parse_generic_method() {
        let source = "<T extends Comparable<T>> T max(T a, T b) { }";
        let parse = parse_method(source);
        
        assert!(parse.errors.is_empty());
        
        let type_params = parse.tree.type_parameters();
        assert_eq!(type_params.len(), 1);
        
        let t = &type_params[0];
        assert_eq!(t.name(), "T");
        assert!(t.bound().is_some());
    }
    
    #[test]
    fn test_error_recovery() {
        let source = "class Foo { int x = ; void bar() { } }";
        let parse = parse_java(source);
        
        // Should have error but still parse bar()
        assert!(!parse.errors.is_empty());
        
        let methods = parse.tree.first_class().unwrap().methods();
        assert_eq!(methods.count(), 1);
    }
}
```

### Integration Tests

```rust
#[cfg(test)]
mod type_checking_tests {
    use crate::test_utils::*;
    
    #[test]
    fn test_method_resolution() {
        let db = TestDatabase::new();
        
        db.add_file("Foo.java", r#"
            class Foo {
                void bar(String s) { }
                void bar(int i) { }
                
                void test() {
                    bar("hello"); // Should resolve to first
                    bar(42);      // Should resolve to second
                }
            }
        "#);
        
        let call1 = db.method_call_at("Foo.java", 6, 13);
        let resolved1 = db.resolve_method_call(call1);
        assert_eq!(resolved1.params[0].ty, Type::string());
        
        let call2 = db.method_call_at("Foo.java", 7, 13);
        let resolved2 = db.resolve_method_call(call2);
        assert_eq!(resolved2.params[0].ty, Type::int());
    }
    
    #[test]
    fn test_generic_inference() {
        let db = TestDatabase::new();
        
        db.add_file("Foo.java", r#"
            import java.util.*;
            
            class Foo {
                void test() {
                    List<String> list = new ArrayList<>();
                    String s = list.get(0);
                }
            }
        "#);
        
        // Diamond should infer ArrayList<String>
        let new_expr = db.new_expr_at("Foo.java", 5, 35);
        let inferred = db.type_of(new_expr);
        assert_eq!(inferred, Type::class("java.util.ArrayList", vec![Type::string()]));
        
        // list.get(0) should return String
        let get_call = db.method_call_at("Foo.java", 6, 26);
        let return_type = db.type_of(get_call);
        assert_eq!(return_type, Type::string());
    }
    
    #[test]
    fn test_lambda_inference() {
        let db = TestDatabase::new();
        
        db.add_file("Foo.java", r#"
            import java.util.function.*;
            
            class Foo {
                void test() {
                    Function<String, Integer> f = s -> s.length();
                }
            }
        "#);
        
        let lambda = db.lambda_at("Foo.java", 5, 47);
        let param = lambda.parameters().next().unwrap();
        
        // Lambda parameter should infer String
        assert_eq!(db.type_of_param(param), Type::string());
    }
}
```

### Specification Tests

```rust
/// Tests derived from Java Language Specification
mod jls_tests {
    /// JLS §5.1.1 - Identity Conversion
    #[test]
    fn test_identity_conversion() {
        assert!(is_assignable(Type::int(), Type::int()));
        assert!(is_assignable(Type::string(), Type::string()));
    }
    
    /// JLS §5.1.2 - Widening Primitive Conversion
    #[test]
    fn test_widening_primitive() {
        assert!(is_assignable(Type::byte(), Type::short()));
        assert!(is_assignable(Type::byte(), Type::int()));
        assert!(is_assignable(Type::byte(), Type::long()));
        assert!(is_assignable(Type::int(), Type::long()));
        assert!(is_assignable(Type::float(), Type::double()));
        
        assert!(!is_assignable(Type::int(), Type::byte()));
        assert!(!is_assignable(Type::double(), Type::float()));
    }
    
    /// JLS §5.1.5 - Widening Reference Conversion
    #[test]
    fn test_widening_reference() {
        let db = TestDatabase::new();
        db.add_file("Types.java", r#"
            class Animal { }
            class Dog extends Animal { }
            interface Runnable { }
            class Runner implements Runnable { }
        "#);
        
        assert!(db.is_assignable(db.type_("Dog"), db.type_("Animal")));
        assert!(db.is_assignable(db.type_("Runner"), db.type_("Runnable")));
        assert!(db.is_assignable(db.type_("Dog"), db.type_("Object")));
        
        assert!(!db.is_assignable(db.type_("Animal"), db.type_("Dog")));
    }
    
    /// JLS §15.12.2 - Method Invocation - Determine Applicable Methods
    #[test]
    fn test_overload_resolution() {
        let db = TestDatabase::new();
        db.add_file("Overload.java", r#"
            class Overload {
                void foo(Object o) { }
                void foo(String s) { }
                
                void test() {
                    foo("hello"); // Should pick String version (more specific)
                    foo(new Object()); // Should pick Object version
                }
            }
        "#);
        
        let call1 = db.method_call_at("Overload.java", 6, 13);
        assert_eq!(db.resolve_method_call(call1).params[0].ty, Type::string());
        
        let call2 = db.method_call_at("Overload.java", 7, 13);
        assert_eq!(db.resolve_method_call(call2).params[0].ty, Type::object());
    }
}
```

### LSP Protocol Tests

```rust
#[cfg(test)]
mod lsp_tests {
    use super::*;
    use lsp_server::*;
    
    #[tokio::test]
    async fn test_completion_protocol() {
        let (server, client) = create_test_server().await;
        
        // Open document
        client.notify("textDocument/didOpen", json!({
            "textDocument": {
                "uri": "file:///test/Main.java",
                "languageId": "java",
                "version": 1,
                "text": "class Main { String s; void foo() { s. } }"
            }
        })).await;
        
        // Request completion
        let response = client.request("textDocument/completion", json!({
            "textDocument": { "uri": "file:///test/Main.java" },
            "position": { "line": 0, "character": 39 }
        })).await;
        
        let completions: CompletionList = serde_json::from_value(response)?;
        
        // Verify String methods present
        assert!(completions.items.iter().any(|i| i.label == "length"));
        assert!(completions.items.iter().any(|i| i.label == "substring"));
        assert!(completions.items.iter().any(|i| i.label == "charAt"));
    }
    
    #[tokio::test]
    async fn test_goto_definition() {
        let (server, client) = create_test_server().await;
        
        client.notify("textDocument/didOpen", json!({
            "textDocument": {
                "uri": "file:///test/Main.java",
                "languageId": "java",
                "version": 1,
                "text": "class Main { void foo() { } void bar() { foo(); } }"
            }
        })).await;
        
        let response = client.request("textDocument/definition", json!({
            "textDocument": { "uri": "file:///test/Main.java" },
            "position": { "line": 0, "character": 43 } // on "foo()"
        })).await;
        
        let location: Location = serde_json::from_value(response)?;
        
        assert_eq!(location.range.start.line, 0);
        assert_eq!(location.range.start.character, 18); // "foo" definition
    }
    
    #[tokio::test]
    async fn test_rename() {
        let (server, client) = create_test_server().await;
        
        client.notify("textDocument/didOpen", json!({
            "textDocument": {
                "uri": "file:///test/Main.java",
                "languageId": "java",
                "version": 1,
                "text": "class Main { int foo; void bar() { foo = 1; int x = foo; } }"
            }
        })).await;
        
        let response = client.request("textDocument/rename", json!({
            "textDocument": { "uri": "file:///test/Main.java" },
            "position": { "line": 0, "character": 17 }, // on "foo"
            "newName": "renamed"
        })).await;
        
        let edit: WorkspaceEdit = serde_json::from_value(response)?;
        let changes = edit.changes.unwrap();
        let file_changes = changes.get("file:///test/Main.java").unwrap();
        
        // Should rename all 3 occurrences
        assert_eq!(file_changes.len(), 3);
    }
}
```

### Performance Tests

```rust
#[cfg(test)]
mod benchmarks {
    use criterion::{criterion_group, Criterion, BenchmarkId};
    
    fn bench_parsing(c: &mut Criterion) {
        let sizes = vec![100, 1000, 10000];
        
        for size in sizes {
            let source = generate_java_file(size);
            
            c.bench_with_input(
                BenchmarkId::new("parse", size),
                &source,
                |b, source| {
                    b.iter(|| parse_java(source))
                },
            );
        }
    }
    
    fn bench_completion(c: &mut Criterion) {
        let db = setup_benchmark_db();
        let file = db.open_file("src/Main.java");
        
        c.bench_function("completion_member", |b| {
            b.iter(|| {
                db.snapshot().completions_at(file, Position::new(100, 10))
            })
        });
    }
    
    fn bench_type_checking(c: &mut Criterion) {
        let db = setup_benchmark_db();
        let file = db.open_file("src/ComplexGenerics.java");
        
        c.bench_function("type_check_generics", |b| {
            b.iter(|| {
                db.snapshot().diagnostics(file)
            })
        });
    }
    
    fn bench_find_references(c: &mut Criterion) {
        let db = setup_large_project();
        let symbol = db.find_symbol("com.example.CommonService.process");
        
        c.bench_function("find_references_100", |b| {
            b.iter(|| {
                db.snapshot().find_references(symbol, false)
            })
        });
    }
    
    criterion_group!(benches, bench_parsing, bench_completion, bench_type_checking, bench_find_references);
}
```

### Fuzz Testing

```rust
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(source) = std::str::from_utf8(data) {
        // Parser should never panic
        let _ = parse_java(source);
    }
});

#[derive(Arbitrary, Debug)]
struct FuzzEdit {
    offset: usize,
    delete_len: usize,
    insert: String,
}

fuzz_target!(|edits: Vec<FuzzEdit>| {
    let mut db = TestDatabase::new();
    let file = db.add_file("Test.java", "class Test { }");
    
    for edit in edits {
        // Incremental updates should never panic
        let _ = db.apply_edit(file, edit.offset, edit.delete_len, &edit.insert);
        
        // Should always be able to get diagnostics
        let _ = db.diagnostics(file);
    }
});
```

### Real Project Tests

```rust
#[cfg(test)]
mod real_project_tests {
    /// Test against real open-source projects
    #[test]
    #[ignore] // Run with --include-ignored
    fn test_spring_petclinic() {
        let db = load_project("test-projects/spring-petclinic");
        
        // Should parse all files
        let files = db.project_files();
        assert!(files.len() > 50);
        
        for file in files {
            let parse = db.parse(file);
            // Errors should match javac
            compare_errors_with_javac(&parse);
        }
        
        // Spot check specific features
        let service = db.find_class("org.springframework.samples.petclinic.owner.OwnerService");
        assert!(service.is_some());
        
        // Spring beans should be detected
        let beans = db.spring_beans();
        assert!(beans.iter().any(|b| b.name == "ownerService"));
    }
    
    #[test]
    #[ignore]
    fn test_guava() {
        let db = load_project("test-projects/guava");
        
        // Heavy generics usage
        let optional = db.find_class("com.google.common.base.Optional");
        assert!(optional.is_some());
        
        // Type parameters should be correct
        let type_params = optional.unwrap().type_parameters();
        assert_eq!(type_params.len(), 1);
    }
}
```

---

## Continuous Integration

```yaml
# .github/workflows/test.yml
name: Test Suite

on: [push, pull_request]

jobs:
  unit-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Run unit tests
        run: cargo test --lib
        
  integration-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Run integration tests
        run: cargo test --test '*'
        
  lsp-tests:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Run LSP protocol tests
        run: cargo test --features lsp-tests
        
  benchmarks:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Run benchmarks
        run: cargo bench --no-run # Just compile to catch regressions
        
  fuzz:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Run fuzz tests (limited)
        run: cargo +nightly fuzz run fuzz_parser -- -max_total_time=60
        
  real-projects:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Clone test projects
        run: ./scripts/clone-test-projects.sh
      - name: Run real project tests
        run: cargo test -- --include-ignored
```

---

## Test Infrastructure

```rust
/// Shared test utilities
pub mod test_utils {
    pub struct TestDatabase {
        db: Database,
        files: HashMap<String, FileId>,
    }
    
    impl TestDatabase {
        pub fn new() -> Self {
            Self {
                db: Database::new(),
                files: HashMap::new(),
            }
        }
        
        pub fn add_file(&mut self, name: &str, content: &str) -> FileId {
            let file = self.db.create_file(name);
            self.db.set_file_content(file, content.into());
            self.files.insert(name.into(), file);
            file
        }
        
        pub fn method_call_at(&self, file: &str, line: u32, col: u32) -> MethodCallId {
            let file_id = self.files[file];
            let pos = Position::new(line, col);
            self.db.method_call_at(file_id, pos).unwrap()
        }
        
        pub fn diagnostics(&self, file: &str) -> Vec<Diagnostic> {
            let file_id = self.files[file];
            self.db.diagnostics(file_id)
        }
        
        pub fn assert_no_errors(&self, file: &str) {
            let diags = self.diagnostics(file);
            let errors: Vec<_> = diags.iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            assert!(errors.is_empty(), "Expected no errors, got: {:?}", errors);
        }
        
        pub fn assert_error(&self, file: &str, code: &str) {
            let diags = self.diagnostics(file);
            assert!(
                diags.iter().any(|d| d.code == code),
                "Expected error {}, got: {:?}",
                code,
                diags
            );
        }
    }
}
```

---

## Next Steps

1. → [Work Breakdown](15-work-breakdown.md): Project organization and phasing

---

[← Previous: AI Augmentation](13-ai-augmentation.md) | [Next: Work Breakdown →](15-work-breakdown.md)

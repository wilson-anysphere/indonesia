# Framework Support Workstream

> **MANDATORY: Read and follow [AGENTS.md](../AGENTS.md) completely before proceeding.**
> **All rules in AGENTS.md apply at all times. This file adds workstream-specific guidance.**

---

## Scope

This workstream owns framework-specific intelligence - understanding Spring, Lombok, JPA, and other frameworks:

| Crate | Purpose |
|-------|---------|
| `nova-framework` | Framework analyzer plugin interface |
| `nova-framework-spring` | Spring/Spring Boot support |
| `nova-framework-lombok` | Lombok annotation processing |
| `nova-framework-jpa` | JPA/Hibernate entity analysis |
| `nova-framework-dagger` | Dagger dependency injection |
| `nova-framework-mapstruct` | MapStruct mapper support |
| `nova-framework-micronaut` | Micronaut framework support |
| `nova-framework-quarkus` | Quarkus framework support |
| `nova-framework-web` | JAX-RS, Servlet analysis |
| `nova-framework-parse` | Framework annotation parsing utilities |
| `nova-apt` | Annotation processing simulation |

---

## Key Documents

**Required reading:**
- [09 - Framework Support](../docs/09-framework-support.md) - Architecture and design

---

## Architecture

### Plugin Interface

```rust
use std::path::Path;

use nova_core::ProjectId;
use nova_framework::{
    CompletionContext, FrameworkData, InlayHint, NavigationTarget, Symbol, VirtualMember,
};
use nova_hir::framework::ClassData;
use nova_types::{ClassId, CompletionItem, Diagnostic};
use nova_vfs::FileId;

/// Query surface a host must provide for `nova-framework` analyzers.
///
/// Notes:
/// - `file_text`, `file_path`, `file_id`, `all_files`, and `all_classes` are *optional* and
///   default to `None`/empty. Analyzers must degrade gracefully when they are unavailable.
pub trait Database {
    fn class(&self, class: ClassId) -> &ClassData;
    fn project_of_class(&self, class: ClassId) -> ProjectId;
    fn project_of_file(&self, file: FileId) -> ProjectId;

    fn file_text(&self, _file: FileId) -> Option<&str> {
        None
    }

    fn file_path(&self, _file: FileId) -> Option<&Path> {
        None
    }

    fn file_id(&self, _path: &Path) -> Option<FileId> {
        None
    }

    fn all_files(&self, _project: ProjectId) -> Vec<FileId> {
        Vec::new()
    }

    fn all_classes(&self, _project: ProjectId) -> Vec<ClassId> {
        Vec::new()
    }

    fn has_dependency(&self, project: ProjectId, group: &str, artifact: &str) -> bool;
    fn has_class_on_classpath(&self, project: ProjectId, binary_name: &str) -> bool;
    fn has_class_on_classpath_prefix(&self, project: ProjectId, prefix: &str) -> bool;
}

/// Extension point for framework analyzers.
///
/// All hooks except `applies_to` are optional (default to no-op) so analyzers can
/// focus on the behavior they care about (e.g. Lombok implements `virtual_members`).
pub trait FrameworkAnalyzer: Send + Sync {
    /// Check if this analyzer applies to `project`.
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool;

    /// Optional: extract framework-specific data from a file.
    fn analyze_file(&self, _db: &dyn Database, _file: FileId) -> Option<FrameworkData> {
        None
    }

    /// Optional: provide additional diagnostics for a file.
    fn diagnostics(&self, _db: &dyn Database, _file: FileId) -> Vec<Diagnostic> {
        Vec::new()
    }

    /// Optional: provide completion items at a cursor location.
    fn completions(&self, _db: &dyn Database, _ctx: &CompletionContext) -> Vec<CompletionItem> {
        Vec::new()
    }

    /// Optional: provide navigation targets for a coarse symbol handle.
    fn navigation(&self, _db: &dyn Database, _symbol: &Symbol) -> Vec<NavigationTarget> {
        Vec::new()
    }

    /// Optional: synthesize framework-generated members (e.g., Lombok).
    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        Vec::new()
    }

    /// Optional: provide inlay hints for a file.
    fn inlay_hints(&self, _db: &dyn Database, _file: FileId) -> Vec<InlayHint> {
        Vec::new()
    }
}
```

The `Database` surface is intentionally small and partially-optional: `file_text(file)` may be
`None`, and `all_files(project)`/`all_classes(project)` may be empty when project-wide enumeration
is not available. Analyzers should treat these as "no data" and return best-effort results.

### IDE Integration Constraint (Database Adapter)

`nova_framework::FrameworkAnalyzer` runs on `nova_framework::Database`, which requires structural
queries like `class(ClassId) -> &nova_hir::framework::ClassData` (plus dependency/classpath checks).
The IDE-facing `nova_db::Database` currently provides file text only.

If you want to run framework analyzers inside `nova-ide`, you need an adapter layer that can supply
`ClassData` and project metadata.

- General-purpose adapter: `crates/nova-ide/src/framework_db.rs`
- Focused example that builds a `nova_framework::MemoryDatabase`: `crates/nova-ide/src/lombok_intel.rs`

### Virtual Members

Frameworks like Lombok generate members at compile time. Nova synthesizes them:

```java
@Data
public class User {
    private String name;
    private int age;
    
    // Nova synthesizes:
    // - getName(), setName(String)
    // - getAge(), setAge(int)
    // - equals(), hashCode(), toString()
    // - constructor, builder (if @Builder)
}
```

---

## Framework Implementations

### Lombok

**Supported annotations:**

| Annotation | Virtual Members |
|------------|-----------------|
| `@Getter` | `getField()` methods |
| `@Setter` | `setField(T)` methods |
| `@Data` | getters, setters, equals, hashCode, toString |
| `@Value` | getters, equals, hashCode, toString (immutable) |
| `@Builder` | `builder()`, `Builder` inner class |
| `@NoArgsConstructor` | no-arg constructor |
| `@AllArgsConstructor` | all-fields constructor |
| `@RequiredArgsConstructor` | final-fields constructor |
| `@Slf4j` / `@Log4j2` | `log` field |

### Spring

**Bean discovery:**
```java
@Component   // → Bean
@Service     // → Bean
@Repository  // → Bean
@Controller  // → Bean
@Bean        // → Bean (on method)
@Configuration // → Configuration class
```

**Autowiring resolution:**
```java
@Autowired
private UserService userService;  // → Navigate to bean definition
```

**Configuration properties:**
```java
@ConfigurationProperties(prefix = "app")
public class AppConfig {
    private String name;  // → application.yml: app.name
}
```

### JPA/Hibernate

**Entity analysis:**
```java
@Entity
public class User {
    @Id
    private Long id;
    
    @OneToMany(mappedBy = "user")  // ← Validate mappedBy exists
    private List<Order> orders;
}
```

**Query validation:**
```java
@Query("SELECT u FROM User u WHERE u.name = :name")  // ← Validate JPQL
List<User> findByName(@Param("name") String name);
```

---

## Development Guidelines

### Adding Framework Support

1. **Create crate** - `nova-framework-<name>`
2. **Implement trait** - `FrameworkAnalyzer`
3. **Register analyzer** - In the consumer's `AnalyzerRegistry` (for IDE support, see
   `crates/nova-ide/src/extensions.rs` where the default registry is built)
4. **Add tests** - Framework-specific test cases
5. **Document** - Update framework docs

### Virtual Member Generation

```rust
use nova_core::ProjectId;
use nova_framework::{Database, FrameworkAnalyzer, VirtualMember, VirtualMethod};
use nova_types::ClassId;

pub struct LombokAnalyzer;

impl FrameworkAnalyzer for LombokAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        db.has_dependency(project, "org.projectlombok", "lombok")
    }

    fn virtual_members(&self, db: &dyn Database, class: ClassId) -> Vec<VirtualMember> {
        let class_data = db.class(class);
        if !class_data.has_annotation("Getter") {
            return Vec::new();
        }

        class_data
            .fields
            .iter()
            .filter(|field| !field.is_static)
            .map(|field| {
                VirtualMember::Method(VirtualMethod {
                    name: format!("get{}", field.name),
                    return_type: field.ty.clone(),
                    params: Vec::new(),
                    is_static: false,
                    span: class_data.annotation_span("Getter"),
                })
            })
            .collect()
    }
}
```

### Configuration File Support

Framework analyzers can also provide YAML/properties intelligence (e.g. Spring Boot config keys)
through the normal `diagnostics`/`completions` hooks by using `db.file_path(file)` and
`db.file_text(file)` (see `crates/nova-framework-spring/src/analyzer.rs`).

`nova-ide` additionally contains workspace caches and helpers (see
`crates/nova-ide/src/framework_cache.rs`) to avoid rescanning build roots on every request.

---

## Testing

```bash
# Framework support tests
bash scripts/cargo_agent.sh test -p nova-framework --lib
bash scripts/cargo_agent.sh test -p nova-framework-spring --lib
bash scripts/cargo_agent.sh test -p nova-framework-lombok --lib
bash scripts/cargo_agent.sh test -p nova-framework-jpa --lib
bash scripts/cargo_agent.sh test -p nova-apt --lib
```

### Test Structure

```
testdata/
├── spring/
│   ├── bean_discovery.java
│   ├── autowiring.java
│   └── config_properties.java
├── lombok/
│   ├── data_class.java
│   └── builder.java
└── jpa/
    ├── entity_validation.java
    └── jpql_query.java
```

---

## Common Pitfalls

1. **Annotation inheritance** - `@Component` is inherited, `@Getter` is not
2. **Compile-time vs runtime** - Lombok is compile-time, Spring is runtime
3. **Proxy magic** - Spring creates proxies; method calls within same class bypass them
4. **Framework versions** - Behavior differs across versions

---

## Dependencies

**Upstream:** `nova-syntax`, `nova-types`, `nova-resolve`
**Downstream:** `nova-ide` (virtual members appear in completion)

---

## Coordination

Framework analyzers must integrate cleanly with:
- Type system (virtual members must type-check)
- Completion (framework-specific suggestions)
- Navigation (bean → usage, config → code)
- Diagnostics (framework-specific warnings)

---

*Remember: Always follow [AGENTS.md](../AGENTS.md) rules. Use wrapper scripts. Scope your cargo commands.*

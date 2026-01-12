# Framework Support Workstream

> **⚠️ MANDATORY: Read and follow [AGENTS.md](../AGENTS.md) completely before proceeding.**
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
pub trait FrameworkAnalyzer: Send + Sync {
    /// Framework identifier
    fn id(&self) -> &'static str;
    
    /// Check if this framework is active in the project
    fn is_active(&self, project: &ProjectModel) -> bool;
    
    /// Generate virtual members (e.g., Lombok getters)
    fn virtual_members(&self, class: &ClassDecl) -> Vec<VirtualMember>;
    
    /// Provide additional diagnostics
    fn diagnostics(&self, file: FileId) -> Vec<Diagnostic>;
    
    /// Provide navigation targets (e.g., Spring bean → injection point)
    fn navigation(&self, symbol: SymbolId) -> Vec<NavigationTarget>;
    
    /// Provide additional completions
    fn completions(&self, ctx: &CompletionContext) -> Vec<CompletionItem>;
}
```

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
3. **Register analyzer** - In `nova-framework` registry
4. **Add tests** - Framework-specific test cases
5. **Document** - Update framework docs

### Virtual Member Generation

```rust
impl FrameworkAnalyzer for LombokAnalyzer {
    fn virtual_members(&self, class: &ClassDecl) -> Vec<VirtualMember> {
        let mut members = Vec::new();
        
        if has_annotation(class, "lombok.Data") {
            for field in class.fields() {
                members.push(VirtualMember::Method(getter_for(field)));
                members.push(VirtualMember::Method(setter_for(field)));
            }
            members.push(VirtualMember::Method(equals_method(class)));
            members.push(VirtualMember::Method(hashcode_method(class)));
            members.push(VirtualMember::Method(tostring_method(class)));
        }
        
        members
    }
}
```

### Configuration File Support

Many frameworks use YAML/properties files:

```rust
// Link code to configuration
fn config_completions(&self, ctx: &ConfigContext) -> Vec<CompletionItem> {
    // Offer completions in application.yml based on @ConfigurationProperties
}

fn config_navigation(&self, key: &str) -> Option<Location> {
    // Navigate from config key to Java field
}
```

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

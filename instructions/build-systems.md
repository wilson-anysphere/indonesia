# Build Systems Workstream

> **⚠️ MANDATORY: Read and follow [AGENTS.md](../AGENTS.md) completely before proceeding.**
> **All rules in AGENTS.md apply at all times. This file adds workstream-specific guidance.**

---

## Scope

This workstream owns build system integration - understanding Maven, Gradle, and Bazel projects:

| Crate | Purpose |
|-------|---------|
| `nova-build-model` | Common build model + project model types (`ProjectModel`, classpath buckets, module/source-root metadata) + build-system backend trait (`BuildSystemBackend`) |
| `nova-build` | Maven/Gradle build tool integration (classpath + build diagnostics + optional background build orchestration) |
| `nova-build-bazel` | Bazel workspace support |
| `nova-project` | Project discovery, source roots, dependencies |
| `nova-classpath` | Classpath resolution from build files |
| `nova-deps-cache` | Dependency caching |

---

## Key Documents

**Required reading:**
- [03 - Architecture Overview](../docs/03-architecture-overview.md) - Project model section
- [Gradle build integration](../docs/gradle-build-integration.md) - `nova-build` ↔ `nova-project` snapshot handoff (`.nova/queries/gradle.json`)

---

## Architecture

### Project Model

```
┌─────────────────────────────────────────────────────────────────┐
│                    Project Model                                 │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Workspace                                                      │
│  ├── Project (module)                                           │
│  │   ├── source_roots: [src/main/java, src/test/java]          │
│  │   ├── dependencies: [dep1, dep2, ...]                       │
│  │   ├── java_version: 17                                      │
│  │   └── output_dir: target/classes                            │
│  ├── Project (module)                                           │
│  │   └── ...                                                    │
│  └── ...                                                        │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Build System Abstraction
 
```rust
pub trait BuildSystemBackend: Send + Sync {
    /// Detect if this build system is used
    fn detect(&self, root: &Path) -> bool;
    
    /// Parse project structure
    fn parse_project(&self, root: &Path) -> Result<ProjectModel, BuildSystemError>;
    
    /// Resolve dependencies to classpath
    fn resolve_classpath(&self, project: &ProjectModel) -> Result<Classpath, BuildSystemError>;
    
    /// Watch for build file changes
    fn watch_files(&self) -> Vec<PathPattern>;
}
```

---

## Build Systems

### Maven

**Detection:** `pom.xml` in root

**Project structure:**
```xml
<project>
    <groupId>com.example</groupId>
    <artifactId>my-app</artifactId>
    <version>1.0.0</version>
    
    <properties>
        <maven.compiler.source>17</maven.compiler.source>
        <maven.compiler.target>17</maven.compiler.target>
    </properties>
    
    <dependencies>
        <dependency>
            <groupId>org.springframework</groupId>
            <artifactId>spring-core</artifactId>
            <version>6.0.0</version>
        </dependency>
    </dependencies>
    
    <modules>
        <module>core</module>
        <module>web</module>
    </modules>
</project>
```

**Source roots:**
- `src/main/java` - production code
- `src/test/java` - test code
- `src/main/resources` - resources

**Dependency resolution:**
- Parse `pom.xml` for declared dependencies
- Use Maven local repository (`~/.m2/repository`)
- Resolve transitive dependencies

### Gradle

**Detection:** `build.gradle` or `build.gradle.kts` in root

**Project structure:**
```kotlin
// build.gradle.kts
plugins {
    java
}

java {
    sourceCompatibility = JavaVersion.VERSION_17
}

dependencies {
    implementation("org.springframework:spring-core:6.0.0")
    testImplementation("junit:junit:4.13.2")
}
```

**Source sets:**
```kotlin
sourceSets {
    main {
        java.srcDirs("src/main/java", "src/generated/java")
    }
}
```

**Dependency resolution:**
- **Heuristic mode (`nova-project`)**: parse build scripts (Groovy/Kotlin DSL) and do best-effort jar
  lookup in the local Gradle cache (no transitive resolution, variant selection, etc).
- **Build-tool mode (`nova-build`)**: execute Gradle and extract resolved compilation inputs
  (classpath/source roots/output dirs/language level); results are persisted to
  `.nova/queries/gradle.json` and then reused by `nova-project`.

### Bazel

**Detection:** `WORKSPACE` or `MODULE.bazel` in root

**Build rules:**
```python
# BUILD.bazel
java_library(
    name = "mylib",
    srcs = glob(["src/**/*.java"]),
    deps = [
        "@maven//:com_google_guava_guava",
        "//other:lib",
    ],
)
```

**Challenges:**
- Complex dependency graph
- Hermeticity requirements
- bzlmod vs WORKSPACE

---

## Development Guidelines

### Adding Build System Support

1. **Implement `BuildSystem` trait**
2. **Add project detection**
3. **Parse project structure**
4. **Resolve classpath**
5. **Handle multi-module projects**
6. **Add incremental updates**

### Classpath Resolution

```rust
pub struct Classpath {
    /// Compile classpath (production dependencies)
    pub compile: Vec<ClasspathEntry>,
    /// Runtime classpath
    pub runtime: Vec<ClasspathEntry>,
    /// Test classpath (includes test dependencies)
    pub test: Vec<ClasspathEntry>,
}

pub enum ClasspathEntry {
    /// Local JAR file
    Jar(PathBuf),
    /// Class directory
    Directory(PathBuf),
    /// Maven coordinates (to be resolved)
    Maven { group: String, artifact: String, version: String },
}
```

### Dependency Caching

Dependencies should be cached to avoid re-downloading:

```rust
// Check cache first
if let Some(jar) = cache.get(&coords) {
    return Ok(jar);
}

// Download and cache
let jar = download_jar(&coords)?;
cache.put(&coords, &jar)?;
Ok(jar)
```

### Multi-Module Projects

```
my-project/
├── pom.xml (parent)
├── core/
│   ├── pom.xml
│   └── src/main/java/
├── web/
│   ├── pom.xml
│   └── src/main/java/
└── api/
    ├── pom.xml
    └── src/main/java/
```

**Requirements:**
- Detect parent-child relationships
- Resolve inter-module dependencies
- Separate classpath per module

---

## Testing

```bash
# Build system tests
bash scripts/cargo_agent.sh test --locked -p nova-build --lib
bash scripts/cargo_agent.sh test --locked -p nova-build-bazel --lib
bash scripts/cargo_agent.sh test --locked -p nova-project --lib
bash scripts/cargo_agent.sh test --locked -p nova-classpath --lib
```

### Test Structure

```
testdata/
├── maven/
│   ├── simple/
│   │   └── pom.xml
│   ├── multi-module/
│   │   ├── pom.xml
│   │   ├── core/pom.xml
│   │   └── web/pom.xml
│   └── ...
├── gradle/
│   ├── kotlin-dsl/
│   │   └── build.gradle.kts
│   └── groovy-dsl/
│       └── build.gradle
└── bazel/
    ├── WORKSPACE
    └── src/BUILD.bazel
```

---

## Common Pitfalls

1. **Version conflicts** - Different modules may need different versions
2. **Scope confusion** - `compile` vs `runtime` vs `test` vs `provided`
3. **Custom configurations** - Non-standard source layouts
4. **Build variants** - Gradle build types, Maven profiles
5. **Plugin-generated sources** - Annotation processors, protobuf, etc.

---

## Dependencies

**Upstream:** `nova-core`, `nova-vfs`
**Downstream:** `nova-workspace`, `nova-classpath` → used by type resolution

---

## Coordination

Build system changes affect:
- Project discovery
- Source root detection  
- Dependency resolution
- Classpath for type checking

Coordinate with semantic analysis team when changing project model.

---

*Remember: Always follow [AGENTS.md](../AGENTS.md) rules. Use wrapper scripts. Scope your cargo commands.*

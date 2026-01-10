# 09 - Framework Support

[← Back to Main Document](../AGENTS.md) | [Previous: Refactoring Engine](08-refactoring-engine.md)

## Overview

Modern Java development is dominated by frameworks like Spring, Jakarta EE, and tools like Lombok. Deep framework support is where IntelliJ truly shines and where Nova must invest significantly. This document covers the approach to framework-aware analysis.

---

## Framework Support Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                FRAMEWORK SUPPORT ARCHITECTURE                    │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                    CORE NOVA                             │    │
│  │  • Semantic analysis                                    │    │
│  │  • Type checking                                        │    │
│  │  • Symbol resolution                                    │    │
│  └───────────────────────┬─────────────────────────────────┘    │
│                          │                                       │
│                          ▼                                       │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │              FRAMEWORK ANALYZER INTERFACE                │    │
│  │  • Hooks into resolution                                │    │
│  │  • Provides additional completions                      │    │
│  │  • Adds framework-specific diagnostics                  │    │
│  │  • Extends navigation                                   │    │
│  └───────────────────────┬─────────────────────────────────┘    │
│                          │                                       │
│          ┌───────────────┼───────────────┬───────────────┐      │
│          ▼               ▼               ▼               ▼      │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐ ┌───────────┐  │
│  │   Spring    │ │  Jakarta    │ │   Lombok    │ │  Others   │  │
│  │  Analyzer   │ │  Analyzer   │ │  Analyzer   │ │  ...      │  │
│  └─────────────┘ └─────────────┘ └─────────────┘ └───────────┘  │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Spring Framework Support

### Bean Model

```
┌─────────────────────────────────────────────────────────────────┐
│                    SPRING BEAN MODEL                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  BEAN DISCOVERY                                                 │
│  • @Component, @Service, @Repository, @Controller               │
│  • @Bean methods in @Configuration classes                      │
│  • XML configuration (legacy)                                   │
│  • @Import and @ComponentScan                                   │
│                                                                  │
│  BEAN METADATA                                                  │
│  • Bean name (explicit or derived)                              │
│  • Type and qualifiers                                          │
│  • Scope (@Scope, @Singleton, @Prototype, etc.)                 │
│  • Profile (@Profile)                                           │
│  • Conditional (@ConditionalOnXxx)                              │
│  • Dependencies (constructor args, @Autowired fields)           │
│                                                                  │
│  RESOLUTION CONTEXT                                              │
│  • Active profiles                                              │
│  • Property sources                                             │
│  • Conditional evaluations                                      │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Spring Analyzer Implementation

```rust
pub struct SpringAnalyzer {
    /// Cached bean definitions
    beans: DashMap<ProjectId, Arc<BeanModel>>,
}

impl FrameworkAnalyzer for SpringAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        // Check for Spring dependencies
        db.has_dependency(project, "org.springframework")
    }
    
    fn analyze_file(&self, db: &dyn Database, file: FileId) -> FrameworkData {
        let mut beans = Vec::new();
        let item_tree = db.item_tree(file);
        
        for class in item_tree.classes() {
            // Check for component annotations
            if let Some(bean) = self.analyze_component(db, class) {
                beans.push(bean);
            }
            
            // Check for @Configuration @Bean methods
            if class.has_annotation("Configuration") {
                for method in class.methods() {
                    if method.has_annotation("Bean") {
                        beans.push(self.analyze_bean_method(db, method));
                    }
                }
            }
        }
        
        FrameworkData::Spring { beans }
    }
    
    fn resolve_injection(&self, db: &dyn Database, injection: &InjectionPoint) -> Vec<BeanRef> {
        let bean_model = self.get_bean_model(db, injection.project);
        
        // Find candidates by type
        let candidates: Vec<_> = bean_model.beans
            .iter()
            .filter(|b| db.is_assignable(&b.bean_type, &injection.target_type))
            .collect();
        
        // Filter by qualifier if present
        let qualified = if let Some(qualifier) = &injection.qualifier {
            candidates.iter()
                .filter(|b| b.qualifiers.contains(qualifier))
                .cloned()
                .collect()
        } else {
            candidates
        };
        
        // Filter by name if @Autowired with name
        if let Some(name) = &injection.name {
            qualified.iter()
                .filter(|b| &b.name == name)
                .cloned()
                .collect()
        } else {
            qualified
        }
    }
}
```

### Spring Diagnostics

```rust
impl SpringAnalyzer {
    fn diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let bean_model = self.get_bean_model(db, file.project());
        
        // Check autowired fields
        for injection in db.injection_points(file) {
            let candidates = self.resolve_injection(db, &injection);
            
            match candidates.len() {
                0 => {
                    diagnostics.push(Diagnostic {
                        range: injection.range,
                        severity: Severity::Error,
                        message: format!(
                            "No bean of type '{}' found for injection",
                            format_type(&injection.target_type)
                        ),
                        code: "spring-no-bean",
                        related: suggest_similar_beans(db, &injection),
                    });
                }
                1 => {} // OK
                _ if !injection.has_qualifier => {
                    diagnostics.push(Diagnostic {
                        range: injection.range,
                        severity: Severity::Error,
                        message: format!(
                            "Multiple beans of type '{}' found. Use @Qualifier",
                            format_type(&injection.target_type)
                        ),
                        code: "spring-ambiguous-bean",
                        related: candidates.iter().map(|c| c.location.clone()).collect(),
                    });
                }
                _ => {}
            }
        }
        
        // Check for circular dependencies
        for cycle in detect_circular_dependencies(&bean_model) {
            for bean in &cycle {
                if bean.file == file {
                    diagnostics.push(Diagnostic {
                        range: bean.range,
                        severity: Severity::Warning,
                        message: "Circular dependency detected".into(),
                        code: "spring-circular-dependency",
                        related: cycle.iter().map(|b| b.location.clone()).collect(),
                    });
                }
            }
        }
        
        diagnostics
    }
}
```

### Spring Navigation

```rust
impl SpringAnalyzer {
    fn navigation(&self, db: &dyn Database, symbol: &Symbol) -> Vec<NavigationTarget> {
        let mut targets = Vec::new();
        
        match symbol {
            // Navigate from @Autowired to bean definition
            Symbol::Field(field) if has_autowired_annotation(db, *field) => {
                let injection = injection_point_for_field(db, *field);
                let beans = self.resolve_injection(db, &injection);
                
                for bean in beans {
                    targets.push(NavigationTarget {
                        uri: bean.file,
                        range: bean.definition_range,
                        label: format!("Bean: {}", bean.name),
                    });
                }
            }
            
            // Navigate from bean definition to usages
            Symbol::Class(class) if is_spring_bean(db, *class) => {
                let bean = self.get_bean(db, *class);
                for usage in self.find_bean_usages(db, &bean) {
                    targets.push(NavigationTarget {
                        uri: usage.file,
                        range: usage.range,
                        label: "Injected here".into(),
                    });
                }
            }
            
            _ => {}
        }
        
        targets
    }
}
```

### Spring Completion

```rust
impl SpringAnalyzer {
    fn completions(&self, db: &dyn Database, ctx: &CompletionContext) -> Vec<CompletionItem> {
        let mut items = Vec::new();
        
        match ctx {
            // Complete @Value("${...}") properties
            CompletionContext::Annotation { name: "Value", in_string: true } => {
                for prop in db.spring_properties() {
                    items.push(CompletionItem {
                        label: prop.key.clone(),
                        kind: CompletionKind::Property,
                        detail: Some(prop.value.clone()),
                        insert_text: format!("${{{}}}", prop.key),
                    });
                }
            }
            
            // Complete @Qualifier names
            CompletionContext::Annotation { name: "Qualifier", .. } => {
                let bean_model = self.get_bean_model(db, ctx.project);
                for bean in &bean_model.beans {
                    items.push(CompletionItem {
                        label: bean.name.clone(),
                        kind: CompletionKind::Reference,
                        detail: Some(format_type(&bean.bean_type)),
                    });
                }
            }
            
            // Complete @Profile names
            CompletionContext::Annotation { name: "Profile", .. } => {
                for profile in db.known_profiles() {
                    items.push(CompletionItem {
                        label: profile.clone(),
                        kind: CompletionKind::Constant,
                    });
                }
            }
            
            _ => {}
        }
        
        items
    }
}
```

---

## Lombok Support

### Lombok Processing Model

```
┌─────────────────────────────────────────────────────────────────┐
│                    LOMBOK PROCESSING                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  CHALLENGE                                                      │
│  Lombok generates code at compile time via annotation           │
│  processing. IDE must understand generated members without      │
│  actually running the processor.                                │
│                                                                  │
│  APPROACH: Virtual Members                                      │
│  • Parse Lombok annotations                                     │
│  • Compute what would be generated                              │
│  • Add "virtual" members to class symbol table                  │
│  • These virtual members participate in resolution              │
│                                                                  │
│  SUPPORTED ANNOTATIONS                                          │
│  • @Getter / @Setter                                            │
│  • @Data, @Value                                                │
│  • @Builder                                                     │
│  • @NoArgsConstructor, @AllArgsConstructor, @RequiredArgsConstructor │
│  • @ToString, @EqualsAndHashCode                                │
│  • @Slf4j, @Log, @Log4j2, etc.                                  │
│  • @With                                                        │
│  • @Delegate                                                    │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### Lombok Analyzer

```rust
pub struct LombokAnalyzer;

impl LombokAnalyzer {
    /// Generate virtual members for Lombok annotations
    pub fn virtual_members(&self, db: &dyn Database, class: ClassId) -> Vec<VirtualMember> {
        let class_data = db.class(class);
        let mut members = Vec::new();
        
        // @Getter on class
        if class_data.has_annotation("Getter") {
            for field in &class_data.fields {
                if !field.is_static {
                    members.push(self.generate_getter(field));
                }
            }
        }
        
        // @Getter on individual fields
        for field in &class_data.fields {
            if field.has_annotation("Getter") {
                members.push(self.generate_getter(field));
            }
        }
        
        // @Setter
        if class_data.has_annotation("Setter") || class_data.has_annotation("Data") {
            for field in &class_data.fields {
                if !field.is_static && !field.is_final {
                    members.push(self.generate_setter(field));
                }
            }
        }
        
        // @Builder
        if class_data.has_annotation("Builder") {
            members.extend(self.generate_builder(&class_data));
        }
        
        // @AllArgsConstructor
        if class_data.has_annotation("AllArgsConstructor") || 
           class_data.has_annotation("Data") {
            members.push(self.generate_all_args_constructor(&class_data));
        }
        
        // @NoArgsConstructor
        if class_data.has_annotation("NoArgsConstructor") {
            members.push(self.generate_no_args_constructor(&class_data));
        }
        
        // @Slf4j and logging annotations
        if class_data.has_annotation("Slf4j") {
            members.push(VirtualMember::Field {
                name: "log".into(),
                ty: Type::class("org.slf4j.Logger"),
                is_static: true,
                is_final: true,
            });
        }
        
        members
    }
    
    fn generate_getter(&self, field: &Field) -> VirtualMember {
        let prefix = if field.ty.is_boolean() { "is" } else { "get" };
        let name = format!("{}{}", prefix, capitalize(&field.name));
        
        VirtualMember::Method {
            name,
            return_type: field.ty.clone(),
            parameters: vec![],
            is_static: false,
        }
    }
    
    fn generate_builder(&self, class: &Class) -> Vec<VirtualMember> {
        let builder_class_name = format!("{}Builder", class.name);
        let mut members = Vec::new();
        
        // Static builder() method
        members.push(VirtualMember::Method {
            name: "builder".into(),
            return_type: Type::class(&builder_class_name),
            parameters: vec![],
            is_static: true,
        });
        
        // Builder inner class with fluent setters
        let builder_methods: Vec<_> = class.fields.iter()
            .filter(|f| !f.is_static)
            .map(|f| VirtualMember::Method {
                name: f.name.clone(),
                return_type: Type::class(&builder_class_name),
                parameters: vec![Parameter { name: f.name.clone(), ty: f.ty.clone() }],
                is_static: false,
            })
            .collect();
        
        // build() method
        members.push(VirtualMember::InnerClass {
            name: builder_class_name,
            members: builder_methods,
        });
        
        members
    }
}
```

---

## Jakarta EE / JPA Support

### JPA Entity Analysis

```rust
pub struct JpaAnalyzer;

impl JpaAnalyzer {
    fn analyze_entity(&self, db: &dyn Database, class: ClassId) -> Option<EntityModel> {
        let class_data = db.class(class);
        
        if !class_data.has_annotation("Entity") {
            return None;
        }
        
        let table_name = class_data.get_annotation("Table")
            .and_then(|a| a.get("name"))
            .unwrap_or_else(|| class_data.name.clone());
        
        let columns: Vec<_> = class_data.fields.iter()
            .filter(|f| !f.has_annotation("Transient"))
            .map(|f| self.analyze_column(db, f))
            .collect();
        
        let id_field = columns.iter()
            .find(|c| c.is_id)
            .map(|c| c.name.clone());
        
        let relationships: Vec<_> = class_data.fields.iter()
            .filter_map(|f| self.analyze_relationship(db, f))
            .collect();
        
        Some(EntityModel {
            class,
            table_name,
            columns,
            id_field,
            relationships,
        })
    }
    
    fn analyze_relationship(&self, db: &dyn Database, field: &Field) -> Option<Relationship> {
        if field.has_annotation("OneToMany") {
            let target = extract_generic_type(&field.ty)?;
            let mapped_by = field.get_annotation("OneToMany")?.get("mappedBy");
            Some(Relationship::OneToMany { target, mapped_by })
        } else if field.has_annotation("ManyToOne") {
            Some(Relationship::ManyToOne { target: field.ty.clone() })
        } else if field.has_annotation("ManyToMany") {
            let target = extract_generic_type(&field.ty)?;
            Some(Relationship::ManyToMany { target })
        } else if field.has_annotation("OneToOne") {
            Some(Relationship::OneToOne { target: field.ty.clone() })
        } else {
            None
        }
    }
    
    fn diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        
        for entity in db.entities_in_file(file) {
            // Check for @Id
            if entity.id_field.is_none() {
                diagnostics.push(Diagnostic {
                    range: entity.class_range,
                    severity: Severity::Error,
                    message: "Entity must have an @Id field".into(),
                    code: "jpa-missing-id",
                });
            }
            
            // Check relationship validity
            for rel in &entity.relationships {
                if let Some(issue) = self.check_relationship(db, rel) {
                    diagnostics.push(issue);
                }
            }
            
            // Check for default constructor
            if !entity.has_no_arg_constructor {
                diagnostics.push(Diagnostic {
                    range: entity.class_range,
                    severity: Severity::Warning,
                    message: "Entity should have a no-argument constructor".into(),
                    code: "jpa-no-default-constructor",
                });
            }
        }
        
        diagnostics
    }
}
```

### JPQL Support

```rust
impl JpaAnalyzer {
    fn analyze_query(&self, db: &dyn Database, query: &str, location: TextRange) -> QueryAnalysis {
        let parse_result = parse_jpql(query);
        
        let mut diagnostics = Vec::new();
        
        // Check entity names
        for entity_ref in parse_result.entity_references() {
            if !db.jpa_entity_exists(&entity_ref.name) {
                diagnostics.push(Diagnostic {
                    range: entity_ref.range.offset(location.start()),
                    severity: Severity::Error,
                    message: format!("Unknown entity: {}", entity_ref.name),
                    code: "jpql-unknown-entity",
                });
            }
        }
        
        // Check field references
        for field_ref in parse_result.field_references() {
            let entity = &field_ref.entity;
            if let Some(entity_model) = db.jpa_entity(entity) {
                if !entity_model.has_field(&field_ref.field) {
                    diagnostics.push(Diagnostic {
                        range: field_ref.range.offset(location.start()),
                        severity: Severity::Error,
                        message: format!("Unknown field: {}", field_ref.field),
                        code: "jpql-unknown-field",
                    });
                }
            }
        }
        
        QueryAnalysis {
            diagnostics,
            completions: self.query_completions(&parse_result, db),
        }
    }
    
    fn query_completions(&self, parse: &JpqlParse, db: &dyn Database) -> Vec<CompletionItem> {
        match parse.completion_context() {
            JpqlContext::EntityName => {
                db.all_jpa_entities()
                    .iter()
                    .map(|e| CompletionItem {
                        label: e.name.clone(),
                        kind: CompletionKind::Class,
                    })
                    .collect()
            }
            JpqlContext::FieldAccess(entity) => {
                if let Some(model) = db.jpa_entity(&entity) {
                    model.columns.iter()
                        .map(|c| CompletionItem {
                            label: c.name.clone(),
                            kind: CompletionKind::Field,
                            detail: Some(format_type(&c.ty)),
                        })
                        .collect()
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }
}
```

---

## Annotation Processing Simulation

```
┌─────────────────────────────────────────────────────────────────┐
│            ANNOTATION PROCESSING IN THE IDE                      │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  CHALLENGES                                                     │
│  • Annotation processors generate source code at compile time   │
│  • IDE needs to understand generated code without compiling     │
│  • Must handle: MapStruct, Dagger, AutoValue, Immutables, etc.  │
│                                                                  │
│  STRATEGIES                                                     │
│                                                                  │
│  1. DEDICATED ANALYZERS (Lombok approach)                       │
│     • Hand-coded simulation of specific processors              │
│     • Most accurate for supported processors                    │
│     • Requires maintenance per processor                        │
│                                                                  │
│  2. GENERATED SOURCE DIRECTORIES                                │
│     • Run processors once, index generated sources              │
│     • Works with any processor                                  │
│     • May be stale until rebuild                                │
│                                                                  │
│  3. INCREMENTAL PROCESSOR INVOCATION                            │
│     • Run processors on demand for specific files               │
│     • Most accurate, but slow                                   │
│     • Best for expensive processors                             │
│                                                                  │
│  RECOMMENDATION: Hybrid approach                                │
│  • Dedicated analyzers for common processors                    │
│  • Generated source indexing for others                         │
│  • Optional on-demand processing for validation                 │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Framework Plugin System

```rust
/// Extension point for framework analyzers
pub trait FrameworkAnalyzer: Send + Sync {
    /// Check if this analyzer applies to the project
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool;
    
    /// Analyze a file for framework-specific data
    fn analyze_file(&self, db: &dyn Database, file: FileId) -> FrameworkData;
    
    /// Provide additional diagnostics
    fn diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic>;
    
    /// Provide additional completions
    fn completions(&self, db: &dyn Database, ctx: &CompletionContext) -> Vec<CompletionItem>;
    
    /// Provide additional navigation targets
    fn navigation(&self, db: &dyn Database, symbol: &Symbol) -> Vec<NavigationTarget>;
    
    /// Provide virtual members (like Lombok-generated methods)
    fn virtual_members(&self, db: &dyn Database, class: ClassId) -> Vec<VirtualMember>;
    
    /// Provide inlay hints
    fn inlay_hints(&self, db: &dyn Database, file: FileId) -> Vec<InlayHint>;
}

/// Register built-in and custom framework analyzers
pub fn register_framework_analyzers(registry: &mut AnalyzerRegistry) {
    registry.register(Box::new(SpringAnalyzer::new()));
    registry.register(Box::new(LombokAnalyzer::new()));
    registry.register(Box::new(JpaAnalyzer::new()));
    registry.register(Box::new(JaxRsAnalyzer::new()));
    registry.register(Box::new(MicronautAnalyzer::new()));
    registry.register(Box::new(QuarkusAnalyzer::new()));
}
```

---

## Next Steps

1. → [Performance Engineering](10-performance-engineering.md): Making it all fast
2. → [Editor Integration](11-editor-integration.md): LSP and beyond

---

[← Previous: Refactoring Engine](08-refactoring-engine.md) | [Next: Performance Engineering →](10-performance-engineering.md)

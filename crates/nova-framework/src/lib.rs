//! Framework analyzer abstraction.
//!
//! Framework analyzers (Spring, Lombok, etc.) augment core resolution by
//! providing additional members and metadata that are generated at compile time.

use std::collections::{HashMap, HashSet};

use nova_hir::framework::ClassData;
use nova_types::{ClassId, Parameter, ProjectId, Type};

/// Maven/Gradle dependency coordinate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DependencyCoordinate {
    pub group: String,
    pub artifact: String,
}

impl DependencyCoordinate {
    pub fn new(group: impl Into<String>, artifact: impl Into<String>) -> Self {
        Self {
            group: group.into(),
            artifact: artifact.into(),
        }
    }
}

/// Query interface used by framework analyzers and member resolution.
///
/// In real Nova this will likely be backed by the incremental database. For
/// unit tests we provide a small in-memory implementation.
pub trait Database {
    fn class(&self, class: ClassId) -> &ClassData;
    fn project_of_class(&self, class: ClassId) -> ProjectId;

    fn has_dependency(&self, project: ProjectId, group: &str, artifact: &str) -> bool;
    fn has_class_on_classpath(&self, project: ProjectId, binary_name: &str) -> bool;
    fn has_class_on_classpath_prefix(&self, project: ProjectId, prefix: &str) -> bool;
}

/// Simple in-memory database for unit tests and examples.
#[derive(Default)]
pub struct MemoryDatabase {
    next_project: u32,
    next_class: u32,
    class_data: HashMap<ClassId, ClassData>,
    class_project: HashMap<ClassId, ProjectId>,
    dependencies: HashMap<ProjectId, HashSet<DependencyCoordinate>>,
    classpath_classes: HashMap<ProjectId, HashSet<String>>,
}

impl MemoryDatabase {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_project(&mut self) -> ProjectId {
        let id = ProjectId::new(self.next_project);
        self.next_project += 1;
        id
    }

    pub fn add_dependency(&mut self, project: ProjectId, group: &str, artifact: &str) {
        self.dependencies
            .entry(project)
            .or_default()
            .insert(DependencyCoordinate::new(group, artifact));
    }

    pub fn add_classpath_class(&mut self, project: ProjectId, binary_name: &str) {
        self.classpath_classes
            .entry(project)
            .or_default()
            .insert(binary_name.to_string());
    }

    pub fn add_class(&mut self, project: ProjectId, class: ClassData) -> ClassId {
        let id = ClassId::new(self.next_class);
        self.next_class += 1;
        self.class_project.insert(id, project);
        self.class_data.insert(id, class);
        id
    }
}

impl Database for MemoryDatabase {
    fn class(&self, class: ClassId) -> &ClassData {
        self.class_data
            .get(&class)
            .expect("unknown ClassId passed to db.class()")
    }

    fn project_of_class(&self, class: ClassId) -> ProjectId {
        *self
            .class_project
            .get(&class)
            .expect("unknown ClassId passed to db.project_of_class()")
    }

    fn has_dependency(&self, project: ProjectId, group: &str, artifact: &str) -> bool {
        self.dependencies
            .get(&project)
            .map(|deps| deps.contains(&DependencyCoordinate::new(group, artifact)))
            .unwrap_or(false)
    }

    fn has_class_on_classpath(&self, project: ProjectId, binary_name: &str) -> bool {
        self.classpath_classes
            .get(&project)
            .map(|classes| classes.contains(binary_name))
            .unwrap_or(false)
    }

    fn has_class_on_classpath_prefix(&self, project: ProjectId, prefix: &str) -> bool {
        self.classpath_classes
            .get(&project)
            .map(|classes| classes.iter().any(|name| name.starts_with(prefix)))
            .unwrap_or(false)
    }
}

/// Virtual members provided by a framework analyzer (e.g. Lombok).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirtualMember {
    Field(VirtualField),
    Method(VirtualMethod),
    Constructor(VirtualConstructor),
    InnerClass(VirtualInnerClass),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualField {
    pub name: String,
    pub ty: Type,
    pub is_static: bool,
    pub is_final: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualMethod {
    pub name: String,
    pub return_type: Type,
    pub params: Vec<Parameter>,
    pub is_static: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualConstructor {
    pub params: Vec<Parameter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualInnerClass {
    pub name: String,
    pub members: Vec<VirtualMember>,
}

/// Extension point for framework analyzers.
pub trait FrameworkAnalyzer: Send + Sync {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool;
    fn virtual_members(&self, db: &dyn Database, class: ClassId) -> Vec<VirtualMember>;
}

#[derive(Default)]
pub struct AnalyzerRegistry {
    analyzers: Vec<Box<dyn FrameworkAnalyzer>>,
}

impl AnalyzerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, analyzer: Box<dyn FrameworkAnalyzer>) {
        self.analyzers.push(analyzer);
    }

    pub fn virtual_members_for_class(
        &self,
        db: &dyn Database,
        class: ClassId,
    ) -> Vec<VirtualMember> {
        let project = db.project_of_class(class);
        self.analyzers
            .iter()
            .filter(|a| a.applies_to(db, project))
            .flat_map(|a| a.virtual_members(db, class))
            .collect()
    }
}


//! Framework analyzer abstraction.
//!
//! Framework analyzers (Spring, Lombok, etc.) augment core resolution by
//! providing additional members and metadata that are generated at compile time.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use nova_core::ProjectId;
use nova_hir::framework::ClassData;
use nova_types::{ClassId, CompletionItem, Diagnostic, Parameter, Span, Type};
use nova_vfs::FileId;

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
    fn project_of_file(&self, file: FileId) -> ProjectId;

    /// Returns the UTF-8 contents of `file` if the database has them available.
    ///
    /// Framework analyzers should treat a `None` return as "no text available"
    /// and either return no results or fall back to structural information (HIR).
    fn file_text(&self, _file: FileId) -> Option<&str> {
        None
    }

    fn has_dependency(&self, project: ProjectId, group: &str, artifact: &str) -> bool;
    fn has_class_on_classpath(&self, project: ProjectId, binary_name: &str) -> bool;
    fn has_class_on_classpath_prefix(&self, project: ProjectId, prefix: &str) -> bool;
}

/// Simple in-memory database for unit tests and examples.
#[derive(Default)]
pub struct MemoryDatabase {
    next_project: u32,
    next_class: u32,
    next_file: u32,
    class_data: HashMap<ClassId, ClassData>,
    class_project: HashMap<ClassId, ProjectId>,
    file_project: HashMap<FileId, ProjectId>,
    file_text: HashMap<FileId, String>,
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

    pub fn add_file(&mut self, project: ProjectId) -> FileId {
        let id = FileId::from_raw(self.next_file);
        self.next_file += 1;
        self.file_project.insert(id, project);
        id
    }

    pub fn add_file_with_text(&mut self, project: ProjectId, text: impl Into<String>) -> FileId {
        let id = self.add_file(project);
        self.set_file_text(id, text);
        id
    }

    pub fn set_file_text(&mut self, file: FileId, text: impl Into<String>) {
        self.file_text.insert(file, text.into());
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

    fn project_of_file(&self, file: FileId) -> ProjectId {
        *self
            .file_project
            .get(&file)
            .expect("unknown FileId passed to db.project_of_file()")
    }

    fn file_text(&self, file: FileId) -> Option<&str> {
        self.file_text.get(&file).map(String::as_str)
    }

    fn has_dependency(&self, project: ProjectId, group: &str, artifact: &str) -> bool {
        self.dependencies
            .get(&project)
            .map(|deps| deps.contains(&DependencyCoordinate::new(group, artifact)))
            .unwrap_or(false)
    }

    fn has_class_on_classpath(&self, project: ProjectId, binary_name: &str) -> bool {
        let Some(classes) = self.classpath_classes.get(&project) else {
            return false;
        };

        if classes.contains(binary_name) {
            return true;
        }

        // Be tolerant of callers mixing Java binary names (`java.lang.String`) and
        // JVM internal names (`java/lang/String`).
        if let Some(alt) = normalize_name_separators(binary_name) {
            if classes.contains(alt.as_ref()) {
                return true;
            }
        }

        false
    }

    fn has_class_on_classpath_prefix(&self, project: ProjectId, prefix: &str) -> bool {
        let Some(classes) = self.classpath_classes.get(&project) else {
            return false;
        };

        if classes.iter().any(|name| name.starts_with(prefix)) {
            return true;
        }

        if let Some(alt) = normalize_name_separators(prefix) {
            if classes.iter().any(|name| name.starts_with(alt.as_ref())) {
                return true;
            }
        }

        false
    }
}

fn normalize_name_separators(value: &str) -> Option<Cow<'_, str>> {
    if value.contains('/') {
        return Some(Cow::Owned(value.replace('/', ".")));
    }
    if value.contains('.') {
        return Some(Cow::Owned(value.replace('.', "/")));
    }
    None
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

// -----------------------------------------------------------------------------
// Shared framework data model (minimal, extensible)
// -----------------------------------------------------------------------------

/// Framework-specific data extracted from a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameworkData {
    Spring(SpringData),
    Lombok(LombokData),
    Jpa(JpaData),
    Other(OtherFrameworkData),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpringData {
    pub beans: Vec<BeanDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeanDefinition {
    pub name: String,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LombokData {
    pub generated_members: Vec<VirtualMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JpaData {
    pub entities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherFrameworkData {
    pub kind: String,
}

/// A dependency injection point (constructor param, field, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectionPoint {
    pub span: Option<Span>,
    pub target_type: Type,
    pub qualifier: Option<String>,
    pub name: Option<String>,
}

/// A resolved reference to a DI container bean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeanRef {
    pub name: String,
    pub ty: Type,
    pub defined_at: Option<Span>,
}

/// Context used to compute completions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionContext {
    pub project: ProjectId,
    pub file: FileId,
    pub offset: usize,
}

/// A navigation target (e.g. "go to definition" result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationTarget {
    pub file: FileId,
    pub span: Option<Span>,
    pub label: String,
}

/// A lightweight symbol handle used for framework navigation hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Symbol {
    File(FileId),
    Class(ClassId),
}

/// An inlay hint produced by a framework analyzer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHint {
    pub span: Option<Span>,
    pub label: String,
}

// -----------------------------------------------------------------------------
// Analyzer trait + registry
// -----------------------------------------------------------------------------

/// Extension point for framework analyzers.
///
/// Most methods have default no-op implementations, allowing analyzers to focus
/// on the hooks they care about (e.g. Lombok only needs `virtual_members`).
pub trait FrameworkAnalyzer: Send + Sync {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool;

    fn analyze_file(&self, _db: &dyn Database, _file: FileId) -> Option<FrameworkData> {
        None
    }

    fn diagnostics(&self, _db: &dyn Database, _file: FileId) -> Vec<Diagnostic> {
        Vec::new()
    }

    fn completions(&self, _db: &dyn Database, _ctx: &CompletionContext) -> Vec<CompletionItem> {
        Vec::new()
    }

    fn navigation(&self, _db: &dyn Database, _symbol: &Symbol) -> Vec<NavigationTarget> {
        Vec::new()
    }

    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        Vec::new()
    }

    fn inlay_hints(&self, _db: &dyn Database, _file: FileId) -> Vec<InlayHint> {
        Vec::new()
    }
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

    fn applicable_analyzers<'a>(
        &'a self,
        db: &'a dyn Database,
        project: ProjectId,
    ) -> impl Iterator<Item = &'a dyn FrameworkAnalyzer> + 'a {
        self.analyzers
            .iter()
            .map(|a| a.as_ref())
            .filter(move |a| a.applies_to(db, project))
    }

    /// Aggregate `FrameworkData` across all applicable analyzers.
    pub fn framework_data(&self, db: &dyn Database, file: FileId) -> Vec<FrameworkData> {
        let project = db.project_of_file(file);
        self.applicable_analyzers(db, project)
            .filter_map(|a| a.analyze_file(db, file))
            .collect()
    }

    /// Collect all framework diagnostics for `file`.
    pub fn framework_diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
        let project = db.project_of_file(file);
        let mut out = Vec::new();
        for analyzer in self.applicable_analyzers(db, project) {
            out.extend(analyzer.diagnostics(db, file));
        }
        out
    }

    /// Collect all framework completion items for a completion context.
    pub fn framework_completions(
        &self,
        db: &dyn Database,
        ctx: &CompletionContext,
    ) -> Vec<CompletionItem> {
        let mut out = Vec::new();
        for analyzer in self.applicable_analyzers(db, ctx.project) {
            out.extend(analyzer.completions(db, ctx));
        }
        out
    }

    /// Aggregate navigation targets for a symbol.
    pub fn framework_navigation_targets(
        &self,
        db: &dyn Database,
        symbol: &Symbol,
    ) -> Vec<NavigationTarget> {
        let project = match *symbol {
            Symbol::File(file) => db.project_of_file(file),
            Symbol::Class(class) => db.project_of_class(class),
        };

        let mut out = Vec::new();
        for analyzer in self.applicable_analyzers(db, project) {
            out.extend(analyzer.navigation(db, symbol));
        }
        out
    }

    /// Collect virtual members for a class.
    pub fn framework_virtual_members(
        &self,
        db: &dyn Database,
        class: ClassId,
    ) -> Vec<VirtualMember> {
        let project = db.project_of_class(class);
        self.applicable_analyzers(db, project)
            .flat_map(|a| a.virtual_members(db, class))
            .collect()
    }

    /// Backwards compatible name used by `nova-resolve`.
    pub fn virtual_members_for_class(
        &self,
        db: &dyn Database,
        class: ClassId,
    ) -> Vec<VirtualMember> {
        self.framework_virtual_members(db, class)
    }

    /// Collect inlay hints for a file.
    pub fn framework_inlay_hints(&self, db: &dyn Database, file: FileId) -> Vec<InlayHint> {
        let project = db.project_of_file(file);
        let mut out = Vec::new();
        for analyzer in self.applicable_analyzers(db, project) {
            out.extend(analyzer.inlay_hints(db, file));
        }
        out
    }
}

/// New name for the analyzer registry (kept as a type alias for existing call sites).
pub type FrameworkRegistry = AnalyzerRegistry;

pub mod ext;
pub use ext::FrameworkAnalyzerAdapter;

//! Framework analyzer abstraction.
//!
//! Framework analyzers (Spring, Lombok, etc.) augment core resolution by
//! providing additional members and metadata that are generated at compile time.
//!
//! The [`Database`] abstraction intentionally provides only a small set of queries.
//! Implementations may optionally expose file paths and project-wide enumeration
//! via [`Database::file_path`], [`Database::file_id`], and [`Database::all_files`].
//!
//! Framework analyzers should gracefully degrade when this information is not
//! available (e.g. `all_files` returning an empty list) by skipping cross-file
//! scanning and returning no project-wide diagnostics.
//!
//! ## IDE integration note
//!
//! `nova_framework::Database` is intentionally separate from Nova's IDE-facing
//! `nova_db::Database` (which is primarily "file text in → analysis out"). Running
//! framework analyzers in the IDE typically requires an adapter that can supply
//! `nova_hir::framework::ClassData` for `Database::class(ClassId)`. See
//! `crates/nova-ide/src/framework_db.rs` and `crates/nova-ide/src/lombok_intel.rs`
//! for in-repo examples.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use nova_core::ProjectId;
use nova_hir::framework::ClassData;
use nova_scheduler::CancellationToken;
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

    /// Returns the on-disk path for `file` if available.
    fn file_path(&self, _file: FileId) -> Option<&Path> {
        None
    }

    /// Returns the file id corresponding to `path` if available.
    fn file_id(&self, _path: &Path) -> Option<FileId> {
        None
    }

    /// Returns all known files for `project`.
    ///
    /// Implementations that do not support project-wide enumeration should
    /// return an empty vector.
    fn all_files(&self, _project: ProjectId) -> Vec<FileId> {
        Vec::new()
    }

    /// Returns all known classes for `project`.
    ///
    /// Implementations that do not support project-wide enumeration should
    /// return an empty vector.
    fn all_classes(&self, _project: ProjectId) -> Vec<ClassId> {
        Vec::new()
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
    file_path: HashMap<FileId, PathBuf>,
    path_file: HashMap<PathBuf, FileId>,
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

    pub fn add_file_with_path(&mut self, project: ProjectId, path: impl Into<PathBuf>) -> FileId {
        let path = path.into();
        if let Some(&existing) = self.path_file.get(&path) {
            let existing_project = *self
                .file_project
                .get(&existing)
                .expect("path_file points at unknown FileId");
            assert_eq!(
                existing_project, project,
                "attempted to intern the same path into two different projects"
            );
            return existing;
        }

        let id = self.add_file(project);
        self.file_path.insert(id, path.clone());
        self.path_file.insert(path, id);
        id
    }

    pub fn add_file_with_path_and_text(
        &mut self,
        project: ProjectId,
        path: impl Into<PathBuf>,
        text: impl Into<String>,
    ) -> FileId {
        let id = self.add_file_with_path(project, path);
        self.set_file_text(id, text);
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

    fn file_path(&self, file: FileId) -> Option<&Path> {
        self.file_path.get(&file).map(PathBuf::as_path)
    }

    fn file_id(&self, path: &Path) -> Option<FileId> {
        self.path_file.get(path).copied()
    }

    fn all_files(&self, project: ProjectId) -> Vec<FileId> {
        let mut out: Vec<_> = self
            .file_project
            .iter()
            .filter_map(|(&file, &file_project)| (file_project == project).then_some(file))
            .collect();
        out.sort();
        out
    }

    fn all_classes(&self, project: ProjectId) -> Vec<ClassId> {
        let mut out: Vec<_> = self
            .class_project
            .iter()
            .filter_map(|(&class, &class_project)| (class_project == project).then_some(class))
            .collect();
        out.sort();
        out
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
    pub span: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualMethod {
    pub name: String,
    pub return_type: Type,
    pub params: Vec<Parameter>,
    pub is_static: bool,
    pub span: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualConstructor {
    pub params: Vec<Parameter>,
    pub span: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualInnerClass {
    pub name: String,
    pub members: Vec<VirtualMember>,
    pub span: Option<Span>,
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

    fn diagnostics_with_cancel(
        &self,
        db: &dyn Database,
        file: FileId,
        cancel: &CancellationToken,
    ) -> Vec<Diagnostic> {
        if cancel.is_cancelled() {
            Vec::new()
        } else {
            self.diagnostics(db, file)
        }
    }

    fn completions(&self, _db: &dyn Database, _ctx: &CompletionContext) -> Vec<CompletionItem> {
        Vec::new()
    }

    fn completions_with_cancel(
        &self,
        db: &dyn Database,
        ctx: &CompletionContext,
        cancel: &CancellationToken,
    ) -> Vec<CompletionItem> {
        if cancel.is_cancelled() {
            Vec::new()
        } else {
            self.completions(db, ctx)
        }
    }

    fn navigation(&self, _db: &dyn Database, _symbol: &Symbol) -> Vec<NavigationTarget> {
        Vec::new()
    }

    fn navigation_with_cancel(
        &self,
        db: &dyn Database,
        symbol: &Symbol,
        cancel: &CancellationToken,
    ) -> Vec<NavigationTarget> {
        if cancel.is_cancelled() {
            Vec::new()
        } else {
            self.navigation(db, symbol)
        }
    }

    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        Vec::new()
    }

    fn inlay_hints(&self, _db: &dyn Database, _file: FileId) -> Vec<InlayHint> {
        Vec::new()
    }

    fn inlay_hints_with_cancel(
        &self,
        db: &dyn Database,
        file: FileId,
        cancel: &CancellationToken,
    ) -> Vec<InlayHint> {
        if cancel.is_cancelled() {
            Vec::new()
        } else {
            self.inlay_hints(db, file)
        }
    }
}

// Allow boxed analyzers (e.g. built-in analyzer lists) to be used as `FrameworkAnalyzer` values in
// generic contexts (such as adapters that are generic over the analyzer type).
impl<T> FrameworkAnalyzer for Box<T>
where
    T: FrameworkAnalyzer + ?Sized,
{
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        (**self).applies_to(db, project)
    }

    fn analyze_file(&self, db: &dyn Database, file: FileId) -> Option<FrameworkData> {
        (**self).analyze_file(db, file)
    }

    fn diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
        (**self).diagnostics(db, file)
    }

    fn diagnostics_with_cancel(
        &self,
        db: &dyn Database,
        file: FileId,
        cancel: &CancellationToken,
    ) -> Vec<Diagnostic> {
        (**self).diagnostics_with_cancel(db, file, cancel)
    }

    fn completions(&self, db: &dyn Database, ctx: &CompletionContext) -> Vec<CompletionItem> {
        (**self).completions(db, ctx)
    }

    fn completions_with_cancel(
        &self,
        db: &dyn Database,
        ctx: &CompletionContext,
        cancel: &CancellationToken,
    ) -> Vec<CompletionItem> {
        (**self).completions_with_cancel(db, ctx, cancel)
    }

    fn navigation(&self, db: &dyn Database, symbol: &Symbol) -> Vec<NavigationTarget> {
        (**self).navigation(db, symbol)
    }

    fn navigation_with_cancel(
        &self,
        db: &dyn Database,
        symbol: &Symbol,
        cancel: &CancellationToken,
    ) -> Vec<NavigationTarget> {
        (**self).navigation_with_cancel(db, symbol, cancel)
    }

    fn virtual_members(&self, db: &dyn Database, class: ClassId) -> Vec<VirtualMember> {
        (**self).virtual_members(db, class)
    }

    fn inlay_hints(&self, db: &dyn Database, file: FileId) -> Vec<InlayHint> {
        (**self).inlay_hints(db, file)
    }

    fn inlay_hints_with_cancel(
        &self,
        db: &dyn Database,
        file: FileId,
        cancel: &CancellationToken,
    ) -> Vec<InlayHint> {
        (**self).inlay_hints_with_cancel(db, file, cancel)
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
            .filter(move |a| catch_unwind(AssertUnwindSafe(|| a.applies_to(db, project))).unwrap_or(false))
    }

    /// Aggregate `FrameworkData` across all applicable analyzers.
    pub fn framework_data(&self, db: &dyn Database, file: FileId) -> Vec<FrameworkData> {
        let project = db.project_of_file(file);
        let mut out = Vec::new();
        for analyzer in self.applicable_analyzers(db, project) {
            let data = catch_unwind(AssertUnwindSafe(|| analyzer.analyze_file(db, file))).unwrap_or(None);
            if let Some(data) = data {
                out.push(data);
            }
        }
        out
    }

    /// Collect all framework diagnostics for `file`.
    pub fn framework_diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
        let project = db.project_of_file(file);
        let mut out = Vec::new();
        for analyzer in self.applicable_analyzers(db, project) {
            let diags =
                catch_unwind(AssertUnwindSafe(|| analyzer.diagnostics(db, file))).unwrap_or_default();
            out.extend(diags);
        }
        out
    }

    /// Collect all framework diagnostics for `file`, cooperating with request cancellation.
    pub fn framework_diagnostics_with_cancel(
        &self,
        db: &dyn Database,
        file: FileId,
        cancel: &CancellationToken,
    ) -> Vec<Diagnostic> {
        if cancel.is_cancelled() {
            return Vec::new();
        }

        let project = db.project_of_file(file);
        let mut out = Vec::new();
        for analyzer in &self.analyzers {
            if cancel.is_cancelled() {
                break;
            }
            let analyzer = analyzer.as_ref();

            let applicable =
                catch_unwind(AssertUnwindSafe(|| analyzer.applies_to(db, project))).unwrap_or(false);
            if !applicable {
                continue;
            }

            if cancel.is_cancelled() {
                break;
            }

            let diags = catch_unwind(AssertUnwindSafe(|| {
                analyzer.diagnostics_with_cancel(db, file, cancel)
            }))
            .unwrap_or_default();
            out.extend(diags);
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
            let completions = catch_unwind(AssertUnwindSafe(|| analyzer.completions(db, ctx)))
                .unwrap_or_default();
            out.extend(completions);
        }
        out
    }

    /// Collect all framework completion items for a completion context, cooperating with request
    /// cancellation.
    pub fn framework_completions_with_cancel(
        &self,
        db: &dyn Database,
        ctx: &CompletionContext,
        cancel: &CancellationToken,
    ) -> Vec<CompletionItem> {
        if cancel.is_cancelled() {
            return Vec::new();
        }

        let mut out = Vec::new();
        for analyzer in &self.analyzers {
            if cancel.is_cancelled() {
                break;
            }
            let analyzer = analyzer.as_ref();

            let applicable =
                catch_unwind(AssertUnwindSafe(|| analyzer.applies_to(db, ctx.project)))
                    .unwrap_or(false);
            if !applicable {
                continue;
            }

            if cancel.is_cancelled() {
                break;
            }

            let completions = catch_unwind(AssertUnwindSafe(|| {
                analyzer.completions_with_cancel(db, ctx, cancel)
            }))
            .unwrap_or_default();
            out.extend(completions);
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
            let nav =
                catch_unwind(AssertUnwindSafe(|| analyzer.navigation(db, symbol))).unwrap_or_default();
            out.extend(nav);
        }
        out
    }

    /// Aggregate navigation targets for a symbol, cooperating with request cancellation.
    pub fn framework_navigation_targets_with_cancel(
        &self,
        db: &dyn Database,
        symbol: &Symbol,
        cancel: &CancellationToken,
    ) -> Vec<NavigationTarget> {
        if cancel.is_cancelled() {
            return Vec::new();
        }

        let project = match *symbol {
            Symbol::File(file) => db.project_of_file(file),
            Symbol::Class(class) => db.project_of_class(class),
        };

        let mut out = Vec::new();
        for analyzer in &self.analyzers {
            if cancel.is_cancelled() {
                break;
            }
            let analyzer = analyzer.as_ref();

            let applicable =
                catch_unwind(AssertUnwindSafe(|| analyzer.applies_to(db, project))).unwrap_or(false);
            if !applicable {
                continue;
            }

            if cancel.is_cancelled() {
                break;
            }

            let nav = catch_unwind(AssertUnwindSafe(|| {
                analyzer.navigation_with_cancel(db, symbol, cancel)
            }))
            .unwrap_or_default();
            out.extend(nav);
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
        let mut out = Vec::new();
        for analyzer in self.applicable_analyzers(db, project) {
            let members = catch_unwind(AssertUnwindSafe(|| analyzer.virtual_members(db, class)))
                .unwrap_or_default();
            out.extend(members);
        }
        out
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
            let hints =
                catch_unwind(AssertUnwindSafe(|| analyzer.inlay_hints(db, file))).unwrap_or_default();
            out.extend(hints);
        }
        out
    }

    /// Collect inlay hints for a file, cooperating with request cancellation.
    pub fn framework_inlay_hints_with_cancel(
        &self,
        db: &dyn Database,
        file: FileId,
        cancel: &CancellationToken,
    ) -> Vec<InlayHint> {
        if cancel.is_cancelled() {
            return Vec::new();
        }

        let project = db.project_of_file(file);
        let mut out = Vec::new();
        for analyzer in &self.analyzers {
            if cancel.is_cancelled() {
                break;
            }
            let analyzer = analyzer.as_ref();

            let applicable =
                catch_unwind(AssertUnwindSafe(|| analyzer.applies_to(db, project))).unwrap_or(false);
            if !applicable {
                continue;
            }

            if cancel.is_cancelled() {
                break;
            }

            let hints = catch_unwind(AssertUnwindSafe(|| {
                analyzer.inlay_hints_with_cancel(db, file, cancel)
            }))
            .unwrap_or_default();
            out.extend(hints);
        }
        out
    }
}

/// New name for the analyzer registry (kept as a type alias for existing call sites).
pub type FrameworkRegistry = AnalyzerRegistry;

use std::collections::{hash_map::DefaultHasher, BTreeSet, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use lsp_types::Uri;
use nova_db::{Database, FileId};
use nova_types::Span;
use once_cell::sync::Lazy;

use crate::parse::{parse_file, ParsedFile, TypeDef};

const MAX_CACHED_WORKSPACES: usize = 8;

#[derive(Debug, Clone)]
struct CachedWorkspaceIndex {
    fingerprint: u64,
    index: Arc<WorkspaceIndex>,
}

#[derive(Debug)]
struct LruCache<K, V> {
    capacity: usize,
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V> LruCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get_cloned(&mut self, key: &K) -> Option<V> {
        let value = self.map.get(key)?.clone();
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: K, value: V) {
        self.map.insert(key.clone(), value);
        self.touch(&key);
        self.evict_if_needed();
    }

    fn touch(&mut self, key: &K) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.clone());
    }

    fn evict_if_needed(&mut self) {
        while self.map.len() > self.capacity {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&key);
        }
    }
}

static WORKSPACE_INDEX_CACHE: Lazy<Mutex<LruCache<u64, CachedWorkspaceIndex>>> =
    Lazy::new(|| Mutex::new(LruCache::new(MAX_CACHED_WORKSPACES)));

// Test-only instrumentation: count how often we rebuild the workspace index.
//
// This intentionally lives in `nav_resolve` because `code_intelligence` creates a
// fresh resolver on each request; we want to assert that repeated navigation
// requests reuse the cached index.
#[cfg(any(test, debug_assertions))]
static WORKSPACE_INDEX_BUILDS_BY_WORKSPACE: Lazy<Mutex<HashMap<u64, usize>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[cfg(any(test, debug_assertions))]
fn record_workspace_index_build(workspace_id: u64) {
    let mut counts = WORKSPACE_INDEX_BUILDS_BY_WORKSPACE
        .lock()
        .expect("nav workspace index build counter lock poisoned");
    *counts.entry(workspace_id).or_insert(0) += 1;
}

#[cfg(any(test, debug_assertions))]
pub(crate) fn workspace_index_build_count(db: &dyn Database) -> usize {
    let (workspace_id, _) = workspace_id_and_fingerprint(db);
    let counts = WORKSPACE_INDEX_BUILDS_BY_WORKSPACE
        .lock()
        .expect("nav workspace index build counter lock poisoned");
    counts.get(&workspace_id).copied().unwrap_or(0)
}

#[cfg(any(test, debug_assertions))]
pub(crate) fn reset_workspace_index_build_counts() {
    {
        let mut counts = WORKSPACE_INDEX_BUILDS_BY_WORKSPACE
            .lock()
            .expect("nav workspace index build counter lock poisoned");
        counts.clear();
    }

    // Keep tests deterministic by resetting the global LRU.
    let mut cache = WORKSPACE_INDEX_CACHE
        .lock()
        .expect("nav workspace index cache lock poisoned");
    cache.map.clear();
    cache.order.clear();
}

fn workspace_id_and_fingerprint(db: &dyn Database) -> (u64, u64) {
    let mut file_ids = db.all_file_ids();
    file_ids.sort_by_key(|id| id.to_raw());

    let mut workspace_hasher = DefaultHasher::new();
    let mut fingerprint_hasher = DefaultHasher::new();

    for file_id in file_ids {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }

        // Workspace identity is derived from the set of Java file paths.
        path.hash(&mut workspace_hasher);

        // Best-effort content fingerprint: path + pointer + len.
        //
        // NOTE: We intentionally avoid hashing full contents here; this runs on every
        // navigation request and would be prohibitively expensive in large workspaces.
        path.hash(&mut fingerprint_hasher);
        let text = db.file_content(file_id);
        text.len().hash(&mut fingerprint_hasher);
        text.as_ptr().hash(&mut fingerprint_hasher);
    }

    (workspace_hasher.finish(), fingerprint_hasher.finish())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SymbolKey {
    pub(crate) file: FileId,
    pub(crate) span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResolvedKind {
    LocalVar { scope: Span },
    Field,
    Method,
    Type,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedSymbol {
    pub(crate) name: String,
    pub(crate) kind: ResolvedKind,
    /// The definition this symbol resolves to.
    pub(crate) def: Definition,
}

#[derive(Clone, Debug)]
pub(crate) struct Definition {
    pub(crate) file: FileId,
    pub(crate) uri: Uri,
    pub(crate) name_span: Span,
    pub(crate) key: SymbolKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OccurrenceKind {
    MemberCall {
        receiver: String,
    },
    MemberField {
        receiver: String,
    },
    /// Java method/constructor reference (`recv::method`, `Type::new`).
    MethodRef {
        receiver: String,
    },
    LocalCall,
    Ident,
}

#[derive(Clone, Debug)]
struct TypeInfo {
    file_id: FileId,
    uri: Uri,
    def: TypeDef,
}

#[derive(Debug, Default)]
struct WorkspaceIndex {
    files: HashMap<FileId, ParsedFile>,
    types: HashMap<String, TypeInfo>,
}

impl WorkspaceIndex {
    fn new(db: &dyn Database) -> Self {
        let mut files = HashMap::new();
        let mut file_ids = db.all_file_ids();
        file_ids.sort_by_key(|id| id.to_raw());

        for file_id in &file_ids {
            let is_java = db
                .file_path(*file_id)
                .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"));
            if !is_java {
                continue;
            }

            let uri = uri_for_file(db, *file_id);
            let text = db.file_content(*file_id).to_string();
            let parsed = parse_file(uri, text);
            files.insert(*file_id, parsed);
        }

        let mut types: HashMap<String, TypeInfo> = HashMap::new();
        for file_id in &file_ids {
            let Some(parsed_file) = files.get(file_id) else {
                continue;
            };
            for ty in &parsed_file.types {
                types.entry(ty.name.clone()).or_insert_with(|| TypeInfo {
                    file_id: *file_id,
                    uri: parsed_file.uri.clone(),
                    def: ty.clone(),
                });
            }
        }

        Self { files, types }
    }

    fn file(&self, file: FileId) -> Option<&ParsedFile> {
        self.files.get(&file)
    }

    fn type_info(&self, name: &str) -> Option<&TypeInfo> {
        self.types.get(name)
    }

    fn resolve_name_type(&self, parsed: &ParsedFile, offset: usize, name: &str) -> Option<String> {
        let ty = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))?;

        if let Some(method) = ty
            .methods
            .iter()
            .find(|m| m.body_span.is_some_and(|span| span_contains(span, offset)))
        {
            if let Some(local) = method.locals.iter().find(|v| v.name == name) {
                return Some(local.ty.clone());
            }
        }

        if let Some(field) = ty.fields.iter().find(|f| f.name == name) {
            return Some(field.ty.clone());
        }

        None
    }

    fn local_or_field_declaration(
        &self,
        file: FileId,
        parsed: &ParsedFile,
        offset: usize,
        name: &str,
    ) -> Option<(ResolvedKind, Definition)> {
        let ty = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))?;

        if let Some(method) = ty
            .methods
            .iter()
            .find(|m| m.body_span.is_some_and(|span| span_contains(span, offset)))
        {
            if let Some(local) = method.locals.iter().find(|v| v.name == name) {
                let def = Definition {
                    file,
                    uri: parsed.uri.clone(),
                    name_span: local.name_span,
                    key: SymbolKey {
                        file,
                        span: local.name_span,
                    },
                };
                return Some((
                    ResolvedKind::LocalVar {
                        scope: method.body_span.expect("checked above"),
                    },
                    def,
                ));
            }
        }

        if let Some(field) = ty.fields.iter().find(|f| f.name == name) {
            let def = Definition {
                file,
                uri: parsed.uri.clone(),
                name_span: field.name_span,
                key: SymbolKey {
                    file,
                    span: field.name_span,
                },
            };
            return Some((ResolvedKind::Field, def));
        }

        None
    }

    fn method_in_enclosing_type(
        &self,
        file: FileId,
        parsed: &ParsedFile,
        offset: usize,
        method_name: &str,
    ) -> Option<Definition> {
        let ty = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))?;
        let method = ty.methods.iter().find(|m| m.name == method_name)?;
        Some(Definition {
            file,
            uri: parsed.uri.clone(),
            name_span: method.name_span,
            key: SymbolKey {
                file,
                span: method.name_span,
            },
        })
    }

    fn resolve_method_definition(&self, ty_name: &str, method_name: &str) -> Option<Definition> {
        let mut visited = BTreeSet::new();
        self.resolve_method_definition_inner(ty_name, method_name, &mut visited)
    }

    fn resolve_method_definition_inner(
        &self,
        ty_name: &str,
        method_name: &str,
        visited: &mut BTreeSet<String>,
    ) -> Option<Definition> {
        if !visited.insert(ty_name.to_string()) {
            return None;
        }

        let type_info = self.type_info(ty_name)?;
        if let Some(method) = type_info.def.methods.iter().find(|m| m.name == method_name) {
            return Some(Definition {
                file: type_info.file_id,
                uri: type_info.uri.clone(),
                name_span: method.name_span,
                key: SymbolKey {
                    file: type_info.file_id,
                    span: method.name_span,
                },
            });
        }

        // Prefer superclass lookup over interfaces (matches Java method lookup precedence).
        if let Some(super_name) = type_info.def.super_class.as_deref() {
            if let Some(def) =
                self.resolve_method_definition_inner(super_name, method_name, visited)
            {
                return Some(def);
            }
        }

        // Fall back to interfaces, including extended interfaces (`interface I1 extends I0`).
        for iface in &type_info.def.interfaces {
            if let Some(found) = self.resolve_method_definition_inner(iface, method_name, visited) {
                return Some(found);
            }
        }

        None
    }

    fn resolve_field_definition(&self, ty_name: &str, field_name: &str) -> Option<Definition> {
        let mut visited = BTreeSet::new();
        self.resolve_field_definition_inner(ty_name, field_name, &mut visited)
    }

    fn resolve_field_definition_inner(
        &self,
        ty_name: &str,
        field_name: &str,
        visited: &mut BTreeSet<String>,
    ) -> Option<Definition> {
        if !visited.insert(ty_name.to_string()) {
            return None;
        }

        let type_info = self.type_info(ty_name)?;
        if let Some(field) = type_info.def.fields.iter().find(|f| f.name == field_name) {
            return Some(Definition {
                file: type_info.file_id,
                uri: type_info.uri.clone(),
                name_span: field.name_span,
                key: SymbolKey {
                    file: type_info.file_id,
                    span: field.name_span,
                },
            });
        }

        // Walk superclass chain first.
        if let Some(super_name) = type_info.def.super_class.as_deref() {
            if let Some(def) = self.resolve_field_definition_inner(super_name, field_name, visited)
            {
                return Some(def);
            }
        }

        // Then check implemented interfaces (interface constants / inherited interface fields).
        for iface in &type_info.def.interfaces {
            if let Some(found) = self.resolve_field_definition_inner(iface, field_name, visited) {
                return Some(found);
            }
        }

        None
    }

    fn resolve_type_definition(&self, ty_name: &str) -> Option<Definition> {
        let info = self.type_info(ty_name)?;
        Some(Definition {
            file: info.file_id,
            uri: info.uri.clone(),
            name_span: info.def.name_span,
            key: SymbolKey {
                file: info.file_id,
                span: info.def.name_span,
            },
        })
    }

    fn resolve_receiver_type(
        &self,
        parsed: &ParsedFile,
        offset: usize,
        receiver: &str,
    ) -> Option<String> {
        let containing_type = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))?;

        let segments: Vec<(&str, bool)> = receiver
            .split('.')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|seg| {
                seg.strip_suffix("()")
                    .map(|name| (name, true))
                    .unwrap_or((seg, false))
            })
            .filter(|(name, _)| !name.is_empty())
            .collect();
        let &(first, first_is_call) = segments.first()?;

        let (last, last_is_call) = segments.last().copied()?;

        let (mut cur_ty, base_is_type) = if first_is_call {
            // Best-effort: treat receiverless calls like `foo().bar` as `this.foo().bar`.
            let this_ty = containing_type.name.clone();
            (
                self.resolve_method_return_type_name(&this_ty, first)?,
                false,
            )
        } else if first == "this" {
            (containing_type.name.clone(), false)
        } else if first == "super" {
            (containing_type.super_class.clone()?, false)
        } else if let Some(ty) = self.resolve_name_type(parsed, offset, first) {
            (ty, false)
        } else if self.type_info(first).is_some() {
            // Type name (static access)
            (first.to_string(), true)
        } else {
            // Qualified type name starting with a package segment: `pkg.Foo` (best-effort).
            if segments.len() > 1 && !last_is_call && self.type_info(last).is_some() {
                return Some(last.to_string());
            }
            return None;
        };

        for (seg, is_call) in segments.iter().copied().skip(1) {
            if is_call {
                cur_ty = self.resolve_method_return_type_name(&cur_ty, seg)?;
                continue;
            }

            match self.resolve_field_type_name(&cur_ty, seg) {
                Some(next) => cur_ty = next,
                None => {
                    // Fallback for qualified type names / nested types like `pkg.Foo` or
                    // `Outer.Inner` when the receiver starts with a type name.
                    if base_is_type && !last_is_call && self.type_info(last).is_some() {
                        return Some(last.to_string());
                    }
                    return None;
                }
            }
        }

        Some(cur_ty)
    }

    fn resolve_field_type_name(&self, ty_name: &str, field_name: &str) -> Option<String> {
        fn go(
            index: &WorkspaceIndex,
            ty_name: &str,
            field_name: &str,
            visited: &mut Vec<String>,
        ) -> Option<String> {
            if visited.iter().any(|t| t == ty_name) {
                return None;
            }
            visited.push(ty_name.to_string());

            let info = index.type_info(ty_name)?;
            if let Some(field) = info.def.fields.iter().find(|f| f.name == field_name) {
                return Some(field.ty.clone());
            }

            if let Some(super_name) = info.def.super_class.as_deref() {
                if let Some(found) = go(index, super_name, field_name, visited) {
                    return Some(found);
                }
            }

            for iface in &info.def.interfaces {
                if let Some(found) = go(index, iface, field_name, visited) {
                    return Some(found);
                }
            }

            None
        }

        go(self, ty_name, field_name, &mut Vec::new())
    }

    fn resolve_method_return_type_name(&self, ty_name: &str, method_name: &str) -> Option<String> {
        fn go(
            index: &WorkspaceIndex,
            ty_name: &str,
            method_name: &str,
            visited: &mut Vec<String>,
        ) -> Option<String> {
            if visited.iter().any(|t| t == ty_name) {
                return None;
            }
            visited.push(ty_name.to_string());

            let info = index.type_info(ty_name)?;
            if let Some(method) = info.def.methods.iter().find(|m| m.name == method_name) {
                let ret = method.ret_ty.clone()?;
                if ret == "void" {
                    return None;
                }
                return Some(ret);
            }

            if let Some(super_name) = info.def.super_class.as_deref() {
                if let Some(found) = go(index, super_name, method_name, visited) {
                    return Some(found);
                }
            }

            for iface in &info.def.interfaces {
                if let Some(found) = go(index, iface, method_name, visited) {
                    return Some(found);
                }
            }

            None
        }

        go(self, ty_name, method_name, &mut Vec::new())
    }
}

/// Per-request core Java symbol resolver.
///
/// This is intentionally lightweight and best-effort: it uses `crate::parse::parse_file`
/// and textual context around the cursor to resolve common symbols (locals, fields,
/// types, and member calls).
pub(crate) struct Resolver {
    index: Arc<WorkspaceIndex>,
}

impl Resolver {
    pub(crate) fn new(db: &dyn Database) -> Self {
        let (workspace_id, fingerprint) = workspace_id_and_fingerprint(db);

        {
            let mut cache = WORKSPACE_INDEX_CACHE
                .lock()
                .expect("nav workspace index cache lock poisoned");
            if let Some(entry) = cache
                .get_cloned(&workspace_id)
                .filter(|e| e.fingerprint == fingerprint)
            {
                return Self { index: entry.index };
            }
        }

        let built = Arc::new(WorkspaceIndex::new(db));
        #[cfg(any(test, debug_assertions))]
        record_workspace_index_build(workspace_id);

        let mut cache = WORKSPACE_INDEX_CACHE
            .lock()
            .expect("nav workspace index cache lock poisoned");
        if let Some(entry) = cache
            .get_cloned(&workspace_id)
            .filter(|e| e.fingerprint == fingerprint)
        {
            return Self { index: entry.index };
        }

        cache.insert(
            workspace_id,
            CachedWorkspaceIndex {
                fingerprint,
                index: Arc::clone(&built),
            },
        );

        Self { index: built }
    }

    pub(crate) fn parsed_file(&self, file: FileId) -> Option<&ParsedFile> {
        self.index.file(file)
    }

    pub(crate) fn java_file_ids_sorted(&self) -> Vec<FileId> {
        let mut ids: Vec<_> = self.index.files.keys().copied().collect();
        ids.sort_by_key(|id| id.to_raw());
        ids
    }

    pub(crate) fn resolve_at(&self, file: FileId, offset: usize) -> Option<ResolvedSymbol> {
        let parsed = self.index.file(file)?;
        let (ident, ident_span) = identifier_at(&parsed.text, offset)?;

        // Import statements (`import p.Foo;`) are a special case: the identifier under the cursor
        // is part of a qualified name where the "receiver" is typically a package segment, not a
        // value/type we can resolve via local context. In this context we only attempt to resolve
        // the simple type name against the workspace index.
        if is_import_statement_line(&parsed.text, ident_span.start) {
            // Avoid shadowing JDK type navigation. The LSP layer has a dedicated JDK fallback that
            // uses the full import path (`java.util.List`), so if this import is clearly targeting
            // a JDK package, return `None` and let that fallback run.
            if import_statement_targets_jdk(&parsed.text, ident_span.start) {
                return None;
            }

            if let Some(def) = self.index.resolve_type_definition(&ident) {
                return Some(ResolvedSymbol {
                    name: ident,
                    kind: ResolvedKind::Type,
                    def,
                });
            }
            return None;
        }

        // Constructor calls (`new Foo(...)` / `new p.Foo(...)`) are often misclassified as
        // receiverless calls or member calls on the package segment. Detect this textual
        // context and resolve the identifier as a type name instead.
        //
        // This is intentionally best-effort: we resolve by simple name only and return `None`
        // when the type isn't found in the workspace index (allowing other navigation layers
        // to handle JDK types).
        if is_constructor_type_context(&parsed.text, ident_span) {
            let def = self.index.resolve_type_definition(&ident)?;
            return Some(ResolvedSymbol {
                name: ident,
                kind: ResolvedKind::Type,
                def,
            });
        }

        let occurrence = classify_occurrence(&parsed.text, ident_span)?;
        let looks_like_type = matches!(occurrence, OccurrenceKind::Ident)
            && looks_like_type_usage(&parsed.text, ident_span);

        // 1) Locals/fields in the current file.
        if matches!(occurrence, OccurrenceKind::Ident) {
            if let Some((kind, def)) =
                self.index
                    .local_or_field_declaration(file, parsed, ident_span.start, &ident)
            {
                return Some(ResolvedSymbol {
                    name: ident,
                    kind,
                    def,
                });
            }
        }

        // 1.5) Inherited field access without an explicit receiver (`foo = 1;` where `foo`
        // is declared on a superclass or implemented interface).
        //
        // We intentionally avoid attempting this in obvious type positions (`Foo x;`) to avoid
        // mis-resolving type names as inherited fields.
        if matches!(occurrence, OccurrenceKind::Ident) && !looks_like_type {
            if let Some(receiver_ty) =
                self.index
                    .resolve_receiver_type(parsed, ident_span.start, "this")
            {
                if let Some(def) = self.index.resolve_field_definition(&receiver_ty, &ident) {
                    return Some(ResolvedSymbol {
                        name: ident,
                        kind: ResolvedKind::Field,
                        def,
                    });
                }
            }
        }

        // 2) Type names.
        if matches!(occurrence, OccurrenceKind::Ident) {
            if let Some(def) = self.index.resolve_type_definition(&ident) {
                return Some(ResolvedSymbol {
                    name: ident,
                    kind: ResolvedKind::Type,
                    def,
                });
            }
        }

        match occurrence {
            OccurrenceKind::MemberCall { receiver } => {
                let receiver_ty =
                    self.index
                        .resolve_receiver_type(parsed, ident_span.start, &receiver)?;
                let def = self.index.resolve_method_definition(&receiver_ty, &ident)?;
                Some(ResolvedSymbol {
                    name: ident,
                    kind: ResolvedKind::Method,
                    def,
                })
            }
            OccurrenceKind::MemberField { receiver } => {
                if let Some(receiver_ty) =
                    self.index
                        .resolve_receiver_type(parsed, ident_span.start, &receiver)
                {
                    let def = self.index.resolve_field_definition(&receiver_ty, &ident)?;
                    Some(ResolvedSymbol {
                        name: ident,
                        kind: ResolvedKind::Field,
                        def,
                    })
                } else {
                    // Best-effort fallback for qualified type references like `p.Foo`:
                    // if the "receiver" doesn't resolve to a value/type (common for package
                    // segments), try resolving the identifier as a type name.
                    if looks_like_jdk_qualified_name(&receiver) {
                        return None;
                    }
                    self.index
                        .resolve_type_definition(&ident)
                        .map(|def| ResolvedSymbol {
                            name: ident,
                            kind: ResolvedKind::Type,
                            def,
                        })
                }
            }
            OccurrenceKind::MethodRef { receiver } => {
                // Method references:
                // - `Type::method`
                // - `expr::method`
                // - `Type::new`
                //
                // Best-effort rules:
                // - Prefer interpreting the receiver as a workspace type name when it matches.
                // - For qualified receivers (`p.Foo`), fall back to the final segment (`Foo`).
                // - Otherwise, resolve the receiver type from local context (`a::m`, `this::m`).

                let receiver_type_name = || -> Option<&str> {
                    let receiver = receiver.trim();
                    if self.index.type_info(receiver).is_some() {
                        return Some(receiver);
                    }

                    let last = receiver.rsplit('.').next().unwrap_or(receiver).trim();
                    if last != receiver && self.index.type_info(last).is_some() {
                        return Some(last);
                    }

                    None
                };

                if ident == "new" {
                    // Constructor reference: `Type::new` resolves to the receiver type definition.
                    let receiver_ty = receiver_type_name()?;
                    let def = self.index.resolve_type_definition(receiver_ty)?;
                    return Some(ResolvedSymbol {
                        name: receiver_ty.to_string(),
                        kind: ResolvedKind::Type,
                        def,
                    });
                }

                // 1) `Type::method` (prefer type lookup when the receiver is a known workspace type).
                if let Some(receiver_ty) = receiver_type_name() {
                    if let Some(def) = self.index.resolve_method_definition(receiver_ty, &ident) {
                        return Some(ResolvedSymbol {
                            name: ident,
                            kind: ResolvedKind::Method,
                            def,
                        });
                    }
                }

                // 2) `expr::method` (locals/fields/this/super + chained receivers).
                let receiver_ty = self
                    .index
                    .resolve_receiver_type(parsed, ident_span.start, receiver.trim())
                    .or_else(|| {
                        // Best-effort fallback for qualified type receivers like `p.Foo::bar`:
                        // if the full receiver doesn't resolve (package segments), try the final
                        // segment as a workspace type name.
                        let last = receiver.rsplit('.').next()?.trim();
                        self.index
                            .type_info(last)
                            .is_some()
                            .then_some(last.to_string())
                    })?;
                let def = self.index.resolve_method_definition(&receiver_ty, &ident)?;
                Some(ResolvedSymbol {
                    name: ident,
                    kind: ResolvedKind::Method,
                    def,
                })
            }
            OccurrenceKind::LocalCall => {
                let receiver_ty =
                    self.index
                        .resolve_receiver_type(parsed, ident_span.start, "this")?;
                let def = self
                    .index
                    .resolve_method_definition(&receiver_ty, &ident)
                    // If we can't resolve via the workspace index, fall back to the old behavior
                    // (current type only).
                    .or_else(|| {
                        self.index
                            .method_in_enclosing_type(file, parsed, ident_span.start, &ident)
                    })?;
                Some(ResolvedSymbol {
                    name: ident,
                    kind: ResolvedKind::Method,
                    def,
                })
            }
            OccurrenceKind::Ident => {
                // Plain identifier usage could still be a type name (handled above). Anything
                // else is currently unresolved.
                None
            }
        }
    }

    pub(crate) fn scan_identifiers_in_span(
        &self,
        file: FileId,
        span: Span,
        ident: &str,
    ) -> Option<Vec<Span>> {
        let parsed = self.index.file(file)?;
        Some(scan_identifier_occurrences(&parsed.text, span, ident))
    }
}

fn looks_like_type_usage(text: &str, ident_span: Span) -> bool {
    let bytes = text.as_bytes();
    let mut i = ident_span.end.min(bytes.len());
    while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return false;
    }

    // `Foo x` (type + identifier).
    if is_ident_start(bytes[i]) {
        return true;
    }

    // `Foo[] x` (array type suffix).
    if bytes[i] == b'[' {
        let mut j = i;
        loop {
            if bytes.get(j) != Some(&b'[') {
                break;
            }
            j += 1;
            while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) != Some(&b']') {
                // Likely indexing (`arr[0]`) rather than a type suffix.
                return false;
            }
            j += 1;
            while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) == Some(&b'[') {
                continue;
            }
            return bytes.get(j).is_some_and(|b| is_ident_start(*b));
        }
    }

    // `Foo<Bar> x` (generic type suffix).
    if bytes[i] == b'<' {
        let mut depth = 0usize;
        let mut j = i;
        // Best-effort scan for a matching `>` within a small window, to avoid
        // misclassifying `<` comparisons as type positions.
        while j < bytes.len() && j - i < 256 {
            match bytes[j] {
                b'<' => depth += 1,
                b'>' => {
                    depth = match depth.checked_sub(1) {
                        Some(v) => v,
                        None => return false,
                    };
                    if depth == 0 {
                        j += 1;
                        while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
                            j += 1;
                        }
                        return bytes
                            .get(j)
                            .is_some_and(|b| is_ident_start(*b) || *b == b'[');
                    }
                }
                _ => {}
            }
            j += 1;
        }
    }

    false
}

/// Returns true when `ident_span` is the last segment of a `new ...` expression:
///
/// - `new Foo(...)`
/// - `new p.Foo(...)`
///
/// This is a textual heuristic used by core navigation to avoid misclassifying constructor
/// calls as receiverless/member method calls.
fn is_constructor_type_context(text: &str, ident_span: Span) -> bool {
    fn skip_ws_and_comments_left(text: &str, mut i: usize) -> usize {
        let bytes = text.as_bytes();
        loop {
            let prev = i;

            // Whitespace.
            while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
                i -= 1;
            }

            // Block comment (`/* ... */`) ending at `i` (i.e. we are positioned just after `*/`).
            if i >= 2 && bytes[i - 2] == b'*' && bytes[i - 1] == b'/' {
                // Walk backwards to find the matching `/*`. Java block comments don't nest, so the
                // first match is sufficient.
                let mut k = i - 2;
                while k >= 1 {
                    if bytes[k - 1] == b'/' && bytes[k] == b'*' {
                        i = k - 1;
                        break;
                    }
                    k -= 1;
                }
                if k == 0 {
                    // Unterminated block comment; treat as beginning-of-file.
                    return 0;
                }
                continue;
            }

            // Line comment (`// ...`) that reaches `i` on the current line. If we find a `//`
            // after the previous newline, treat everything until `i` as comment and skip it.
            let mut line_start = i;
            while line_start > 0 && bytes[line_start - 1] != b'\n' {
                line_start -= 1;
            }
            let mut j = line_start;
            let mut found = None;
            while j + 1 < i {
                if bytes[j] == b'/' && bytes[j + 1] == b'/' {
                    found = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(start) = found {
                i = start;
                continue;
            }

            if i == prev {
                break;
            }
        }
        i
    }

    let bytes = text.as_bytes();
    let mut i = ident_span.start.min(bytes.len());

    // Scan left over whitespace/comments.
    i = skip_ws_and_comments_left(text, i);

    // Scan left over a dotted identifier chain (qualified name), allowing whitespace around '.'.
    loop {
        // Allow whitespace/comments between '.' and the identifier.
        i = skip_ws_and_comments_left(text, i);

        // Look for `.` before the current segment.
        if i == 0 || bytes[i - 1] != b'.' {
            break;
        }

        // Skip `.` and any whitespace/comments before it.
        i -= 1;
        i = skip_ws_and_comments_left(text, i);

        // Consume the previous identifier segment.
        let seg_end = i;
        while i > 0 && is_ident_continue(bytes[i - 1]) {
            i -= 1;
        }

        // Invalid chain: `.` not preceded by an identifier.
        if i == seg_end {
            return false;
        }
    }

    // Type-use annotations can appear between `new` and the type:
    //
    // - `new @Deprecated Foo()`
    // - `new @p.Ann(arg) Foo()`
    //
    // Best-effort: skip over one or more leading annotations before checking for `new`.
    loop {
        let mut j = skip_ws_and_comments_left(text, i);

        if j == 0 {
            i = j;
            break;
        }

        // Explicit type arguments can also appear between `new` and the type:
        // `new <T> Foo()`.
        if bytes.get(j - 1) == Some(&b'>') {
            let close_angle_idx = j - 1;
            let open_angle_idx = match matching_open_angle(bytes, close_angle_idx) {
                Some(idx) => idx,
                None => return false,
            };
            i = open_angle_idx;
            continue;
        }

        // Skip a parenthesized annotation argument list, if present (`@Ann(...)`).
        if bytes.get(j - 1) == Some(&b')') {
            let close_paren_idx = j - 1;
            let open_paren_idx = match matching_open_paren(bytes, close_paren_idx) {
                Some(idx) => idx,
                None => break,
            };
            j = open_paren_idx;
            j = skip_ws_and_comments_left(text, j);
        }

        // Parse an identifier (or qualified identifier chain) before `j`.
        let name_end = j;
        let mut name_start = name_end;
        while name_start > 0 && is_ident_continue(bytes[name_start - 1]) {
            name_start -= 1;
        }
        if name_start == name_end {
            i = j;
            break;
        }

        // Consume preceding `.ident` segments, allowing whitespace/comments around `.`.
        loop {
            let mut k = skip_ws_and_comments_left(text, name_start);
            if k == 0 || bytes[k - 1] != b'.' {
                name_start = k;
                break;
            }
            k -= 1;
            k = skip_ws_and_comments_left(text, k);

            let seg_end = k;
            while k > 0 && is_ident_continue(bytes[k - 1]) {
                k -= 1;
            }
            if k == seg_end {
                return false;
            }
            name_start = k;
        }

        // Look for `@` immediately preceding the annotation name (allowing whitespace/comments).
        let k = skip_ws_and_comments_left(text, name_start);
        if k > 0 && bytes[k - 1] == b'@' {
            i = k - 1;
            continue;
        }

        i = j;
        break;
    }

    // After the chain (and any annotations), scan left over whitespace/comments and check for `new`.
    i = skip_ws_and_comments_left(text, i);

    if i < 3 || &bytes[i - 3..i] != b"new" {
        return false;
    }

    // Identifier boundary before `new`.
    if i >= 4 && is_ident_continue(bytes[i - 4]) {
        return false;
    }
    // Identifier boundary after `new`.
    if i < bytes.len() && is_ident_continue(bytes[i]) {
        return false;
    }

    true
}

fn classify_occurrence(text: &str, ident_span: Span) -> Option<OccurrenceKind> {
    let bytes = text.as_bytes();

    // Look backwards for `.`, allowing whitespace between `.` and the identifier.
    let mut i = ident_span.start;
    while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
        i -= 1;
    }

    // Support:
    // - `recv . ident`
    // - `recv . <T> ident` (generic invocation: `recv.<T>method(...)`)
    let dot_idx = if i > 0 && bytes[i - 1] == b'.' {
        Some(i - 1)
    } else if i > 0 && bytes[i - 1] == b'>' {
        dot_before_generic_invocation(bytes, i - 1)
    } else {
        None
    };

    let receiver = dot_idx.and_then(|dot_idx| receiver_before_dot(text, bytes, dot_idx));

    // Look backwards for `::` (method reference), allowing whitespace between
    // `::` and the identifier / type args, but requiring the `::` tokens to be
    // adjacent.
    //
    // Supports:
    // - `Type :: method`
    // - `expr :: method`
    // - `Type :: new`
    // - `Type :: <T> method` (generic method reference)
    let colon_colon_idx = if i >= 2 && bytes[i - 1] == b':' && bytes[i - 2] == b':' {
        Some(i - 2)
    } else if i > 0 && bytes[i - 1] == b'>' {
        colon_colon_before_generic_invocation(bytes, i - 1)
    } else {
        None
    };
    let method_ref_receiver =
        colon_colon_idx.and_then(|idx| receiver_before_colon_colon(text, bytes, idx));

    // Look forwards for `(`, allowing whitespace between the identifier and `(`.
    let mut j = ident_span.end;
    while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
        j += 1;
    }
    let is_call = j < bytes.len() && bytes[j] == b'(';

    if let Some(receiver) = method_ref_receiver {
        return Some(OccurrenceKind::MethodRef { receiver });
    }

    match (dot_idx, receiver, is_call) {
        (Some(_), Some(receiver), true) => Some(OccurrenceKind::MemberCall { receiver }),
        (Some(_), Some(receiver), false) => Some(OccurrenceKind::MemberField { receiver }),
        // There's a dot but we couldn't parse a receiver. This is likely a chained call like
        // `foo().bar()` or `(expr).bar` where we don't have enough context to resolve the
        // receiver type safely. Returning `None` is preferable to returning an unrelated `this.bar()`.
        (Some(_), None, _) => None,
        (None, _, true) => Some(OccurrenceKind::LocalCall),
        (None, _, false) => Some(OccurrenceKind::Ident),
    }
}

fn is_import_statement_line(text: &str, offset: usize) -> bool {
    let bytes = text.as_bytes();
    let offset = offset.min(bytes.len());

    let mut line_start = offset;
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }

    let mut line_end = offset;
    while line_end < bytes.len() && bytes[line_end] != b'\n' {
        line_end += 1;
    }

    let line = &text[line_start..line_end];
    let trimmed = line.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let Some(rest) = trimmed.strip_prefix("import") else {
        return false;
    };

    if !(rest.is_empty() || rest.chars().next().is_some_and(|c| c.is_ascii_whitespace())) {
        return false;
    }

    // `is_import_statement_line` is intentionally best-effort and historically assumed imports are
    // written on their own line. Java allows multiple statements on one line though, e.g.
    // `import p.Foo; class C { ... }`. In that case we should only treat the `import ...;` portion
    // as an import statement so navigation keeps working after the semicolon.
    let leading_ws = line.len() - trimmed.len();
    if let Some(semi_idx) = trimmed.find(';') {
        let semi_abs = line_start + leading_ws + semi_idx;
        if offset > semi_abs {
            return false;
        }
    }

    true
}

fn import_statement_targets_jdk(text: &str, offset: usize) -> bool {
    let bytes = text.as_bytes();
    let offset = offset.min(bytes.len());

    let mut line_start = offset;
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }

    let mut line_end = offset;
    while line_end < bytes.len() && bytes[line_end] != b'\n' {
        line_end += 1;
    }

    let line = &text[line_start..line_end];
    let trimmed = line.trim_start_matches(|c: char| c.is_ascii_whitespace());

    let Some(rest) = trimmed.strip_prefix("import") else {
        return false;
    };
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());

    // Ignore the `static` keyword for the purposes of checking whether the import targets a JDK
    // package; `import static java.lang.Math.max;` should also be treated as a JDK import.
    let rest = if let Some(after_static) = rest.strip_prefix("static") {
        if after_static
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_whitespace())
        {
            after_static.trim_start_matches(|c: char| c.is_ascii_whitespace())
        } else {
            rest
        }
    } else {
        rest
    };

    // Best-effort: treat `java.*`, `javax.*`, and `jdk.*` as JDK packages.
    rest.starts_with("java.") || rest.starts_with("javax.") || rest.starts_with("jdk.")
}

fn looks_like_jdk_qualified_name(receiver: &str) -> bool {
    receiver.starts_with("java.") || receiver.starts_with("javax.") || receiver.starts_with("jdk.")
}

fn receiver_before_dot(text: &str, bytes: &[u8], dot_idx: usize) -> Option<String> {
    // Best-effort support for chained receivers like:
    // - `a.b.c` (receiver for `c` is `a.b`)
    // - `a.b().c` (receiver for `c` is `a.b()`, but only for empty-arg calls)
    //
    // We accept identifier chains separated by dots with optional whitespace around dots.
    // Segments may also be empty-arg calls (`ident()`) with optional whitespace inside `()`.
    //
    //     a.b.c
    //     a . b . c
    //     this.a.b
    //     a.b().c
    //
    // We intentionally do *not* try to parse arbitrary expressions like calls with
    // arguments, indexing, or parenthesized expressions.
    let mut recv_end = dot_idx;
    while recv_end > 0 && (bytes[recv_end - 1] as char).is_ascii_whitespace() {
        recv_end -= 1;
    }

    let mut segments_rev: Vec<String> = Vec::new();
    let mut end = recv_end;
    loop {
        if end == 0 {
            return None;
        }

        // Best-effort: handle constructor receivers like `new Type(...).member`.
        //
        // This is intentionally limited to extracting the constructed type name so we can
        // resolve member navigation for patterns like `new C().foo()` and `new pkg.C().foo()`.
        if let Some(ty) = receiver_type_from_new_expression(text, bytes, end) {
            segments_rev.push(ty);
            segments_rev.reverse();
            return Some(segments_rev.join("."));
        }

        // Segment can be either:
        // - identifier (`foo`)
        // - empty-arg call (`foo()`)
        let (seg_start, seg_end, seg_is_call) = if bytes.get(end - 1) == Some(&b')') {
            // Best-effort parse `foo()` (no args; allow whitespace inside parens).
            let close_paren_idx = end - 1;
            let mut open_search = close_paren_idx;
            while open_search > 0 && (bytes[open_search - 1] as char).is_ascii_whitespace() {
                open_search -= 1;
            }
            if open_search == 0 || bytes[open_search - 1] != b'(' {
                return None;
            }
            let open_paren_idx = open_search - 1;

            let mut name_end = open_paren_idx;
            while name_end > 0 && (bytes[name_end - 1] as char).is_ascii_whitespace() {
                name_end -= 1;
            }
            let mut name_start = name_end;
            while name_start > 0 && is_ident_continue(bytes[name_start - 1]) {
                name_start -= 1;
            }
            if name_start == name_end {
                return None;
            }
            if !is_ident_start(bytes[name_start]) {
                return None;
            }

            // Special-case constructor receivers like `new C().m()`:
            // treat `new C()` as a receiver of type `C`, not as a call `C()`.
            let mut is_constructor_call = false;
            let mut kw_end = name_start;
            while kw_end > 0 && (bytes[kw_end - 1] as char).is_ascii_whitespace() {
                kw_end -= 1;
            }
            if kw_end < name_start {
                let mut kw_start = kw_end;
                while kw_start > 0 && is_ident_continue(bytes[kw_start - 1]) {
                    kw_start -= 1;
                }
                let kw = text.get(kw_start..kw_end).unwrap_or("");
                if kw == "new" && (kw_start == 0 || !is_ident_continue(bytes[kw_start - 1])) {
                    is_constructor_call = true;
                }
            }

            (name_start, name_end, !is_constructor_call)
        } else {
            let mut start = end;
            while start > 0 && is_ident_continue(bytes[start - 1]) {
                start -= 1;
            }
            if start == end {
                return None;
            }
            if !is_ident_start(bytes[start]) {
                return None;
            }
            (start, end, false)
        };

        let seg = &text[seg_start..seg_end];
        segments_rev.push(if seg_is_call {
            format!("{seg}()")
        } else {
            seg.to_string()
        });

        // Skip whitespace before this segment.
        let mut i = seg_start;
        while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
            i -= 1;
        }

        // Continue only if there's a dot before the segment (with optional whitespace).
        if i > 0 && bytes[i - 1] == b'.' {
            i -= 1;
            while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
                i -= 1;
            }
            end = i;
            if end == 0 {
                return None;
            }
            continue;
        }

        segments_rev.reverse();
        return Some(segments_rev.join("."));
    }
}

fn receiver_type_from_new_expression(text: &str, bytes: &[u8], recv_end: usize) -> Option<String> {
    // Best-effort support for `new Type(...).method` receivers.
    //
    // This allows navigation in patterns like:
    // - `new C().foo()`
    // - `new pkg.C().foo()`
    //
    // Without this, such calls would be treated as "receiverless" and resolved as `this.foo()`,
    // which can yield incorrect results (especially when `this` has an inherited `foo`).
    if recv_end == 0 || bytes.get(recv_end.wrapping_sub(1)) != Some(&b')') {
        return None;
    }

    let close_paren_idx = recv_end - 1;
    let open_paren_idx = matching_open_paren(bytes, close_paren_idx)?;

    // We expect a type name (or `Type<...>`) before the constructor parens.
    let mut end = open_paren_idx;
    while end > 0 && (bytes[end - 1] as char).is_ascii_whitespace() {
        end -= 1;
    }

    // Skip generic type args: `Foo<...>(...)`.
    if end > 0 && bytes[end - 1] == b'>' {
        let open_angle_idx = matching_open_angle(bytes, end - 1)?;
        end = open_angle_idx;
        while end > 0 && (bytes[end - 1] as char).is_ascii_whitespace() {
            end -= 1;
        }
    }

    // Parse a dotted type path ending at `end`, keeping the last segment as the receiver type.
    let (type_chain_start, type_name) = parse_ident_chain_last_segment(text, bytes, end)?;

    // Verify this is actually a `new` expression (`new Type(...)`).
    let mut i = type_chain_start;
    while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
        i -= 1;
    }
    let new_end = i;
    let mut new_start = new_end;
    while new_start > 0 && is_ident_continue(bytes[new_start - 1]) {
        new_start -= 1;
    }
    if new_start == new_end || !is_ident_start(bytes[new_start]) {
        return None;
    }
    if text.get(new_start..new_end) != Some("new") {
        return None;
    }

    Some(type_name.to_string())
}

fn matching_open_paren(bytes: &[u8], close_paren_idx: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = close_paren_idx;
    loop {
        match bytes.get(j)? {
            b')' => depth += 1,
            b'(' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }

        if j == 0 {
            break;
        }
        j -= 1;
    }

    None
}

fn matching_open_angle(bytes: &[u8], close_angle_idx: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = close_angle_idx;
    loop {
        match bytes.get(j)? {
            b'>' => depth += 1,
            b'<' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }

        if j == 0 {
            break;
        }
        j -= 1;
    }

    None
}

fn parse_ident_chain_last_segment<'a>(
    text: &'a str,
    bytes: &[u8],
    mut end: usize,
) -> Option<(usize, &'a str)> {
    let mut last_segment: Option<&'a str> = None;

    loop {
        let mut start = end;
        while start > 0 && is_ident_continue(bytes[start - 1]) {
            start -= 1;
        }
        if start == end || !is_ident_start(*bytes.get(start)?) {
            return None;
        }
        if last_segment.is_none() {
            last_segment = Some(&text[start..end]);
        }

        // Skip whitespace before this identifier.
        let mut i = start;
        while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
            i -= 1;
        }

        if i > 0 && bytes[i - 1] == b'.' {
            i -= 1;
            while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
                i -= 1;
            }
            end = i;
            continue;
        }

        return Some((start, last_segment?));
    }
}

fn receiver_before_colon_colon(text: &str, bytes: &[u8], colon_idx: usize) -> Option<String> {
    let mut recv_end = colon_idx;
    while recv_end > 0 && (bytes[recv_end - 1] as char).is_ascii_whitespace() {
        recv_end -= 1;
    }
    if recv_end == 0 {
        return None;
    }

    // Best-effort parse of a qualified receiver chain for method references:
    //
    // - `Type::method`
    // - `pkg.Type::method`
    // - `Type<T>::method` (strip type args)
    // - `expr.field::method`
    // - `expr.call()::method` (only empty-arg calls, mirroring `receiver_before_dot`)
    // - `new Foo()::method` / `new pkg.Foo()::method` (treated as receiver type `Foo`)
    // - `Foo[]::new` / `pkg.Foo[]::new` (best-effort: strip the array suffix, resolve `Foo`)
    //
    // We normalize whitespace by joining segments with `.` and representing empty-arg calls as
    // `name()`. We intentionally do *not* attempt to parse arbitrary expressions or calls with
    // arguments.
    let mut segments_rev: Vec<String> = Vec::new();
    let mut end = recv_end;
    loop {
        if end == 0 {
            return None;
        }

        // Best-effort: handle constructor receivers like `new Type(... )::method`.
        //
        // This mirrors `receiver_before_dot` so method-reference navigation works for patterns like
        // `new Foo()::bar`.
        if let Some(ty) = receiver_type_from_new_expression(text, bytes, end) {
            segments_rev.push(ty);
            segments_rev.reverse();
            return Some(segments_rev.join("."));
        }

        // Segment can be either:
        // - identifier (`foo`)
        // - identifier with type args (`Foo<...>`) (we keep only `Foo`)
        // - empty-arg call (`foo()`) with optional whitespace inside parens.
        let (seg_start, seg) = if bytes.get(end - 1) == Some(&b')') {
            // Best-effort parse `foo()` (no args; allow whitespace inside parens).
            let close_paren_idx = end - 1;
            let mut open_search = close_paren_idx;
            while open_search > 0 && (bytes[open_search - 1] as char).is_ascii_whitespace() {
                open_search -= 1;
            }
            if open_search == 0 || bytes[open_search - 1] != b'(' {
                return None;
            }
            let open_paren_idx = open_search - 1;

            let mut name_end = open_paren_idx;
            while name_end > 0 && (bytes[name_end - 1] as char).is_ascii_whitespace() {
                name_end -= 1;
            }
            let mut name_start = name_end;
            while name_start > 0 && is_ident_continue(bytes[name_start - 1]) {
                name_start -= 1;
            }
            if name_start == name_end || !is_ident_start(*bytes.get(name_start)?) {
                return None;
            }

            let name = &text[name_start..name_end];
            (name_start, format!("{name}()"))
        } else {
            let mut seg_end = end;
            while seg_end > 0 && (bytes[seg_end - 1] as char).is_ascii_whitespace() {
                seg_end -= 1;
            }
            if seg_end == 0 {
                return None;
            }

            // Strip array suffixes like `Foo[]::new` or `Foo<T>[]::new`.
            //
            // This intentionally only supports empty brackets (with optional whitespace inside),
            // so we don't misinterpret `arr[0]::method` (array access) as a type.
            loop {
                while seg_end > 0 && (bytes[seg_end - 1] as char).is_ascii_whitespace() {
                    seg_end -= 1;
                }
                if seg_end == 0 || bytes.get(seg_end - 1) != Some(&b']') {
                    break;
                }

                // Allow whitespace inside the brackets: `[ ]`.
                let close_bracket_idx = seg_end - 1;
                let mut open_search = close_bracket_idx;
                while open_search > 0 && (bytes[open_search - 1] as char).is_ascii_whitespace() {
                    open_search -= 1;
                }
                if open_search == 0 || bytes.get(open_search - 1) != Some(&b'[') {
                    return None;
                }
                seg_end = open_search - 1;
            }

            while seg_end > 0 && (bytes[seg_end - 1] as char).is_ascii_whitespace() {
                seg_end -= 1;
            }
            if seg_end == 0 {
                return None;
            }

            // Strip generic type args: `Foo<...>::method` / `pkg.Foo<...>::method`.
            if bytes.get(seg_end - 1) == Some(&b'>') {
                let open_angle_idx = matching_open_angle(bytes, seg_end - 1)?;
                seg_end = open_angle_idx;
                while seg_end > 0 && (bytes[seg_end - 1] as char).is_ascii_whitespace() {
                    seg_end -= 1;
                }
                if seg_end == 0 {
                    return None;
                }
            }

            let mut seg_start = seg_end;
            while seg_start > 0 && is_ident_continue(bytes[seg_start - 1]) {
                seg_start -= 1;
            }
            if seg_start == seg_end || !is_ident_start(*bytes.get(seg_start)?) {
                return None;
            }
            (seg_start, text[seg_start..seg_end].to_string())
        };

        segments_rev.push(seg);

        // Look for a preceding `.`, allowing whitespace around it.
        let mut k = seg_start;
        while k > 0 && (bytes[k - 1] as char).is_ascii_whitespace() {
            k -= 1;
        }
        if k == 0 || bytes[k - 1] != b'.' {
            break;
        }
        end = k - 1;
    }

    segments_rev.reverse();
    Some(segments_rev.join("."))
}

fn dot_before_generic_invocation(bytes: &[u8], close_angle_idx: usize) -> Option<usize> {
    // We are positioned at the `>` directly before an identifier. Walk backwards
    // to find the matching `<` and then check for a `.` before the type args.
    let mut depth = 0usize;
    let mut j = close_angle_idx;
    loop {
        match bytes.get(j)? {
            b'>' => depth += 1,
            b'<' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let mut k = j;
                    while k > 0 && (bytes[k - 1] as char).is_ascii_whitespace() {
                        k -= 1;
                    }
                    // Note: `bool::then_some` eagerly evaluates its argument, so avoid
                    // `cond.then_some(k - 1)` (underflows when `k == 0`).
                    return (k > 0 && bytes[k - 1] == b'.').then(|| k - 1);
                }
            }
            _ => {}
        }

        if j == 0 {
            break;
        }
        j -= 1;
    }

    None
}

fn colon_colon_before_generic_invocation(bytes: &[u8], close_angle_idx: usize) -> Option<usize> {
    // We are positioned at the `>` directly before an identifier. Walk backwards
    // to find the matching `<` and then check for a `::` before the type args.
    let mut depth = 0usize;
    let mut j = close_angle_idx;
    loop {
        match bytes.get(j)? {
            b'>' => depth += 1,
            b'<' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let mut k = j;
                    while k > 0 && (bytes[k - 1] as char).is_ascii_whitespace() {
                        k -= 1;
                    }
                    // Note: `bool::then_some` eagerly evaluates its argument, so avoid
                    // `cond.then_some(k - 2)` (underflows when `k < 2`).
                    return (k >= 2 && bytes[k - 1] == b':' && bytes[k - 2] == b':').then(|| k - 2);
                }
            }
            _ => {}
        }

        if j == 0 {
            break;
        }
        j -= 1;
    }

    None
}

fn identifier_at(text: &str, offset: usize) -> Option<(String, Span)> {
    let bytes = text.as_bytes();
    let mut offset = offset.min(bytes.len());

    if offset == bytes.len() {
        if offset > 0 && is_ident_continue(bytes[offset - 1]) {
            offset -= 1;
        }
    } else if !is_ident_continue(bytes[offset]) {
        if offset > 0 && is_ident_continue(bytes[offset - 1]) {
            offset -= 1;
        }
    }

    if offset >= bytes.len() || !is_ident_continue(bytes[offset]) {
        return None;
    }

    let mut start = offset;
    while start > 0 && is_ident_continue(bytes[start - 1]) {
        start -= 1;
    }

    let mut end = offset + 1;
    while end < bytes.len() && is_ident_continue(bytes[end]) {
        end += 1;
    }

    if start == end {
        return None;
    }

    Some((text[start..end].to_string(), Span::new(start, end)))
}

fn span_contains(span: Span, offset: usize) -> bool {
    span.start <= offset && offset < span.end
}

fn is_ident_continue(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$')
}

fn is_ident_start(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$')
}

pub(crate) fn scan_identifier_occurrences(text: &str, span: Span, ident: &str) -> Vec<Span> {
    let bytes = text.as_bytes();
    let start = span.start.min(bytes.len());
    let end = span.end.min(bytes.len());

    let mut out = Vec::new();
    let mut i = start;
    while i < end {
        let b = bytes[i];

        // Line comment
        if b == b'/' && i + 1 < end && bytes[i + 1] == b'/' {
            i += 2;
            while i < end && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Block comment
        if b == b'/' && i + 1 < end && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < end {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }

        // String literal
        if b == b'"' {
            i += 1;
            while i < end {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(end);
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        // Char literal (best-effort)
        if b == b'\'' {
            i += 1;
            while i < end {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(end);
                    continue;
                }
                if bytes[i] == b'\'' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        if is_ident_start(b) {
            let tok_start = i;
            i += 1;
            while i < end && is_ident_continue(bytes[i]) {
                i += 1;
            }
            let tok_end = i;
            if text.get(tok_start..tok_end) == Some(ident) {
                out.push(Span::new(tok_start, tok_end));
            }
            continue;
        }

        i += 1;
    }

    out
}

fn uri_for_file(db: &dyn Database, file_id: FileId) -> Uri {
    if let Some(path) = db.file_path(file_id) {
        if let Some(uri) = uri_for_path(path) {
            return uri;
        }
    }

    Uri::from_str(&format!("file:///unknown/{}.java", file_id.to_raw()))
        .expect("fallback URI is valid")
}

fn uri_for_path(path: &Path) -> Option<Uri> {
    crate::uri::uri_from_path_best_effort(path, "nav_resolve.uri_for_path")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_before_generic_invocation_does_not_underflow_at_bof() {
        let text = "<T>method";
        let close = text.find('>').expect("expected closing angle");
        assert_eq!(dot_before_generic_invocation(text.as_bytes(), close), None);
    }

    #[test]
    fn colon_colon_before_generic_invocation_does_not_underflow_at_bof() {
        let text = "<T>method";
        let close = text.find('>').expect("expected closing angle");
        assert_eq!(
            colon_colon_before_generic_invocation(text.as_bytes(), close),
            None
        );
    }

    #[test]
    fn dot_before_generic_invocation_finds_dot() {
        let text = "Foo.<T>method";
        let close = text.find('>').expect("expected closing angle");
        assert_eq!(
            dot_before_generic_invocation(text.as_bytes(), close),
            Some(text.find('.').expect("expected dot"))
        );
    }

    #[test]
    fn colon_colon_before_generic_invocation_finds_double_colon() {
        let text = "Foo::<T>method";
        let close = text.find('>').expect("expected closing angle");
        assert_eq!(
            colon_colon_before_generic_invocation(text.as_bytes(), close),
            Some(text.find("::").expect("expected ::"))
        );
    }
}

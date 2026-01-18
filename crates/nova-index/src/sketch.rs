//! Lightweight, best-effort symbol discovery used by refactorings.
//!
//! This module intentionally favors recall over precision. Refactorings are
//! expected to follow up with semantic verification passes.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

impl TextRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn shift(self, delta: usize) -> Self {
        Self {
            start: self.start.saturating_add(delta),
            end: self.end.saturating_add(delta),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Class,
    Method,
    Field,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub id: SymbolId,
    pub kind: SymbolKind,
    pub name: String,
    /// Container (e.g. class name for a method/field).
    pub container: Option<String>,
    pub file: String,
    /// Byte range of the identifier token.
    pub name_range: TextRange,
    /// Byte range of the full declaration (best-effort).
    pub decl_range: TextRange,
    /// Best-effort method parameter types, if this symbol is a method.
    ///
    /// These are lexical strings extracted from the method's parameter list and
    /// are *not* semantically resolved. Intended for overload disambiguation in
    /// refactorings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_types: Option<Vec<String>>,
    /// Best-effort method parameter names, if this symbol is a method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_names: Option<Vec<String>>,
    /// Whether the declaration is annotated with `@Override`.
    pub is_override: bool,
    /// Base class name if this symbol is a class with an `extends` clause.
    pub extends: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferenceKind {
    Call,
    FieldAccess,
    TypeUsage,
    Override,
    Implements,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceCandidate {
    pub file: String,
    pub range: TextRange,
    pub kind: ReferenceKind,
}

/// A small in-memory index used by tests and refactorings.
///
/// In a real implementation this would be backed by an incremental database.
#[derive(Debug, Clone)]
pub struct Index {
    files: BTreeMap<String, String>,
    symbols: Vec<Symbol>,
    /// Per-file symbol lists (indices into `symbols`), sorted by `decl_range.start` then
    /// `decl_range.len()`.
    ///
    /// This allows common queries like "symbol at cursor" to avoid scanning every symbol in the
    /// workspace.
    symbols_by_file: HashMap<String, Vec<usize>>,
    /// O(1) lookup by `SymbolId` (index into `symbols`).
    symbols_by_id: HashMap<SymbolId, usize>,
    /// Maps (class_name, method_name) -> method symbol ids (one per overload).
    method_symbols: HashMap<(String, String), Vec<SymbolId>>,
    class_extends: HashMap<String, String>,
    class_implements: HashMap<String, Vec<String>>,
    interface_extends: HashMap<String, Vec<String>>,
    type_kinds: HashMap<String, TypeKind>,
    /// Reverse edges for the type hierarchy.
    ///
    /// Contains direct edges for:
    /// - `class Foo extends Bar` (Bar -> Foo), and
    /// - `class Foo implements Baz` / `interface Foo extends Baz` (Baz -> Foo).
    subtypes: HashMap<String, Vec<String>>,
}

impl Index {
    pub fn new(files: BTreeMap<String, String>) -> Self {
        let mut index = Self {
            files,
            symbols: Vec::new(),
            symbols_by_file: HashMap::new(),
            symbols_by_id: HashMap::new(),
            method_symbols: HashMap::new(),
            class_extends: HashMap::new(),
            class_implements: HashMap::new(),
            interface_extends: HashMap::new(),
            type_kinds: HashMap::new(),
            subtypes: HashMap::new(),
        };
        index.rebuild();
        index
    }

    pub fn files(&self) -> &BTreeMap<String, String> {
        &self.files
    }

    pub fn symbols(&self) -> &[Symbol] {
        &self.symbols
    }

    /// Iterate over symbols discovered in a specific file.
    ///
    /// Symbols are returned in deterministic order (sorted by `decl_range.start` then
    /// `decl_range.len()`).
    pub fn symbols_in_file(&self, file: &str) -> impl Iterator<Item = &Symbol> {
        self.symbols_by_file
            .get(file)
            .into_iter()
            .flat_map(|indices| indices.iter().map(|&idx| &self.symbols[idx]))
    }

    /// Returns the *most nested* symbol (smallest `decl_range.len()`) whose declaration range
    /// covers `offset`.
    ///
    /// If `kinds` is `None`, all symbol kinds are considered.
    /// If `kinds` is provided, only symbols with a kind in that set are considered.
    pub fn symbol_at_offset(
        &self,
        file: &str,
        offset: usize,
        kinds: Option<&[SymbolKind]>,
    ) -> Option<&Symbol> {
        let indices = self.symbols_by_file.get(file)?;

        let mut best: Option<&Symbol> = None;
        for &idx in indices {
            let sym = &self.symbols[idx];
            if sym.decl_range.start > offset {
                break;
            }

            if offset < sym.decl_range.start || offset >= sym.decl_range.end {
                continue;
            }

            if let Some(kinds) = kinds {
                if !kinds.contains(&sym.kind) {
                    continue;
                }
            }

            match best {
                None => best = Some(sym),
                Some(current) => {
                    let sym_len = sym.decl_range.len();
                    let current_len = current.decl_range.len();
                    if sym_len < current_len {
                        best = Some(sym);
                        continue;
                    }
                    if sym_len == current_len {
                        // Tie-breaker: when multiple symbols share the same declaration range
                        // (e.g. `int a, b;`), prefer the one whose *name* covers the cursor.
                        //
                        // We intentionally treat `name_range.end` as inclusive here so callers can
                        // pass cursor offsets that sit "between" characters (common in LSP).
                        let sym_on_name =
                            offset >= sym.name_range.start && offset <= sym.name_range.end;
                        let current_on_name =
                            offset >= current.name_range.start && offset <= current.name_range.end;
                        if sym_on_name && !current_on_name {
                            best = Some(sym);
                            continue;
                        }
                        if sym_on_name
                            && current_on_name
                            && sym.name_range.len() < current.name_range.len()
                        {
                            best = Some(sym);
                            continue;
                        }
                    }
                }
            }
        }

        best
    }

    /// Returns all symbols in `file` whose declaration range fully covers `range`.
    ///
    /// Results are ordered from most-nested to least-nested (smallest to largest
    /// `decl_range.len()`).
    pub fn symbols_covering_range(
        &self,
        file: &str,
        range: TextRange,
        kinds: Option<&[SymbolKind]>,
    ) -> Vec<&Symbol> {
        let Some(indices) = self.symbols_by_file.get(file) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for &idx in indices {
            let sym = &self.symbols[idx];
            if sym.decl_range.start > range.start {
                break;
            }

            if sym.decl_range.start <= range.start && sym.decl_range.end >= range.end {
                if let Some(kinds) = kinds {
                    if !kinds.contains(&sym.kind) {
                        continue;
                    }
                }
                out.push(sym);
            }
        }

        out.sort_by_key(|sym| sym.decl_range.len());
        out
    }

    pub fn file_text(&self, file: &str) -> Option<&str> {
        self.files.get(file).map(String::as_str)
    }

    /// Find a field declaration in `class_name` named `field_name`.
    ///
    /// This returns `Some` only when the `(class_name, field_name)` pair is **unambiguous**
    /// (exactly one declaration exists). If there are zero declarations or multiple declarations,
    /// this returns `None`.
    pub fn find_field(&self, class_name: &str, field_name: &str) -> Option<&Symbol> {
        let mut found: Option<&Symbol> = None;
        for sym in &self.symbols {
            if sym.kind != SymbolKind::Field {
                continue;
            }
            if sym.container.as_deref() != Some(class_name) {
                continue;
            }
            if sym.name != field_name {
                continue;
            }
            if found.is_some() {
                // Avoid ambiguity in the presence of duplicate/partial parses.
                return None;
            }
            found = Some(sym);
        }
        found
    }

    /// Return the [`SymbolId`] for the unambiguous field `class_name.field_name`.
    pub fn field_symbol_id(&self, class_name: &str, field_name: &str) -> Option<SymbolId> {
        self.find_field(class_name, field_name).map(|sym| sym.id)
    }

    /// Find a method declaration in `class_name` named `method_name`.
    ///
    /// Overloads are supported: this returns `Some` only when the `(class_name, method_name)` pair
    /// is **unambiguous** (exactly one declaration exists). If there are zero declarations or
    /// multiple overloads, this returns `None`.
    ///
    /// Use [`Index::find_method_by_signature`] to disambiguate overloaded methods.
    pub fn find_method(&self, class_name: &str, method_name: &str) -> Option<&Symbol> {
        let overloads = self
            .method_symbols
            .get(&(class_name.to_string(), method_name.to_string()))?;
        if overloads.len() != 1 {
            return None;
        }
        self.find_symbol(overloads[0])
    }

    /// Find a method declaration in `class_name` by its signature.
    ///
    /// `param_types` should contain the best-effort textual parameter types as they appear in the
    /// declaration (whitespace-insensitive).
    pub fn find_method_by_signature(
        &self,
        class_name: &str,
        method_name: &str,
        param_types: &[&str],
    ) -> Option<&Symbol> {
        let normalized: Vec<String> = param_types.iter().map(|ty| normalize_ws(ty)).collect();
        let id = self.method_overload_by_param_types(class_name, method_name, &normalized)?;
        self.find_symbol(id)
    }

    pub fn find_symbol(&self, id: SymbolId) -> Option<&Symbol> {
        self.symbols_by_id
            .get(&id)
            .copied()
            .and_then(|idx| self.symbols.get(idx))
    }

    /// Finds candidates for a name across the workspace.
    ///
    /// This is intentionally a purely lexical search that returns best-effort
    /// classifications based on local context.
    pub fn find_name_candidates(&self, name: &str) -> Vec<ReferenceCandidate> {
        let mut out = Vec::new();
        for (file, text) in &self.files {
            // Precompute `@Override` method declaration name ranges for this file so we can quickly
            // distinguish declaration-site occurrences from call-sites.
            let override_method_names: HashSet<TextRange> = self
                .symbols_in_file(file)
                .filter(|sym| sym.kind == SymbolKind::Method && sym.is_override)
                .map(|sym| sym.name_range)
                .collect();
            out.extend(
                find_identifier_occurrences(text, name)
                    .into_iter()
                    .map(|range| {
                        let kind = if override_method_names.contains(&range) {
                            ReferenceKind::Override
                        } else {
                            classify_occurrence_extended(text, range)
                        };
                        ReferenceCandidate {
                            file: file.clone(),
                            range,
                            kind,
                        }
                    }),
            );
        }
        out
    }

    fn rebuild(&mut self) {
        self.symbols.clear();
        self.symbols_by_file.clear();
        self.symbols_by_id.clear();
        self.method_symbols.clear();
        self.class_extends.clear();
        self.class_implements.clear();
        self.interface_extends.clear();
        self.type_kinds.clear();
        self.subtypes.clear();

        let mut next_id: u32 = 1;
        for (file, text) in &self.files {
            let mut parser = JavaSketchParser::new(text);
            for class in parser.parse_types() {
                self.type_kinds.insert(class.name.clone(), class.kind);
                if let Some(base) = class.extends.clone() {
                    self.class_extends.insert(class.name.clone(), base.clone());
                    self.subtypes
                        .entry(base)
                        .or_default()
                        .push(class.name.clone());
                }
                if !class.implements.is_empty() {
                    self.class_implements
                        .insert(class.name.clone(), class.implements.clone());
                    for iface in &class.implements {
                        self.subtypes
                            .entry(iface.clone())
                            .or_default()
                            .push(class.name.clone());
                    }
                }
                if !class.extends_interfaces.is_empty() {
                    self.interface_extends
                        .insert(class.name.clone(), class.extends_interfaces.clone());
                    for iface in &class.extends_interfaces {
                        self.subtypes
                            .entry(iface.clone())
                            .or_default()
                            .push(class.name.clone());
                    }
                }
                let class_sym = Symbol {
                    id: SymbolId(next_id),
                    kind: SymbolKind::Class,
                    name: class.name.clone(),
                    container: None,
                    file: file.clone(),
                    name_range: class.name_range,
                    decl_range: class.decl_range,
                    param_types: None,
                    param_names: None,
                    is_override: false,
                    extends: class.extends.clone(),
                };
                next_id += 1;

                let class_idx = self.symbols.len();
                let class_id = class_sym.id;
                self.symbols.push(class_sym);
                self.symbols_by_id.insert(class_id, class_idx);
                self.symbols_by_file
                    .entry(file.clone())
                    .or_default()
                    .push(class_idx);

                for method in class.methods {
                    let id = SymbolId(next_id);
                    next_id += 1;
                    self.method_symbols
                        .entry((class.name.clone(), method.name.clone()))
                        .or_default()
                        .push(id);
                    let method_idx = self.symbols.len();
                    self.symbols.push(Symbol {
                        id,
                        kind: SymbolKind::Method,
                        name: method.name,
                        container: Some(class.name.clone()),
                        file: file.clone(),
                        name_range: method.name_range,
                        decl_range: method.decl_range,
                        param_types: Some(method.param_types),
                        param_names: Some(method.param_names),
                        is_override: method.is_override,
                        extends: None,
                    });
                    self.symbols_by_id.insert(id, method_idx);
                    self.symbols_by_file
                        .entry(file.clone())
                        .or_default()
                        .push(method_idx);
                }

                for field in class.fields {
                    let id = SymbolId(next_id);
                    next_id += 1;
                    let field_idx = self.symbols.len();
                    self.symbols.push(Symbol {
                        id,
                        kind: SymbolKind::Field,
                        name: field.name,
                        container: Some(class.name.clone()),
                        file: file.clone(),
                        name_range: field.name_range,
                        decl_range: field.decl_range,
                        param_types: None,
                        param_names: None,
                        is_override: false,
                        extends: None,
                    });
                    self.symbols_by_id.insert(id, field_idx);
                    self.symbols_by_file
                        .entry(file.clone())
                        .or_default()
                        .push(field_idx);
                }
            }
        }

        // Keep hierarchy maps deterministic.
        for interfaces in self.class_implements.values_mut() {
            interfaces.sort();
            interfaces.dedup();
        }
        for interfaces in self.interface_extends.values_mut() {
            interfaces.sort();
            interfaces.dedup();
        }
        for subs in self.subtypes.values_mut() {
            subs.sort();
            subs.dedup();
        }

        // Keep per-file symbol lists stable + enable early-exit scans.
        for indices in self.symbols_by_file.values_mut() {
            indices.sort_by_key(|&idx| {
                let sym = &self.symbols[idx];
                (
                    sym.decl_range.start,
                    sym.decl_range.len(),
                    sym.name_range.start,
                    sym.name_range.len(),
                )
            });
        }
    }

    pub fn class_extends(&self, class_name: &str) -> Option<&str> {
        self.class_extends.get(class_name).map(String::as_str)
    }

    /// Best-effort list of interfaces implemented by a class/enum/record.
    ///
    /// Names are normalized to simple identifiers by stripping:
    /// - package qualifiers (`foo.bar.Baz` -> `Baz`)
    /// - generic argument lists (`Baz<String>` -> `Baz`)
    /// - array suffixes (`Baz[]` -> `Baz`)
    #[must_use]
    pub fn class_implements(&self, class_name: &str) -> &[String] {
        self.class_implements
            .get(class_name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn interface_extends(&self, interface_name: &str) -> Option<&[String]> {
        self.interface_extends
            .get(interface_name)
            .map(|v| v.as_slice())
    }

    pub fn is_interface(&self, type_name: &str) -> bool {
        matches!(self.type_kinds.get(type_name), Some(TypeKind::Interface))
    }

    /// Returns all transitive subtypes of `ty`.
    ///
    /// The result order is deterministic:
    /// - traversal is breadth-first (nearest subtypes first), and
    /// - sibling subtypes are ordered lexicographically by type name.
    #[must_use]
    pub fn all_subtypes(&self, ty: &str) -> Vec<String> {
        let Some(direct) = self.subtypes.get(ty) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.extend(direct.iter().cloned());

        while let Some(current) = queue.pop_front() {
            if !visited.insert(current.clone()) {
                continue;
            }
            out.push(current.clone());
            if let Some(children) = self.subtypes.get(&current) {
                queue.extend(children.iter().cloned());
            }
        }

        out
    }

    fn class_hierarchy_method_matches(
        &self,
        class_name: &str,
        method_name: &str,
        target_param_types: Option<&[String]>,
        target_arity: Option<usize>,
    ) -> Vec<SymbolId> {
        let mut cur: Option<&str> = Some(class_name);
        let mut visited: HashSet<&str> = HashSet::new();
        while let Some(cls) = cur {
            if !visited.insert(cls) {
                break;
            }

            let mut matches: Vec<SymbolId> = if let Some(param_types) = target_param_types {
                self.method_overload_by_param_types(cls, method_name, param_types)
                    .into_iter()
                    .collect()
            } else if let Some(arity) = target_arity {
                self.method_overloads_by_arity(cls, method_name, arity)
            } else {
                self.method_overloads(cls, method_name)
            };

            if !matches.is_empty() {
                // Keep ordering stable for deterministic tests.
                matches.sort_by_key(|id| id.0);
                matches.dedup();
                return matches;
            }

            cur = self.class_extends(cls);
        }

        Vec::new()
    }

    /// Finds overriding/implementing method declarations in transitive subtypes.
    ///
    /// Matching is **overload-safe**: methods are compared by `(name, param_types)`.
    #[must_use]
    pub fn find_overrides(&self, method: SymbolId) -> Vec<SymbolId> {
        let Some(sym) = self.find_symbol(method) else {
            return Vec::new();
        };
        if sym.kind != SymbolKind::Method {
            return Vec::new();
        }
        let Some(container) = sym.container.as_deref() else {
            return Vec::new();
        };
        let target_is_interface = self.is_interface(container);
        let target_param_types = sym.param_types.as_deref();
        let target_arity = target_param_types
            .map(|tys| tys.len())
            .or_else(|| sym.param_names.as_ref().map(|names| names.len()));

        let mut out: Vec<SymbolId> = Vec::new();
        let mut seen: HashSet<SymbolId> = HashSet::new();
        for subtype in self.all_subtypes(container) {
            if target_is_interface {
                match self.type_kinds.get(&subtype) {
                    Some(TypeKind::Interface) => {
                        let ids: Vec<SymbolId> = if let Some(param_types) = target_param_types {
                            self.method_overload_by_param_types(&subtype, &sym.name, param_types)
                                .into_iter()
                                .collect()
                        } else if let Some(arity) = target_arity {
                            self.method_overloads_by_arity(&subtype, &sym.name, arity)
                        } else {
                            self.method_overloads(&subtype, &sym.name)
                        };
                        for id in ids {
                            if seen.insert(id) {
                                out.push(id);
                            }
                        }
                    }
                    Some(TypeKind::Class) | None => {
                        for id in self.class_hierarchy_method_matches(
                            &subtype,
                            &sym.name,
                            target_param_types,
                            target_arity,
                        ) {
                            if seen.insert(id) {
                                out.push(id);
                            }
                        }
                    }
                }
            } else {
                let ids: Vec<SymbolId> = if let Some(param_types) = target_param_types {
                    self.method_overload_by_param_types(&subtype, &sym.name, param_types)
                        .into_iter()
                        .collect()
                } else if let Some(arity) = target_arity {
                    self.method_overloads_by_arity(&subtype, &sym.name, arity)
                } else {
                    self.method_overloads(&subtype, &sym.name)
                };
                for id in ids {
                    if seen.insert(id) {
                        out.push(id);
                    }
                }
            }
        }
        out
    }

    /// Finds the first overridden/implemented method declaration in supertypes.
    ///
    /// Matching is **overload-safe**: methods are compared by `(name, param_types)`.
    pub fn find_overridden(&self, method: SymbolId) -> Option<SymbolId> {
        let sym = self.find_symbol(method)?;
        if sym.kind != SymbolKind::Method {
            return None;
        }
        let container = sym.container.as_deref()?;
        let param_types = sym.param_types.as_ref()?;

        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();

        // Prefer superclass chain over interfaces when both are at the same distance.
        if let Some(base) = self.class_extends.get(container) {
            queue.push_back(base.clone());
        }
        if let Some(ifaces) = self.class_implements.get(container) {
            queue.extend(ifaces.iter().cloned());
        }
        if let Some(ifaces) = self.interface_extends.get(container) {
            queue.extend(ifaces.iter().cloned());
        }

        while let Some(ty) = queue.pop_front() {
            if !visited.insert(ty.clone()) {
                continue;
            }

            if let Some(id) = self.method_overload_by_param_types(&ty, &sym.name, param_types) {
                return Some(id);
            }

            if let Some(base) = self.class_extends.get(&ty) {
                queue.push_back(base.clone());
            }
            if let Some(ifaces) = self.class_implements.get(&ty) {
                queue.extend(ifaces.iter().cloned());
            }
            if let Some(ifaces) = self.interface_extends.get(&ty) {
                queue.extend(ifaces.iter().cloned());
            }
        }

        None
    }

    /// Legacy name-only lookup for a method symbol id.
    ///
    /// Overloads are supported in the sketch index, so this function returns `Some(id)` **only**
    /// when `class_name.method_name` is unambiguous (exactly one declaration exists).
    ///
    /// If there are zero declarations, or if the method is overloaded (multiple declarations),
    /// this returns `None` to avoid accidentally picking an arbitrary overload.
    ///
    /// Prefer overload-aware APIs like [`Index::method_overload_by_param_types`] or
    /// [`Index::find_method_by_signature`].
    pub fn method_symbol_id(&self, class_name: &str, method_name: &str) -> Option<SymbolId> {
        self.find_method(class_name, method_name).map(|sym| sym.id)
    }

    /// Return all method symbol ids matching `class_name.method_name`.
    ///
    /// This is the overload-aware replacement for [`Index::method_symbol_id`].
    #[must_use]
    pub fn method_symbol_ids(&self, class_name: &str, method_name: &str) -> Vec<SymbolId> {
        self.method_overloads(class_name, method_name)
    }

    /// Return all method overloads matching `class_name.method_name`.
    #[must_use]
    pub fn method_overloads(&self, class_name: &str, method_name: &str) -> Vec<SymbolId> {
        self.method_symbols
            .get(&(class_name.to_string(), method_name.to_string()))
            .cloned()
            .unwrap_or_else(Vec::new)
    }

    /// Return all method overloads matching `class_name.method_name` with the given arity.
    #[must_use]
    pub fn method_overloads_by_arity(
        &self,
        class_name: &str,
        method_name: &str,
        arity: usize,
    ) -> Vec<SymbolId> {
        self.method_overloads(class_name, method_name)
            .into_iter()
            .filter(|id| {
                self.method_param_types(*id)
                    .is_some_and(|tys| tys.len() == arity)
            })
            .collect()
    }

    /// Return the unique method overload matching `class_name.method_name(param_types...)`.
    ///
    /// This is a best-effort lexical match. Parameter type strings are compared in a
    /// whitespace-insensitive way against [`Symbol::param_types`].
    pub fn method_overload_by_param_types(
        &self,
        class_name: &str,
        method_name: &str,
        param_types: &[String],
    ) -> Option<SymbolId> {
        self.method_overloads(class_name, method_name)
            .into_iter()
            .find(|id| {
                let Some(stored) = self.method_param_types(*id) else {
                    return false;
                };
                if stored.len() != param_types.len() {
                    return false;
                }
                stored
                    .iter()
                    .zip(param_types)
                    .all(|(a, b)| eq_ignore_ascii_ws(a, b))
            })
    }

    /// Return the unique method overload matching `class_name.method_name(param_types...)`.
    ///
    /// This differs from [`Index::method_overload_by_param_types`] by normalizing the provided
    /// `param_types` in the same way the sketch parser normalizes stored signatures. This makes
    /// lookups resilient to formatting differences such as spaces after commas in generic type
    /// arguments.
    pub fn method_symbol_id_by_signature(
        &self,
        class_name: &str,
        method_name: &str,
        param_types: &[String],
    ) -> Option<SymbolId> {
        let normalized: Vec<String> = param_types
            .iter()
            .map(|s| normalize_type_signature(s))
            .collect();
        self.method_overload_by_param_types(class_name, method_name, &normalized)
    }

    /// Best-effort method signature for a method symbol.
    ///
    /// Currently this returns the method's parameter type strings.
    pub fn method_signature(&self, id: SymbolId) -> Option<&[String]> {
        self.method_param_types(id)
    }

    /// Best-effort parameter type strings for a method symbol.
    pub fn method_param_types(&self, id: SymbolId) -> Option<&[String]> {
        let sym = self.find_symbol(id)?;
        if sym.kind != SymbolKind::Method {
            return None;
        }
        sym.param_types.as_deref()
    }

    /// Best-effort parameter names for a method symbol.
    pub fn method_param_names(&self, id: SymbolId) -> Option<&[String]> {
        let sym = self.find_symbol(id)?;
        if sym.kind != SymbolKind::Method {
            return None;
        }
        sym.param_names.as_deref()
    }
}

fn is_ident_start(b: u8) -> bool {
    (b as char).is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || (b as char).is_ascii_digit()
}

fn eq_ignore_ascii_ws(a: &str, b: &str) -> bool {
    let mut ia = a
        .as_bytes()
        .iter()
        .copied()
        .filter(|c| !c.is_ascii_whitespace());
    let mut ib = b
        .as_bytes()
        .iter()
        .copied()
        .filter(|c| !c.is_ascii_whitespace());
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x == y => {}
            _ => return false,
        }
    }
}

fn skip_java_string(bytes: &[u8], mut i: usize) -> usize {
    debug_assert_eq!(bytes.get(i), Some(&b'"'));

    // Best-effort support for Java text blocks: """ ... """
    if i + 2 < bytes.len() && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
        i += 3;
        while i + 2 < bytes.len() {
            if bytes[i] == b'\\' {
                // Text blocks still allow escapes; treat them similarly to normal strings.
                i = (i + 2).min(bytes.len());
                continue;
            }
            if bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                return i + 3;
            }
            i += 1;
        }
        return bytes.len();
    }

    i += 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        if bytes[i] == b'"' {
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

fn skip_java_char(bytes: &[u8], mut i: usize) -> usize {
    debug_assert_eq!(bytes.get(i), Some(&b'\''));

    i += 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        if bytes[i] == b'\'' {
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

fn trim_end_ws_and_comments(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut end = bytes.len();
    loop {
        while end > 0 && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        if end < 2 {
            return end;
        }

        // Strip trailing block comments: `/* ... */`
        if bytes[end - 2] == b'*' && bytes[end - 1] == b'/' {
            if let Some(start) = text[..end - 2].rfind("/*") {
                end = start;
                continue;
            }
        }

        // Strip trailing line comments: `// ...`
        let line_start = text[..end].rfind('\n').map(|p| p + 1).unwrap_or(0);
        if let Some(pos) = text[line_start..end].rfind("//") {
            end = line_start + pos;
            continue;
        }

        return end;
    }
}

fn find_identifier_occurrences(text: &str, name: &str) -> Vec<TextRange> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();

    let mut i = 0;
    while i < bytes.len() {
        // Skip strings and comments.
        if bytes[i] == b'"' {
            i = skip_java_string(bytes, i);
            continue;
        }

        if bytes[i] == b'\'' {
            i = skip_java_char(bytes, i);
            continue;
        }

        if bytes[i] == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
        }

        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            let end = i;
            if &text[start..end] == name {
                let before_ok = start == 0 || !is_ident_continue(bytes[start - 1]);
                let after_ok = end == bytes.len() || !is_ident_continue(bytes[end]);
                if before_ok && after_ok {
                    out.push(TextRange::new(start, end));
                }
            }
            continue;
        }

        i += 1;
    }

    out
}

fn classify_occurrence(text: &str, range: TextRange) -> ReferenceKind {
    let bytes = text.as_bytes();

    // Look ahead for `(` to guess call/type usage.
    let mut j = range.end;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b'(' {
        // Either a call or a declaration, we treat it as call candidate.
        return ReferenceKind::Call;
    }

    // Look behind for `.` to guess field access.
    let mut k = range.start;
    while k > 0 && bytes[k - 1].is_ascii_whitespace() {
        k -= 1;
    }
    if k > 0 && bytes[k - 1] == b'.' {
        return ReferenceKind::FieldAccess;
    }

    ReferenceKind::Unknown
}

fn classify_occurrence_extended(text: &str, range: TextRange) -> ReferenceKind {
    if is_in_extends_or_implements_clause(text, range) {
        return ReferenceKind::Implements;
    }

    if is_type_after_new(text, range) {
        return ReferenceKind::TypeUsage;
    }

    classify_occurrence(text, range)
}

fn is_type_after_new(text: &str, range: TextRange) -> bool {
    let before = &text[..range.start.min(text.len())];
    let end = trim_end_ws_and_comments(before);
    let before = &before[..end];
    let Some((tok_start, tok_end)) = last_identifier_range(before) else {
        return false;
    };
    &before[tok_start..tok_end] == "new"
}

fn is_in_extends_or_implements_clause(text: &str, range: TextRange) -> bool {
    let start = range.start.min(text.len());
    let line_start = text[..start].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let prefix = &text[line_start..start];

    // Don't scan past statement or block boundaries.
    let stop = match (prefix.rfind('{'), prefix.rfind(';')) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    let prefix = match stop {
        Some(pos) => &prefix[pos + 1..],
        None => prefix,
    };

    let implements_pos = rfind_keyword_token(prefix, "implements");
    let extends_pos = rfind_keyword_token(prefix, "extends");

    implements_pos.is_some() || extends_pos.is_some()
}

fn rfind_keyword_token(haystack: &str, needle: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut search_end = haystack.len();
    while let Some(pos) = haystack[..search_end].rfind(needle) {
        let before_ok = pos == 0 || !is_ident_continue(bytes[pos - 1]);
        let after_pos = pos + needle.len();
        let after_ok = after_pos == haystack.len() || !is_ident_continue(bytes[after_pos]);
        if before_ok && after_ok {
            return Some(pos);
        }
        search_end = pos;
    }
    None
}

#[derive(Debug, Clone)]
struct ParsedClass {
    kind: TypeKind,
    name: String,
    name_range: TextRange,
    decl_range: TextRange,
    extends: Option<String>,
    implements: Vec<String>,
    extends_interfaces: Vec<String>,
    methods: Vec<ParsedMethod>,
    fields: Vec<ParsedField>,
}

#[derive(Debug, Clone)]
struct ParsedMethod {
    name: String,
    name_range: TextRange,
    decl_range: TextRange,
    param_types: Vec<String>,
    param_names: Vec<String>,
    is_override: bool,
}

#[derive(Debug, Clone)]
struct ParsedField {
    name: String,
    name_range: TextRange,
    decl_range: TextRange,
}

/// A very small "parser" that understands just enough Java syntax for tests.
struct JavaSketchParser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JavaTypeDeclKind {
    Class,
    Interface,
    Enum,
    Record,
}

impl<'a> JavaSketchParser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            cursor: 0,
        }
    }

    fn parse_types(&mut self) -> Vec<ParsedClass> {
        let mut classes = Vec::new();
        while let Some((token, token_range)) = self.scan_identifier() {
            // Best-effort: treat common Java type declarations as containers for symbol discovery.
            let decl_kind = match token.as_str() {
                "class" => JavaTypeDeclKind::Class,
                "interface" => JavaTypeDeclKind::Interface,
                "enum" => JavaTypeDeclKind::Enum,
                "record" => JavaTypeDeclKind::Record,
                _ => continue,
            };

            let kind = match decl_kind {
                JavaTypeDeclKind::Interface => TypeKind::Interface,
                JavaTypeDeclKind::Class | JavaTypeDeclKind::Enum | JavaTypeDeclKind::Record => {
                    TypeKind::Class
                }
            };

            // Best-effort support for annotation type declarations: `@interface Foo {}`.
            let mut decl_start = token_range.start;
            if decl_kind == JavaTypeDeclKind::Interface
                && decl_start > 0
                && self.bytes[decl_start - 1] == b'@'
            {
                decl_start -= 1;
            }

            let decl_kw_range = TextRange::new(decl_start, token_range.end);
            if let Some((name, name_range)) = self.next_identifier() {
                // Skip type parameters after the identifier (`<T, U>`).
                self.skip_ws_and_comments();
                if self.bytes.get(self.cursor) == Some(&b'<') {
                    self.skip_angle_brackets();
                }

                // Records have a mandatory header: `record R(int x) ... {}`.
                if decl_kind == JavaTypeDeclKind::Record {
                    self.skip_ws_and_comments();
                    if self.bytes.get(self.cursor) == Some(&b'(') {
                        if let Some(close_paren) = find_matching_paren(self.text, self.cursor) {
                            self.cursor = close_paren;
                        }
                    }
                }

                let mut extends = None;
                let mut implements: Vec<String> = Vec::new();
                let mut extends_interfaces: Vec<String> = Vec::new();

                match decl_kind {
                    JavaTypeDeclKind::Interface => {
                        // Parse optional `extends I, J`
                        let saved = self.cursor;
                        if let Some((kw, _)) = self.next_identifier() {
                            if kw == "extends" {
                                extends_interfaces = self.parse_simple_type_name_list();
                            } else {
                                self.cursor = saved;
                            }
                        } else {
                            self.cursor = saved;
                        }
                    }
                    JavaTypeDeclKind::Class => {
                        // Parse optional `extends Foo`
                        let saved = self.cursor;
                        if let Some((kw, _)) = self.next_identifier() {
                            if kw == "extends" {
                                extends = self.next_simple_type_name();
                            } else {
                                self.cursor = saved;
                            }
                        } else {
                            self.cursor = saved;
                        }

                        // Parse optional `implements I, J`
                        self.skip_ws_and_comments();
                        let saved = self.cursor;
                        if let Some((kw, _)) = self.next_identifier() {
                            if kw == "implements" {
                                implements = self.parse_simple_type_name_list();
                            } else {
                                self.cursor = saved;
                            }
                        } else {
                            self.cursor = saved;
                        }
                    }
                    JavaTypeDeclKind::Enum | JavaTypeDeclKind::Record => {
                        // Enums and records can implement interfaces. They can't declare a named
                        // base class in Java, but we may encounter `extends` in malformed code.
                        // Best-effort: skip it without recording so we can still locate `implements`.
                        let saved = self.cursor;
                        if let Some((kw, _)) = self.next_identifier() {
                            if kw == "extends" {
                                let _ = self.next_type_name();
                            } else {
                                self.cursor = saved;
                            }
                        } else {
                            self.cursor = saved;
                        }

                        // Parse optional `implements I, J`
                        self.skip_ws_and_comments();
                        let saved = self.cursor;
                        if let Some((kw, _)) = self.next_identifier() {
                            if kw == "implements" {
                                implements = self.parse_simple_type_name_list();
                            } else {
                                self.cursor = saved;
                            }
                        } else {
                            self.cursor = saved;
                        }
                    }
                }

                // Find opening brace.
                self.skip_ws_and_comments();
                let body_start = match self.find_next_code_byte(b'{') {
                    Some(pos) => pos,
                    None => continue,
                };
                let body_end = match find_matching_brace(self.text, body_start) {
                    Some(end) => end,
                    None => continue,
                };

                let decl_range = TextRange::new(decl_kw_range.start, body_end);

                // Parse methods within the type body.
                let body_text = &self.text[body_start + 1..body_end - 1];
                let body_offset = body_start + 1;
                let (methods, fields) = parse_members_in_class(body_text, body_offset);

                // Recursively parse nested types. We intentionally run this on the raw body text
                // slice and then shift ranges, so nested symbols remain positioned in the
                // original file.
                let mut nested_parser = JavaSketchParser::new(body_text);
                let mut nested_classes = nested_parser.parse_types();
                for nested in &mut nested_classes {
                    nested.name_range = nested.name_range.shift(body_offset);
                    nested.decl_range = nested.decl_range.shift(body_offset);
                    for method in &mut nested.methods {
                        method.name_range = method.name_range.shift(body_offset);
                        method.decl_range = method.decl_range.shift(body_offset);
                    }
                    for field in &mut nested.fields {
                        field.name_range = field.name_range.shift(body_offset);
                        field.decl_range = field.decl_range.shift(body_offset);
                    }
                }

                classes.push(ParsedClass {
                    kind,
                    name,
                    name_range,
                    decl_range,
                    extends,
                    implements,
                    extends_interfaces,
                    methods,
                    fields,
                });
                classes.extend(nested_classes);
                self.cursor = body_end;
            }
        }
        classes
    }

    fn next_identifier(&mut self) -> Option<(String, TextRange)> {
        self.skip_ws_and_comments();
        let start = self.cursor;
        if start >= self.bytes.len() || !is_ident_start(self.bytes[start]) {
            return None;
        }
        let mut end = start + 1;
        while end < self.bytes.len() && is_ident_continue(self.bytes[end]) {
            end += 1;
        }
        self.cursor = end;
        Some((
            self.text[start..end].to_string(),
            TextRange::new(start, end),
        ))
    }

    fn scan_identifier(&mut self) -> Option<(String, TextRange)> {
        while self.cursor < self.bytes.len() {
            self.skip_ws_and_comments();
            if self.cursor >= self.bytes.len() {
                return None;
            }
            match self.bytes[self.cursor] {
                b'"' => {
                    self.cursor = skip_java_string(self.bytes, self.cursor);
                    continue;
                }
                b'\'' => {
                    self.cursor = skip_java_char(self.bytes, self.cursor);
                    continue;
                }
                _ => {}
            }
            if is_ident_start(self.bytes[self.cursor]) {
                return self.next_identifier();
            }
            self.cursor += 1;
        }
        None
    }

    fn skip_ws_and_comments(&mut self) {
        while self.cursor < self.bytes.len() {
            let b = self.bytes[self.cursor];
            if b.is_ascii_whitespace() {
                self.cursor += 1;
                continue;
            }
            if b == b'/' && self.cursor + 1 < self.bytes.len() {
                if self.bytes[self.cursor + 1] == b'/' {
                    self.cursor += 2;
                    while self.cursor < self.bytes.len() && self.bytes[self.cursor] != b'\n' {
                        self.cursor += 1;
                    }
                    continue;
                }
                if self.bytes[self.cursor + 1] == b'*' {
                    self.cursor += 2;
                    while self.cursor + 1 < self.bytes.len() {
                        if self.bytes[self.cursor] == b'*' && self.bytes[self.cursor + 1] == b'/' {
                            self.cursor += 2;
                            break;
                        }
                        self.cursor += 1;
                    }
                    continue;
                }
            }
            break;
        }
    }

    fn find_next_code_byte(&self, needle: u8) -> Option<usize> {
        let bytes = self.bytes;
        let mut i = self.cursor;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    i = skip_java_string(bytes, i);
                    continue;
                }
                b'\'' => {
                    i = skip_java_char(bytes, i);
                    continue;
                }
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                    i += 2;
                    while i + 1 < bytes.len() {
                        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
                _ => {}
            }

            if bytes[i] == needle {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    fn next_simple_type_name(&mut self) -> Option<String> {
        self.next_type_name()
            .map(|name| simple_type_name(&name).to_string())
    }

    fn parse_simple_type_name_list(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            self.skip_ws_and_comments();
            // Skip type annotations like `@Nullable`.
            while self.bytes.get(self.cursor) == Some(&b'@') {
                self.cursor += 1;
                let _ = self.next_identifier();
                self.skip_ws_and_comments();
            }

            let Some(name) = self.next_simple_type_name() else {
                break;
            };
            out.push(name);
            self.skip_ws_and_comments();
            if self.bytes.get(self.cursor) == Some(&b',') {
                self.cursor += 1;
                continue;
            }
            break;
        }
        out
    }

    fn next_type_name(&mut self) -> Option<String> {
        self.skip_ws_and_comments();
        let (first, _) = self.next_identifier()?;
        let mut name = first;

        loop {
            self.skip_ws_and_comments();
            if self.bytes.get(self.cursor) != Some(&b'.') {
                break;
            }
            self.cursor += 1;
            self.skip_ws_and_comments();
            let Some((seg, _)) = self.next_identifier() else {
                break;
            };
            name.push('.');
            name.push_str(&seg);
        }

        self.skip_ws_and_comments();
        if self.bytes.get(self.cursor) == Some(&b'<') {
            self.skip_angle_brackets();
        }

        self.skip_array_suffix();

        Some(name)
    }

    fn skip_angle_brackets(&mut self) {
        if self.bytes.get(self.cursor) != Some(&b'<') {
            return;
        }
        let mut depth: i32 = 0;
        while self.cursor < self.bytes.len() {
            match self.bytes[self.cursor] {
                b'<' => {
                    depth += 1;
                    self.cursor += 1;
                }
                b'>' => {
                    depth -= 1;
                    self.cursor += 1;
                    if depth <= 0 {
                        break;
                    }
                }
                b'"' => {
                    // Skip strings inside type args (unlikely, but keeps us robust).
                    self.cursor += 1;
                    while self.cursor < self.bytes.len() {
                        if self.bytes[self.cursor] == b'\\' {
                            self.cursor = (self.cursor + 2).min(self.bytes.len());
                            continue;
                        }
                        let b = self.bytes[self.cursor];
                        self.cursor += 1;
                        if b == b'"' {
                            break;
                        }
                    }
                }
                b'\'' => {
                    self.cursor += 1;
                    while self.cursor < self.bytes.len() {
                        if self.bytes[self.cursor] == b'\\' {
                            self.cursor = (self.cursor + 2).min(self.bytes.len());
                            continue;
                        }
                        let b = self.bytes[self.cursor];
                        self.cursor += 1;
                        if b == b'\'' {
                            break;
                        }
                    }
                }
                _ => {
                    self.cursor += 1;
                }
            }
        }
    }

    fn skip_array_suffix(&mut self) {
        loop {
            self.skip_ws_and_comments();
            if self.bytes.get(self.cursor) != Some(&b'[') {
                break;
            }
            self.cursor += 1;
            self.skip_ws_and_comments();
            if self.bytes.get(self.cursor) == Some(&b']') {
                self.cursor += 1;
                continue;
            }
            break;
        }
    }
}

fn parse_members_in_class(
    body_text: &str,
    body_offset: usize,
) -> (Vec<ParsedMethod>, Vec<ParsedField>) {
    // Extremely simple brace-depth based scanner. We only consider declarations at depth 0
    // (relative to class body).
    let bytes = body_text.as_bytes();
    let mut methods = Vec::new();
    let mut fields = Vec::new();
    let mut i = 0;
    let mut depth = 0usize;
    // Tracks the start of the current class-body "member" (field, method, initializer block,
    // nested type, etc).
    //
    // We use this as a best-effort boundary for:
    // - avoiding misclassifying call expressions in field initializers as method declarations, and
    // - extracting multi-line field declaration statements.
    let mut member_start = 0usize;
    // Tracks whether the current brace nesting started a top-level block member (initializer block /
    // nested type). We intentionally *don't* treat braces inside field initializers (e.g.
    // `int[] xs = { ... };`) as member boundaries.
    let mut in_block_member = false;
    let mut pending_override = false;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                if depth == 0 {
                    let scan = scan_member_for_assignment_and_parens(body_text, member_start, i);
                    if scan.paren_depth == 0 && !scan.has_assignment {
                        in_block_member = true;
                    }
                }
                depth += 1;
                i += 1;
                continue;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
                if depth == 0 && in_block_member {
                    // A top-level block member ended.
                    member_start = i;
                    pending_override = false;
                    in_block_member = false;
                }
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            b'"' => {
                i = skip_java_string(bytes, i);
                continue;
            }
            b'\'' => {
                i = skip_java_char(bytes, i);
                continue;
            }
            _ => {}
        }

        if depth == 0 {
            // Track `@Override` annotations so we can attach them to the next method declaration.
            if bytes[i] == b'@' && body_text[i..].starts_with("@Override") {
                pending_override = true;
                i += "@Override".len();
                continue;
            }

            // Find next identifier token and see if it looks like a method declaration.
            if is_ident_start(bytes[i]) {
                let name_start = i;
                i += 1;
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                let name_end = i;

                // Skip whitespace.
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'(' {
                    // Best-effort: avoid misclassifying call expressions in field initializers
                    // (e.g. `int x = foo();`) as method declarations.
                    if !looks_like_decl_name(body_text, name_start) {
                        continue;
                    }
                    // Heuristic: if there's an `=` earlier in this member (e.g. `int x = (foo());`),
                    // this `name(` is very likely a call inside a field initializer, not a method
                    // declaration.
                    if member_contains_assignment(body_text, member_start, name_start) {
                        continue;
                    }

                    // Find matching `)` and then `{` or `;`.
                    let open_paren = i;
                    if let Some(close_paren) = find_matching_paren(body_text, open_paren) {
                        let mut j = close_paren;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if j < bytes.len() && (bytes[j] == b'{' || bytes[j] == b';') {
                            let params_src = &body_text[open_paren + 1..close_paren - 1];
                            let (param_types, param_names) = parse_param_list(params_src);
                            // Determine the start of the declaration by walking back to the start
                            // of the current member and then to the start of its line.
                            //
                            // This is intentionally best-effort and may include doc comments and
                            // annotations (which is desirable for deletion refactorings).
                            let member_slice = &body_text[member_start..name_start];
                            let rel_first_token =
                                member_slice.find(|c: char| !c.is_whitespace()).unwrap_or(0);
                            let token_start = member_start + rel_first_token;
                            let line_start = body_text[..token_start]
                                .rfind('\n')
                                .map(|p| p + 1)
                                .unwrap_or(0);
                            let decl_start = member_start.max(line_start);

                            let decl_end = if bytes[j] == b';' {
                                // include `;`
                                body_offset + j + 1
                            } else {
                                let body_abs = body_offset + j;
                                find_matching_brace_with_offset(body_text, body_offset, j)
                                    .unwrap_or(body_abs + 1)
                            };
                            methods.push(ParsedMethod {
                                name: body_text[name_start..name_end].to_string(),
                                name_range: TextRange::new(
                                    body_offset + name_start,
                                    body_offset + name_end,
                                ),
                                decl_range: TextRange::new(body_offset + decl_start, decl_end),
                                param_types,
                                param_names,
                                is_override: pending_override,
                            });
                            pending_override = false;

                            // Skip scanning inside the declaration we just recorded.
                            i = decl_end.saturating_sub(body_offset);
                            member_start = i;
                            continue;
                        }
                    }
                }
                continue;
            }

            // Field declarations terminate with `;` at depth 0.
            if bytes[i] == b';' {
                let stmt_end = i;
                let stmt_slice = &body_text[member_start..stmt_end];
                let rel_first_token = stmt_slice.find(|c: char| !c.is_whitespace()).unwrap_or(0);
                let token_start = member_start + rel_first_token;
                let line_start = body_text[..token_start]
                    .rfind('\n')
                    .map(|p| p + 1)
                    .unwrap_or(0);
                // If multiple members appear on the same line (e.g. `int a; int b;`), we don't
                // want to extend the declaration range back past the previous member's `;`.
                let stmt_start = member_start.max(line_start);
                let stmt_text = &body_text[stmt_start..stmt_end];
                let decl_range =
                    TextRange::new(body_offset + stmt_start, body_offset + stmt_end + 1);
                fields.extend(parse_fields_in_statement(
                    stmt_text,
                    body_offset + stmt_start,
                    decl_range,
                ));
                pending_override = false;
                i += 1;
                member_start = i;
                continue;
            }
        }

        i += 1;
    }
    (methods, fields)
}

#[derive(Debug, Clone, Copy)]
struct MemberScanResult {
    has_assignment: bool,
    /// Parentheses nesting depth at `until` relative to `member_start`.
    ///
    /// Used as a heuristic to ignore `=` in annotation argument lists and to avoid treating
    /// `{...}` inside `(...)` (e.g. annotation arrays) as top-level block members.
    paren_depth: usize,
}

fn scan_member_for_assignment_and_parens(
    text: &str,
    member_start: usize,
    until: usize,
) -> MemberScanResult {
    let bytes = text.as_bytes();
    let mut i = member_start.min(bytes.len());
    let until = until.min(bytes.len());
    let mut paren_depth = 0usize;
    let mut has_assignment = false;

    while i < until {
        match bytes[i] {
            b'(' => {
                paren_depth += 1;
                i += 1;
                continue;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                i += 1;
                continue;
            }
            b'"' => {
                i = skip_java_string(bytes, i).min(until);
                continue;
            }
            b'\'' => {
                i = skip_java_char(bytes, i).min(until);
                continue;
            }
            b'/' if i + 1 < until && bytes[i + 1] == b'/' => {
                // Skip line comment.
                i += 2;
                while i < until && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < until && bytes[i + 1] == b'*' => {
                // Skip block comment.
                i += 2;
                while i + 1 < until {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            b'=' if paren_depth == 0 => {
                has_assignment = true;
                i += 1;
                continue;
            }
            _ => {
                i += 1;
            }
        }
    }

    MemberScanResult {
        has_assignment,
        paren_depth,
    }
}

fn member_contains_assignment(text: &str, member_start: usize, until: usize) -> bool {
    scan_member_for_assignment_and_parens(text, member_start, until).has_assignment
}

fn find_matching_paren(text: &str, open_paren: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = open_paren;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
                continue;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i + 1);
                }
                i += 1;
                continue;
            }
            b'"' => {
                // Skip strings / text blocks.
                i = skip_java_string(bytes, i);
                continue;
            }
            b'\'' => {
                // Skip char literals.
                i = skip_java_char(bytes, i);
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn looks_like_decl_name(text: &str, ident_start: usize) -> bool {
    let bytes = text.as_bytes();
    let mut i = ident_start;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    !matches!(bytes.get(i.wrapping_sub(1)), Some(b'=') | Some(b'.'))
}

fn parse_fields_in_statement(
    stmt_text: &str,
    stmt_offset_abs: usize,
    decl_range: TextRange,
) -> Vec<ParsedField> {
    let mut out = Vec::new();
    for (seg_start, seg_end) in split_top_level_ranges(stmt_text, b',') {
        let seg = &stmt_text[seg_start..seg_end];
        let lhs_end = find_top_level_byte(seg, b'=').unwrap_or(seg.len());
        let lhs = &seg[..lhs_end];
        let Some((name_start, name_end)) = last_identifier_range(lhs) else {
            continue;
        };
        let name_abs_start = stmt_offset_abs + seg_start + name_start;
        let name_abs_end = stmt_offset_abs + seg_start + name_end;
        out.push(ParsedField {
            name: stmt_text[seg_start + name_start..seg_start + name_end].to_string(),
            name_range: TextRange::new(name_abs_start, name_abs_end),
            decl_range,
        });
    }
    out
}

fn last_identifier_range(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut end = trim_end_ws_and_comments(text);

    // Support `name[]` style declarators by stripping trailing `[]` pairs.
    while end >= 2 && &text[end - 2..end] == "[]" {
        end -= 2;
        end = trim_end_ws_and_comments(&text[..end]);
    }

    if end == 0 {
        return None;
    }

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
    Some((start, end))
}

fn parse_param_list(params_src: &str) -> (Vec<String>, Vec<String>) {
    let params_src = params_src.trim();
    if params_src.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut param_types = Vec::new();
    let mut param_names = Vec::new();

    for (idx, part) in split_top_level(params_src, b',').into_iter().enumerate() {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        let (ty, name) = parse_single_param(part);
        param_types.push(ty);
        param_names.push(name.unwrap_or_else(|| format!("arg{}", idx + 1)));
    }

    (param_types, param_names)
}

fn parse_single_param(param: &str) -> (String, Option<String>) {
    // Strip any trailing array suffix on the name token (e.g. `int x[]`).
    let mut end = trim_end_ws_and_comments(param);
    let mut array_suffix = 0usize;
    while end >= 2 && &param[end - 2..end] == "[]" {
        array_suffix += 1;
        end -= 2;
        end = trim_end_ws_and_comments(&param[..end]);
    }
    let core = &param[..end];
    let Some((name_start, name_end)) = last_identifier_range(core) else {
        return (normalize_ws(param), None);
    };
    let name = core[name_start..name_end].to_string();
    let mut ty = core[..name_start].trim().to_string();

    // Drop leading annotations/modifiers from the type part.
    ty = strip_param_prefix_modifiers(&ty);

    for _ in 0..array_suffix {
        ty.push_str("[]");
    }

    (normalize_ws(&ty), Some(name))
}

fn strip_param_prefix_modifiers(ty: &str) -> String {
    // Best-effort: remove leading annotations (including argument lists) and `final`.
    let mut s = ty.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("final") {
            // Ensure we're stripping a full token.
            let next = rest.as_bytes().first().copied();
            if next.is_none() || next.unwrap().is_ascii_whitespace() {
                s = rest.trim_start();
                continue;
            }
        }

        if s.starts_with('@') {
            // Skip `@Ident` and optional `( ... )`.
            let bytes = s.as_bytes();
            let mut i = 1usize;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            // Skip whitespace.
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'(' {
                if let Some(close) = find_matching_paren(s, i) {
                    i = close;
                } else {
                    break;
                }
            }
            s = s[i..].trim_start();
            continue;
        }

        break;
    }

    s.to_string()
}

pub fn normalize_type_signature(text: &str) -> String {
    // Step 1: collapse whitespace to single spaces.
    let mut collapsed = String::with_capacity(text.len());
    let mut prev_ws = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                collapsed.push(' ');
                prev_ws = true;
            }
        } else {
            prev_ws = false;
            collapsed.push(ch);
        }
    }
    let collapsed = collapsed.trim();

    // Step 2: remove spaces around punctuation that commonly appears in type signatures so that
    // formatting differences (e.g. `Map<String, Integer>` vs `Map<String,Integer>`) don't break
    // overload + override matching.
    //
    // We intentionally do *not* attempt to fully parse Java types here; this is a best-effort
    // lexical normalization.
    fn no_space_around(ch: char) -> bool {
        matches!(ch, '<' | '>' | ',' | '[' | ']' | '.')
    }

    let mut out = String::with_capacity(collapsed.len());
    let mut chars = collapsed.chars().peekable();
    let mut prev: Option<char> = None;
    while let Some(ch) = chars.next() {
        if ch == ' ' {
            let Some(prev_ch) = prev else {
                continue;
            };
            let next = chars.peek().copied();
            if no_space_around(prev_ch) || next.is_some_and(no_space_around) {
                continue;
            }
            // Only emit a single space when it isn't adjacent to punctuation we normalize.
            out.push(' ');
            prev = Some(' ');
            continue;
        }
        out.push(ch);
        prev = Some(ch);
    }

    out.trim().to_string()
}

fn normalize_ws(text: &str) -> String {
    normalize_type_signature(text)
}

fn split_top_level(text: &str, sep: u8) -> Vec<String> {
    split_top_level_ranges(text, sep)
        .into_iter()
        .map(|(s, e)| text[s..e].to_string())
        .collect()
}

fn looks_like_generic_angle_open(text: &str, lt_pos: usize) -> bool {
    let bytes = text.as_bytes();
    if lt_pos == 0 {
        return false;
    }

    // Find the identifier immediately preceding `<` (skipping whitespace).
    let mut end = lt_pos;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let had_whitespace = end != lt_pos;
    if end == 0 {
        return false;
    }

    let mut start = end;
    while start > 0 && is_ident_continue(bytes[start - 1]) {
        start -= 1;
    }
    if start == end || !is_ident_start(bytes[start]) {
        return false;
    }
    let token = &text[start..end];

    // Best-effort heuristic to avoid treating `<` comparisons (e.g. `a < b`) as generic
    // delimiters. We only consider `<` to open a generic argument list if the preceding token
    // looks like a type name (CamelCase) or a type variable (`T`).
    //
    // This intentionally accepts false negatives for unusual code styles (e.g. all-uppercase
    // generic type names) in favor of not breaking statement splitting for common comparisons.
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    let has_lowercase = token.chars().any(|c| c.is_ascii_lowercase());
    if has_lowercase {
        return true;
    }

    // Single-letter tokens are frequently used as generic type parameters (`T`, `K`, ...). Require
    // them to be adjacent to `<` to avoid misclassifying comparisons like `A < B`.
    if token.len() == 1 {
        return !had_whitespace;
    }

    // Allow ALLCAPS identifiers like `URL<String>` when adjacent to `<`.
    !had_whitespace
}

fn split_top_level_ranges(text: &str, sep: u8) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut depth_brace = 0i32;
    let mut depth_angle = 0i32;
    let mut start = 0usize;
    let mut i = 0usize;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if in_char {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }

        // Skip comments.
        if b == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
        }

        match b {
            b'"' => in_string = true,
            b'\'' => in_char = true,
            b'(' => depth_paren += 1,
            b')' => depth_paren -= 1,
            b'[' => depth_brack += 1,
            b']' => depth_brack -= 1,
            b'{' => depth_brace += 1,
            b'}' => depth_brace -= 1,
            b'<' => {
                if depth_paren == 0
                    && depth_brack == 0
                    && depth_brace == 0
                    && looks_like_generic_angle_open(text, i)
                {
                    depth_angle += 1;
                }
            }
            b'>' => {
                if depth_angle > 0 && depth_paren == 0 && depth_brack == 0 && depth_brace == 0 {
                    depth_angle -= 1;
                }
            }
            _ => {}
        }

        if b == sep && depth_paren == 0 && depth_brack == 0 && depth_brace == 0 && depth_angle == 0
        {
            out.push((start, i));
            start = i + 1;
        }

        i += 1;
    }

    out.push((start, bytes.len()));
    out
}

fn find_top_level_byte(text: &str, needle: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut depth_brace = 0i32;
    let mut depth_angle = 0i32;
    let mut i = 0usize;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if in_char {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }

        // Skip comments.
        if b == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
        }

        match b {
            b'"' => in_string = true,
            b'\'' => in_char = true,
            b'(' => depth_paren += 1,
            b')' => depth_paren -= 1,
            b'[' => depth_brack += 1,
            b']' => depth_brack -= 1,
            b'{' => depth_brace += 1,
            b'}' => depth_brace -= 1,
            b'<' => {
                if depth_paren == 0
                    && depth_brack == 0
                    && depth_brace == 0
                    && looks_like_generic_angle_open(text, i)
                {
                    depth_angle += 1;
                }
            }
            b'>' => {
                if depth_angle > 0 && depth_paren == 0 && depth_brack == 0 && depth_brace == 0 {
                    depth_angle -= 1;
                }
            }
            _ => {}
        }

        if b == needle
            && depth_paren == 0
            && depth_brack == 0
            && depth_brace == 0
            && depth_angle == 0
        {
            return Some(i);
        }

        i += 1;
    }
    None
}

fn find_matching_brace(text: &str, open_brace: usize) -> Option<usize> {
    find_matching_brace_with_offset(text, 0, open_brace)
}

fn find_matching_brace_with_offset(
    text: &str,
    base_offset: usize,
    open_brace: usize,
) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = open_brace;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                depth += 1;
                i += 1;
                continue;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    // include closing brace
                    return Some(base_offset + i + 1);
                }
                i += 1;
                continue;
            }
            b'"' => {
                // Skip strings / text blocks.
                i = skip_java_string(bytes, i);
                continue;
            }
            b'\'' => {
                // Skip char literals.
                i = skip_java_char(bytes, i);
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeKind {
    Class,
    Interface,
}

fn simple_type_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

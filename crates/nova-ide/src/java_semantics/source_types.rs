use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nova_core::FileId;
use nova_hir::ast_id::AstIdMap;
use nova_hir::item_tree::{FieldKind, Item, ItemTree, Member, Modifiers};
use nova_hir::lowering::lower_item_tree;
use nova_types::{
    ClassDef, ClassKind, ConstructorDef, FieldDef, MethodDef, PrimitiveType, Type, TypeEnv,
    TypeStore,
};

/// Incrementally extracts type signatures from Java source files and registers
/// them into a shared [`TypeStore`].
///
/// This is an MVP implementation intended to unblock typing for user-defined
/// code. It extracts:
/// - class/interface declarations (including nested types)
/// - field types
/// - method signatures (params + return type)
/// - constructor param types
///
/// Generics are currently ignored and type resolution is best-effort, but we do
/// capture basic `extends`/`implements` relationships for workspace types so
/// that subtyping and inherited member completion can work.
#[derive(Debug, Default)]
pub struct SourceTypeProvider {
    file_classes: HashMap<PathBuf, Vec<String>>,
    file_ids: HashMap<PathBuf, FileId>,
    next_file_id: u32,
}

impl SourceTypeProvider {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the type environment with the classes contributed by `text`.
    ///
    /// Existing classes previously produced from `file_path` are removed first,
    /// then the new declarations are inserted.
    pub fn update_file(
        &mut self,
        store: &mut TypeStore,
        file_path: impl Into<PathBuf>,
        text: &str,
    ) {
        let file_path = file_path.into();
        self.remove_file(store, &file_path);

        let file_id = *self.file_ids.entry(file_path.clone()).or_insert_with(|| {
            let id = FileId::from_raw(self.next_file_id);
            self.next_file_id = self.next_file_id.saturating_add(1);
            id
        });
        let parse_java = nova_syntax::parse_java(text);
        let syntax = parse_java.syntax();
        let ast_id_map = AstIdMap::new(&syntax);
        let parse = nova_syntax::java::parse_with_syntax(&syntax, text.len());
        let tree = lower_item_tree(file_id, parse.compilation_unit(), &parse_java, &ast_id_map);
        let ctx = ResolveCtx::new(
            tree.package.as_ref().map(|p| p.name.as_str()),
            &tree.imports,
        );

        // Reserve ids for all types declared in this file first. This enables
        // us to resolve `extends`/`implements` clauses (and member signatures)
        // against other workspace types declared in the same file.
        let mut declared_names = Vec::new();
        for item in &tree.items {
            collect_class_names(&tree, &ctx, item, None, &mut declared_names);
        }
        for name in &declared_names {
            store.intern_class_id(name);
        }

        // `extends` / `implements` clauses can reference types from other files. The completion
        // environment is built by processing files in path order, which isn't guaranteed to visit
        // supertypes before subtypes. Pre-intern ids for referenced supertypes/interfaces so
        // `parse_type_ref` can resolve them as `Type::Class(...)` and subtyping traversals can walk
        // through the placeholder graph.
        for item in &tree.items {
            preintern_inheritance_refs(&tree, store, &ctx, item);
        }

        let object = Type::class(store.well_known().object, vec![]);
        let defs = {
            let store_ro: &TypeStore = &*store;
            let mut defs = Vec::new();
            for item in &tree.items {
                collect_class_defs(&tree, store_ro, &ctx, text, item, None, &object, &mut defs);
            }
            defs
        };

        let mut names = Vec::with_capacity(defs.len());
        for def in defs {
            names.push(def.name.clone());
            store.upsert_class(def);
        }

        self.file_classes.insert(file_path, names);
    }

    /// Remove all type declarations previously associated with `file_path`.
    pub fn remove_file(&mut self, store: &mut TypeStore, file_path: &Path) {
        let Some(old) = self.file_classes.remove(file_path) else {
            return;
        };

        for name in old {
            store.remove_class(&name);
        }
    }
}

fn collect_class_names(
    tree: &ItemTree,
    ctx: &ResolveCtx,
    item: &Item,
    outer: Option<&str>,
    out: &mut Vec<String>,
) {
    let (name, members) = match *item {
        Item::Class(id) => {
            let data = tree.class(id);
            (data.name.as_str(), data.members.as_slice())
        }
        Item::Interface(id) => {
            let data = tree.interface(id);
            (data.name.as_str(), data.members.as_slice())
        }
        Item::Enum(id) => {
            let data = tree.enum_(id);
            (data.name.as_str(), data.members.as_slice())
        }
        Item::Record(id) => {
            let data = tree.record(id);
            (data.name.as_str(), data.members.as_slice())
        }
        Item::Annotation(id) => {
            let data = tree.annotation(id);
            (data.name.as_str(), data.members.as_slice())
        }
    };

    let binary_name = binary_name(ctx.package.as_deref(), outer, name);
    out.push(binary_name.clone());

    for member in members {
        if let Member::Type(nested) = member {
            collect_class_names(tree, ctx, nested, Some(&binary_name), out);
        }
    }
}

fn preintern_inheritance_refs(
    tree: &ItemTree,
    store: &mut TypeStore,
    ctx: &ResolveCtx,
    item: &Item,
) {
    let (extends, implements, members) = match *item {
        Item::Class(id) => {
            let data = tree.class(id);
            (
                data.extends.as_slice(),
                data.implements.as_slice(),
                data.members.as_slice(),
            )
        }
        Item::Interface(id) => {
            let data = tree.interface(id);
            (data.extends.as_slice(), &[][..], data.members.as_slice())
        }
        Item::Enum(id) => {
            let data = tree.enum_(id);
            (&[][..], data.implements.as_slice(), data.members.as_slice())
        }
        Item::Record(id) => {
            let data = tree.record(id);
            (&[][..], data.implements.as_slice(), data.members.as_slice())
        }
        Item::Annotation(id) => {
            let data = tree.annotation(id);
            (&[][..], &[][..], data.members.as_slice())
        }
    };

    for ty in extends.iter().chain(implements) {
        let resolved = {
            let store_ro: &TypeStore = &*store;
            parse_type_ref(ctx, store_ro, ty)
        };
        if let Type::Named(name) = resolved {
            store.intern_class_id(&name);
        }
    }

    for member in members {
        if let Member::Type(nested) = member {
            preintern_inheritance_refs(tree, store, ctx, nested);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InheritanceMode {
    Class,
    Interface,
    ImplementsOnly,
}

fn collect_class_defs(
    tree: &ItemTree,
    store: &TypeStore,
    ctx: &ResolveCtx,
    source_text: &str,
    item: &Item,
    outer: Option<&str>,
    object: &Type,
    out: &mut Vec<ClassDef>,
) {
    let (name, kind, members, name_range, body_range, mode) = match *item {
        Item::Class(id) => {
            let data = tree.class(id);
            (
                data.name.as_str(),
                ClassKind::Class,
                data.members.as_slice(),
                data.name_range,
                data.body_range,
                InheritanceMode::Class,
            )
        }
        Item::Interface(id) => {
            let data = tree.interface(id);
            (
                data.name.as_str(),
                ClassKind::Interface,
                data.members.as_slice(),
                data.name_range,
                data.body_range,
                InheritanceMode::Interface,
            )
        }
        Item::Enum(id) => {
            let data = tree.enum_(id);
            (
                data.name.as_str(),
                ClassKind::Class,
                data.members.as_slice(),
                data.name_range,
                data.body_range,
                InheritanceMode::ImplementsOnly,
            )
        }
        Item::Record(id) => {
            let data = tree.record(id);
            (
                data.name.as_str(),
                ClassKind::Class,
                data.members.as_slice(),
                data.name_range,
                data.body_range,
                InheritanceMode::ImplementsOnly,
            )
        }
        Item::Annotation(id) => {
            let data = tree.annotation(id);
            (
                data.name.as_str(),
                ClassKind::Interface,
                data.members.as_slice(),
                data.name_range,
                data.body_range,
                InheritanceMode::Interface,
            )
        }
    };

    let binary_name = binary_name(ctx.package.as_deref(), outer, name);

    let mut fields = Vec::new();
    let mut constructors = Vec::new();
    let mut methods = Vec::new();

    for member in members {
        match member {
            Member::Field(id) => {
                let data = tree.field(*id);
                let (is_static, is_final) = match data.kind {
                    FieldKind::EnumConstant => (true, true),
                    FieldKind::RecordComponent => (false, true),
                    FieldKind::Field => {
                        let raw = data.modifiers.raw;
                        (raw & Modifiers::STATIC != 0, raw & Modifiers::FINAL != 0)
                    }
                };
                fields.push(FieldDef {
                    name: data.name.clone(),
                    ty: parse_type_ref(ctx, store, &data.ty),
                    is_static,
                    is_final,
                });
            }
            Member::Method(id) => {
                let data = tree.method(*id);
                let is_static = data.modifiers.raw & Modifiers::STATIC != 0;
                // Methods without bodies are usually abstract, but `native` methods are also
                // declared without bodies.
                let is_abstract = data.modifiers.raw & Modifiers::ABSTRACT != 0
                    || (data.body.is_none() && data.modifiers.raw & Modifiers::NATIVE == 0);
                let mut params = Vec::with_capacity(data.params.len());
                let mut is_varargs = false;
                for param in &data.params {
                    let (ty, varargs) = parse_param_type_ref(ctx, store, &param.ty);
                    params.push(ty);
                    is_varargs |= varargs;
                }

                methods.push(MethodDef {
                    name: data.name.clone(),
                    type_params: vec![],
                    params,
                    return_type: parse_type_ref(ctx, store, &data.return_ty),
                    is_static,
                    is_varargs,
                    is_abstract,
                });
            }
            Member::Constructor(id) => {
                let data = tree.constructor(*id);
                let is_accessible = data.modifiers.raw & Modifiers::PRIVATE == 0;
                let mut params = Vec::with_capacity(data.params.len());
                let mut is_varargs = false;
                for param in &data.params {
                    let (ty, varargs) = parse_param_type_ref(ctx, store, &param.ty);
                    params.push(ty);
                    is_varargs |= varargs;
                }

                constructors.push(ConstructorDef {
                    params,
                    is_varargs,
                    is_accessible,
                });
            }
            Member::Initializer(_) => {}
            Member::Type(nested) => collect_class_defs(
                tree,
                store,
                ctx,
                source_text,
                nested,
                Some(&binary_name),
                object,
                out,
            ),
        }
    }

    let (super_class, interfaces) = parse_inheritance_clauses(
        store,
        ctx,
        source_text,
        name_range,
        body_range,
        mode,
        object,
    );

    out.push(ClassDef {
        name: binary_name,
        kind,
        type_params: vec![],
        super_class,
        interfaces,
        fields,
        constructors,
        methods,
    });
}

fn parse_inheritance_clauses(
    store: &TypeStore,
    ctx: &ResolveCtx,
    source_text: &str,
    name_range: nova_types::Span,
    body_range: nova_types::Span,
    mode: InheritanceMode,
    object: &Type,
) -> (Option<Type>, Vec<Type>) {
    let default_super = match mode {
        InheritanceMode::Class | InheritanceMode::ImplementsOnly => Some(object.clone()),
        InheritanceMode::Interface => None,
    };
    let mut super_class = default_super;
    let mut interfaces = Vec::new();

    // Extract the declaration header portion (after the type name and before the body).
    let Some(header) = source_text.get(name_range.end..body_range.start) else {
        return (super_class, interfaces);
    };
    let header = strip_java_comments(header);
    let header = header.trim();
    if header.is_empty() {
        return (super_class, interfaces);
    }

    let keywords = find_top_level_keywords(header);

    match mode {
        InheritanceMode::Class => {
            if let Some(range) = keywords.extends.as_ref() {
                let end = next_keyword_start(&keywords, range.end, header.len());
                if let Some(clause) = header.get(range.end..end).map(str::trim) {
                    if !clause.is_empty() {
                        let ty = parse_type_ref(ctx, store, clause);
                        if !matches!(ty, Type::Unknown | Type::Error) {
                            super_class = Some(ty);
                        }
                    }
                }
            }
            if let Some(range) = keywords.implements.as_ref() {
                let end = next_keyword_start(&keywords, range.end, header.len());
                if let Some(clause) = header.get(range.end..end) {
                    interfaces.extend(
                        split_type_list(clause)
                            .into_iter()
                            .map(|t| parse_type_ref(ctx, store, &t))
                            .filter(|t| !matches!(t, Type::Unknown | Type::Error)),
                    );
                }
            }
        }
        InheritanceMode::Interface => {
            if let Some(range) = keywords.extends.as_ref() {
                let end = next_keyword_start(&keywords, range.end, header.len());
                if let Some(clause) = header.get(range.end..end) {
                    interfaces.extend(
                        split_type_list(clause)
                            .into_iter()
                            .map(|t| parse_type_ref(ctx, store, &t))
                            .filter(|t| !matches!(t, Type::Unknown | Type::Error)),
                    );
                }
            }
        }
        InheritanceMode::ImplementsOnly => {
            if let Some(range) = keywords.implements.as_ref() {
                let end = next_keyword_start(&keywords, range.end, header.len());
                if let Some(clause) = header.get(range.end..end) {
                    interfaces.extend(
                        split_type_list(clause)
                            .into_iter()
                            .map(|t| parse_type_ref(ctx, store, &t))
                            .filter(|t| !matches!(t, Type::Unknown | Type::Error)),
                    );
                }
            }
        }
    }

    (super_class, interfaces)
}

#[derive(Debug, Default, Clone)]
struct HeaderKeywords {
    extends: Option<std::ops::Range<usize>>,
    implements: Option<std::ops::Range<usize>>,
    permits: Option<std::ops::Range<usize>>,
}

fn find_top_level_keywords(header: &str) -> HeaderKeywords {
    let bytes = header.as_bytes();
    let mut angle_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut i = 0usize;
    let mut out = HeaderKeywords::default();

    while i < bytes.len() {
        match bytes[i] {
            b'<' => {
                angle_depth += 1;
                i += 1;
                continue;
            }
            b'>' => {
                if angle_depth > 0 {
                    angle_depth -= 1;
                }
                i += 1;
                continue;
            }
            b'(' => {
                paren_depth += 1;
                i += 1;
                continue;
            }
            b')' => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                }
                i += 1;
                continue;
            }
            _ => {}
        }

        if angle_depth == 0 && paren_depth == 0 && is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_part(bytes[i]) {
                i += 1;
            }
            let ident = &header[start..i];
            match ident {
                "extends" if out.extends.is_none() => out.extends = Some(start..i),
                "implements" if out.implements.is_none() => out.implements = Some(start..i),
                "permits" if out.permits.is_none() => out.permits = Some(start..i),
                _ => {}
            }
            continue;
        }

        i += 1;
    }

    out
}

fn is_ident_start(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'$')
}

fn is_ident_part(b: u8) -> bool {
    is_ident_start(b) || matches!(b, b'0'..=b'9')
}

fn next_keyword_start(keywords: &HeaderKeywords, after: usize, header_len: usize) -> usize {
    let mut next = header_len;
    for range in [
        keywords.extends.as_ref(),
        keywords.implements.as_ref(),
        keywords.permits.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if range.start >= after {
            next = next.min(range.start);
        }
    }
    next
}

fn split_type_list(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }

    let bytes = text.as_bytes();
    let mut angle_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut start = 0usize;
    let mut i = 0usize;
    let mut out = Vec::new();

    while i < bytes.len() {
        match bytes[i] {
            b'<' => angle_depth += 1,
            b'>' => {
                if angle_depth > 0 {
                    angle_depth -= 1;
                }
            }
            b'(' => paren_depth += 1,
            b')' => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                }
            }
            b',' if angle_depth == 0 && paren_depth == 0 => {
                if let Some(part) = text.get(start..i).map(str::trim) {
                    if !part.is_empty() {
                        out.push(part.to_string());
                    }
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }

    if let Some(part) = text.get(start..bytes.len()).map(str::trim) {
        if !part.is_empty() {
            out.push(part.to_string());
        }
    }

    out
}

fn strip_java_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    let mut last = 0usize;

    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' {
            // Flush preceding segment.
            out.push_str(&text[last..i]);
            // Skip line comment.
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(' ');
            last = i;
            continue;
        }

        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            out.push_str(&text[last..i]);
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            } else {
                i = bytes.len();
            }
            out.push(' ');
            last = i;
            continue;
        }

        i += 1;
    }

    if last < bytes.len() {
        out.push_str(&text[last..]);
    }

    out
}

fn binary_name(package: Option<&str>, outer: Option<&str>, name: &str) -> String {
    match (package, outer) {
        (_, Some(outer)) => format!("{outer}${name}"),
        (Some(pkg), None) if !pkg.is_empty() => format!("{pkg}.{name}"),
        _ => name.to_string(),
    }
}

#[derive(Debug, Clone)]
struct ResolveCtx {
    package: Option<String>,
    single_type_imports: HashMap<String, String>,
    star_imports: Vec<String>,
}

impl ResolveCtx {
    fn new(package: Option<&str>, imports: &[nova_hir::item_tree::Import]) -> Self {
        let mut single_type_imports = HashMap::new();
        let mut star_imports = Vec::new();

        for import in imports {
            if import.is_static {
                continue;
            }

            if import.is_star {
                star_imports.push(import.path.clone());
                continue;
            }

            let simple = import
                .path
                .rsplit('.')
                .next()
                .unwrap_or(import.path.as_str())
                .to_string();
            single_type_imports.insert(simple, import.path.clone());
        }

        Self {
            package: package.map(ToString::to_string),
            single_type_imports,
            star_imports,
        }
    }

    fn resolve_type_name(&self, store: &TypeStore, name: &str) -> Type {
        let name = name.trim();
        if name.contains('.') {
            return self.resolve_qualified_name(store, name);
        }
        self.resolve_simple_name(store, name)
    }

    fn resolve_simple_name(&self, store: &TypeStore, name: &str) -> Type {
        if let Some(path) = self.single_type_imports.get(name) {
            // `import p.Foo;` paths are always fully-qualified (package) names. We should not
            // attempt to treat the first segment as an in-scope type, since that can rewrite an
            // unknown `p.Foo` import into a nested binary name like `current.p$Foo`.
            return self.resolve_imported_name(store, path);
        }

        if let Some(pkg) = &self.package {
            let candidate = format!("{pkg}.{name}");
            if let Some(id) = store.lookup_class(&candidate) {
                return Type::class(id, vec![]);
            }
        }

        // `java.lang.*` is implicitly imported.
        if let Some(id) = store.lookup_class(name) {
            return Type::class(id, vec![]);
        }

        for pkg in &self.star_imports {
            let candidate = format!("{pkg}.{name}");
            if let Some(id) = store.lookup_class(&candidate) {
                return Type::class(id, vec![]);
            }
        }

        if let Some(pkg) = &self.package {
            return Type::Named(format!("{pkg}.{name}"));
        }

        Type::Named(name.to_string())
    }

    fn resolve_qualified_name(&self, store: &TypeStore, name: &str) -> Type {
        if let Some(id) = store.lookup_class(name) {
            return Type::class(id, vec![]);
        }

        let segments: Vec<&str> = name.split('.').collect();
        if segments.len() >= 2 {
            // Try interpreting the name as a fully-qualified binary name with nested types.
            for outer_idx in 0..segments.len() {
                let (pkg, rest) = segments.split_at(outer_idx);
                let Some((outer, nested)) = rest.split_first() else {
                    continue;
                };

                let mut candidate = String::new();
                if !pkg.is_empty() {
                    candidate.push_str(&pkg.join("."));
                    candidate.push('.');
                }
                candidate.push_str(outer);
                for seg in nested {
                    candidate.push('$');
                    candidate.push_str(seg);
                }

                if let Some(id) = store.lookup_class(&candidate) {
                    return Type::class(id, vec![]);
                }
            }

            // Fall back to resolving the first segment in-scope, then treat the rest as nested.
            //
            // Be careful: for unknown qualified names we don't want to interpret the first segment
            // as a type unless we have some evidence it is a type. Otherwise we can incorrectly
            // rewrite a fully-qualified package name like `z.I` into a nested binary name like
            // `current.z$I`.
            let (first, rest) = segments.split_first().unwrap();
            if let Some(owner) = self.outer_type_binary_name(store, first) {
                let mut candidate = owner;
                for seg in rest {
                    candidate.push('$');
                    candidate.push_str(seg);
                }

                if let Some(id) = store.lookup_class(&candidate) {
                    return Type::class(id, vec![]);
                }

                return Type::Named(candidate);
            }
        }

        // If the type isn't known yet, but the *outer* type is known, keep the type reference in a
        // canonical binary form so it can resolve once the nested type is later added to the store.
        if let Some(candidate) = guess_nested_binary_name(store, name) {
            return Type::Named(candidate);
        }

        Type::Named(name.to_string())
    }

    fn resolve_imported_name(&self, store: &TypeStore, name: &str) -> Type {
        if let Some(id) = store.lookup_class(name) {
            return Type::class(id, vec![]);
        }

        let segments: Vec<&str> = name.split('.').collect();
        if segments.len() >= 2 {
            // Import paths like `java.util.Map.Entry` use source syntax (`.`), but the binary name
            // for nested types is `$`-separated. Try converting suffixes to `$` and see if we can
            // find a known class id, but do not fall back to resolving the first segment in-scope
            // (imports are already fully-qualified).
            for outer_idx in 0..segments.len() {
                let (pkg, rest) = segments.split_at(outer_idx);
                let Some((outer, nested)) = rest.split_first() else {
                    continue;
                };

                let mut candidate = String::new();
                if !pkg.is_empty() {
                    candidate.push_str(&pkg.join("."));
                    candidate.push('.');
                }
                candidate.push_str(outer);
                for seg in nested {
                    candidate.push('$');
                    candidate.push_str(seg);
                }

                if let Some(id) = store.lookup_class(&candidate) {
                    return Type::class(id, vec![]);
                }
            }
        }

        if let Some(candidate) = guess_nested_binary_name(store, name) {
            return Type::Named(candidate);
        }

        Type::Named(name.to_string())
    }

    fn outer_type_binary_name(&self, store: &TypeStore, name: &str) -> Option<String> {
        // If `name` is imported as a single type, we can treat it as a type name even if the class
        // isn't in the store yet.
        if let Some(path) = self.single_type_imports.get(name) {
            return guess_nested_binary_name(store, path).or_else(|| Some(path.clone()));
        }

        match self.resolve_simple_name(store, name) {
            Type::Class(ty) => store.class(ty.def).map(|c| c.name.clone()),
            Type::Named(binary) if looks_like_type_segment(name) => Some(binary),
            _ => None,
        }
    }
}

fn looks_like_type_segment(segment: &str) -> bool {
    match segment.as_bytes().first().copied() {
        Some(b'A'..=b'Z') | Some(b'_') | Some(b'$') => true,
        _ => false,
    }
}

fn guess_nested_binary_name(store: &TypeStore, name: &str) -> Option<String> {
    let segments: Vec<&str> = name.split('.').collect();
    if segments.len() < 2 {
        return None;
    }

    // Only convert `.` -> `$` when we have evidence that the outer type exists in the store.
    //
    // This avoids misinterpreting uppercase package segments (which are legal, if uncommon) as
    // type names. Example:
    //
    // - `x.Y.C` might be a top-level type `C` in package `x.Y`.
    // - It *might also* be a nested type `C` inside type `Y` in package `x`.
    //
    // Without additional evidence, we keep the source name unchanged. If we can confirm that
    // `x.Y` exists as a class in the current store, we treat `.C` as a nested suffix and
    // canonicalize it to `x.Y$C`.
    for outer_end in (0..segments.len() - 1).rev() {
        let outer = segments[..=outer_end].join(".");
        if store.lookup_class(&outer).is_some() {
            let mut candidate = outer;
            for seg in &segments[outer_end + 1..] {
                candidate.push('$');
                candidate.push_str(seg);
            }
            return Some(candidate);
        }
    }

    None
}

fn parse_type_ref(ctx: &ResolveCtx, store: &TypeStore, text: &str) -> Type {
    let (ty, _varargs) = parse_param_type_ref(ctx, store, text);
    ty
}

fn parse_param_type_ref(ctx: &ResolveCtx, store: &TypeStore, text: &str) -> (Type, bool) {
    let text = text.trim();

    let (text, is_varargs) = match text.strip_suffix("...") {
        Some(stripped) => (stripped.trim(), true),
        None => (text, false),
    };

    let mut base = text;
    let mut dims = 0usize;
    while let Some(stripped) = base.strip_suffix("[]") {
        dims += 1;
        base = stripped.trim_end();
    }

    let base = base.trim();
    let base = base
        .split_once('<')
        .map_or(base, |(head, _)| head.trim_end());

    let mut ty = match base {
        "void" => Type::Void,
        "boolean" => Type::Primitive(PrimitiveType::Boolean),
        "byte" => Type::Primitive(PrimitiveType::Byte),
        "short" => Type::Primitive(PrimitiveType::Short),
        "char" => Type::Primitive(PrimitiveType::Char),
        "int" => Type::Primitive(PrimitiveType::Int),
        "long" => Type::Primitive(PrimitiveType::Long),
        "float" => Type::Primitive(PrimitiveType::Float),
        "double" => Type::Primitive(PrimitiveType::Double),
        "" => Type::Unknown,
        other => ctx.resolve_type_name(store, other),
    };

    for _ in 0..dims {
        ty = Type::Array(Box::new(ty));
    }
    if is_varargs {
        ty = Type::Array(Box::new(ty));
    }

    (ty, is_varargs)
}

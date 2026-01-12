use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nova_core::FileId;
use nova_hir::ast_id::AstIdMap;
use nova_hir::item_tree::{Item, ItemTree, Member, Modifiers};
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
/// Generics and inheritance clauses are currently ignored: classes default to
/// extending `java.lang.Object` and implement no interfaces.
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
        let ast_id_map = AstIdMap::new(&parse_java.syntax());
        let parse = nova_syntax::java::parse(text);
        let tree = lower_item_tree(file_id, parse.compilation_unit(), &parse_java, &ast_id_map);
        let ctx = ResolveCtx::new(
            tree.package.as_ref().map(|p| p.name.as_str()),
            &tree.imports,
        );

        let object = Type::class(store.well_known().object, vec![]);
        let defs = {
            let store_ro: &TypeStore = &*store;
            let mut defs = Vec::new();
            for item in &tree.items {
                collect_class_defs(&tree, store_ro, &ctx, item, None, &object, &mut defs);
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

fn collect_class_defs(
    tree: &ItemTree,
    store: &TypeStore,
    ctx: &ResolveCtx,
    item: &Item,
    outer: Option<&str>,
    object: &Type,
    out: &mut Vec<ClassDef>,
) {
    let (name, kind, members) = match *item {
        Item::Class(id) => {
            let data = tree.class(id);
            (
                data.name.as_str(),
                ClassKind::Class,
                data.members.as_slice(),
            )
        }
        Item::Interface(id) => {
            let data = tree.interface(id);
            (
                data.name.as_str(),
                ClassKind::Interface,
                data.members.as_slice(),
            )
        }
        Item::Enum(id) => {
            let data = tree.enum_(id);
            (
                data.name.as_str(),
                ClassKind::Class,
                data.members.as_slice(),
            )
        }
        Item::Record(id) => {
            let data = tree.record(id);
            (
                data.name.as_str(),
                ClassKind::Class,
                data.members.as_slice(),
            )
        }
        Item::Annotation(id) => {
            let data = tree.annotation(id);
            (
                data.name.as_str(),
                ClassKind::Interface,
                data.members.as_slice(),
            )
        }
    };

    let binary_name = binary_name(ctx.package.as_deref(), outer, name);

    let mut fields = Vec::new();
    let mut constructors = Vec::new();
    let mut methods = Vec::new();

    for member in members {
        match *member {
            Member::Field(id) => {
                let data = tree.field(id);
                let is_static = data.modifiers.raw & Modifiers::STATIC != 0;
                let is_final = data.modifiers.raw & Modifiers::FINAL != 0;
                fields.push(FieldDef {
                    name: data.name.clone(),
                    ty: parse_type_ref(ctx, store, &data.ty),
                    is_static,
                    is_final,
                });
            }
            Member::Method(id) => {
                let data = tree.method(id);
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
                let data = tree.constructor(id);
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
            Member::Type(nested) => {
                collect_class_defs(tree, store, ctx, &nested, Some(&binary_name), object, out)
            }
        }
    }

    let super_class = match kind {
        ClassKind::Interface => None,
        ClassKind::Class => Some(object.clone()),
    };

    out.push(ClassDef {
        name: binary_name,
        kind,
        type_params: vec![],
        super_class,
        interfaces: vec![],
        fields,
        constructors,
        methods,
    });
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
            return self.resolve_type_name(store, path);
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
            let (first, rest) = segments.split_first().unwrap();
            if let Some(owner) = self.simple_type_binary_name(store, first) {
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

        Type::Named(name.to_string())
    }

    fn simple_type_binary_name(&self, store: &TypeStore, name: &str) -> Option<String> {
        match self.resolve_simple_name(store, name) {
            Type::Class(ty) => store.class(ty.def).map(|c| c.name.clone()),
            Type::Named(name) => Some(name),
            _ => None,
        }
    }
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

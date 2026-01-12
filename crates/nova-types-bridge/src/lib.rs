//! Integration utilities for bridging external class stubs into Nova's core type system.
//!
//! `nova-types` intentionally keeps its dependencies minimal (it defines the core `Type` model and
//! type-checking algorithms). Anything that depends on parsing Java classfiles/signatures lives in a
//! higher layer.
//!
//! This crate owns the canonical implementation for loading external `nova_types::TypeProvider`
//! stubs into a `nova_types::TypeStore`: [`ExternalTypeLoader`]. Keeping a single loader
//! implementation ensures Salsa typechecking and unit tests exercise the same code path and avoids
//! competing `ClassId` allocation behavior.
#![forbid(unsafe_code)]

use std::collections::HashSet;

use nova_classfile::{
    parse_class_signature, parse_field_descriptor, parse_field_signature, parse_method_descriptor,
    parse_method_signature, ClassSignature, ClassTypeSignature, FieldType, MethodDescriptor,
    MethodSignature, ReturnType, TypeArgument, TypeParameter, TypeSignature,
};
use nova_types::{
    ClassDef, ClassId, ClassKind, ConstructorDef, FieldDef, MethodDef, Type, TypeEnv, TypeProvider,
    TypeStore,
};
use nova_types_signature::{SignatureTranslator, TypeVarScope};

const ACC_INTERFACE: u16 = 0x0200;
const ACC_PRIVATE: u16 = 0x0002;
const ACC_FINAL: u16 = 0x0010;
const ACC_STATIC: u16 = 0x0008;
const ACC_VARARGS: u16 = 0x0080;
const ACC_ABSTRACT: u16 = 0x0400;

/// Loads external `TypeProvider` stubs into a `TypeStore` on demand.
pub struct ExternalTypeLoader<'a> {
    pub store: &'a mut TypeStore,
    pub provider: &'a dyn TypeProvider,
    in_progress: HashSet<String>,
    loaded: HashSet<String>,
}

impl<'a> ExternalTypeLoader<'a> {
    pub fn new(store: &'a mut TypeStore, provider: &'a dyn TypeProvider) -> Self {
        Self {
            store,
            provider,
            in_progress: HashSet::new(),
            loaded: HashSet::new(),
        }
    }

    /// Ensure `binary_name` is present in the store; returns its `ClassId` if found/loaded.
    pub fn ensure_class(&mut self, binary_name: &str) -> Option<ClassId> {
        if self.loaded.contains(binary_name) {
            return self.store.lookup_class(binary_name);
        }

        if self.in_progress.contains(binary_name) {
            return self.store.lookup_class(binary_name);
        }

        let existing = self.store.lookup_class(binary_name);
        if let Some(id) = existing {
            // `ExternalTypeLoader` is designed to populate conservative placeholders created by
            // `TypeStore::intern_class_id`. Overwriting an existing, non-placeholder definition is a
            // footgun:
            // - it can clobber workspace/source definitions (nova-db)
            // - it can clobber the built-in minimal JDK type model (used for stable core types)
            // - it can allocate duplicate type params (since `define_class` replaces the `ClassDef`
            //   but does not reclaim `TypeVarId`s)
            //
            // If callers want richer `java.*` stubs than the built-in minimal model, they should
            // ensure those types are represented as placeholders (or avoid defining them up-front)
            // so the loader can safely populate them.
            if self
                .store
                .class(id)
                .is_some_and(|def| !is_placeholder_class_def(def))
            {
                self.loaded.insert(binary_name.to_string());
                return Some(id);
            }
        }
        let Some(stub) = self.provider.lookup_type(binary_name) else {
            return existing;
        };

        let id = self.store.intern_class_id(binary_name);
        self.in_progress.insert(binary_name.to_string());

        let def = self.build_class_def(binary_name, &stub);
        self.store.define_class(id, def);

        self.in_progress.remove(binary_name);
        self.loaded.insert(binary_name.to_string());

        Some(id)
    }

    fn build_class_def(&mut self, binary_name: &str, stub: &nova_types::TypeDefStub) -> ClassDef {
        let kind = if stub.access_flags & ACC_INTERFACE != 0 {
            ClassKind::Interface
        } else {
            ClassKind::Class
        };

        // Ensure all referenced types are at least interned so signature translation produces
        // `Type::Class` (with type args) instead of erasing to `Type::Named`.
        self.preload_referenced_classes(stub);

        let object_ty = self.object_type();

        let mut class_scope = TypeVarScope::new();
        let (type_params, mut super_class, interfaces) = if let Some(sig) = stub
            .signature
            .as_deref()
            .and_then(|sig| parse_class_signature(sig).ok())
        {
            let empty = TypeVarScope::new();
            let (type_params, super_class, interfaces) = {
                let mut translator = SignatureTranslator::new(self.store);
                translator.class_sig_from_classfile(&empty, &sig)
            };
            for (tp, id) in sig.type_parameters.iter().zip(type_params.iter().copied()) {
                class_scope.insert(tp.name.clone(), id);
            }
            (type_params, super_class, interfaces)
        } else {
            let super_class = stub
                .super_binary_name
                .as_deref()
                .map(|name| self.binary_class_ref(name));
            let interfaces = stub
                .interfaces
                .iter()
                .map(|name| self.binary_class_ref(name))
                .collect();
            (Vec::new(), super_class, interfaces)
        };

        let mut translator = SignatureTranslator::new(self.store);

        // Preserve `Object` as an explicit supertype for member resolution. This matches
        // `TypeStore::with_minimal_jdk`, which models the implicit `Object` super chain
        // (particularly for interfaces) so inherited `toString()`/etc can be discovered.
        if binary_name != "java.lang.Object" && super_class.is_none() {
            super_class = Some(object_ty.clone());
        }

        let fields = stub
            .fields
            .iter()
            .map(|field| {
                let ty = field
                    .signature
                    .as_deref()
                    .and_then(|sig| parse_field_signature(sig).ok())
                    .map(|sig| translator.ty_from_field_sig(&class_scope, &sig))
                    .or_else(|| {
                        parse_field_descriptor(&field.descriptor)
                            .ok()
                            .map(|desc| translator.ty_from_descriptor_field(&desc))
                    })
                    .unwrap_or(Type::Unknown);

                FieldDef {
                    name: field.name.clone(),
                    ty,
                    is_static: field.access_flags & ACC_STATIC != 0,
                    is_final: field.access_flags & ACC_FINAL != 0,
                }
            })
            .collect::<Vec<_>>();

        let mut methods = Vec::new();
        let mut constructors = Vec::new();
        for method in &stub.methods {
            match method.name.as_str() {
                "<clinit>" => continue,
                "<init>" => constructors.push(constructor_def(
                    &mut translator,
                    &class_scope,
                    method,
                    method.access_flags,
                )),
                _ => methods.push(method_def(
                    &mut translator,
                    &class_scope,
                    method,
                    method.access_flags,
                )),
            }
        }

        ClassDef {
            name: binary_name.to_string(),
            kind,
            type_params,
            super_class,
            interfaces,
            fields,
            constructors,
            methods,
        }
    }

    fn preload_referenced_classes(&mut self, stub: &nova_types::TypeDefStub) {
        if let Some(super_name) = stub.super_binary_name.as_deref() {
            self.ensure_class(super_name);
        }
        for iface in &stub.interfaces {
            self.ensure_class(iface);
        }

        let mut internals = Vec::new();

        if let Some(sig) = stub
            .signature
            .as_deref()
            .and_then(|sig| parse_class_signature(sig).ok())
        {
            collect_internal_from_class_signature(&sig, &mut internals);
        }

        for field in &stub.fields {
            if let Some(sig) = field
                .signature
                .as_deref()
                .and_then(|sig| parse_field_signature(sig).ok())
            {
                collect_internal_from_type_sig(&sig, &mut internals);
            } else if let Ok(desc) = parse_field_descriptor(&field.descriptor) {
                collect_internal_from_field_desc(&desc, &mut internals);
            }
        }

        for method in &stub.methods {
            if let Ok(desc) = parse_method_descriptor(&method.descriptor) {
                collect_internal_from_method_desc(&desc, &mut internals);
            }
            if let Some(sig) = method
                .signature
                .as_deref()
                .and_then(|sig| parse_method_signature(sig).ok())
            {
                collect_internal_from_method_signature(&sig, &mut internals);
            }
        }

        for internal in internals {
            let binary = internal_to_binary(&internal);
            self.ensure_class(&binary);
        }
    }

    fn object_type(&mut self) -> Type {
        let name = "java.lang.Object";
        self.store
            .lookup_class(name)
            .map(|id| Type::class(id, vec![]))
            .unwrap_or_else(|| Type::Named(name.to_string()))
    }

    fn binary_class_ref(&mut self, binary_name: &str) -> Type {
        self.ensure_class(binary_name)
            .map(|id| Type::class(id, vec![]))
            .unwrap_or_else(|| Type::Named(binary_name.to_string()))
    }
}

fn is_placeholder_class_def(def: &ClassDef) -> bool {
    def.kind == ClassKind::Class
        && def.name != "java.lang.Object"
        && def.super_class.is_none()
        && def.type_params.is_empty()
        && def.interfaces.is_empty()
        && def.fields.is_empty()
        && def.constructors.is_empty()
        && def.methods.is_empty()
}

fn constructor_def(
    translator: &mut SignatureTranslator<'_>,
    class_scope: &TypeVarScope,
    stub: &nova_types::MethodStub,
    access_flags: u16,
) -> ConstructorDef {
    let is_varargs = access_flags & ACC_VARARGS != 0;
    let is_accessible = access_flags & ACC_PRIVATE == 0;

    let params = if let Some(sig) = stub
        .signature
        .as_deref()
        .and_then(|s| parse_method_signature(s).ok())
    {
        if let Ok(desc) = parse_method_descriptor(&stub.descriptor) {
            let (_, params, _) = translator.method_sig_from_classfile(class_scope, &sig, &desc);
            params
        } else {
            sig.parameters
                .iter()
                .map(|p| translator.ty_from_type_sig(class_scope, p))
                .collect()
        }
    } else if let Ok(desc) = parse_method_descriptor(&stub.descriptor) {
        desc.params
            .iter()
            .map(|p| translator.ty_from_descriptor_field(p))
            .collect()
    } else {
        Vec::new()
    };

    ConstructorDef {
        params,
        is_varargs,
        is_accessible,
    }
}

fn method_def(
    translator: &mut SignatureTranslator<'_>,
    class_scope: &TypeVarScope,
    stub: &nova_types::MethodStub,
    access_flags: u16,
) -> MethodDef {
    let is_static = access_flags & ACC_STATIC != 0;
    let is_varargs = access_flags & ACC_VARARGS != 0;
    let is_abstract = access_flags & ACC_ABSTRACT != 0;

    let Ok(desc) = parse_method_descriptor(&stub.descriptor) else {
        return MethodDef {
            name: stub.name.clone(),
            type_params: Vec::new(),
            params: Vec::new(),
            return_type: Type::Unknown,
            is_static,
            is_varargs,
            is_abstract,
        };
    };

    if let Some(sig) = stub
        .signature
        .as_deref()
        .and_then(|s| parse_method_signature(s).ok())
    {
        let (type_params, params, return_type) =
            translator.method_sig_from_classfile(class_scope, &sig, &desc);
        return MethodDef {
            name: stub.name.clone(),
            type_params,
            params,
            return_type,
            is_static,
            is_varargs,
            is_abstract,
        };
    }

    let params = desc
        .params
        .iter()
        .map(|p| translator.ty_from_descriptor_field(p))
        .collect();
    let return_type = match &desc.return_type {
        ReturnType::Void => Type::Void,
        ReturnType::Type(field) => translator.ty_from_descriptor_field(field),
    };

    MethodDef {
        name: stub.name.clone(),
        type_params: Vec::new(),
        params,
        return_type,
        is_static,
        is_varargs,
        is_abstract,
    }
}

fn internal_to_binary(internal: &str) -> String {
    internal.replace('/', ".")
}

fn collect_internal_from_class_signature(sig: &ClassSignature, out: &mut Vec<String>) {
    for tp in &sig.type_parameters {
        collect_internal_from_type_parameter(tp, out);
    }
    collect_internal_from_class_type_sig(&sig.super_class, out);
    for iface in &sig.interfaces {
        collect_internal_from_class_type_sig(iface, out);
    }
}

fn collect_internal_from_type_parameter(tp: &TypeParameter, out: &mut Vec<String>) {
    if let Some(bound) = &tp.class_bound {
        collect_internal_from_type_sig(bound, out);
    }
    for bound in &tp.interface_bounds {
        collect_internal_from_type_sig(bound, out);
    }
}

fn collect_internal_from_method_signature(sig: &MethodSignature, out: &mut Vec<String>) {
    for tp in &sig.type_parameters {
        collect_internal_from_type_parameter(tp, out);
    }
    for param in &sig.parameters {
        collect_internal_from_type_sig(param, out);
    }
    if let Some(ret) = &sig.return_type {
        collect_internal_from_type_sig(ret, out);
    }
    for thrown in &sig.throws {
        collect_internal_from_type_sig(thrown, out);
    }
}

fn collect_internal_from_type_sig(sig: &TypeSignature, out: &mut Vec<String>) {
    match sig {
        TypeSignature::Base(_) => {}
        TypeSignature::Array(inner) => collect_internal_from_type_sig(inner.as_ref(), out),
        TypeSignature::Class(cls) => collect_internal_from_class_type_sig(cls, out),
        TypeSignature::TypeVariable(_) => {}
    }
}

fn collect_internal_from_class_type_sig(sig: &ClassTypeSignature, out: &mut Vec<String>) {
    out.push(sig.internal_name());
    for seg in &sig.segments {
        for arg in &seg.type_arguments {
            collect_internal_from_type_argument(arg, out);
        }
    }
}

fn collect_internal_from_type_argument(arg: &TypeArgument, out: &mut Vec<String>) {
    match arg {
        TypeArgument::Any => {}
        TypeArgument::Exact(inner) | TypeArgument::Extends(inner) | TypeArgument::Super(inner) => {
            collect_internal_from_type_sig(inner.as_ref(), out);
        }
    }
}

fn collect_internal_from_field_desc(desc: &FieldType, out: &mut Vec<String>) {
    match desc {
        FieldType::Base(_) => {}
        FieldType::Array(inner) => collect_internal_from_field_desc(inner.as_ref(), out),
        FieldType::Object(internal) => out.push(internal.to_string()),
    }
}

fn collect_internal_from_method_desc(desc: &MethodDescriptor, out: &mut Vec<String>) {
    for param in &desc.params {
        collect_internal_from_field_desc(param, out);
    }
    if let ReturnType::Type(ret) = &desc.return_type {
        collect_internal_from_field_desc(ret, out);
    }
}

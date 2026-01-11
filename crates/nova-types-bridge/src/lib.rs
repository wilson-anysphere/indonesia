#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use nova_classfile::{
    parse_class_signature, parse_field_descriptor, parse_field_signature, parse_method_descriptor,
    parse_method_signature, BaseType, ClassTypeSignature, FieldType, MethodDescriptor, ReturnType,
    TypeArgument, TypeParameter, TypeSignature,
};
use nova_types::{
    ClassDef, ClassId, ClassKind, ClassType, ConstructorDef, FieldDef, MethodDef, PrimitiveType,
    Type, TypeEnv, TypeParamDef, TypeProvider, TypeStore, TypeVarId, WildcardBound,
};

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

        let mut class_type_vars = HashMap::<String, TypeVarId>::new();
        let mut type_params = Vec::new();
        let empty_method_type_vars = HashMap::<String, TypeVarId>::new();

        let parsed_sig = stub
            .signature
            .as_deref()
            .and_then(|sig| parse_class_signature(sig).ok());

        let (super_class, interfaces) = if let Some(sig) = parsed_sig {
            // Two-pass allocation so self-referential bounds (`T extends Comparable<T>`) can resolve.
            let placeholder_bounds = vec![self.object_type()];
            for tp in &sig.type_parameters {
                let id = self
                    .store
                    .add_type_param(tp.name.clone(), placeholder_bounds.clone());
                class_type_vars.insert(tp.name.clone(), id);
                type_params.push(id);
            }
            for tp in &sig.type_parameters {
                let Some(id) = class_type_vars.get(&tp.name).copied() else {
                    continue;
                };
                let bounds =
                    self.convert_type_parameter_bounds(tp, &class_type_vars, &empty_method_type_vars);
                self.store.define_type_param(
                    id,
                    TypeParamDef {
                        name: tp.name.clone(),
                        upper_bounds: bounds,
                        lower_bound: None,
                    },
                );
            }

            let super_class = match kind {
                ClassKind::Interface => None,
                ClassKind::Class => Some(self.class_type_signature(
                    &sig.super_class,
                    &class_type_vars,
                    &empty_method_type_vars,
                )),
            };
            let interfaces = sig
                .interfaces
                .iter()
                .map(|iface| {
                    self.class_type_signature(iface, &class_type_vars, &empty_method_type_vars)
                })
                .collect();
            (super_class, interfaces)
        } else {
            let super_class = match kind {
                ClassKind::Interface => None,
                ClassKind::Class => stub
                    .super_binary_name
                    .as_deref()
                    .map(|name| self.binary_class_ref(name)),
            };
            let interfaces = stub
                .interfaces
                .iter()
                .map(|name| self.binary_class_ref(name))
                .collect();
            (super_class, interfaces)
        };

        let fields = stub
            .fields
            .iter()
            .map(|field| {
                let ty = field
                    .signature
                    .as_deref()
                    .and_then(|sig| parse_field_signature(sig).ok())
                    .map(|sig| self.type_signature(&sig, &class_type_vars, &empty_method_type_vars))
                    .or_else(|| {
                        parse_field_descriptor(&field.descriptor)
                            .ok()
                            .map(|desc| self.field_type(&desc))
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
                "<init>" => constructors.push(self.constructor_def(method, &class_type_vars)),
                _ => methods.push(self.method_def(method, &class_type_vars)),
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

    fn constructor_def(
        &mut self,
        stub: &nova_types::MethodStub,
        class_type_vars: &HashMap<String, TypeVarId>,
    ) -> ConstructorDef {
        let is_varargs = stub.access_flags & ACC_VARARGS != 0;
        let is_accessible = stub.access_flags & ACC_PRIVATE == 0;
        let empty_method_type_vars = HashMap::<String, TypeVarId>::new();

        if let Some(sig) = stub.signature.as_deref().and_then(|s| parse_method_signature(s).ok()) {
            let params = sig
                .parameters
                .iter()
                .map(|p| self.type_signature(p, class_type_vars, &empty_method_type_vars))
                .collect();
            return ConstructorDef {
                params,
                is_varargs,
                is_accessible,
            };
        }

        let params = parse_method_descriptor(&stub.descriptor)
            .ok()
            .map(|d| self.method_descriptor(&d).0)
            .unwrap_or_default();

        ConstructorDef {
            params,
            is_varargs,
            is_accessible,
        }
    }

    fn method_def(
        &mut self,
        stub: &nova_types::MethodStub,
        class_type_vars: &HashMap<String, TypeVarId>,
    ) -> MethodDef {
        let is_static = stub.access_flags & ACC_STATIC != 0;
        let is_varargs = stub.access_flags & ACC_VARARGS != 0;
        let is_abstract = stub.access_flags & ACC_ABSTRACT != 0;

        if let Some(sig) = stub.signature.as_deref().and_then(|s| parse_method_signature(s).ok()) {
            let mut method_type_vars = HashMap::<String, TypeVarId>::new();
            let mut type_params = Vec::new();
            // Two-pass allocation so self-referential bounds can resolve.
            let placeholder_bounds = vec![self.object_type()];
            for tp in &sig.type_parameters {
                let id = self
                    .store
                    .add_type_param(tp.name.clone(), placeholder_bounds.clone());
                method_type_vars.insert(tp.name.clone(), id);
                type_params.push(id);
            }
            for tp in &sig.type_parameters {
                let Some(id) = method_type_vars.get(&tp.name).copied() else {
                    continue;
                };
                let bounds = self.convert_type_parameter_bounds(tp, class_type_vars, &method_type_vars);
                self.store.define_type_param(
                    id,
                    TypeParamDef {
                        name: tp.name.clone(),
                        upper_bounds: bounds,
                        lower_bound: None,
                    },
                );
            }

            let params = sig
                .parameters
                .iter()
                .map(|p| self.type_signature(p, class_type_vars, &method_type_vars))
                .collect();
            let return_type = sig
                .return_type
                .as_ref()
                .map(|rt| self.type_signature(rt, class_type_vars, &method_type_vars))
                .unwrap_or(Type::Void);

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

        let parsed_desc = parse_method_descriptor(&stub.descriptor).ok();
        let (params, return_type) = parsed_desc
            .as_ref()
            .map(|d| self.method_descriptor(d))
            .unwrap_or_else(|| (Vec::new(), Type::Unknown));

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

    fn convert_type_parameter_bounds(
        &mut self,
        tp: &TypeParameter,
        class_type_vars: &HashMap<String, TypeVarId>,
        method_type_vars: &HashMap<String, TypeVarId>,
    ) -> Vec<Type> {
        let mut out = Vec::new();

        match &tp.class_bound {
            Some(bound) => out.push(self.type_signature(bound, class_type_vars, method_type_vars)),
            None => out.push(self.object_type()),
        }

        out.extend(
            tp.interface_bounds
                .iter()
                .map(|b| self.type_signature(b, class_type_vars, method_type_vars)),
        );

        out
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

    fn internal_to_binary(internal: &str) -> String {
        internal.replace('/', ".")
    }

    fn class_type_signature(
        &mut self,
        sig: &ClassTypeSignature,
        class_type_vars: &HashMap<String, TypeVarId>,
        method_type_vars: &HashMap<String, TypeVarId>,
    ) -> Type {
        let binary_name = Self::internal_to_binary(&sig.internal_name());

        // Best-effort: inner class signatures can carry type arguments for the outer segments, but
        // `nova_types::Type` currently only supports arguments on the leaf class.
        let args = sig
            .segments
            .last()
            .map(|seg| {
                seg.type_arguments
                    .iter()
                    .map(|arg| self.type_argument(arg, class_type_vars, method_type_vars))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        match self.ensure_class(&binary_name) {
            Some(id) => Type::Class(ClassType { def: id, args }),
            None => Type::Named(binary_name),
        }
    }

    fn type_argument(
        &mut self,
        arg: &TypeArgument,
        class_type_vars: &HashMap<String, TypeVarId>,
        method_type_vars: &HashMap<String, TypeVarId>,
    ) -> Type {
        match arg {
            TypeArgument::Any => Type::Wildcard(WildcardBound::Unbounded),
            TypeArgument::Exact(ty) => self.type_signature(ty, class_type_vars, method_type_vars),
            TypeArgument::Extends(ty) => Type::Wildcard(WildcardBound::Extends(Box::new(
                self.type_signature(ty, class_type_vars, method_type_vars),
            ))),
            TypeArgument::Super(ty) => Type::Wildcard(WildcardBound::Super(Box::new(
                self.type_signature(ty, class_type_vars, method_type_vars),
            ))),
        }
    }

    fn type_signature(
        &mut self,
        sig: &TypeSignature,
        class_type_vars: &HashMap<String, TypeVarId>,
        method_type_vars: &HashMap<String, TypeVarId>,
    ) -> Type {
        match sig {
            TypeSignature::Base(base) => Type::Primitive(match base {
                BaseType::Byte => PrimitiveType::Byte,
                BaseType::Char => PrimitiveType::Char,
                BaseType::Double => PrimitiveType::Double,
                BaseType::Float => PrimitiveType::Float,
                BaseType::Int => PrimitiveType::Int,
                BaseType::Long => PrimitiveType::Long,
                BaseType::Short => PrimitiveType::Short,
                BaseType::Boolean => PrimitiveType::Boolean,
            }),
            TypeSignature::Array(elem) => Type::Array(Box::new(self.type_signature(
                elem,
                class_type_vars,
                method_type_vars,
            ))),
            TypeSignature::Class(cls) => self.class_type_signature(cls, class_type_vars, method_type_vars),
            TypeSignature::TypeVariable(name) => method_type_vars
                .get(name)
                .or_else(|| class_type_vars.get(name))
                .copied()
                .map(Type::TypeVar)
                .unwrap_or(Type::Unknown),
        }
    }

    fn method_descriptor(&mut self, desc: &MethodDescriptor) -> (Vec<Type>, Type) {
        let params = desc.params.iter().map(|p| self.field_type(p)).collect();
        let return_type = match &desc.return_type {
            ReturnType::Void => Type::Void,
            ReturnType::Type(ty) => self.field_type(ty),
        };
        (params, return_type)
    }

    fn field_type(&mut self, ty: &FieldType) -> Type {
        match ty {
            FieldType::Base(base) => Type::Primitive(match base {
                BaseType::Byte => PrimitiveType::Byte,
                BaseType::Char => PrimitiveType::Char,
                BaseType::Double => PrimitiveType::Double,
                BaseType::Float => PrimitiveType::Float,
                BaseType::Int => PrimitiveType::Int,
                BaseType::Long => PrimitiveType::Long,
                BaseType::Short => PrimitiveType::Short,
                BaseType::Boolean => PrimitiveType::Boolean,
            }),
            FieldType::Array(elem) => Type::Array(Box::new(self.field_type(elem))),
            FieldType::Object(internal) => {
                let binary = Self::internal_to_binary(internal);
                self.ensure_class(&binary)
                    .map(|id| Type::class(id, vec![]))
                    .unwrap_or_else(|| Type::Named(binary))
            }
        }
    }
}

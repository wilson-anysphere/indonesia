use std::collections::HashMap;
use std::fmt;

use nova_classfile::{
    parse_class_signature, parse_field_descriptor, parse_field_signature, parse_method_descriptor,
    parse_method_signature, BaseType, ClassTypeSignature, FieldType, ReturnType, TypeArgument, TypeParameter,
    TypeSignature,
};

use crate::{
    ClassDef, ClassId, ClassKind, ConstructorDef, FieldDef, FieldStub, MethodDef, MethodStub, PrimitiveType,
    Type, TypeDefStub, TypeEnv, TypeProvider, TypeStore, TypeVarId, WellKnownTypes, WildcardBound,
};

#[derive(Debug)]
pub enum TypeLoadError {
    MissingType(String),
    WellKnownNotBootstrapped,
    Classfile(nova_classfile::Error),
}

impl fmt::Display for TypeLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeLoadError::MissingType(name) => write!(f, "missing type stub for `{name}`"),
            TypeLoadError::WellKnownNotBootstrapped => {
                write!(f, "well-known types not bootstrapped")
            }
            TypeLoadError::Classfile(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for TypeLoadError {}

impl From<nova_classfile::Error> for TypeLoadError {
    fn from(value: nova_classfile::Error) -> Self {
        TypeLoadError::Classfile(value)
    }
}

/// Loads class stubs from a [`TypeProvider`] into a [`TypeStore`].
///
/// This is a best-effort bridge used by the early semantic layers: it converts
/// classfile-derived descriptors/signatures into Nova's compact `Type` model so
/// that `nova-types` algorithms (generic substitution, method resolution, etc.)
/// can operate over project/JDK symbols.
pub struct TypeStoreLoader<'a> {
    store: &'a mut TypeStore,
    provider: &'a dyn TypeProvider,
}

impl<'a> TypeStoreLoader<'a> {
    pub fn new(store: &'a mut TypeStore, provider: &'a dyn TypeProvider) -> Self {
        Self { store, provider }
    }

    pub fn store(&self) -> &TypeStore {
        self.store
    }

    pub fn store_mut(&mut self) -> &mut TypeStore {
        self.store
    }

    /// Initializes `TypeStore::well_known` with a minimal set of core JDK types.
    ///
    /// This is intentionally dependency-free: we create placeholder class defs so
    /// that type-system algorithms can run even when using partial fixtures (the
    /// unit-test fake-JDK omits `java.lang.Object`, for example).
    pub fn bootstrap_well_known(&mut self) -> Result<(), TypeLoadError> {
        if self.store.well_known.is_some() {
            return Ok(());
        }

        let object = self.ensure_builtin_class("java.lang.Object", ClassKind::Class, None);
        let object_ty = Type::class(object, vec![]);

        let string = self.ensure_builtin_class(
            "java.lang.String",
            ClassKind::Class,
            Some(object_ty.clone()),
        );
        let integer = self.ensure_builtin_class(
            "java.lang.Integer",
            ClassKind::Class,
            Some(object_ty.clone()),
        );
        let cloneable = self.ensure_builtin_class("java.lang.Cloneable", ClassKind::Interface, None);
        let serializable = self.ensure_builtin_class("java.io.Serializable", ClassKind::Interface, None);

        self.store.well_known = Some(WellKnownTypes {
            object,
            string,
            integer,
            cloneable,
            serializable,
        });

        Ok(())
    }

    pub fn ensure_class(&mut self, binary_name: &str) -> Result<ClassId, TypeLoadError> {
        if self.store.well_known.is_none() {
            self.bootstrap_well_known()?;
        }

        if let Some(id) = self.store.lookup_class(binary_name) {
            return Ok(id);
        }

        let stub = self
            .provider
            .lookup_type(binary_name)
            .ok_or_else(|| TypeLoadError::MissingType(binary_name.to_string()))?;

        // Reserve an id/placeholder first so self-referential signatures (e.g.
        // `Enum<E extends Enum<E>>`) can resolve back to this id during parsing.
        let id = self.store.intern_class_id(binary_name);
        let def = self.build_class_def(binary_name, &stub)?;
        self.store.define_class(id, def);

        Ok(id)
    }

    fn ensure_builtin_class(
        &mut self,
        binary_name: &str,
        kind: ClassKind,
        super_class: Option<Type>,
    ) -> ClassId {
        if let Some(id) = self.store.class_by_name.get(binary_name).copied() {
            return id;
        }

        self.store.add_class(ClassDef {
            name: binary_name.to_string(),
            kind,
            type_params: vec![],
            super_class,
            interfaces: vec![],
            fields: vec![],
            constructors: vec![],
            methods: vec![],
        })
    }

    fn well_known(&self) -> Result<&WellKnownTypes, TypeLoadError> {
        self.store
            .well_known
            .as_ref()
            .ok_or(TypeLoadError::WellKnownNotBootstrapped)
    }

    fn object_type(&self) -> Result<Type, TypeLoadError> {
        Ok(Type::class(self.well_known()?.object, vec![]))
    }

    fn build_class_def(
        &mut self,
        binary_name: &str,
        stub: &TypeDefStub,
    ) -> Result<ClassDef, TypeLoadError> {
        let kind = if stub.access_flags & 0x0200 != 0 {
            ClassKind::Interface
        } else {
            ClassKind::Class
        };

        let mut type_vars: HashMap<String, TypeVarId> = HashMap::new();
        let mut type_params: Vec<TypeVarId> = Vec::new();
        let object = self.object_type()?;

        let (super_class, interfaces) = if let Some(sig) = stub.signature.as_deref() {
            let sig = parse_class_signature(sig)?;

            for tp in &sig.type_parameters {
                let id = self
                    .store
                    .add_type_param(tp.name.clone(), vec![object.clone()]);
                type_vars.insert(tp.name.clone(), id);
                type_params.push(id);
            }

            for tp in &sig.type_parameters {
                let id = type_vars
                    .get(&tp.name)
                    .copied()
                    .expect("type vars just inserted");
                let bounds = self.bounds_for_type_parameter(tp, &type_vars)?;
                if let Some(slot) = self.store.type_params.get_mut(id.0 as usize) {
                    slot.upper_bounds = bounds;
                }
            }

            let super_class = match kind {
                ClassKind::Interface => None,
                ClassKind::Class => Some(self.class_sig_to_type(&sig.super_class, &type_vars)?),
            };
            let interfaces = sig
                .interfaces
                .iter()
                .map(|i| self.class_sig_to_type(i, &type_vars))
                .collect::<Result<Vec<_>, _>>()?;
            (super_class, interfaces)
        } else {
            let super_class = match kind {
                ClassKind::Interface => None,
                ClassKind::Class => stub
                    .super_binary_name
                    .as_deref()
                    .map(|name| self.class_name_to_type(name, vec![]))
                    .transpose()?,
            };

            let interfaces = stub
                .interfaces
                .iter()
                .map(|name| self.class_name_to_type(name, vec![]))
                .collect::<Result<Vec<_>, _>>()?;

            (super_class, interfaces)
        };

        let fields = stub
            .fields
            .iter()
            .map(|f| self.build_field_def(f, &type_vars))
            .collect::<Result<Vec<_>, _>>()?;

        let mut constructors = Vec::new();
        let mut methods = Vec::new();
        for m in &stub.methods {
            match m.name.as_str() {
                "<clinit>" => continue,
                "<init>" => constructors.push(self.build_constructor_def(m, &type_vars)?),
                _ => methods.push(self.build_method_def(m, &type_vars)?),
            }
        }

        Ok(ClassDef {
            name: binary_name.to_string(),
            kind,
            type_params,
            super_class,
            interfaces,
            fields,
            constructors,
            methods,
        })
    }

    fn build_field_def(&mut self, stub: &FieldStub, type_vars: &HashMap<String, TypeVarId>) -> Result<FieldDef, TypeLoadError> {
        const ACC_STATIC: u16 = 0x0008;
        const ACC_FINAL: u16 = 0x0010;

        let ty = if let Some(sig) = stub.signature.as_deref() {
            let sig = parse_field_signature(sig)?;
            self.type_sig_to_type(&sig, type_vars)?
        } else {
            let desc = parse_field_descriptor(&stub.descriptor)?;
            self.field_type_to_type(&desc)?
        };

        Ok(FieldDef {
            name: stub.name.clone(),
            ty,
            is_static: stub.access_flags & ACC_STATIC != 0,
            is_final: stub.access_flags & ACC_FINAL != 0,
        })
    }

    fn build_constructor_def(
        &mut self,
        stub: &MethodStub,
        class_type_vars: &HashMap<String, TypeVarId>,
    ) -> Result<ConstructorDef, TypeLoadError> {
        const ACC_PRIVATE: u16 = 0x0002;
        const ACC_VARARGS: u16 = 0x0080;

        let is_varargs = stub.access_flags & ACC_VARARGS != 0;
        let is_accessible = stub.access_flags & ACC_PRIVATE == 0;

        let params = if let Some(sig) = stub.signature.as_deref() {
            let sig = parse_method_signature(sig)?;

            let object = self.object_type()?;
            let mut type_vars = class_type_vars.clone();

            for tp in &sig.type_parameters {
                let id = self.store.add_type_param(tp.name.clone(), vec![object.clone()]);
                type_vars.insert(tp.name.clone(), id);
            }
            for tp in &sig.type_parameters {
                let id = type_vars
                    .get(&tp.name)
                    .copied()
                    .expect("constructor type vars just inserted");
                let bounds = self.bounds_for_type_parameter(tp, &type_vars)?;
                if let Some(slot) = self.store.type_params.get_mut(id.0 as usize) {
                    slot.upper_bounds = bounds;
                }
            }

            sig.parameters
                .iter()
                .map(|p| self.type_sig_to_type(p, &type_vars))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            let desc = parse_method_descriptor(&stub.descriptor)?;
            desc.params
                .iter()
                .map(|p| self.field_type_to_type(p))
                .collect::<Result<Vec<_>, _>>()?
        };

        Ok(ConstructorDef {
            params,
            is_varargs,
            is_accessible,
        })
    }

    fn build_method_def(
        &mut self,
        stub: &MethodStub,
        class_type_vars: &HashMap<String, TypeVarId>,
    ) -> Result<MethodDef, TypeLoadError> {
        const ACC_STATIC: u16 = 0x0008;
        const ACC_VARARGS: u16 = 0x0080;
        const ACC_ABSTRACT: u16 = 0x0400;

        let is_static = stub.access_flags & ACC_STATIC != 0;
        let is_varargs = stub.access_flags & ACC_VARARGS != 0;
        let is_abstract = stub.access_flags & ACC_ABSTRACT != 0;

        if let Some(sig) = stub.signature.as_deref() {
            let sig = parse_method_signature(sig)?;

            let object = self.object_type()?;
            let mut type_vars = class_type_vars.clone();
            let mut type_params = Vec::new();

            for tp in &sig.type_parameters {
                let id = self
                    .store
                    .add_type_param(tp.name.clone(), vec![object.clone()]);
                type_vars.insert(tp.name.clone(), id);
                type_params.push(id);
            }

            for tp in &sig.type_parameters {
                let id = type_vars
                    .get(&tp.name)
                    .copied()
                    .expect("method type vars just inserted");
                let bounds = self.bounds_for_type_parameter(tp, &type_vars)?;
                if let Some(slot) = self.store.type_params.get_mut(id.0 as usize) {
                    slot.upper_bounds = bounds;
                }
            }

            let params = sig
                .parameters
                .iter()
                .map(|p| self.type_sig_to_type(p, &type_vars))
                .collect::<Result<Vec<_>, _>>()?;
            let return_type = match &sig.return_type {
                None => Type::Void,
                Some(ret) => self.type_sig_to_type(ret, &type_vars)?,
            };

            return Ok(MethodDef {
                name: stub.name.clone(),
                type_params,
                params,
                return_type,
                is_static,
                is_varargs,
                is_abstract,
            });
        }

        let desc = parse_method_descriptor(&stub.descriptor)?;
        let params = desc
            .params
            .iter()
            .map(|p| self.field_type_to_type(p))
            .collect::<Result<Vec<_>, _>>()?;
        let return_type = match &desc.return_type {
            ReturnType::Void => Type::Void,
            ReturnType::Type(ty) => self.field_type_to_type(ty)?,
        };

        Ok(MethodDef {
            name: stub.name.clone(),
            type_params: vec![],
            params,
            return_type,
            is_static,
            is_varargs,
            is_abstract,
        })
    }

    fn bounds_for_type_parameter(
        &mut self,
        tp: &TypeParameter,
        type_vars: &HashMap<String, TypeVarId>,
    ) -> Result<Vec<Type>, TypeLoadError> {
        let mut out = Vec::new();
        if let Some(bound) = &tp.class_bound {
            out.push(self.type_sig_to_type(bound, type_vars)?);
        } else {
            out.push(self.object_type()?);
        }
        for bound in &tp.interface_bounds {
            out.push(self.type_sig_to_type(bound, type_vars)?);
        }
        Ok(out)
    }

    fn field_type_to_type(&mut self, ty: &FieldType) -> Result<Type, TypeLoadError> {
        Ok(match ty {
            FieldType::Base(base) => Type::Primitive(map_primitive(*base)),
            FieldType::Object(internal) => {
                let binary = internal.replace('/', ".");
                self.class_name_to_type(&binary, vec![])?
            }
            FieldType::Array(elem) => Type::Array(Box::new(self.field_type_to_type(elem)?)),
        })
    }

    fn class_sig_to_type(
        &mut self,
        sig: &ClassTypeSignature,
        type_vars: &HashMap<String, TypeVarId>,
    ) -> Result<Type, TypeLoadError> {
        let binary_name = class_sig_binary_name(sig);
        let args = sig
            .segments
            .last()
            .map(|seg| {
                seg.type_arguments
                    .iter()
                    .map(|a| self.type_arg_to_type(a, type_vars))
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_else(|| Ok(Vec::new()))?;
        self.class_name_to_type(&binary_name, args)
    }

    fn class_name_to_type(
        &mut self,
        binary_name: &str,
        args: Vec<Type>,
    ) -> Result<Type, TypeLoadError> {
        match self.ensure_class(binary_name) {
            Ok(id) => Ok(Type::class(id, args)),
            Err(TypeLoadError::MissingType(_)) => Ok(Type::Named(binary_name.to_string())),
            Err(other) => Err(other),
        }
    }

    fn type_arg_to_type(
        &mut self,
        arg: &TypeArgument,
        type_vars: &HashMap<String, TypeVarId>,
    ) -> Result<Type, TypeLoadError> {
        Ok(match arg {
            TypeArgument::Any => Type::Wildcard(WildcardBound::Unbounded),
            TypeArgument::Exact(ty) => self.type_sig_to_type(ty, type_vars)?,
            TypeArgument::Extends(upper) => Type::Wildcard(WildcardBound::Extends(Box::new(
                self.type_sig_to_type(upper, type_vars)?,
            ))),
            TypeArgument::Super(lower) => Type::Wildcard(WildcardBound::Super(Box::new(
                self.type_sig_to_type(lower, type_vars)?,
            ))),
        })
    }

    fn type_sig_to_type(
        &mut self,
        sig: &TypeSignature,
        type_vars: &HashMap<String, TypeVarId>,
    ) -> Result<Type, TypeLoadError> {
        Ok(match sig {
            TypeSignature::Base(base) => Type::Primitive(map_primitive(*base)),
            TypeSignature::Array(elem) => {
                Type::Array(Box::new(self.type_sig_to_type(elem, type_vars)?))
            }
            TypeSignature::Class(cls) => self.class_sig_to_type(cls, type_vars)?,
            TypeSignature::TypeVariable(name) => type_vars
                .get(name)
                .copied()
                .map(Type::TypeVar)
                .unwrap_or(Type::Unknown),
        })
    }
}

fn class_sig_binary_name(sig: &ClassTypeSignature) -> String {
    let mut out = String::new();
    if !sig.package.is_empty() {
        out.push_str(&sig.package.join("."));
        out.push('.');
    }
    for (idx, seg) in sig.segments.iter().enumerate() {
        if idx > 0 {
            out.push('$');
        }
        out.push_str(&seg.name);
    }
    out
}

fn map_primitive(base: BaseType) -> PrimitiveType {
    match base {
        BaseType::Boolean => PrimitiveType::Boolean,
        BaseType::Byte => PrimitiveType::Byte,
        BaseType::Short => PrimitiveType::Short,
        BaseType::Char => PrimitiveType::Char,
        BaseType::Int => PrimitiveType::Int,
        BaseType::Long => PrimitiveType::Long,
        BaseType::Float => PrimitiveType::Float,
        BaseType::Double => PrimitiveType::Double,
    }
}

//! Framework-oriented member lookup helpers.

use nova_framework::{AnalyzerRegistry, Database as FrameworkDatabase, VirtualMember};
use nova_hir::framework::{ConstructorData, FieldData, MethodData};
use nova_types::{Parameter, Type};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    Static,
    Instance,
}

/// Return member names suitable for member completion.
pub fn complete_member_names(
    db: &dyn FrameworkDatabase,
    registry: &AnalyzerRegistry,
    ty: &Type,
) -> Vec<String> {
    members_of_type(db, registry, ty)
        .into_iter()
        .filter_map(|m| match m {
            MemberInfo::Field { name, .. } => Some(name),
            MemberInfo::Method { name, .. } => Some(name),
            MemberInfo::InnerClass { name } => Some(name),
            MemberInfo::Constructor { .. } => None,
        })
        .collect()
}

/// Resolve a method call against a receiver type and return the method's return type.
pub fn resolve_method_call(
    db: &dyn FrameworkDatabase,
    registry: &AnalyzerRegistry,
    receiver: &Type,
    call_kind: CallKind,
    name: &str,
    args: &[Type],
) -> Option<Type> {
    methods_of_type(db, registry, receiver)
        .into_iter()
        .filter(|m| match (call_kind, m.is_static) {
            (CallKind::Static, true) => true,
            (CallKind::Instance, false) => true,
            // Best-effort: allow static methods from an instance receiver.
            (CallKind::Instance, true) => true,
            (CallKind::Static, false) => false,
        })
        .find(|m| m.name == name && params_match(&m.params, args))
        .map(|m| m.return_type)
}

/// Resolve a constructor call and return the constructed type.
pub fn resolve_constructor_call(
    db: &dyn FrameworkDatabase,
    registry: &AnalyzerRegistry,
    ty: &Type,
    args: &[Type],
) -> Option<Type> {
    constructors_of_type(db, registry, ty)
        .into_iter()
        .find(|c| params_match(&c.params, args))
        .map(|_| ty.clone())
}

#[derive(Debug, Clone)]
struct ResolvedMethod {
    name: String,
    return_type: Type,
    params: Vec<Parameter>,
    is_static: bool,
}

#[derive(Debug, Clone)]
struct ResolvedConstructor {
    params: Vec<Parameter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MemberInfo {
    Field {
        name: String,
        ty: Type,
    },
    Method {
        name: String,
        return_type: Type,
        params: Vec<Parameter>,
    },
    Constructor {
        params: Vec<Parameter>,
    },
    InnerClass {
        name: String,
    },
}

fn params_match(params: &[Parameter], args: &[Type]) -> bool {
    if params.len() != args.len() {
        return false;
    }
    params.iter().zip(args.iter()).all(|(p, a)| p.ty == *a)
}

fn members_of_type(
    db: &dyn FrameworkDatabase,
    registry: &AnalyzerRegistry,
    ty: &Type,
) -> Vec<MemberInfo> {
    match ty {
        Type::Class(nova_types::ClassType { def, .. }) => members_of_class(db, registry, *def),
        Type::VirtualInner { owner, name } => members_of_virtual_inner(db, registry, *owner, name),
        _ => Vec::new(),
    }
}

fn members_of_class(
    db: &dyn FrameworkDatabase,
    registry: &AnalyzerRegistry,
    class: nova_types::ClassId,
) -> Vec<MemberInfo> {
    let class_data = db.class(class);
    let mut members = Vec::new();

    for FieldData { name, ty, .. } in &class_data.fields {
        members.push(MemberInfo::Field {
            name: name.clone(),
            ty: ty.clone(),
        });
    }

    for MethodData {
        name,
        return_type,
        params,
        ..
    } in &class_data.methods
    {
        members.push(MemberInfo::Method {
            name: name.clone(),
            return_type: return_type.clone(),
            params: params.clone(),
        });
    }

    for ConstructorData { params } in &class_data.constructors {
        members.push(MemberInfo::Constructor {
            params: params.clone(),
        });
    }

    for vm in registry.virtual_members_for_class(db, class) {
        push_virtual_member_info(&mut members, vm);
    }

    members
}

fn push_virtual_member_info(out: &mut Vec<MemberInfo>, vm: VirtualMember) {
    match vm {
        VirtualMember::Field(f) => out.push(MemberInfo::Field {
            name: f.name,
            ty: f.ty,
        }),
        VirtualMember::Method(m) => out.push(MemberInfo::Method {
            name: m.name,
            return_type: m.return_type,
            params: m.params,
        }),
        VirtualMember::Constructor(c) => out.push(MemberInfo::Constructor { params: c.params }),
        VirtualMember::InnerClass(c) => out.push(MemberInfo::InnerClass { name: c.name }),
    }
}

fn methods_of_type(
    db: &dyn FrameworkDatabase,
    registry: &AnalyzerRegistry,
    ty: &Type,
) -> Vec<ResolvedMethod> {
    match ty {
        Type::Class(nova_types::ClassType { def, .. }) => methods_of_class(db, registry, *def),
        Type::VirtualInner { owner, name } => methods_of_virtual_inner(db, registry, *owner, name),
        _ => Vec::new(),
    }
}

fn constructors_of_type(
    db: &dyn FrameworkDatabase,
    registry: &AnalyzerRegistry,
    ty: &Type,
) -> Vec<ResolvedConstructor> {
    match ty {
        Type::Class(nova_types::ClassType { def, .. }) => constructors_of_class(db, registry, *def),
        Type::VirtualInner { owner, name } => constructors_of_virtual_inner(db, registry, *owner, name),
        _ => Vec::new(),
    }
}

fn methods_of_class(
    db: &dyn FrameworkDatabase,
    registry: &AnalyzerRegistry,
    class: nova_types::ClassId,
) -> Vec<ResolvedMethod> {
    let class_data = db.class(class);
    let mut methods = Vec::new();

    for m in &class_data.methods {
        methods.push(ResolvedMethod {
            name: m.name.clone(),
            return_type: m.return_type.clone(),
            params: m.params.clone(),
            is_static: m.is_static,
        });
    }

    for vm in registry.virtual_members_for_class(db, class) {
        collect_virtual_methods(&mut methods, vm);
    }

    methods
}

fn constructors_of_class(
    db: &dyn FrameworkDatabase,
    registry: &AnalyzerRegistry,
    class: nova_types::ClassId,
) -> Vec<ResolvedConstructor> {
    let class_data = db.class(class);
    let mut ctors = Vec::new();

    for ctor in &class_data.constructors {
        ctors.push(ResolvedConstructor {
            params: ctor.params.clone(),
        });
    }

    for vm in registry.virtual_members_for_class(db, class) {
        collect_virtual_constructors(&mut ctors, vm);
    }

    ctors
}

fn collect_virtual_methods(out: &mut Vec<ResolvedMethod>, vm: VirtualMember) {
    match vm {
        VirtualMember::Method(m) => out.push(ResolvedMethod {
            name: m.name,
            return_type: m.return_type,
            params: m.params,
            is_static: m.is_static,
        }),
        VirtualMember::InnerClass(c) => {
            for member in c.members {
                collect_virtual_methods(out, member);
            }
        }
        _ => {}
    }
}

fn collect_virtual_constructors(out: &mut Vec<ResolvedConstructor>, vm: VirtualMember) {
    match vm {
        VirtualMember::Constructor(c) => out.push(ResolvedConstructor { params: c.params }),
        VirtualMember::InnerClass(c) => {
            for member in c.members {
                collect_virtual_constructors(out, member);
            }
        }
        _ => {}
    }
}

fn members_of_virtual_inner(
    db: &dyn FrameworkDatabase,
    registry: &AnalyzerRegistry,
    owner: nova_types::ClassId,
    inner_name: &str,
) -> Vec<MemberInfo> {
    let mut out = Vec::new();

    for vm in registry.virtual_members_for_class(db, owner) {
        if let VirtualMember::InnerClass(c) = vm {
            if c.name == inner_name {
                for member in c.members {
                    push_virtual_member_info(&mut out, member);
                }
                break;
            }
        }
    }

    out
}

fn methods_of_virtual_inner(
    db: &dyn FrameworkDatabase,
    registry: &AnalyzerRegistry,
    owner: nova_types::ClassId,
    inner_name: &str,
) -> Vec<ResolvedMethod> {
    let mut out = Vec::new();

    for vm in registry.virtual_members_for_class(db, owner) {
        if let VirtualMember::InnerClass(c) = vm {
            if c.name == inner_name {
                for member in c.members {
                    collect_virtual_methods(&mut out, member);
                }
                break;
            }
        }
    }

    out
}

fn constructors_of_virtual_inner(
    db: &dyn FrameworkDatabase,
    registry: &AnalyzerRegistry,
    owner: nova_types::ClassId,
    inner_name: &str,
) -> Vec<ResolvedConstructor> {
    let mut out = Vec::new();

    for vm in registry.virtual_members_for_class(db, owner) {
        if let VirtualMember::InnerClass(c) = vm {
            if c.name == inner_name {
                for member in c.members {
                    collect_virtual_constructors(&mut out, member);
                }
                break;
            }
        }
    }

    out
}

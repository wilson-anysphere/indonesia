//! Lombok framework analyzer.
//!
//! Lombok generates members at compile time through annotation processors.
//! Nova needs to understand common Lombok patterns without running those
//! processors by synthesising *virtual members* during resolution/completion.

use std::collections::HashSet;

use nova_framework::{
    Database, FrameworkAnalyzer, VirtualConstructor, VirtualField, VirtualInnerClass, VirtualMember,
    VirtualMethod,
};
use nova_hir::framework::{ClassData, FieldData};
use nova_types::{ClassId, Parameter, ProjectId, Type};

pub struct LombokAnalyzer;

impl LombokAnalyzer {
    pub fn new() -> Self {
        Self
    }

    fn class_uses_lombok(class: &ClassData) -> bool {
        // Heuristic: any recognized Lombok annotation.
        const LOMBOK_ANNOTATIONS: [&str; 10] = [
            "Getter",
            "Setter",
            "Data",
            "Value",
            "Builder",
            "NoArgsConstructor",
            "AllArgsConstructor",
            "RequiredArgsConstructor",
            "Slf4j",
            "Log4j2",
        ];
        LOMBOK_ANNOTATIONS
            .iter()
            .any(|name| class.has_annotation(name))
            || class
                .fields
                .iter()
                .any(|f| LOMBOK_ANNOTATIONS.iter().any(|name| f.has_annotation(name)))
    }

    fn generate_getter(field: &FieldData) -> VirtualMember {
        let is_boolean = field.ty.is_primitive_boolean();
        let (getter_name, _) = accessor_names(&field.name, is_boolean);
        VirtualMember::Method(VirtualMethod {
            name: getter_name,
            return_type: field.ty.clone(),
            params: Vec::new(),
            is_static: field.is_static,
        })
    }

    fn generate_setter(field: &FieldData) -> VirtualMember {
        let is_boolean = field.ty.is_primitive_boolean();
        let (_, property_name) = accessor_names(&field.name, is_boolean);
        VirtualMember::Method(VirtualMethod {
            name: format!("set{}", capitalize(&property_name)),
            return_type: Type::Void,
            params: vec![Parameter::new(field.name.clone(), field.ty.clone())],
            is_static: field.is_static,
        })
    }

    fn builder_type(class: ClassId, class_name: &str) -> Type {
        Type::VirtualInner {
            owner: class,
            name: format!("{class_name}Builder"),
        }
    }

    fn generate_builder(class: ClassId, class_data: &ClassData) -> Vec<VirtualMember> {
        let builder_ty = Self::builder_type(class, &class_data.name);
        let builder_name = match &builder_ty {
            Type::VirtualInner { name, .. } => name.clone(),
            _ => unreachable!(),
        };

        let mut builder_members = Vec::new();
        for field in &class_data.fields {
            if field.is_static {
                continue;
            }
            builder_members.push(VirtualMember::Method(VirtualMethod {
                name: field.name.clone(),
                return_type: builder_ty.clone(),
                params: vec![Parameter::new(field.name.clone(), field.ty.clone())],
                is_static: false,
            }));
        }
        builder_members.push(VirtualMember::Method(VirtualMethod {
            name: "build".into(),
            return_type: Type::Class(class),
            params: Vec::new(),
            is_static: false,
        }));

        vec![
            VirtualMember::Method(VirtualMethod {
                name: "builder".into(),
                return_type: builder_ty,
                params: Vec::new(),
                is_static: true,
            }),
            VirtualMember::InnerClass(VirtualInnerClass {
                name: builder_name,
                members: builder_members,
            }),
        ]
    }

    fn generate_constructors(class_data: &ClassData) -> Vec<VirtualMember> {
        let mut out = Vec::new();

        let want_no_args = class_data.has_annotation("NoArgsConstructor");

        let want_all_args =
            class_data.has_annotation("AllArgsConstructor") || class_data.has_annotation("Value");

        let want_required_args = class_data.has_annotation("RequiredArgsConstructor")
            || class_data.has_annotation("Data");

        if want_no_args {
            out.push(VirtualMember::Constructor(VirtualConstructor {
                params: vec![],
            }));
        }

        if want_all_args {
            let params = class_data
                .fields
                .iter()
                .filter(|f| !f.is_static)
                .map(|f| Parameter::new(f.name.clone(), f.ty.clone()))
                .collect();
            out.push(VirtualMember::Constructor(VirtualConstructor { params }));
        } else if want_required_args {
            let params = class_data
                .fields
                .iter()
                .filter(|f| !f.is_static && f.is_final)
                .map(|f| Parameter::new(f.name.clone(), f.ty.clone()))
                .collect();
            out.push(VirtualMember::Constructor(VirtualConstructor { params }));
        }

        out
    }

    fn generate_logger(class_data: &ClassData) -> Option<VirtualMember> {
        let logging_annotation = ["Slf4j", "Log4j2"]
            .into_iter()
            .find(|a| class_data.has_annotation(a))?;

        let ty = match logging_annotation {
            "Slf4j" => Type::Named("org.slf4j.Logger".into()),
            "Log4j2" => Type::Named("org.apache.logging.log4j.Logger".into()),
            _ => Type::Named("java.lang.Object".into()),
        };

        Some(VirtualMember::Field(VirtualField {
            name: "log".into(),
            ty,
            is_static: true,
            is_final: true,
        }))
    }
}

impl Default for LombokAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkAnalyzer for LombokAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        // Maven coordinate based detection.
        if db.has_dependency(project, "org.projectlombok", "lombok") {
            return true;
        }

        // Fallback: any lombok.* class on the classpath.
        db.has_class_on_classpath_prefix(project, "lombok.")
            || db.has_class_on_classpath_prefix(project, "lombok/")
    }

    fn virtual_members(&self, db: &dyn Database, class: ClassId) -> Vec<VirtualMember> {
        let class_data = db.class(class);

        if !Self::class_uses_lombok(class_data) {
            return Vec::new();
        }

        let mut members = Vec::new();
        let mut seen = HashSet::<MemberKey>::new();

        let class_getters = class_data.has_annotation("Getter")
            || class_data.has_annotation("Data")
            || class_data.has_annotation("Value");

        let class_setters =
            class_data.has_annotation("Setter") || class_data.has_annotation("Data");

        for field in &class_data.fields {
            let want_getter = class_getters || field.has_annotation("Getter");
            if want_getter {
                let member = Self::generate_getter(field);
                if seen.insert(MemberKey::from(&member)) {
                    members.push(member);
                }
            }

            let want_setter = class_setters || field.has_annotation("Setter");
            if want_setter && !field.is_final {
                let member = Self::generate_setter(field);
                if seen.insert(MemberKey::from(&member)) {
                    members.push(member);
                }
            }
        }

        if class_data.has_annotation("Builder") {
            for member in Self::generate_builder(class, class_data) {
                if seen.insert(MemberKey::from(&member)) {
                    members.push(member);
                }
            }
        }

        for member in Self::generate_constructors(class_data) {
            if seen.insert(MemberKey::from(&member)) {
                members.push(member);
            }
        }

        // Limited support for @ToString / @EqualsAndHashCode.
        if class_data.has_annotation("ToString")
            || class_data.has_annotation("Data")
            || class_data.has_annotation("Value")
        {
            let member = VirtualMember::Method(VirtualMethod {
                name: "toString".into(),
                return_type: Type::Named("java.lang.String".into()),
                params: Vec::new(),
                is_static: false,
            });
            if seen.insert(MemberKey::from(&member)) {
                members.push(member);
            }
        }

        if class_data.has_annotation("EqualsAndHashCode")
            || class_data.has_annotation("Data")
            || class_data.has_annotation("Value")
        {
            let equals = VirtualMember::Method(VirtualMethod {
                name: "equals".into(),
                return_type: Type::boolean(),
                params: vec![Parameter::new("o", Type::Named("java.lang.Object".into()))],
                is_static: false,
            });
            if seen.insert(MemberKey::from(&equals)) {
                members.push(equals);
            }

            let hash_code = VirtualMember::Method(VirtualMethod {
                name: "hashCode".into(),
                return_type: Type::int(),
                params: Vec::new(),
                is_static: false,
            });
            if seen.insert(MemberKey::from(&hash_code)) {
                members.push(hash_code);
            }
        }

        if let Some(member) = Self::generate_logger(class_data) {
            if seen.insert(MemberKey::from(&member)) {
                members.push(member);
            }
        }

        members
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum MemberKey {
    Field(String),
    Method {
        name: String,
        arity: usize,
        is_static: bool,
    },
    Constructor {
        arity: usize,
    },
    InnerClass(String),
}

impl From<&VirtualMember> for MemberKey {
    fn from(value: &VirtualMember) -> Self {
        match value {
            VirtualMember::Field(f) => MemberKey::Field(f.name.clone()),
            VirtualMember::Method(m) => MemberKey::Method {
                name: m.name.clone(),
                arity: m.params.len(),
                is_static: m.is_static,
            },
            VirtualMember::Constructor(c) => MemberKey::Constructor {
                arity: c.params.len(),
            },
            VirtualMember::InnerClass(c) => MemberKey::InnerClass(c.name.clone()),
        }
    }
}

fn accessor_names(field_name: &str, is_boolean: bool) -> (String, String) {
    if is_boolean {
        if let Some(rest) = field_name.strip_prefix("is") {
            if rest.chars().next().is_some_and(|c| c.is_uppercase()) {
                // `boolean isActive` => getter: `isActive()`, property: `active`.
                let prop = decapitalize(rest);
                return (field_name.to_string(), prop);
            }
        }
        (
            format!("is{}", capitalize(field_name)),
            field_name.to_string(),
        )
    } else {
        (
            format!("get{}", capitalize(field_name)),
            field_name.to_string(),
        )
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

fn decapitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use nova_framework::{AnalyzerRegistry, MemoryDatabase};
    use nova_hir::framework::{Annotation, ClassData, FieldData};
    use nova_resolve::{complete_member_names, resolve_method_call, CallKind};
    use nova_types::{PrimitiveType, Type};

    use super::LombokAnalyzer;

    #[test]
    fn completion_includes_getter() {
        let mut db = MemoryDatabase::new();
        let project = db.add_project();
        db.add_dependency(project, "org.projectlombok", "lombok");

        let class_id = db.add_class(
            project,
            ClassData {
                name: "Foo".into(),
                annotations: vec![Annotation::new("Getter")],
                fields: vec![FieldData {
                    name: "x".into(),
                    ty: Type::Primitive(PrimitiveType::Int),
                    is_static: false,
                    is_final: false,
                    annotations: vec![],
                }],
                ..ClassData::default()
            },
        );

        let mut registry = AnalyzerRegistry::new();
        registry.register(Box::new(LombokAnalyzer::new()));

        let members = complete_member_names(&db, &registry, &Type::Class(class_id));
        assert!(members.iter().any(|m| m == "getX"), "{members:?}");
    }

    #[test]
    fn resolves_builder_chain() {
        let mut db = MemoryDatabase::new();
        let project = db.add_project();
        db.add_dependency(project, "org.projectlombok", "lombok");

        let class_id = db.add_class(
            project,
            ClassData {
                name: "Foo".into(),
                annotations: vec![Annotation::new("Builder")],
                fields: vec![FieldData {
                    name: "x".into(),
                    ty: Type::int(),
                    is_static: false,
                    is_final: false,
                    annotations: vec![],
                }],
                ..ClassData::default()
            },
        );

        let mut registry = AnalyzerRegistry::new();
        registry.register(Box::new(LombokAnalyzer::new()));

        let foo_ty = Type::Class(class_id);
        let builder_ty =
            resolve_method_call(&db, &registry, &foo_ty, CallKind::Static, "builder", &[])
                .expect("builder() should resolve");

        let after_x = resolve_method_call(
            &db,
            &registry,
            &builder_ty,
            CallKind::Instance,
            "x",
            &[Type::int()],
        )
        .expect("x(int) should resolve");

        assert_eq!(after_x, builder_ty);

        let built = resolve_method_call(&db, &registry, &after_x, CallKind::Instance, "build", &[])
            .expect("build() should resolve");

        assert_eq!(built, foo_ty);
    }
}

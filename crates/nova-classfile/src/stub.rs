use crate::classfile::ClassFile;
use crate::descriptor::{parse_field_descriptor, parse_method_descriptor, FieldType, MethodDescriptor};
use crate::error::Result;
use crate::signature::{
    parse_class_signature, parse_field_signature, parse_method_signature, ClassSignature,
    FieldTypeSignature, MethodSignature,
};
use crate::Annotation;

#[derive(Debug, Clone)]
pub struct ClassStub {
    pub internal_name: String,
    pub access_flags: u16,
    pub super_class: Option<String>,
    pub interfaces: Vec<String>,
    pub signature: Option<ClassSignature>,
    pub annotations: Vec<Annotation>,
    pub inner_classes: Vec<crate::InnerClassInfo>,
    pub fields: Vec<FieldStub>,
    pub methods: Vec<MethodStub>,
}

#[derive(Debug, Clone)]
pub struct FieldStub {
    pub access_flags: u16,
    pub name: String,
    pub descriptor: String,
    pub parsed_descriptor: FieldType,
    pub signature: Option<FieldTypeSignature>,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone)]
pub struct MethodStub {
    pub access_flags: u16,
    pub name: String,
    pub descriptor: String,
    pub parsed_descriptor: MethodDescriptor,
    pub signature: Option<MethodSignature>,
    pub annotations: Vec<Annotation>,
}

impl ClassStub {
    pub fn from_classfile(class: &ClassFile) -> Result<Self> {
        let mut class_annotations = class.runtime_visible_annotations.clone();
        class_annotations.extend(class.runtime_invisible_annotations.clone());
        let signature = match class.signature.as_deref() {
            Some(sig) => Some(parse_class_signature(sig)?),
            None => None,
        };

        let fields = class
            .fields
            .iter()
            .map(|f| {
                let parsed_descriptor = parse_field_descriptor(&f.descriptor)?;
                let signature = match f.signature.as_deref() {
                    Some(sig) => Some(parse_field_signature(sig)?),
                    None => None,
                };
                Ok(FieldStub {
                    access_flags: f.access_flags,
                    name: f.name.clone(),
                    descriptor: f.descriptor.clone(),
                    parsed_descriptor,
                    signature,
                    annotations: {
                        let mut annotations = f.runtime_visible_annotations.clone();
                        annotations.extend(f.runtime_invisible_annotations.clone());
                        annotations
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let methods = class
            .methods
            .iter()
            .map(|m| {
                let parsed_descriptor = parse_method_descriptor(&m.descriptor)?;
                let signature = match m.signature.as_deref() {
                    Some(sig) => Some(parse_method_signature(sig)?),
                    None => None,
                };
                Ok(MethodStub {
                    access_flags: m.access_flags,
                    name: m.name.clone(),
                    descriptor: m.descriptor.clone(),
                    parsed_descriptor,
                    signature,
                    annotations: {
                        let mut annotations = m.runtime_visible_annotations.clone();
                        annotations.extend(m.runtime_invisible_annotations.clone());
                        annotations
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(ClassStub {
            internal_name: class.this_class.clone(),
            access_flags: class.access_flags,
            super_class: class.super_class.clone(),
            interfaces: class.interfaces.clone(),
            signature,
            annotations: class_annotations,
            inner_classes: class.inner_classes.clone(),
            fields,
            methods,
        })
    }
}

impl ClassFile {
    pub fn stub(&self) -> Result<ClassStub> {
        ClassStub::from_classfile(self)
    }
}

#![forbid(unsafe_code)]

mod annotation;
mod classfile;
mod constant_pool;
mod descriptor;
mod error;
mod module_info;
mod reader;
mod signature;
mod stub;

pub use crate::annotation::{Annotation, ConstValue, ElementValue};
pub use crate::classfile::{ClassFile, ClassMember, InnerClassInfo};
pub use crate::descriptor::{parse_field_descriptor, parse_method_descriptor};
pub use crate::descriptor::{BaseType, FieldType, MethodDescriptor, ReturnType};
pub use crate::error::{Error, Result};
pub use crate::module_info::parse_module_info_class;
pub use crate::signature::{
    parse_class_signature, parse_field_signature, parse_method_signature, ClassSignature,
    ClassTypeSignature, FieldTypeSignature, MethodSignature, TypeArgument, TypeParameter,
    TypeSignature,
};
pub use crate::stub::{ClassStub, FieldStub, MethodStub};

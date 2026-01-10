use crate::annotation::Annotation;
use crate::constant_pool::ConstantPool;
use crate::error::{Error, Result};
use crate::reader::Reader;

#[derive(Debug, Clone)]
pub struct ClassFile {
    pub minor_version: u16,
    pub major_version: u16,
    pub access_flags: u16,
    pub this_class: String,
    pub super_class: Option<String>,
    pub interfaces: Vec<String>,
    pub fields: Vec<ClassMember>,
    pub methods: Vec<ClassMember>,
    pub signature: Option<String>,
    pub runtime_visible_annotations: Vec<Annotation>,
    pub runtime_invisible_annotations: Vec<Annotation>,
    pub inner_classes: Vec<InnerClassInfo>,
}

#[derive(Debug, Clone)]
pub struct ClassMember {
    pub access_flags: u16,
    pub name: String,
    pub descriptor: String,
    pub signature: Option<String>,
    pub runtime_visible_annotations: Vec<Annotation>,
    pub runtime_invisible_annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InnerClassInfo {
    pub inner_class: String,
    pub outer_class: Option<String>,
    pub inner_name: Option<String>,
    pub access_flags: u16,
}

impl ClassFile {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let magic = reader.read_u4()?;
        if magic != 0xCAFEBABE {
            return Err(Error::InvalidMagic(magic));
        }

        let minor_version = reader.read_u2()?;
        let major_version = reader.read_u2()?;
        let cp = ConstantPool::parse(&mut reader)?;

        let access_flags = reader.read_u2()?;
        let this_class = cp.get_class_name(reader.read_u2()?)?;
        let super_class_idx = reader.read_u2()?;
        let super_class = if super_class_idx == 0 {
            None
        } else {
            Some(cp.get_class_name(super_class_idx)?)
        };

        let interfaces_count = reader.read_u2()? as usize;
        let mut interfaces = Vec::with_capacity(interfaces_count);
        for _ in 0..interfaces_count {
            interfaces.push(cp.get_class_name(reader.read_u2()?)?);
        }

        let fields_count = reader.read_u2()? as usize;
        let mut fields = Vec::with_capacity(fields_count);
        for _ in 0..fields_count {
            fields.push(parse_member(&mut reader, &cp)?);
        }

        let methods_count = reader.read_u2()? as usize;
        let mut methods = Vec::with_capacity(methods_count);
        for _ in 0..methods_count {
            methods.push(parse_member(&mut reader, &cp)?);
        }

        let class_attrs = parse_attributes(&mut reader, &cp, AttributeTarget::Class)?;

        reader.ensure_empty()?;

        Ok(Self {
            minor_version,
            major_version,
            access_flags,
            this_class,
            super_class,
            interfaces,
            fields,
            methods,
            signature: class_attrs.signature,
            runtime_visible_annotations: class_attrs.runtime_visible_annotations,
            runtime_invisible_annotations: class_attrs.runtime_invisible_annotations,
            inner_classes: class_attrs.inner_classes,
        })
    }
}

fn parse_member(reader: &mut Reader<'_>, cp: &ConstantPool) -> Result<ClassMember> {
    let access_flags = reader.read_u2()?;
    let name = cp.get_utf8(reader.read_u2()?)?.to_string();
    let descriptor = cp.get_utf8(reader.read_u2()?)?.to_string();

    let attrs = parse_attributes(reader, cp, AttributeTarget::Member)?;
    Ok(ClassMember {
        access_flags,
        name,
        descriptor,
        signature: attrs.signature,
        runtime_visible_annotations: attrs.runtime_visible_annotations,
        runtime_invisible_annotations: attrs.runtime_invisible_annotations,
    })
}

#[derive(Default)]
struct ParsedAttributes {
    signature: Option<String>,
    runtime_visible_annotations: Vec<Annotation>,
    runtime_invisible_annotations: Vec<Annotation>,
    inner_classes: Vec<InnerClassInfo>,
}

enum AttributeTarget {
    Class,
    Member,
}

fn parse_attributes(
    reader: &mut Reader<'_>,
    cp: &ConstantPool,
    target: AttributeTarget,
) -> Result<ParsedAttributes> {
    let attributes_count = reader.read_u2()? as usize;
    let mut parsed = ParsedAttributes::default();
    for _ in 0..attributes_count {
        let name_index = reader.read_u2()?;
        let length = reader.read_u4()? as usize;
        let info = reader.read_bytes(length)?;
        let name = cp.get_utf8(name_index)?;

        let mut sub = Reader::new(info);
        match name {
            "Signature" => {
                let sig_index = sub.read_u2()?;
                parsed.signature = Some(cp.get_utf8(sig_index)?.to_string());
                sub.ensure_empty()?;
            }
            "RuntimeVisibleAnnotations" => {
                let num = sub.read_u2()? as usize;
                let mut anns = Vec::with_capacity(num);
                for _ in 0..num {
                    anns.push(Annotation::parse(&mut sub, cp)?);
                }
                parsed.runtime_visible_annotations.extend(anns);
                sub.ensure_empty()?;
            }
            "RuntimeInvisibleAnnotations" => {
                let num = sub.read_u2()? as usize;
                let mut anns = Vec::with_capacity(num);
                for _ in 0..num {
                    anns.push(Annotation::parse(&mut sub, cp)?);
                }
                parsed.runtime_invisible_annotations.extend(anns);
                sub.ensure_empty()?;
            }
            "InnerClasses" if matches!(target, AttributeTarget::Class) => {
                let num = sub.read_u2()? as usize;
                let mut inners = Vec::with_capacity(num);
                for _ in 0..num {
                    let inner_class_info_index = sub.read_u2()?;
                    let outer_class_info_index = sub.read_u2()?;
                    let inner_name_index = sub.read_u2()?;
                    let inner_access_flags = sub.read_u2()?;

                    let inner_class = cp.get_class_name(inner_class_info_index)?;
                    let outer_class = if outer_class_info_index == 0 {
                        None
                    } else {
                        Some(cp.get_class_name(outer_class_info_index)?)
                    };
                    let inner_name = if inner_name_index == 0 {
                        None
                    } else {
                        Some(cp.get_utf8(inner_name_index)?.to_string())
                    };

                    inners.push(InnerClassInfo {
                        inner_class,
                        outer_class,
                        inner_name,
                        access_flags: inner_access_flags,
                    });
                }
                parsed.inner_classes.extend(inners);
                sub.ensure_empty()?;
            }
            _ => {
                // Unknown attribute: intentionally skipped.
            }
        }
    }

    Ok(parsed)
}

use crate::constant_pool::{ConstantPool, CpInfo};
use crate::error::{Error, Result};
use crate::reader::Reader;

#[derive(Debug, Clone, PartialEq)]
pub struct Annotation {
    pub type_descriptor: String,
    pub type_internal_name: Option<String>,
    pub elements: Vec<(String, ElementValue)>,
}

impl Annotation {
    pub(crate) fn parse(reader: &mut Reader<'_>, cp: &ConstantPool) -> Result<Self> {
        let type_index = reader.read_u2()?;
        let type_descriptor = cp.get_utf8(type_index)?.to_string();
        let type_internal_name = descriptor_to_internal_name(&type_descriptor);

        let num_element_value_pairs = reader.read_u2()? as usize;
        let mut elements = Vec::with_capacity(num_element_value_pairs);
        for _ in 0..num_element_value_pairs {
            let element_name_index = reader.read_u2()?;
            let name = cp.get_utf8(element_name_index)?.to_string();
            let value = ElementValue::parse(reader, cp)?;
            elements.push((name, value));
        }

        Ok(Self {
            type_descriptor,
            type_internal_name,
            elements,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElementValue {
    Const(ConstValue),
    Enum {
        type_descriptor: String,
        const_name: String,
    },
    Class(String),
    Annotation(Box<Annotation>),
    Array(Vec<ElementValue>),
}

impl ElementValue {
    fn parse(reader: &mut Reader<'_>, cp: &ConstantPool) -> Result<Self> {
        let tag = reader.read_u1()? as char;
        match tag {
            'B' | 'C' | 'I' | 'S' | 'Z' => {
                let idx = reader.read_u2()?;
                let value = match cp.get(idx)? {
                    CpInfo::Integer(v) => *v,
                    other => {
                        return Err(Error::ConstantPoolTypeMismatch {
                            index: idx,
                            expected: "Integer",
                            found: other.kind(),
                        })
                    }
                };

                let cv = match tag {
                    'B' => ConstValue::Byte(value as i8),
                    'C' => ConstValue::Char(char::from_u32(value as u32).ok_or_else(|| {
                        Error::MalformedAttribute("RuntimeVisibleAnnotations")
                    })?),
                    'I' => ConstValue::Int(value),
                    'S' => ConstValue::Short(value as i16),
                    'Z' => ConstValue::Boolean(value != 0),
                    _ => unreachable!(),
                };
                Ok(ElementValue::Const(cv))
            }
            'D' => {
                let idx = reader.read_u2()?;
                let value = match cp.get(idx)? {
                    CpInfo::Double(v) => *v,
                    other => {
                        return Err(Error::ConstantPoolTypeMismatch {
                            index: idx,
                            expected: "Double",
                            found: other.kind(),
                        })
                    }
                };
                Ok(ElementValue::Const(ConstValue::Double(value)))
            }
            'F' => {
                let idx = reader.read_u2()?;
                let value = match cp.get(idx)? {
                    CpInfo::Float(v) => *v,
                    other => {
                        return Err(Error::ConstantPoolTypeMismatch {
                            index: idx,
                            expected: "Float",
                            found: other.kind(),
                        })
                    }
                };
                Ok(ElementValue::Const(ConstValue::Float(value)))
            }
            'J' => {
                let idx = reader.read_u2()?;
                let value = match cp.get(idx)? {
                    CpInfo::Long(v) => *v,
                    other => {
                        return Err(Error::ConstantPoolTypeMismatch {
                            index: idx,
                            expected: "Long",
                            found: other.kind(),
                        })
                    }
                };
                Ok(ElementValue::Const(ConstValue::Long(value)))
            }
            's' => {
                let idx = reader.read_u2()?;
                let value = cp.get_string_constant(idx)?;
                Ok(ElementValue::Const(ConstValue::String(value)))
            }
            'e' => {
                let type_name_index = reader.read_u2()?;
                let const_name_index = reader.read_u2()?;
                Ok(ElementValue::Enum {
                    type_descriptor: cp.get_utf8(type_name_index)?.to_string(),
                    const_name: cp.get_utf8(const_name_index)?.to_string(),
                })
            }
            'c' => {
                let class_info_index = reader.read_u2()?;
                Ok(ElementValue::Class(
                    cp.get_utf8(class_info_index)?.to_string(),
                ))
            }
            '@' => Ok(ElementValue::Annotation(Box::new(Annotation::parse(
                reader, cp,
            )?))),
            '[' => {
                let num_values = reader.read_u2()? as usize;
                let mut values = Vec::with_capacity(num_values);
                for _ in 0..num_values {
                    values.push(ElementValue::parse(reader, cp)?);
                }
                Ok(ElementValue::Array(values))
            }
            _ => Err(Error::MalformedAttribute("RuntimeVisibleAnnotations")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConstValue {
    Byte(i8),
    Char(char),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Boolean(bool),
    String(String),
}

pub fn descriptor_to_internal_name(desc: &str) -> Option<String> {
    desc.strip_prefix('L')
        .and_then(|rest| rest.strip_suffix(';'))
        .map(|name| name.to_string())
}


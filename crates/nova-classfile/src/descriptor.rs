use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseType {
    Byte,
    Char,
    Double,
    Float,
    Int,
    Long,
    Short,
    Boolean,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    Base(BaseType),
    Object(String),
    Array(Box<FieldType>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnType {
    Void,
    Type(FieldType),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDescriptor {
    pub params: Vec<FieldType>,
    pub return_type: ReturnType,
}

pub fn parse_field_descriptor(desc: &str) -> Result<FieldType> {
    let (ty, rest) = parse_field_type(desc)?;
    if !rest.is_empty() {
        return Err(Error::InvalidDescriptor(desc.to_string()));
    }
    Ok(ty)
}

pub fn parse_method_descriptor(desc: &str) -> Result<MethodDescriptor> {
    let mut chars = desc.chars();
    if chars.next() != Some('(') {
        return Err(Error::InvalidDescriptor(desc.to_string()));
    }

    let mut idx = 1usize;
    let mut params = Vec::new();
    while idx < desc.len() {
        let b = desc.as_bytes()[idx] as char;
        if b == ')' {
            idx += 1;
            break;
        }
        let (param, rest) = parse_field_type(&desc[idx..])?;
        idx = desc.len() - rest.len();
        params.push(param);
    }

    if idx > desc.len() {
        return Err(Error::InvalidDescriptor(desc.to_string()));
    }
    let return_part = &desc[idx..];
    if return_part.is_empty() {
        return Err(Error::InvalidDescriptor(desc.to_string()));
    }

    let (return_type, rest) = if let Some(rest) = return_part.strip_prefix('V') {
        (ReturnType::Void, rest)
    } else {
        let (ty, rest) = parse_field_type(return_part)?;
        (ReturnType::Type(ty), rest)
    };

    if !rest.is_empty() {
        return Err(Error::InvalidDescriptor(desc.to_string()));
    }

    Ok(MethodDescriptor { params, return_type })
}

fn parse_field_type(input: &str) -> Result<(FieldType, &str)> {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return Err(Error::InvalidDescriptor(input.to_string()));
    }
    match bytes[0] as char {
        'B' => Ok((FieldType::Base(BaseType::Byte), &input[1..])),
        'C' => Ok((FieldType::Base(BaseType::Char), &input[1..])),
        'D' => Ok((FieldType::Base(BaseType::Double), &input[1..])),
        'F' => Ok((FieldType::Base(BaseType::Float), &input[1..])),
        'I' => Ok((FieldType::Base(BaseType::Int), &input[1..])),
        'J' => Ok((FieldType::Base(BaseType::Long), &input[1..])),
        'S' => Ok((FieldType::Base(BaseType::Short), &input[1..])),
        'Z' => Ok((FieldType::Base(BaseType::Boolean), &input[1..])),
        'L' => {
            if let Some(end) = input.find(';') {
                let name = &input[1..end];
                Ok((FieldType::Object(name.to_string()), &input[end + 1..]))
            } else {
                Err(Error::InvalidDescriptor(input.to_string()))
            }
        }
        '[' => {
            let (component, rest) = parse_field_type(&input[1..])?;
            Ok((FieldType::Array(Box::new(component)), rest))
        }
        _ => Err(Error::InvalidDescriptor(input.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_field_descriptor_primitives_and_arrays() {
        assert_eq!(parse_field_descriptor("I").unwrap(), FieldType::Base(BaseType::Int));
        assert_eq!(
            parse_field_descriptor("[[Ljava/lang/String;").unwrap(),
            FieldType::Array(Box::new(FieldType::Array(Box::new(FieldType::Object(
                "java/lang/String".to_string()
            )))))
        );
    }

    #[test]
    fn parse_method_descriptor_basic() {
        let desc = parse_method_descriptor("(ILjava/lang/String;)[I").unwrap();
        assert_eq!(
            desc.params,
            vec![
                FieldType::Base(BaseType::Int),
                FieldType::Object("java/lang/String".to_string())
            ]
        );
        assert_eq!(
            desc.return_type,
            ReturnType::Type(FieldType::Array(Box::new(FieldType::Base(BaseType::Int))))
        );
    }
}

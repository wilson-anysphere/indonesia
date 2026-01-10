use crate::descriptor::BaseType;
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParameter {
    pub name: String,
    pub class_bound: Option<FieldTypeSignature>,
    pub interface_bounds: Vec<FieldTypeSignature>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassSignature {
    pub type_parameters: Vec<TypeParameter>,
    pub super_class: ClassTypeSignature,
    pub interfaces: Vec<ClassTypeSignature>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodSignature {
    pub type_parameters: Vec<TypeParameter>,
    pub parameters: Vec<TypeSignature>,
    pub return_type: Option<TypeSignature>, // None => void
    pub throws: Vec<TypeSignature>,         // class or type variable
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassTypeSignature {
    pub package: Vec<String>,
    pub segments: Vec<ClassTypeSegment>,
}

impl ClassTypeSignature {
    pub fn internal_name(&self) -> String {
        let mut out = String::new();
        if !self.package.is_empty() {
            out.push_str(&self.package.join("/"));
            out.push('/');
        }
        for (idx, seg) in self.segments.iter().enumerate() {
            if idx > 0 {
                out.push('$');
            }
            out.push_str(&seg.name);
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassTypeSegment {
    pub name: String,
    pub type_arguments: Vec<TypeArgument>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeArgument {
    Any,
    Exact(Box<FieldTypeSignature>),
    Extends(Box<FieldTypeSignature>),
    Super(Box<FieldTypeSignature>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeSignature {
    Base(BaseType),
    Array(Box<TypeSignature>),
    Class(ClassTypeSignature),
    TypeVariable(String),
}

pub type FieldTypeSignature = TypeSignature;

pub fn parse_class_signature(sig: &str) -> Result<ClassSignature> {
    let mut p = Parser::new(sig);
    let type_parameters = p.parse_type_parameters_if_present()?;
    let super_class = p.parse_class_type_signature()?;
    let mut interfaces = Vec::new();
    while !p.is_eof() {
        interfaces.push(p.parse_class_type_signature()?);
    }
    Ok(ClassSignature {
        type_parameters,
        super_class,
        interfaces,
    })
}

pub fn parse_method_signature(sig: &str) -> Result<MethodSignature> {
    let mut p = Parser::new(sig);
    let type_parameters = p.parse_type_parameters_if_present()?;
    p.expect('(')?;
    let mut parameters = Vec::new();
    while !p.is_eof() && p.peek() != Some(')') {
        parameters.push(p.parse_type_signature()?);
    }
    p.expect(')')?;
    let return_type = if p.peek() == Some('V') {
        p.bump();
        None
    } else {
        Some(p.parse_type_signature()?)
    };

    let mut throws = Vec::new();
    while p.peek() == Some('^') {
        p.bump();
        let ty = match p.peek() {
            Some('T') => p.parse_type_variable_signature()?,
            Some('L') => TypeSignature::Class(p.parse_class_type_signature()?),
            _ => return Err(Error::InvalidSignature(sig.to_string())),
        };
        throws.push(ty);
    }

    if !p.is_eof() {
        return Err(Error::InvalidSignature(sig.to_string()));
    }

    Ok(MethodSignature {
        type_parameters,
        parameters,
        return_type,
        throws,
    })
}

pub fn parse_field_signature(sig: &str) -> Result<FieldTypeSignature> {
    let mut p = Parser::new(sig);
    let ty = p.parse_field_type_signature()?;
    if !p.is_eof() {
        return Err(Error::InvalidSignature(sig.to_string()));
    }
    Ok(ty)
}

struct Parser<'a> {
    sig: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(sig: &'a str) -> Self {
        Self {
            sig,
            bytes: sig.as_bytes(),
            pos: 0,
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn peek(&self) -> Option<char> {
        self.bytes.get(self.pos).copied().map(|b| b as char)
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += 1;
        Some(ch)
    }

    fn expect(&mut self, ch: char) -> Result<()> {
        match self.bump() {
            Some(c) if c == ch => Ok(()),
            _ => Err(Error::InvalidSignature(self.sig.to_string())),
        }
    }

    fn parse_type_parameters_if_present(&mut self) -> Result<Vec<TypeParameter>> {
        if self.peek() != Some('<') {
            return Ok(Vec::new());
        }
        self.expect('<')?;
        let mut out = Vec::new();
        while self.peek() != Some('>') {
            if self.is_eof() {
                return Err(Error::InvalidSignature(self.sig.to_string()));
            }
            out.push(self.parse_type_parameter()?);
        }
        self.expect('>')?;
        Ok(out)
    }

    fn parse_type_parameter(&mut self) -> Result<TypeParameter> {
        let name = self.parse_identifier_until(':')?;
        self.expect(':')?;

        let class_bound = match self.peek() {
            Some(':') => None,
            Some('L') | Some('T') | Some('[') => Some(self.parse_field_type_signature()?),
            _ => return Err(Error::InvalidSignature(self.sig.to_string())),
        };

        let mut interface_bounds = Vec::new();
        while self.peek() == Some(':') {
            self.bump();
            interface_bounds.push(self.parse_field_type_signature()?);
        }

        Ok(TypeParameter {
            name,
            class_bound,
            interface_bounds,
        })
    }

    fn parse_type_signature(&mut self) -> Result<TypeSignature> {
        match self.peek() {
            Some('B') => {
                self.bump();
                Ok(TypeSignature::Base(BaseType::Byte))
            }
            Some('C') => {
                self.bump();
                Ok(TypeSignature::Base(BaseType::Char))
            }
            Some('D') => {
                self.bump();
                Ok(TypeSignature::Base(BaseType::Double))
            }
            Some('F') => {
                self.bump();
                Ok(TypeSignature::Base(BaseType::Float))
            }
            Some('I') => {
                self.bump();
                Ok(TypeSignature::Base(BaseType::Int))
            }
            Some('J') => {
                self.bump();
                Ok(TypeSignature::Base(BaseType::Long))
            }
            Some('S') => {
                self.bump();
                Ok(TypeSignature::Base(BaseType::Short))
            }
            Some('Z') => {
                self.bump();
                Ok(TypeSignature::Base(BaseType::Boolean))
            }
            Some('L') | Some('T') | Some('[') => self.parse_field_type_signature(),
            _ => Err(Error::InvalidSignature(self.sig.to_string())),
        }
    }

    fn parse_field_type_signature(&mut self) -> Result<FieldTypeSignature> {
        match self.peek() {
            Some('L') => Ok(TypeSignature::Class(self.parse_class_type_signature()?)),
            Some('T') => self.parse_type_variable_signature(),
            Some('[') => {
                self.bump();
                let component = self.parse_type_signature()?;
                Ok(TypeSignature::Array(Box::new(component)))
            }
            _ => Err(Error::InvalidSignature(self.sig.to_string())),
        }
    }

    fn parse_type_variable_signature(&mut self) -> Result<TypeSignature> {
        self.expect('T')?;
        let name = self.parse_identifier_until(';')?;
        self.expect(';')?;
        Ok(TypeSignature::TypeVariable(name))
    }

    fn parse_class_type_signature(&mut self) -> Result<ClassTypeSignature> {
        self.expect('L')?;

        let mut package = Vec::new();
        // The class name begins with an identifier, which may be a package segment if followed by '/'.
        let mut first = self.parse_identifier()?;
        while self.peek() == Some('/') {
            self.bump();
            package.push(first);
            first = self.parse_identifier()?;
        }

        let mut segments = Vec::new();
        let type_arguments = self.parse_type_arguments_if_present()?;
        segments.push(ClassTypeSegment {
            name: first,
            type_arguments,
        });

        while self.peek() == Some('.') {
            self.bump();
            let name = self.parse_identifier()?;
            let type_arguments = self.parse_type_arguments_if_present()?;
            segments.push(ClassTypeSegment { name, type_arguments });
        }

        self.expect(';')?;
        Ok(ClassTypeSignature { package, segments })
    }

    fn parse_type_arguments_if_present(&mut self) -> Result<Vec<TypeArgument>> {
        if self.peek() != Some('<') {
            return Ok(Vec::new());
        }
        self.expect('<')?;
        let mut args = Vec::new();
        while self.peek() != Some('>') {
            if self.is_eof() {
                return Err(Error::InvalidSignature(self.sig.to_string()));
            }
            args.push(self.parse_type_argument()?);
        }
        self.expect('>')?;
        Ok(args)
    }

    fn parse_type_argument(&mut self) -> Result<TypeArgument> {
        match self.peek() {
            Some('*') => {
                self.bump();
                Ok(TypeArgument::Any)
            }
            Some('+') => {
                self.bump();
                Ok(TypeArgument::Extends(Box::new(self.parse_field_type_signature()?)))
            }
            Some('-') => {
                self.bump();
                Ok(TypeArgument::Super(Box::new(self.parse_field_type_signature()?)))
            }
            Some('L') | Some('T') | Some('[') => {
                Ok(TypeArgument::Exact(Box::new(self.parse_field_type_signature()?)))
            }
            _ => Err(Error::InvalidSignature(self.sig.to_string())),
        }
    }

    fn parse_identifier_until(&mut self, delim: char) -> Result<String> {
        let start = self.pos;
        while !self.is_eof() && self.peek() != Some(delim) {
            let ch = self.peek().unwrap();
            if is_forbidden_in_identifier(ch) {
                return Err(Error::InvalidSignature(self.sig.to_string()));
            }
            self.pos += 1;
        }
        if self.is_eof() {
            return Err(Error::InvalidSignature(self.sig.to_string()));
        }
        if start == self.pos {
            return Err(Error::InvalidSignature(self.sig.to_string()));
        }
        Ok(self.sig[start..self.pos].to_string())
    }

    fn parse_identifier(&mut self) -> Result<String> {
        let start = self.pos;
        while !self.is_eof() {
            let ch = self.peek().unwrap();
            if ch == '/' || ch == ';' || ch == '<' || ch == '>' || ch == '.' || ch == ':' {
                break;
            }
            if is_forbidden_in_identifier(ch) {
                return Err(Error::InvalidSignature(self.sig.to_string()));
            }
            self.pos += 1;
        }

        if start == self.pos {
            return Err(Error::InvalidSignature(self.sig.to_string()));
        }

        Ok(self.sig[start..self.pos].to_string())
    }
}

fn is_forbidden_in_identifier(ch: char) -> bool {
    matches!(ch, '[' | '^' | '(' | ')' | '*' | '+' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_class_signature_with_bound() {
        let sig = parse_class_signature("<T:Ljava/lang/Number;>Ljava/lang/Object;").unwrap();
        assert_eq!(sig.type_parameters.len(), 1);
        assert_eq!(sig.type_parameters[0].name, "T");
        assert_eq!(
            sig.type_parameters[0].class_bound,
            Some(TypeSignature::Class(ClassTypeSignature {
                package: vec!["java".into(), "lang".into()],
                segments: vec![ClassTypeSegment {
                    name: "Number".into(),
                    type_arguments: vec![]
                }]
            }))
        );
        assert_eq!(sig.super_class.internal_name(), "java/lang/Object");
    }

    #[test]
    fn parse_method_signature_with_type_param() {
        let sig = parse_method_signature("<U:Ljava/lang/Object;>(TU;)TU;").unwrap();
        assert_eq!(sig.type_parameters.len(), 1);
        assert_eq!(sig.type_parameters[0].name, "U");
        assert_eq!(sig.parameters.len(), 1);
        assert_eq!(
            sig.parameters[0],
            TypeSignature::TypeVariable("U".to_string())
        );
        assert_eq!(
            sig.return_type,
            Some(TypeSignature::TypeVariable("U".to_string()))
        );
    }
}

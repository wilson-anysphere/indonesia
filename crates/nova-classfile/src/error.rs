use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    UnexpectedEof,
    InvalidMagic(u32),
    InvalidConstantPoolIndex(u16),
    InvalidConstantPoolTag(u8),
    ConstantPoolTypeMismatch {
        index: u16,
        expected: &'static str,
        found: &'static str,
    },
    InvalidModifiedUtf8,
    InvalidDescriptor(String),
    InvalidSignature(String),
    MalformedAttribute(&'static str),
    Other(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnexpectedEof => write!(f, "unexpected end of input"),
            Error::InvalidMagic(magic) => write!(f, "invalid classfile magic: 0x{magic:08x}"),
            Error::InvalidConstantPoolIndex(index) => {
                write!(f, "invalid constant pool index: {index}")
            }
            Error::InvalidConstantPoolTag(tag) => write!(f, "invalid constant pool tag: {tag}"),
            Error::ConstantPoolTypeMismatch {
                index,
                expected,
                found,
            } => write!(
                f,
                "constant pool type mismatch at index {index}: expected {expected}, found {found}"
            ),
            Error::InvalidModifiedUtf8 => write!(f, "invalid modified UTF-8 constant"),
            Error::InvalidDescriptor(desc) => write!(f, "invalid descriptor: {desc}"),
            Error::InvalidSignature(sig) => write!(f, "invalid signature: {sig}"),
            Error::MalformedAttribute(name) => write!(f, "malformed {name} attribute"),
            Error::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

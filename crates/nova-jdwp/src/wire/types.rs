use std::fmt;

use thiserror::Error;

pub type ObjectId = u64;
pub type ThreadId = u64;
pub type ReferenceTypeId = u64;
pub type MethodId = u64;
pub type FieldId = u64;
pub type FrameId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JdwpIdSizes {
    pub field_id: usize,
    pub method_id: usize,
    pub object_id: usize,
    pub reference_type_id: usize,
    pub frame_id: usize,
}

impl Default for JdwpIdSizes {
    fn default() -> Self {
        // Most modern JVMs use 8 byte IDs.
        Self {
            field_id: 8,
            method_id: 8,
            object_id: 8,
            reference_type_id: 8,
            frame_id: 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub type_tag: u8,
    pub class_id: ReferenceTypeId,
    pub method_id: MethodId,
    pub index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassInfo {
    pub ref_type_tag: u8,
    pub type_id: ReferenceTypeId,
    pub signature: String,
    pub status: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodInfo {
    pub method_id: MethodId,
    pub name: String,
    pub signature: String,
    pub mod_bits: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    pub frame_id: FrameId,
    pub location: Location,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineTableEntry {
    pub code_index: u64,
    pub line: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineTable {
    pub start: u64,
    pub end: u64,
    pub lines: Vec<LineTableEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableInfo {
    pub code_index: u64,
    pub name: String,
    pub signature: String,
    pub length: u32,
    pub slot: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldInfo {
    pub field_id: FieldId,
    pub name: String,
    pub signature: String,
    pub mod_bits: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JdwpValue {
    Boolean(bool),
    Byte(i8),
    Char(u16),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Object { tag: u8, id: ObjectId },
    Void,
}

impl fmt::Display for JdwpValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JdwpValue::Boolean(v) => write!(f, "{v}"),
            JdwpValue::Byte(v) => write!(f, "{v}"),
            JdwpValue::Char(v) => write!(f, "{v}"),
            JdwpValue::Short(v) => write!(f, "{v}"),
            JdwpValue::Int(v) => write!(f, "{v}"),
            JdwpValue::Long(v) => write!(f, "{v}"),
            JdwpValue::Float(v) => write!(f, "{v}"),
            JdwpValue::Double(v) => write!(f, "{v}"),
            JdwpValue::Object { tag, id } => write!(f, "{:02x}:{tag}", id),
            JdwpValue::Void => write!(f, "void"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JdwpEvent {
    VmStart {
        request_id: i32,
        thread: ThreadId,
    },
    Breakpoint {
        request_id: i32,
        thread: ThreadId,
        location: Location,
    },
    SingleStep {
        request_id: i32,
        thread: ThreadId,
        location: Location,
    },
    Exception {
        request_id: i32,
        thread: ThreadId,
        location: Location,
        exception: ObjectId,
        catch_location: Option<Location>,
    },
    ClassPrepare {
        request_id: i32,
        thread: ThreadId,
        ref_type_tag: u8,
        type_id: ReferenceTypeId,
        signature: String,
        status: u32,
    },
    VmDeath,
}

#[derive(Debug, Error)]
pub enum JdwpError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("VM returned error code {0}")]
    VmError(u16),

    #[error("request timed out")]
    Timeout,

    #[error("request cancelled")]
    Cancelled,

    #[error("connection closed")]
    ConnectionClosed,
}

pub type Result<T> = std::result::Result<T, JdwpError>;

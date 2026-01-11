//! Wire-level JDWP implementation.
//!
//! This module speaks the actual JDWP binary protocol over TCP. It is designed
//! to be async-capable (`tokio`) and cancellation-aware.

mod client;
mod codec;
pub mod inspect;
pub mod types;

pub use client::{EventModifier, JdwpClient, JdwpClientConfig};
pub use types::{
    ClassInfo, FieldId, FieldInfo, FrameId, FrameInfo, JdwpError, JdwpEvent, JdwpIdSizes,
    JdwpValue, LineTable, LineTableEntry, Location, MethodId, MethodInfo, ObjectId,
    ReferenceTypeId, ThreadId, VariableInfo,
};

#[cfg(feature = "wire-test-support")]
pub mod mock;

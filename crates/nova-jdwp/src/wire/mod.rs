//! Wire-level JDWP implementation.
//!
//! This module speaks the actual JDWP binary protocol over TCP. It is designed
//! to be async-capable (`tokio`) and cancellation-aware.

mod client;
mod codec;
pub mod inspect;
mod poison;
pub mod types;

pub use client::{EventModifier, JdwpClient, JdwpClientConfig};
pub use types::{
    ClassInfo, FieldId, FieldInfo, FrameId, FrameInfo, JdwpError, JdwpEvent, JdwpIdSizes,
    JdwpValue, LineTable, LineTableEntry, Location, MethodId, MethodInfo, ObjectId,
    ReferenceTypeId, ThreadId, VariableInfo, VmClassPaths,
};

// The wire-protocol mock server is only needed for tests and downstream integration suites.
// Compile it for nova-jdwp's own unit tests unconditionally (via `cfg(test)`), while keeping
// it behind the `wire-test-support` feature for normal builds and for downstream crates.
#[cfg(any(test, feature = "wire-test-support"))]
pub mod mock;

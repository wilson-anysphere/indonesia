//! Java Debug Wire Protocol (JDWP) client façade for Nova.
//!
//! `nova-dap` consumes this crate to speak to the JVM and to power debugger UX
//! features (return values, stable object IDs, rich previews, and object
//! pinning).
//!
//! The network client (`TcpJdwpClient`) currently implements only a small
//! subset of JDWP (handshake, thread enumeration, stack frames, basic stepping
//! and breakpoints). Value inspection APIs intentionally return
//! [`JdwpError::NotImplemented`] until the underlying wire protocol support is
//! fleshed out.

mod mock;
mod tcp;

use std::io;

use thiserror::Error;

pub use mock::{MockJdwpClient, MockObject};
pub use tcp::TcpJdwpClient;

pub type ThreadId = u64;
pub type FrameId = u64;
pub type ObjectId = u64;

#[derive(Clone, Debug, PartialEq)]
pub enum JdwpValue {
    Null,
    Void,
    Boolean(bool),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Char(char),
    Object(ObjectRef),
}

impl JdwpValue {
    pub fn object_id(&self) -> Option<ObjectId> {
        match self {
            Self::Object(obj) => Some(obj.id),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectRef {
    pub id: ObjectId,
    pub runtime_type: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectPreview {
    pub runtime_type: String,
    pub kind: ObjectKindPreview,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObjectKindPreview {
    Plain,
    String { value: String },
    PrimitiveWrapper { value: Box<JdwpValue> },
    Array {
        element_type: String,
        length: usize,
        sample: Vec<JdwpValue>,
    },
    List { size: usize, sample: Vec<JdwpValue> },
    Set { size: usize, sample: Vec<JdwpValue> },
    Map {
        size: usize,
        sample: Vec<(JdwpValue, JdwpValue)>,
    },
    Optional { value: Option<Box<JdwpValue>> },
    Stream { size: Option<usize> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct JdwpVariable {
    pub name: String,
    pub value: JdwpValue,
    /// Static type inferred from Nova (optional). This can be more useful to
    /// show as the DAP `type` than the runtime type when debugging interfaces,
    /// generics, etc.
    pub static_type: Option<String>,
    /// Best-effort expression to re-evaluate the value in the current frame.
    pub evaluate_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ThreadInfo {
    pub id: ThreadId,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct StackFrameInfo {
    pub id: FrameId,
    pub name: String,
    pub source_path: Option<String>,
    pub line: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    Into,
    Over,
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Breakpoint,
    Step,
    Exception,
    Other,
}

impl StopReason {
    pub fn as_dap_reason(self) -> &'static str {
        match self {
            StopReason::Breakpoint => "breakpoint",
            StopReason::Step => "step",
            StopReason::Exception => "exception",
            StopReason::Other => "pause",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoppedEvent {
    pub reason: StopReason,
    pub thread_id: ThreadId,
    /// JDWP event request id that produced this stop (if known).
    pub request_id: u32,
    /// Return value observed while stepping (best-effort).
    pub return_value: Option<JdwpValue>,
    /// Value of the last expression on the stepped line (best-effort).
    pub expression_value: Option<JdwpValue>,
}

#[derive(Debug, Clone)]
pub enum JdwpEvent {
    Stopped(StoppedEvent),
}

#[derive(Debug, Error)]
pub enum JdwpError {
    #[error("JDWP client is not connected")]
    NotConnected,
    #[error("JDWP operation not implemented")]
    NotImplemented,
    #[error("JDWP protocol error: {0}")]
    Protocol(String),
    #[error("JDWP command failed with error code {error_code}")]
    CommandFailed { error_code: u16 },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("JDWP handshake failed")]
    HandshakeFailed,
    #[error("JDWP string was not valid UTF-8")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    #[error("invalid object id {0}")]
    InvalidObjectId(ObjectId),
    #[error("{0}")]
    Other(String),
}

/// Minimal, mock-friendly interface for JDWP.
///
/// The network implementation included in this repository purposefully keeps
/// the wire-level support small; the trait is designed so Nova's DAP layer can
/// grow richer value inspection without rewriting call sites.
pub trait JdwpClient: Send {
    fn connect(&mut self, host: &str, port: u16) -> Result<(), JdwpError>;

    fn set_line_breakpoint(
        &mut self,
        class: &str,
        method: Option<&str>,
        line: u32,
    ) -> Result<(), JdwpError>;

    fn threads(&mut self) -> Result<Vec<ThreadInfo>, JdwpError>;
    fn stack_frames(&mut self, thread_id: ThreadId) -> Result<Vec<StackFrameInfo>, JdwpError>;

    fn r#continue(&mut self, thread_id: ThreadId) -> Result<(), JdwpError>;
    fn next(&mut self, thread_id: ThreadId) -> Result<(), JdwpError>;
    fn step_in(&mut self, thread_id: ThreadId) -> Result<(), JdwpError>;
    fn step_out(&mut self, thread_id: ThreadId) -> Result<(), JdwpError>;
    fn pause(&mut self, thread_id: ThreadId) -> Result<(), JdwpError>;

    /// Wait until the next asynchronous event is received from the JVM.
    ///
    /// A real debugger would typically run an event loop and forward events to
    /// the DAP client asynchronously. Nova's current DAP server runs a simple,
    /// synchronous loop, so we expose a blocking read here.
    fn wait_for_event(&mut self) -> Result<Option<JdwpEvent>, JdwpError> {
        Ok(None)
    }

    /// Convenience helper that performs a step and returns the resulting stop.
    ///
    /// This is primarily used by debugger UX code that wants to surface return
    /// values / expression values alongside the stop event.
    fn step(&mut self, thread_id: ThreadId, kind: StepKind) -> Result<StoppedEvent, JdwpError> {
        match kind {
            StepKind::Into => self.step_in(thread_id)?,
            StepKind::Over => self.next(thread_id)?,
            StepKind::Out => self.step_out(thread_id)?,
        }

        loop {
            match self.wait_for_event()? {
                Some(JdwpEvent::Stopped(stopped)) => return Ok(stopped),
                None => {
                    return Err(JdwpError::Other(
                        "expected a stopped event after stepping".to_string(),
                    ))
                }
            }
        }
    }

    fn evaluate(&mut self, _expression: &str, _frame_id: FrameId) -> Result<JdwpValue, JdwpError> {
        Err(JdwpError::NotImplemented)
    }

    fn preview_object(&mut self, _object_id: ObjectId) -> Result<ObjectPreview, JdwpError> {
        Err(JdwpError::NotImplemented)
    }

    fn object_children(&mut self, _object_id: ObjectId) -> Result<Vec<JdwpVariable>, JdwpError> {
        Err(JdwpError::NotImplemented)
    }

    fn disable_collection(&mut self, _object_id: ObjectId) -> Result<(), JdwpError> {
        Err(JdwpError::NotImplemented)
    }

    fn enable_collection(&mut self, _object_id: ObjectId) -> Result<(), JdwpError> {
        Err(JdwpError::NotImplemented)
    }
}

/// Wire-level JDWP client implementation (async, tokio).
///
/// This is intentionally namespaced to avoid breaking the existing `nova-jdwp`
/// mock interfaces used by debugger UX tests.
pub mod wire;

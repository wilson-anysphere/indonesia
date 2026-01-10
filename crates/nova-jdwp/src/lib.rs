//! Minimal JDWP façade used by `nova-dap` tests.
//!
//! This crate is intentionally small: it models only the JDWP surface area
//! needed for debugger UX work (value previews, stable object IDs, and stepping
//! outcomes). Real JDWP support can grow behind the same trait without changing
//! `nova-dap` consumers.

use std::collections::{BTreeSet, HashMap, VecDeque};

use thiserror::Error;

pub type ObjectId = u64;
pub type ThreadId = u64;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    Into,
    Over,
    Out,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Step,
    Breakpoint,
    Exception,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoppedEvent {
    pub thread_id: ThreadId,
    pub reason: StopReason,
    /// Return value observed while stepping (best-effort).
    pub return_value: Option<JdwpValue>,
    /// Value of the last expression on the stepped line (best-effort).
    pub expression_value: Option<JdwpValue>,
}

#[derive(Error, Debug, PartialEq, Clone)]
pub enum JdwpError {
    #[error("invalid object id {0}")]
    InvalidObjectId(ObjectId),
    #[error("vm disconnected")]
    VmDisconnected,
    #[error("{0}")]
    Other(String),
}

pub trait JdwpClient {
    fn step(&mut self, thread_id: ThreadId, kind: StepKind) -> Result<StoppedEvent, JdwpError>;
    fn evaluate(&mut self, thread_id: ThreadId, expression: &str) -> Result<JdwpValue, JdwpError>;
    fn preview_object(&mut self, object_id: ObjectId) -> Result<ObjectPreview, JdwpError>;
    fn object_children(&mut self, object_id: ObjectId) -> Result<Vec<JdwpVariable>, JdwpError>;
    fn disable_collection(&mut self, object_id: ObjectId) -> Result<(), JdwpError>;
    fn enable_collection(&mut self, object_id: ObjectId) -> Result<(), JdwpError>;
}

#[derive(Clone, Debug)]
pub struct MockObject {
    pub preview: ObjectPreview,
    pub children: Vec<JdwpVariable>,
}

/// Deterministic, in-memory JDWP test double.
#[derive(Default)]
pub struct MockJdwpClient {
    steps: VecDeque<Result<StoppedEvent, JdwpError>>,
    evaluations: HashMap<(ThreadId, String), Result<JdwpValue, JdwpError>>,
    objects: HashMap<ObjectId, MockObject>,
    collection_disabled: BTreeSet<ObjectId>,
    pub disable_collection_calls: Vec<ObjectId>,
    pub enable_collection_calls: Vec<ObjectId>,
}

impl MockJdwpClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_step(&mut self, event: Result<StoppedEvent, JdwpError>) {
        self.steps.push_back(event);
    }

    pub fn set_evaluation(
        &mut self,
        thread_id: ThreadId,
        expression: impl Into<String>,
        result: Result<JdwpValue, JdwpError>,
    ) {
        self.evaluations
            .insert((thread_id, expression.into()), result);
    }

    pub fn insert_object(&mut self, object_id: ObjectId, obj: MockObject) {
        self.objects.insert(object_id, obj);
    }

    pub fn collect_object(&mut self, object_id: ObjectId) {
        self.objects.remove(&object_id);
        self.collection_disabled.remove(&object_id);
    }

    pub fn is_collection_disabled(&self, object_id: ObjectId) -> bool {
        self.collection_disabled.contains(&object_id)
    }
}

impl JdwpClient for MockJdwpClient {
    fn step(&mut self, thread_id: ThreadId, _kind: StepKind) -> Result<StoppedEvent, JdwpError> {
        match self.steps.pop_front() {
            Some(event) => event,
            None => Err(JdwpError::Other(format!(
                "no mock step result queued for thread {thread_id}"
            ))),
        }
    }

    fn evaluate(&mut self, thread_id: ThreadId, expression: &str) -> Result<JdwpValue, JdwpError> {
        self.evaluations
            .get(&(thread_id, expression.to_string()))
            .cloned()
            .unwrap_or_else(|| {
                Err(JdwpError::Other(format!(
                    "no mock evaluation result queued for `{expression}`"
                )))
            })
    }

    fn preview_object(&mut self, object_id: ObjectId) -> Result<ObjectPreview, JdwpError> {
        self.objects
            .get(&object_id)
            .map(|o| o.preview.clone())
            .ok_or(JdwpError::InvalidObjectId(object_id))
    }

    fn object_children(&mut self, object_id: ObjectId) -> Result<Vec<JdwpVariable>, JdwpError> {
        self.objects
            .get(&object_id)
            .map(|o| o.children.clone())
            .ok_or(JdwpError::InvalidObjectId(object_id))
    }

    fn disable_collection(&mut self, object_id: ObjectId) -> Result<(), JdwpError> {
        if !self.objects.contains_key(&object_id) {
            return Err(JdwpError::InvalidObjectId(object_id));
        }
        self.collection_disabled.insert(object_id);
        self.disable_collection_calls.push(object_id);
        Ok(())
    }

    fn enable_collection(&mut self, object_id: ObjectId) -> Result<(), JdwpError> {
        if !self.objects.contains_key(&object_id) {
            return Err(JdwpError::InvalidObjectId(object_id));
        }
        self.collection_disabled.remove(&object_id);
        self.enable_collection_calls.push(object_id);
        Ok(())
    }
}

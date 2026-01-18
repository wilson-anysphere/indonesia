use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::{
    FrameId, JdwpClient, JdwpError, JdwpEvent, JdwpValue, ObjectId, ObjectPreview, StackFrameInfo,
    StoppedEvent, ThreadId, ThreadInfo,
};

#[derive(Clone, Debug)]
pub struct MockObject {
    pub preview: ObjectPreview,
    pub children: Vec<crate::JdwpVariable>,
}

/// Deterministic, in-memory JDWP test double.
#[derive(Default)]
pub struct MockJdwpClient {
    steps: VecDeque<Result<StoppedEvent, JdwpError>>,
    evaluations: HashMap<(FrameId, String), VecDeque<Result<JdwpValue, JdwpError>>>,
    objects: HashMap<ObjectId, MockObject>,
    collection_disabled: BTreeSet<ObjectId>,
    threads: Vec<ThreadInfo>,
    frames: HashMap<ThreadId, Vec<StackFrameInfo>>,
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

    pub fn push_event(&mut self, event: Result<JdwpEvent, JdwpError>) {
        match event {
            Ok(JdwpEvent::Stopped(stopped)) => self.steps.push_back(Ok(stopped)),
            Err(err) => self.steps.push_back(Err(err)),
        }
    }

    pub fn set_evaluation(
        &mut self,
        frame_id: FrameId,
        expression: impl Into<String>,
        result: Result<JdwpValue, JdwpError>,
    ) {
        self.evaluations
            .entry((frame_id, expression.into()))
            .or_default()
            .push_back(result);
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

    pub fn set_threads(&mut self, threads: Vec<ThreadInfo>) {
        self.threads = threads;
    }

    pub fn set_stack_frames(&mut self, thread_id: ThreadId, frames: Vec<StackFrameInfo>) {
        self.frames.insert(thread_id, frames);
    }
}

impl JdwpClient for MockJdwpClient {
    fn connect(&mut self, _host: &str, _port: u16) -> Result<(), JdwpError> {
        Ok(())
    }

    fn set_line_breakpoint(
        &mut self,
        _class: &str,
        _method: Option<&str>,
        _line: u32,
    ) -> Result<(), JdwpError> {
        Ok(())
    }

    fn threads(&mut self) -> Result<Vec<ThreadInfo>, JdwpError> {
        Ok(self.threads.clone())
    }

    fn stack_frames(&mut self, thread_id: ThreadId) -> Result<Vec<StackFrameInfo>, JdwpError> {
        match self.frames.get(&thread_id) {
            Some(frames) => Ok(frames.clone()),
            None => Err(JdwpError::Other(format!(
                "no mock stack frames configured for thread {thread_id}"
            ))),
        }
    }

    fn r#continue(&mut self, _thread_id: ThreadId) -> Result<(), JdwpError> {
        Ok(())
    }

    fn next(&mut self, _thread_id: ThreadId) -> Result<(), JdwpError> {
        Ok(())
    }

    fn step_in(&mut self, _thread_id: ThreadId) -> Result<(), JdwpError> {
        Ok(())
    }

    fn step_out(&mut self, _thread_id: ThreadId) -> Result<(), JdwpError> {
        Ok(())
    }

    fn pause(&mut self, _thread_id: ThreadId) -> Result<(), JdwpError> {
        Ok(())
    }

    fn wait_for_event(&mut self) -> Result<Option<JdwpEvent>, JdwpError> {
        match self.steps.pop_front() {
            Some(Ok(stopped)) => Ok(Some(JdwpEvent::Stopped(stopped))),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }

    fn step(
        &mut self,
        thread_id: ThreadId,
        _kind: crate::StepKind,
    ) -> Result<StoppedEvent, JdwpError> {
        match self.steps.pop_front() {
            Some(event) => event,
            None => Err(JdwpError::Other(format!(
                "no mock step result queued for thread {thread_id}"
            ))),
        }
    }

    fn evaluate(&mut self, expression: &str, frame_id: FrameId) -> Result<JdwpValue, JdwpError> {
        let key = (frame_id, expression.to_string());
        match self.evaluations.get_mut(&key).and_then(|q| q.pop_front()) {
            Some(result) => result,
            None => Err(JdwpError::Other(format!(
                "no mock evaluation result queued for `{expression}`"
            ))),
        }
    }

    fn preview_object(&mut self, object_id: ObjectId) -> Result<ObjectPreview, JdwpError> {
        self.objects
            .get(&object_id)
            .map(|o| o.preview.clone())
            .ok_or(JdwpError::InvalidObjectId(object_id))
    }

    fn object_children(
        &mut self,
        object_id: ObjectId,
    ) -> Result<Vec<crate::JdwpVariable>, JdwpError> {
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

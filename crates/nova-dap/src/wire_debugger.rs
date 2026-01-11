use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::Path,
};

use serde_json::json;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use nova_jdwp::wire::{
    ClassInfo, EventModifier, FrameInfo, JdwpClient, JdwpError, JdwpEvent, JdwpValue, LineTable,
    Location, MethodInfo, ObjectId, ReferenceTypeId, ThreadId, VariableInfo,
};

#[derive(Debug, Error)]
pub enum DebuggerError {
    #[error(transparent)]
    Jdwp(#[from] JdwpError),

    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

pub type Result<T> = std::result::Result<T, DebuggerError>;

#[derive(Debug, Clone)]
pub struct AttachArgs {
    pub host: IpAddr,
    pub port: u16,
}

#[derive(Debug, Clone, Copy)]
pub enum StepDepth {
    Into,
    Over,
    Out,
}

#[derive(Debug, Clone)]
struct BreakpointEntry {
    request_id: i32,
}

#[derive(Debug, Clone, Copy)]
struct FrameHandle {
    thread: ThreadId,
    frame_id: u64,
    location: Location,
}

#[derive(Debug, Clone)]
enum VarRef {
    FrameLocals(FrameHandle),
    ObjectFields(ObjectId),
    ArrayElements(ObjectId),
}

struct HandleTable<T> {
    next: i64,
    map: HashMap<i64, T>,
}

impl<T> Default for HandleTable<T> {
    fn default() -> Self {
        Self {
            next: 0,
            map: HashMap::new(),
        }
    }
}

impl<T> HandleTable<T> {
    fn alloc(&mut self, value: T) -> i64 {
        self.next += 1;
        let id = self.next;
        self.map.insert(id, value);
        id
    }

    fn get(&self, id: i64) -> Option<&T> {
        self.map.get(&id)
    }
}

pub struct Debugger {
    jdwp: JdwpClient,
    breakpoints: HashMap<String, Vec<BreakpointEntry>>,
    requested_breakpoints: HashMap<String, Vec<i32>>,
    class_prepare_request: Option<i32>,
    step_request: Option<i32>,
    exception_requests: Vec<i32>,

    source_cache: HashMap<ReferenceTypeId, String>,
    methods_cache: HashMap<ReferenceTypeId, Vec<MethodInfo>>,
    line_table_cache: HashMap<(ReferenceTypeId, u64), LineTable>,

    frame_handles: HandleTable<FrameHandle>,
    var_handles: HandleTable<VarRef>,
}

impl Debugger {
    pub async fn attach(args: AttachArgs) -> Result<Self> {
        let addr = SocketAddr::new(args.host, args.port);
        let jdwp = JdwpClient::connect(addr).await?;

        let mut dbg = Self {
            jdwp,
            breakpoints: HashMap::new(),
            requested_breakpoints: HashMap::new(),
            class_prepare_request: None,
            step_request: None,
            exception_requests: Vec::new(),
            source_cache: HashMap::new(),
            methods_cache: HashMap::new(),
            line_table_cache: HashMap::new(),
            frame_handles: HandleTable::default(),
            var_handles: HandleTable::default(),
        };

        // Track class loads to support setting breakpoints before the target class is loaded.
        let req = dbg
            .jdwp
            .event_request_set(8, 0, vec![EventModifier::ClassMatch { pattern: "*".to_string() }])
            .await?;
        dbg.class_prepare_request = Some(req);

        Ok(dbg)
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<JdwpEvent> {
        self.jdwp.subscribe_events()
    }

    pub fn jdwp_shutdown_token(&self) -> CancellationToken {
        self.jdwp.shutdown_token()
    }

    pub async fn disconnect(&mut self) {
        self.jdwp.shutdown();
    }

    pub async fn threads(&self) -> Result<Vec<(i64, String)>> {
        let threads = self.jdwp.all_threads().await?;
        let mut out = Vec::with_capacity(threads.len());
        for thread_id in threads {
            let name = self.jdwp.thread_name(thread_id).await.unwrap_or_else(|_| "thread".to_string());
            out.push((thread_id as i64, name));
        }
        Ok(out)
    }

    pub async fn stack_trace(&mut self, dap_thread_id: i64) -> Result<Vec<serde_json::Value>> {
        let thread = dap_thread_id
            .try_into()
            .map_err(|_| DebuggerError::InvalidRequest(format!("invalid threadId {dap_thread_id}")))?;

        let frames = self.jdwp.frames(thread, 0, 100).await?;
        let mut out = Vec::with_capacity(frames.len());
        for frame in frames {
            let frame_id = self.alloc_frame_handle(thread, &frame);
            let name = self.method_name(frame.location.class_id, frame.location.method_id).await.unwrap_or_else(|| "frame".to_string());
            let source = self.source_file(frame.location.class_id).await.ok();
            let line = self.line_number(frame.location.class_id, frame.location.method_id, frame.location.index).await.unwrap_or(1);

            out.push(json!({
                "id": frame_id,
                "name": name,
                "source": source.map(|s| json!({"name": s, "path": s})),
                "line": line,
                "column": 1
            }));
        }

        Ok(out)
    }

    pub fn scopes(&mut self, frame_id: i64) -> Result<Vec<serde_json::Value>> {
        let Some(frame) = self.frame_handles.get(frame_id).copied() else {
            return Err(DebuggerError::InvalidRequest(format!("unknown frameId {frame_id}")));
        };

        let locals_ref = self.var_handles.alloc(VarRef::FrameLocals(frame));

        Ok(vec![json!({
            "name": "Locals",
            "presentationHint": "locals",
            "variablesReference": locals_ref,
            "expensive": false,
        })])
    }

    pub async fn variables(&mut self, variables_reference: i64, start: Option<i64>, count: Option<i64>) -> Result<Vec<serde_json::Value>> {
        let Some(var_ref) = self.var_handles.get(variables_reference).cloned() else {
            return Ok(Vec::new());
        };

        match var_ref {
            VarRef::FrameLocals(frame) => self.locals_variables(&frame).await,
            VarRef::ObjectFields(object_id) => self.object_fields(object_id).await,
            VarRef::ArrayElements(array_id) => self.array_elements(array_id, start, count).await,
        }
    }

    pub async fn set_breakpoints(&mut self, source_path: &str, lines: Vec<i32>) -> Result<Vec<serde_json::Value>> {
        let file = Path::new(source_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(source_path)
            .to_string();

        if let Some(existing) = self.breakpoints.remove(&file) {
            for bp in existing {
                let _ = self.jdwp.event_request_clear(2, bp.request_id).await;
            }
        }

        self.requested_breakpoints.insert(file.clone(), lines.clone());

        let mut results = Vec::with_capacity(lines.len());

        // Best-effort: attempt to apply now for already-loaded classes.
        let classes = self.jdwp.all_classes().await?;
        let mut class_candidates = Vec::new();
        for class_info in classes {
            if self.source_file(class_info.type_id).await.ok().as_deref() == Some(file.as_str()) {
                class_candidates.push(class_info);
            }
        }

        let class = class_candidates.into_iter().next();
        if let Some(class) = class {
            let mut entries = Vec::new();
            for &line in &lines {
                match self.location_for_line(&class, line).await? {
                    Some(location) => {
                        match self
                            .jdwp
                            .event_request_set(2, 1, vec![EventModifier::LocationOnly { location }])
                            .await
                        {
                            Ok(request_id) => {
                                entries.push(BreakpointEntry { request_id });
                                results.push(json!({"verified": true, "line": line}));
                            }
                            Err(err) => {
                                results.push(json!({"verified": false, "line": line, "message": err.to_string()}));
                            }
                        }
                    }
                    None => {
                        results.push(json!({"verified": false, "line": line, "message": "no executable code at this line"}));
                    }
                }
            }
            self.breakpoints.insert(file.clone(), entries);
        } else {
            for line in lines {
                results.push(json!({"verified": false, "line": line, "message": "class not loaded yet"}));
            }
        }

        Ok(results)
    }

    pub async fn continue_(&self) -> Result<()> {
        self.jdwp.vm_resume().await?;
        Ok(())
    }

    pub async fn pause(&self) -> Result<()> {
        self.jdwp.vm_suspend().await?;
        Ok(())
    }

    pub async fn step(&mut self, dap_thread_id: i64, depth: StepDepth) -> Result<()> {
        let thread: ThreadId = dap_thread_id
            .try_into()
            .map_err(|_| DebuggerError::InvalidRequest(format!("invalid threadId {dap_thread_id}")))?;

        if let Some(old) = self.step_request.take() {
            let _ = self.jdwp.event_request_clear(1, old).await;
        }

        let depth = match depth {
            StepDepth::Into => 0,
            StepDepth::Over => 1,
            StepDepth::Out => 2,
        };

        let req = self
            .jdwp
            .event_request_set(
                1,
                1,
                vec![EventModifier::Step {
                    thread,
                    size: 1, // line
                    depth,
                }],
            )
            .await?;
        self.step_request = Some(req);
        self.jdwp.vm_resume().await?;
        Ok(())
    }

    pub async fn set_exception_breakpoints(&mut self, caught: bool, uncaught: bool) -> Result<()> {
        self.clear_exception_breakpoints().await;
        if !caught && !uncaught {
            return Ok(());
        }

        let request_id = self
            .jdwp
            .event_request_set(
                4,
                1,
                vec![EventModifier::ExceptionOnly {
                    exception_or_null: 0,
                    caught,
                    uncaught,
                }],
            )
            .await?;
        self.exception_requests.push(request_id);
        Ok(())
    }

    pub async fn handle_vm_event(&mut self, event: &JdwpEvent) {
        match event {
            JdwpEvent::ClassPrepare { ref_type_tag, type_id, .. } => {
                let _ = self.on_class_prepare(*ref_type_tag, *type_id).await;
            }
            JdwpEvent::SingleStep { request_id, .. } => {
                if self.step_request == Some(*request_id) {
                    let _ = self.jdwp.event_request_clear(1, *request_id).await;
                    self.step_request = None;
                }
            }
            _ => {}
        }
    }

    pub async fn evaluate(&mut self, frame_id: i64, expression: &str) -> Result<Option<serde_json::Value>> {
        let expr = expression.trim();
        if expr.is_empty() {
            return Ok(Some(json!({"result": "", "variablesReference": 0})));
        }
        if !is_identifier(expr) {
            return Ok(Some(json!({"result": format!("unsupported expression: {expr}"), "variablesReference": 0})));
        }
        let Some(frame) = self.frame_handles.get(frame_id).copied() else {
            return Err(DebuggerError::InvalidRequest(format!("unknown frameId {frame_id}")));
        };
        let vars = self.locals_variables(&frame).await?;
        for v in vars {
            if v.get("name").and_then(|v| v.as_str()) == Some(expr) {
                let result = v.get("value").cloned().unwrap_or(Value::String(String::new()));
                return Ok(Some(json!({"result": result, "variablesReference": v.get("variablesReference").cloned().unwrap_or(json!(0))})));
            }
        }
        Ok(Some(json!({"result": format!("not found: {expr}"), "variablesReference": 0})))
    }

    fn alloc_frame_handle(&mut self, thread: ThreadId, frame: &FrameInfo) -> i64 {
        self.frame_handles.alloc(FrameHandle {
            thread,
            frame_id: frame.frame_id,
            location: frame.location,
        })
    }

    async fn source_file(&mut self, class_id: ReferenceTypeId) -> std::result::Result<String, JdwpError> {
        if let Some(v) = self.source_cache.get(&class_id) {
            return Ok(v.clone());
        }
        let file = self.jdwp.reference_type_source_file(class_id).await?;
        self.source_cache.insert(class_id, file.clone());
        Ok(file)
    }

    async fn method_name(&mut self, class_id: ReferenceTypeId, method_id: u64) -> Option<String> {
        if let Some(methods) = self.methods_cache.get(&class_id) {
            if let Some(m) = methods.iter().find(|m| m.method_id == method_id) {
                return Some(m.name.clone());
            }
        }
        let methods = self.jdwp.reference_type_methods(class_id).await.ok()?;
        let name = methods.iter().find(|m| m.method_id == method_id).map(|m| m.name.clone());
        self.methods_cache.insert(class_id, methods);
        name
    }

    async fn line_number(&mut self, class_id: ReferenceTypeId, method_id: u64, index: u64) -> std::result::Result<i32, JdwpError> {
        let key = (class_id, method_id);
        let table = if let Some(t) = self.line_table_cache.get(&key) {
            t.clone()
        } else {
            let t = self.jdwp.method_line_table(class_id, method_id).await?;
            self.line_table_cache.insert(key, t.clone());
            t
        };

        let mut best = None;
        for entry in &table.lines {
            if entry.code_index <= index {
                best = Some(entry.line);
            }
        }
        Ok(best.unwrap_or(1))
    }

    async fn locals_variables(&mut self, frame: &FrameHandle) -> Result<Vec<serde_json::Value>> {
        let (_argc, vars) = self
            .jdwp
            .method_variable_table(frame.location.class_id, frame.location.method_id)
            .await?;

        let in_scope: Vec<VariableInfo> = vars
            .into_iter()
            .filter(|v| v.code_index <= frame.location.index && frame.location.index < v.code_index + (v.length as u64))
            .collect();

        let slots: Vec<(u32, String)> = in_scope.iter().map(|v| (v.slot, v.signature.clone())).collect();
        let values = self
            .jdwp
            .stack_frame_get_values(frame.thread, frame.frame_id, &slots)
            .await?;

        let mut out = Vec::with_capacity(in_scope.len());
        for (var, value) in in_scope.into_iter().zip(values.into_iter()) {
            out.push(self.render_variable(var.name, value));
        }
        Ok(out)
    }

    async fn object_fields(&mut self, object_id: ObjectId) -> Result<Vec<serde_json::Value>> {
        if object_id == 0 {
            return Ok(Vec::new());
        }
        let class_id = self.jdwp.object_reference_reference_type(object_id).await?;
        let fields = self.jdwp.reference_type_fields(class_id).await?;
        let field_ids: Vec<u64> = fields.iter().map(|f| f.field_id).collect();
        let values = self.jdwp.object_reference_get_values(object_id, &field_ids).await?;
        let mut out = Vec::with_capacity(fields.len());
        for (field, value) in fields.into_iter().zip(values.into_iter()) {
            out.push(self.render_variable(field.name, value));
        }
        Ok(out)
    }

    async fn array_elements(&mut self, array_id: ObjectId, start: Option<i64>, count: Option<i64>) -> Result<Vec<serde_json::Value>> {
        if array_id == 0 {
            return Ok(Vec::new());
        }
        let total = self.jdwp.array_reference_length(array_id).await? as i64;
        let start = start.unwrap_or(0).clamp(0, total);
        let count = count.unwrap_or(100).clamp(0, total - start);
        let values = self
            .jdwp
            .array_reference_get_values(array_id, start as i32, count as i32)
            .await?;

        let mut out = Vec::with_capacity(values.len());
        for (idx, value) in (start..start + count).zip(values.into_iter()) {
            out.push(self.render_variable(format!("[{idx}]"), value));
        }
        Ok(out)
    }

    fn render_variable(&mut self, name: impl Into<String>, value: JdwpValue) -> serde_json::Value {
        let name = name.into();
        match value {
            JdwpValue::Object { tag, id } => {
                if id == 0 {
                    json!({"name": name, "value": "null", "variablesReference": 0})
                } else if tag == b'[' {
                    let ref_id = self.var_handles.alloc(VarRef::ArrayElements(id));
                    json!({"name": name, "value": format!("array@0x{id:x}"), "variablesReference": ref_id})
                } else {
                    let ref_id = self.var_handles.alloc(VarRef::ObjectFields(id));
                    json!({"name": name, "value": format!("object@0x{id:x}"), "variablesReference": ref_id})
                }
            }
            v => json!({"name": name, "value": v.to_string(), "variablesReference": 0}),
        }
    }

    async fn location_for_line(&mut self, class: &ClassInfo, line: i32) -> Result<Option<Location>> {
        let methods = if let Some(methods) = self.methods_cache.get(&class.type_id) {
            methods.clone()
        } else {
            let methods = self.jdwp.reference_type_methods(class.type_id).await?;
            self.methods_cache.insert(class.type_id, methods.clone());
            methods
        };

        for method in methods {
            let table = self
                .jdwp
                .method_line_table(class.type_id, method.method_id)
                .await
                .ok();
            let Some(table) = table else { continue; };
            if let Some(entry) = table.lines.iter().find(|e| e.line == line) {
                return Ok(Some(Location {
                    type_tag: class.ref_type_tag,
                    class_id: class.type_id,
                    method_id: method.method_id,
                    index: entry.code_index,
                }));
            }
        }
        Ok(None)
    }

    async fn on_class_prepare(&mut self, ref_type_tag: u8, type_id: ReferenceTypeId) -> Result<()> {
        let file = self.source_file(type_id).await?;
        let Some(lines) = self.requested_breakpoints.get(&file).cloned() else {
            return Ok(());
        };
        let class = ClassInfo {
            ref_type_tag,
            type_id,
            signature: self.jdwp.reference_type_signature(type_id).await.unwrap_or_default(),
            status: 0,
        };
        let mut entries = Vec::new();
        for line in lines {
            if let Some(location) = self.location_for_line(&class, line).await? {
                if let Ok(request_id) = self
                    .jdwp
                    .event_request_set(2, 1, vec![EventModifier::LocationOnly { location }])
                    .await
                {
                    entries.push(BreakpointEntry { request_id });
                }
            }
        }
        self.breakpoints.insert(file, entries);
        Ok(())
    }

    async fn clear_exception_breakpoints(&mut self) {
        let existing = std::mem::take(&mut self.exception_requests);
        for request_id in existing {
            let _ = self.jdwp.event_request_clear(4, request_id).await;
        }
    }
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

// `serde_json::Value` isn't in scope by default in this module, but we use it in evaluate to avoid
// repeated `serde_json::json!` conversions.
use serde_json::Value;

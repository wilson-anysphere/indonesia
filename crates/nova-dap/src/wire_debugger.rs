use std::{
    collections::HashMap,
    future::Future,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
};

use serde_json::json;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use nova_jdwp::wire::{
    inspect::{Inspector, ObjectKindPreview, ERROR_INVALID_OBJECT},
    ClassInfo, EventModifier, FrameInfo, JdwpClient, JdwpError, JdwpEvent, JdwpValue, LineTable,
    Location, MethodInfo, ObjectId, ReferenceTypeId, ThreadId, VariableInfo,
};

use crate::object_registry::{ObjectHandle, ObjectRegistry, PINNED_SCOPE_REF};

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
pub struct ExceptionInfo {
    pub exception_id: String,
    pub description: Option<String>,
    pub break_mode: String,
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
}

struct HandleTable<T> {
    next: i64,
    map: HashMap<i64, T>,
}

#[derive(Debug, Clone)]
struct ExceptionStopContext {
    exception: ObjectId,
    throw_location: Location,
    catch_location: Option<Location>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Breakpoint,
    Step,
    Exception,
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
    inspector: Inspector,
    objects: ObjectRegistry,
    breakpoints: HashMap<String, Vec<BreakpointEntry>>,
    requested_breakpoints: HashMap<String, Vec<i32>>,
    class_prepare_request: Option<i32>,
    step_request: Option<i32>,
    exception_requests: Vec<i32>,
    last_stop_reason: HashMap<ThreadId, StopReason>,
    exception_stop_context: HashMap<ThreadId, ExceptionStopContext>,

    /// Mapping from JDWP `ReferenceType.SourceFile` (usually just `Main.java`) to the
    /// best-effort full path provided by the DAP client.
    ///
    /// The JVM typically does not expose absolute source paths over JDWP, but DAP
    /// clients expect stack frames to contain a resolvable `source.path`. We can
    /// recover that by remembering the source paths passed to `setBreakpoints`.
    source_paths: HashMap<String, String>,
    source_cache: HashMap<ReferenceTypeId, String>,
    methods_cache: HashMap<ReferenceTypeId, Vec<MethodInfo>>,
    line_table_cache: HashMap<(ReferenceTypeId, u64), LineTable>,

    frame_handles: HandleTable<FrameHandle>,
    var_handles: HandleTable<VarRef>,
}

fn check_cancel(token: &CancellationToken) -> Result<()> {
    if token.is_cancelled() {
        Err(JdwpError::Cancelled.into())
    } else {
        Ok(())
    }
}

fn check_cancel_jdwp(token: &CancellationToken) -> std::result::Result<(), JdwpError> {
    if token.is_cancelled() {
        Err(JdwpError::Cancelled)
    } else {
        Ok(())
    }
}

async fn cancellable_jdwp<T, F>(
    token: &CancellationToken,
    fut: F,
) -> std::result::Result<T, JdwpError>
where
    F: Future<Output = std::result::Result<T, JdwpError>>,
{
    tokio::select! {
        _ = token.cancelled() => Err(JdwpError::Cancelled),
        res = fut => res,
    }
}

impl Debugger {
    pub async fn attach(args: AttachArgs) -> Result<Self> {
        let addr = SocketAddr::new(args.host, args.port);
        let jdwp = JdwpClient::connect(addr).await?;

        let mut dbg = Self {
            inspector: Inspector::new(jdwp.clone()),
            objects: ObjectRegistry::new(),
            jdwp,
            breakpoints: HashMap::new(),
            requested_breakpoints: HashMap::new(),
            class_prepare_request: None,
            step_request: None,
            exception_requests: Vec::new(),
            last_stop_reason: HashMap::new(),
            exception_stop_context: HashMap::new(),
            source_paths: HashMap::new(),
            source_cache: HashMap::new(),
            methods_cache: HashMap::new(),
            line_table_cache: HashMap::new(),
            frame_handles: HandleTable::default(),
            var_handles: HandleTable::default(),
        };

        // Track class loads to support setting breakpoints before the target class is loaded.
        let req = dbg
            .jdwp
            .event_request_set(
                8,
                0,
                vec![EventModifier::ClassMatch {
                    pattern: "*".to_string(),
                }],
            )
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

    pub async fn threads(&self, cancel: &CancellationToken) -> Result<Vec<(i64, String)>> {
        check_cancel(cancel)?;
        let threads = cancellable_jdwp(cancel, self.jdwp.all_threads()).await?;
        let mut out = Vec::with_capacity(threads.len());
        for thread_id in threads {
            check_cancel(cancel)?;
            let name = match cancellable_jdwp(cancel, self.jdwp.thread_name(thread_id)).await {
                Ok(name) => name,
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(_) => "thread".to_string(),
            };
            out.push((thread_id as i64, name));
        }
        Ok(out)
    }

    pub async fn stack_trace(
        &mut self,
        cancel: &CancellationToken,
        dap_thread_id: i64,
    ) -> Result<Vec<serde_json::Value>> {
        // Thread ids originate from JDWP `ObjectId` values, which are opaque 64-bit numbers.
        // Represent them in DAP as `i64` (required by the protocol) using a lossless bit-cast:
        // `u64 -> i64` and back via `as` preserves the underlying bits even if the sign flips.
        let thread = dap_thread_id as ThreadId;

        // Some JVMs treat an oversized `length` as `INVALID_LENGTH` instead of clamping.
        // JDWP allows `length = -1` to request all frames starting at `start`.
        let frames = cancellable_jdwp(cancel, self.jdwp.frames(thread, 0, -1)).await?;
        let mut out = Vec::with_capacity(frames.len());
        for frame in frames {
            check_cancel(cancel)?;
            let frame_id = self.alloc_frame_handle(thread, &frame);
            let name = self
                .method_name(cancel, frame.location.class_id, frame.location.method_id)
                .await?
                .unwrap_or_else(|| "frame".to_string());
            let source_name = match self.source_file(cancel, frame.location.class_id).await {
                Ok(name) => Some(name),
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(_) => None,
            };
            let source = source_name.as_ref().map(|name| {
                let path = self
                    .source_paths
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone());
                json!({"name": name, "path": path})
            });
            let line = match self
                .line_number(
                    cancel,
                    frame.location.class_id,
                    frame.location.method_id,
                    frame.location.index,
                )
                .await
            {
                Ok(line) => line,
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(_) => 1,
            };

            out.push(json!({
                "id": frame_id,
                "name": name,
                "source": source,
                "line": line,
                "column": 1
            }));
        }

        Ok(out)
    }

    pub fn scopes(&mut self, frame_id: i64) -> Result<Vec<serde_json::Value>> {
        let Some(frame) = self.frame_handles.get(frame_id).copied() else {
            return Err(DebuggerError::InvalidRequest(format!(
                "unknown frameId {frame_id}"
            )));
        };

        let locals_ref = self.var_handles.alloc(VarRef::FrameLocals(frame));

        Ok(vec![
            json!({
                "name": "Locals",
                "presentationHint": "locals",
                "variablesReference": locals_ref,
                "expensive": false,
            }),
            json!({
                "name": "Pinned Objects",
                "presentationHint": "pinned",
                "variablesReference": PINNED_SCOPE_REF,
                "expensive": false,
            }),
        ])
    }

    pub async fn variables(
        &mut self,
        cancel: &CancellationToken,
        variables_reference: i64,
        start: Option<i64>,
        count: Option<i64>,
    ) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;
        if variables_reference == PINNED_SCOPE_REF {
            return self.pinned_variables(cancel).await;
        }

        if let Some(handle) = self.objects.handle_from_variables_reference(variables_reference) {
            return self.object_variables(cancel, handle, start, count).await;
        }

        let Some(var_ref) = self.var_handles.get(variables_reference).cloned() else {
            return Ok(Vec::new());
        };

        match var_ref {
            VarRef::FrameLocals(frame) => self.locals_variables(cancel, &frame).await,
        }
    }

    pub async fn set_breakpoints(
        &mut self,
        cancel: &CancellationToken,
        source_path: &str,
        lines: Vec<i32>,
    ) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;
        let file = Path::new(source_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(source_path)
            .to_string();

        if !source_path.is_empty() {
            let full_path = std::fs::canonicalize(source_path)
                .unwrap_or_else(|_| PathBuf::from(source_path))
                .to_string_lossy()
                .to_string();
            self.source_paths.insert(file.clone(), full_path);
        }

        if let Some(existing) = self.breakpoints.remove(&file) {
            for bp in existing {
                check_cancel(cancel)?;
                let _ = cancellable_jdwp(cancel, self.jdwp.event_request_clear(2, bp.request_id)).await;
            }
        }

        self.requested_breakpoints
            .insert(file.clone(), lines.clone());

        let mut results = Vec::with_capacity(lines.len());

        // Best-effort: attempt to apply now for already-loaded classes.
        let classes = cancellable_jdwp(cancel, self.jdwp.all_classes()).await?;
        let mut class_candidates = Vec::new();
        for class_info in classes {
            check_cancel(cancel)?;
            match self.source_file(cancel, class_info.type_id).await {
                Ok(source_file) => {
                    if source_file.as_str() == file.as_str() {
                        class_candidates.push(class_info);
                    }
                }
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(_) => {}
            }
        }

        let class = class_candidates.into_iter().next();
        if let Some(class) = class {
            let mut entries = Vec::new();
            for &line in &lines {
                check_cancel(cancel)?;
                match self.location_for_line(cancel, &class, line).await? {
                    Some(location) => {
                        match cancellable_jdwp(
                            cancel,
                            self.jdwp
                                .event_request_set(2, 1, vec![EventModifier::LocationOnly { location }]),
                        )
                        .await {
                            Ok(request_id) => {
                                entries.push(BreakpointEntry { request_id });
                                results.push(json!({"verified": true, "line": line}));
                            }
                            Err(err) => {
                                if matches!(err, JdwpError::Cancelled) {
                                    return Err(JdwpError::Cancelled.into());
                                }
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
                results.push(
                    json!({"verified": false, "line": line, "message": "class not loaded yet"}),
                );
            }
        }

        Ok(results)
    }

    pub async fn continue_(&self, cancel: &CancellationToken) -> Result<()> {
        cancellable_jdwp(cancel, self.jdwp.vm_resume()).await?;
        Ok(())
    }

    pub async fn pause(&self, cancel: &CancellationToken) -> Result<()> {
        cancellable_jdwp(cancel, self.jdwp.vm_suspend()).await?;
        Ok(())
    }

    pub async fn exception_info(
        &mut self,
        cancel: &CancellationToken,
        dap_thread_id: i64,
    ) -> Result<Option<ExceptionInfo>> {
        check_cancel(cancel)?;

        // `dap_thread_id` is an `i64` bit-cast of the JDWP `ObjectId` representing the thread.
        let thread: ThreadId = dap_thread_id as ThreadId;

        if self.last_stop_reason.get(&thread) != Some(&StopReason::Exception) {
            return Ok(None);
        }

        let Some(ctx) = self.exception_stop_context.get(&thread).cloned() else {
            return Ok(None);
        };

        let exception_id = match self.object_type_name(cancel, ctx.exception).await {
            Ok(Some(name)) => name,
            Ok(None) => format!("exception@0x{:x}", ctx.exception),
            Err(_) => format!("exception@0x{:x}", ctx.exception),
        };

        let break_mode = if ctx.catch_location.is_some() {
            "always".to_string()
        } else {
            "unhandled".to_string()
        };

        Ok(Some(ExceptionInfo {
            exception_id,
            description: None,
            break_mode,
        }))
    }

    pub async fn step(
        &mut self,
        cancel: &CancellationToken,
        dap_thread_id: i64,
        depth: StepDepth,
    ) -> Result<()> {
        check_cancel(cancel)?;
        let thread: ThreadId = dap_thread_id as ThreadId;

        if let Some(old) = self.step_request.take() {
            let _ = cancellable_jdwp(cancel, self.jdwp.event_request_clear(1, old)).await;
        }

        let depth = match depth {
            StepDepth::Into => 0,
            StepDepth::Over => 1,
            StepDepth::Out => 2,
        };

        let req = cancellable_jdwp(
            cancel,
            self.jdwp.event_request_set(
                1,
                1,
                vec![EventModifier::Step {
                    thread,
                    size: 1, // line
                    depth,
                }],
            ),
        )
        .await?;
        self.step_request = Some(req);
        cancellable_jdwp(cancel, self.jdwp.vm_resume()).await?;
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
            JdwpEvent::ClassPrepare {
                ref_type_tag,
                type_id,
                ..
            } => {
                let _ = self.on_class_prepare(*ref_type_tag, *type_id).await;
            }
            JdwpEvent::Breakpoint { thread, .. } => {
                self.last_stop_reason.insert(*thread, StopReason::Breakpoint);
                self.exception_stop_context.remove(thread);
            }
            JdwpEvent::SingleStep { request_id, thread, .. } => {
                if self.step_request == Some(*request_id) {
                    let _ = self.jdwp.event_request_clear(1, *request_id).await;
                    self.step_request = None;
                }
                self.last_stop_reason.insert(*thread, StopReason::Step);
                self.exception_stop_context.remove(thread);
            }
            JdwpEvent::Exception {
                thread,
                location,
                exception,
                catch_location,
                ..
            } => {
                self.last_stop_reason.insert(*thread, StopReason::Exception);
                self.exception_stop_context.insert(
                    *thread,
                    ExceptionStopContext {
                        exception: *exception,
                        throw_location: *location,
                        catch_location: *catch_location,
                    },
                );
            }
            _ => {}
        }
    }

    pub async fn evaluate(
        &mut self,
        cancel: &CancellationToken,
        frame_id: i64,
        expression: &str,
    ) -> Result<Option<serde_json::Value>> {
        let expr = expression.trim();
        if expr.is_empty() {
            return Ok(Some(json!({"result": "", "variablesReference": 0})));
        }
        if !is_identifier(expr) {
            return Ok(Some(
                json!({"result": format!("unsupported expression: {expr}"), "variablesReference": 0}),
            ));
        }
        let Some(frame) = self.frame_handles.get(frame_id).copied() else {
            return Err(DebuggerError::InvalidRequest(format!(
                "unknown frameId {frame_id}"
            )));
        };
        let vars = self.locals_variables(cancel, &frame).await?;
        for v in vars {
            check_cancel(cancel)?;
            if v.get("name").and_then(|v| v.as_str()) == Some(expr) {
                let result = v
                    .get("value")
                    .cloned()
                    .unwrap_or(Value::String(String::new()));
                return Ok(Some(
                    json!({"result": result, "variablesReference": v.get("variablesReference").cloned().unwrap_or(json!(0))}),
                ));
            }
        }
        Ok(Some(
            json!({"result": format!("not found: {expr}"), "variablesReference": 0}),
        ))
    }

    fn alloc_frame_handle(&mut self, thread: ThreadId, frame: &FrameInfo) -> i64 {
        self.frame_handles.alloc(FrameHandle {
            thread,
            frame_id: frame.frame_id,
            location: frame.location,
        })
    }

    async fn source_file(
        &mut self,
        cancel: &CancellationToken,
        class_id: ReferenceTypeId,
    ) -> std::result::Result<String, JdwpError> {
        check_cancel_jdwp(cancel)?;
        if let Some(v) = self.source_cache.get(&class_id) {
            return Ok(v.clone());
        }
        let file =
            cancellable_jdwp(cancel, self.jdwp.reference_type_source_file(class_id)).await?;
        self.source_cache.insert(class_id, file.clone());
        Ok(file)
    }

    async fn method_name(
        &mut self,
        cancel: &CancellationToken,
        class_id: ReferenceTypeId,
        method_id: u64,
    ) -> std::result::Result<Option<String>, JdwpError> {
        check_cancel_jdwp(cancel)?;
        if let Some(methods) = self.methods_cache.get(&class_id) {
            if let Some(m) = methods.iter().find(|m| m.method_id == method_id) {
                return Ok(Some(m.name.clone()));
            }
        }
        let methods =
            cancellable_jdwp(cancel, self.jdwp.reference_type_methods(class_id)).await?;
        let name = methods
            .iter()
            .find(|m| m.method_id == method_id)
            .map(|m| m.name.clone());
        self.methods_cache.insert(class_id, methods);
        Ok(name)
    }

    async fn line_number(
        &mut self,
        cancel: &CancellationToken,
        class_id: ReferenceTypeId,
        method_id: u64,
        index: u64,
    ) -> std::result::Result<i32, JdwpError> {
        check_cancel_jdwp(cancel)?;
        let key = (class_id, method_id);
        let table = if let Some(t) = self.line_table_cache.get(&key) {
            t.clone()
        } else {
            let t = cancellable_jdwp(cancel, self.jdwp.method_line_table(class_id, method_id)).await?;
            self.line_table_cache.insert(key, t.clone());
            t
        };

        let mut best = None;
        for entry in &table.lines {
            check_cancel_jdwp(cancel)?;
            if entry.code_index <= index {
                best = Some(entry.line);
            }
        }
        Ok(best.unwrap_or(1))
    }

    async fn locals_variables(
        &mut self,
        cancel: &CancellationToken,
        frame: &FrameHandle,
    ) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;
        let (_argc, vars) = cancellable_jdwp(
            cancel,
            self.jdwp
                .method_variable_table(frame.location.class_id, frame.location.method_id),
        )
        .await?;

        let in_scope: Vec<VariableInfo> = vars
            .into_iter()
            .filter(|v| {
                v.code_index <= frame.location.index
                    && frame.location.index < v.code_index + (v.length as u64)
            })
            .collect();

        let slots: Vec<(u32, String)> = in_scope
            .iter()
            .map(|v| (v.slot, v.signature.clone()))
            .collect();
        let values = cancellable_jdwp(
            cancel,
            self.jdwp
                .stack_frame_get_values(frame.thread, frame.frame_id, &slots),
        )
        .await?;

        let mut out = Vec::with_capacity(in_scope.len());
        for (var, value) in in_scope.into_iter().zip(values.into_iter()) {
            out.push(
                self.render_variable(
                    cancel,
                    var.name,
                    value,
                    Some(signature_to_type_name(&var.signature)),
                )
                    .await?,
            );
        }
        Ok(out)
    }

    async fn object_variables(
        &mut self,
        cancel: &CancellationToken,
        handle: ObjectHandle,
        _start: Option<i64>,
        _count: Option<i64>,
    ) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;
        let Some(object_id) = self.objects.object_id(handle) else {
            return Ok(Vec::new());
        };

        let children = match cancellable_jdwp(cancel, self.inspector.object_children(object_id)).await {
            Ok(children) => children,
            Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
            Err(JdwpError::VmError(code)) if code == ERROR_INVALID_OBJECT => {
                self.objects.mark_invalid_object_id(object_id);
                return Ok(vec![invalid_collected_variable()]);
            }
            Err(err) => return Err(err.into()),
        };

        let mut out = Vec::with_capacity(children.len());
        for child in children {
            check_cancel(cancel)?;
            out.push(
                self.render_variable(cancel, child.name, child.value, child.static_type)
                    .await?,
            );
        }
        Ok(out)
    }

    async fn pinned_variables(&mut self, cancel: &CancellationToken) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;
        let pinned: Vec<_> = self.objects.pinned_handles().collect();
        let mut vars = Vec::with_capacity(pinned.len());

        for handle in pinned {
            check_cancel(cancel)?;
            let Some(object_id) = self.objects.object_id(handle) else {
                continue;
            };

            let value = JdwpValue::Object { tag: b'L', id: object_id };
            let formatted = self.format_value(cancel, &value, None, 0).await?;
            vars.push(variable_json(
                handle.to_string(),
                formatted.value,
                formatted.type_name,
                formatted.variables_reference,
                Some(format!("__novaPinned[{}]", handle.as_u32())),
                Some(json!({
                    "kind": "data",
                    "attributes": ["pinned"],
                })),
            ));
        }

        Ok(vars)
    }

    async fn render_variable(
        &mut self,
        cancel: &CancellationToken,
        name: impl Into<String>,
        value: JdwpValue,
        static_type: Option<String>,
    ) -> Result<serde_json::Value> {
        check_cancel(cancel)?;
        let name = name.into();
        let formatted = self
            .format_value(cancel, &value, static_type.as_deref(), 0)
            .await?;
        Ok(variable_json(
            name.clone(),
            formatted.value,
            formatted.type_name,
            formatted.variables_reference,
            Some(name),
            formatted.presentation_hint,
        ))
    }

    async fn object_type_name(
        &mut self,
        cancel: &CancellationToken,
        object_id: ObjectId,
    ) -> Result<Option<String>> {
        if object_id == 0 {
            return Ok(None);
        }
        check_cancel(cancel)?;
        let class_id =
            cancellable_jdwp(cancel, self.jdwp.object_reference_reference_type(object_id)).await?;
        let sig = cancellable_jdwp(cancel, self.jdwp.reference_type_signature(class_id)).await?;
        Ok(signature_to_object_type_name(&sig))
    }

    async fn location_for_line(
        &mut self,
        cancel: &CancellationToken,
        class: &ClassInfo,
        line: i32,
    ) -> Result<Option<Location>> {
        check_cancel(cancel)?;
        let methods = if let Some(methods) = self.methods_cache.get(&class.type_id) {
            methods.clone()
        } else {
            let methods =
                cancellable_jdwp(cancel, self.jdwp.reference_type_methods(class.type_id)).await?;
            self.methods_cache.insert(class.type_id, methods.clone());
            methods
        };

        for method in methods {
            check_cancel(cancel)?;
            let table = match cancellable_jdwp(
                cancel,
                self.jdwp.method_line_table(class.type_id, method.method_id),
            )
            .await
            {
                Ok(table) => Some(table),
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(_) => None,
            };
            let Some(table) = table else {
                continue;
            };
            for entry in &table.lines {
                check_cancel(cancel)?;
                if entry.line == line {
                    return Ok(Some(Location {
                        type_tag: class.ref_type_tag,
                        class_id: class.type_id,
                        method_id: method.method_id,
                        index: entry.code_index,
                    }));
                }
            }
        }
        Ok(None)
    }

    async fn on_class_prepare(&mut self, ref_type_tag: u8, type_id: ReferenceTypeId) -> Result<()> {
        let cancel = CancellationToken::new();
        let file = self.source_file(&cancel, type_id).await?;
        let Some(lines) = self.requested_breakpoints.get(&file).cloned() else {
            return Ok(());
        };
        let class = ClassInfo {
            ref_type_tag,
            type_id,
            signature: self
                .jdwp
                .reference_type_signature(type_id)
                .await
                .unwrap_or_default(),
            status: 0,
        };
        let mut entries = Vec::new();
        for line in lines {
            if let Some(location) = self.location_for_line(&cancel, &class, line).await? {
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

    pub async fn set_object_pinned(
        &mut self,
        cancel: &CancellationToken,
        variables_reference: i64,
        pinned: bool,
    ) -> Result<bool> {
        check_cancel(cancel)?;
        let Some(handle) = ObjectHandle::from_variables_reference(variables_reference) else {
            return Ok(false);
        };

        if pinned {
            self.pin_object(cancel, handle).await?;
        } else {
            self.unpin_object(cancel, handle).await?;
        }

        Ok(pinned)
    }

    async fn pin_object(&mut self, cancel: &CancellationToken, handle: ObjectHandle) -> Result<()> {
        check_cancel(cancel)?;
        let Some(object_id) = self.objects.object_id(handle) else {
            return Ok(());
        };

        match cancellable_jdwp(cancel, self.jdwp.object_reference_disable_collection(object_id)).await {
            Ok(()) => {}
            Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
            Err(JdwpError::VmError(code)) if code == ERROR_INVALID_OBJECT => {
                self.objects.mark_invalid_object_id(object_id);
            }
            Err(err) => return Err(err.into()),
        }

        self.objects.pin(handle);
        Ok(())
    }

    async fn unpin_object(&mut self, cancel: &CancellationToken, handle: ObjectHandle) -> Result<()> {
        check_cancel(cancel)?;
        if !self.objects.is_pinned(handle) {
            return Ok(());
        }

        if let Some(object_id) = self.objects.object_id(handle) {
            match cancellable_jdwp(cancel, self.jdwp.object_reference_enable_collection(object_id)).await {
                Ok(()) => {}
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(JdwpError::VmError(code)) if code == ERROR_INVALID_OBJECT => {
                    self.objects.mark_invalid_object_id(object_id);
                }
                Err(err) => return Err(err.into()),
            }
        }

        self.objects.unpin(handle);
        Ok(())
    }
}

fn signature_to_object_type_name(sig: &str) -> Option<String> {
    let mut sig = sig.trim();
    let mut dims = 0;
    while let Some(rest) = sig.strip_prefix('[') {
        dims += 1;
        sig = rest;
    }

    let base = if let Some(inner) = sig.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        inner.replace('/', ".")
    } else {
        return None;
    };

    let mut out = base;
    for _ in 0..dims {
        out.push_str("[]");
    }
    Some(out)
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

#[derive(Clone, Debug)]
struct FormattedValue {
    value: String,
    type_name: Option<String>,
    variables_reference: i64,
    presentation_hint: Option<Value>,
}

impl Debugger {
    async fn format_value(
        &mut self,
        cancel: &CancellationToken,
        value: &JdwpValue,
        static_type: Option<&str>,
        depth: usize,
    ) -> Result<FormattedValue> {
        check_cancel(cancel)?;
        let (display, variables_reference, presentation_hint, runtime_type) =
            self.format_value_display(cancel, value, static_type, depth).await?;

        Ok(FormattedValue {
            value: display,
            type_name: static_type
                .map(|s| s.to_string())
                .or_else(|| value_type_name(value, runtime_type.as_deref())),
            variables_reference,
            presentation_hint,
        })
    }

    async fn format_value_display(
        &mut self,
        cancel: &CancellationToken,
        value: &JdwpValue,
        static_type: Option<&str>,
        depth: usize,
    ) -> Result<(String, i64, Option<Value>, Option<String>)> {
        check_cancel(cancel)?;
        match value {
            JdwpValue::Void => Ok(("void".to_string(), 0, None, None)),
            JdwpValue::Boolean(v) => Ok((v.to_string(), 0, None, None)),
            JdwpValue::Byte(v) => Ok((v.to_string(), 0, None, None)),
            JdwpValue::Short(v) => Ok((v.to_string(), 0, None, None)),
            JdwpValue::Int(v) => Ok((v.to_string(), 0, None, None)),
            JdwpValue::Long(v) => Ok((v.to_string(), 0, None, None)),
            JdwpValue::Float(v) => Ok((trim_float(*v as f64), 0, None, None)),
            JdwpValue::Double(v) => Ok((trim_float(*v), 0, None, None)),
            JdwpValue::Char(v) => Ok((format!("'{}'", decode_java_char(*v)), 0, None, None)),
            JdwpValue::Object { id: 0, .. } => Ok(("null".to_string(), 0, None, None)),
            JdwpValue::Object { id, .. } => self.format_object(cancel, *id, static_type, depth).await,
        }
    }

    async fn format_object(
        &mut self,
        cancel: &CancellationToken,
        object_id: ObjectId,
        static_type: Option<&str>,
        depth: usize,
    ) -> Result<(String, i64, Option<Value>, Option<String>)> {
        check_cancel(cancel)?;
        let fallback_runtime = static_type.unwrap_or("<object>").to_string();

        let runtime_type = match cancellable_jdwp(cancel, self.inspector.runtime_type_name(object_id)).await {
            Ok(t) => t,
            Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
            Err(JdwpError::VmError(code)) if code == ERROR_INVALID_OBJECT => {
                let handle = self.objects.track_object(object_id, &fallback_runtime);
                self.objects.mark_invalid_object_id(object_id);
                let ty = self
                    .objects
                    .runtime_type(handle)
                    .map(simple_type_name)
                    .unwrap_or("<object>");
                return Ok((
                    format!("{ty}{handle} <collected>"),
                    handle.as_variables_reference(),
                    Some(json!({
                        "kind": "virtual",
                        "attributes": ["invalid"],
                    })),
                    Some(fallback_runtime),
                ));
            }
            Err(_err) => fallback_runtime.clone(),
        };

        let handle = self.objects.track_object(object_id, &runtime_type);
        let variables_reference = handle.as_variables_reference();

        if depth >= 2 {
            return Ok((
                format!("{}{handle}", simple_type_name(&runtime_type)),
                variables_reference,
                None,
                Some(runtime_type),
            ));
        }

        let preview = match cancellable_jdwp(cancel, self.inspector.preview_object(object_id)).await {
            Ok(preview) => preview,
            Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
            Err(JdwpError::VmError(code)) if code == ERROR_INVALID_OBJECT => {
                self.objects.mark_invalid_object_id(object_id);
                let ty = self
                    .objects
                    .runtime_type(handle)
                    .map(simple_type_name)
                    .unwrap_or("<object>");
                return Ok((
                    format!("{ty}{handle} <collected>"),
                    variables_reference,
                    Some(json!({
                        "kind": "virtual",
                        "attributes": ["invalid"],
                    })),
                    Some(runtime_type),
                ));
            }
            Err(_err) => {
                return Ok((
                    format!("{}{handle}", simple_type_name(&runtime_type)),
                    variables_reference,
                    Some(json!({ "kind": "data" })),
                    Some(runtime_type),
                ));
            }
        };

        // Ensure the registry sees the most specific runtime type.
        let handle = self.objects.track_object(object_id, &preview.runtime_type);
        let variables_reference = handle.as_variables_reference();

        let runtime_simple = simple_type_name(&preview.runtime_type).to_string();
        let value = match preview.kind {
            ObjectKindPreview::Plain => format!("{runtime_simple}{handle}"),
            ObjectKindPreview::String { value } => {
                let escaped = escape_java_string(&value, 80);
                format!("\"{escaped}\"{handle}")
            }
            ObjectKindPreview::PrimitiveWrapper { ref value } => {
                let inner = self.format_inline(cancel, value, depth + 1).await?;
                format!("{runtime_simple}{handle}({inner})")
            }
            ObjectKindPreview::Array {
                element_type,
                length,
                sample,
            } => {
                let sample = self.format_sample_list(cancel, &sample, depth + 1).await?;
                let element = simple_type_name(&element_type);
                format!("{element}[{length}]{handle} {{{sample}}}")
            }
            ObjectKindPreview::List { size, sample } => {
                let sample = self.format_sample_list(cancel, &sample, depth + 1).await?;
                format!("{runtime_simple}{handle}(size={size}) [{sample}]")
            }
            ObjectKindPreview::Set { size, sample } => {
                let sample = self.format_sample_list(cancel, &sample, depth + 1).await?;
                format!("{runtime_simple}{handle}(size={size}) [{sample}]")
            }
            ObjectKindPreview::Map { size, sample } => {
                let sample = self.format_sample_map(cancel, &sample, depth + 1).await?;
                format!("{runtime_simple}{handle}(size={size}) {{{sample}}}")
            }
            ObjectKindPreview::Optional { value } => match value {
                Some(inner) => {
                    let inner = self.format_inline(cancel, &inner, depth + 1).await?;
                    format!("{runtime_simple}{handle}[{inner}]")
                }
                None => format!("{runtime_simple}{handle}.empty"),
            },
            ObjectKindPreview::Stream { size } => match size {
                Some(size) => format!("{runtime_simple}{handle}(size={size})"),
                None => format!("{runtime_simple}{handle}(size=unknown)"),
            },
        };

        Ok((
            value,
            variables_reference,
            Some(json!({ "kind": "data" })),
            Some(preview.runtime_type),
        ))
    }

    async fn format_inline(
        &mut self,
        cancel: &CancellationToken,
        value: &JdwpValue,
        depth: usize,
    ) -> Result<String> {
        let (display, _ref, _hint, _rt) =
            Box::pin(self.format_value_display(cancel, value, None, depth)).await?;
        Ok(display)
    }

    async fn format_sample_list(
        &mut self,
        cancel: &CancellationToken,
        sample: &[JdwpValue],
        depth: usize,
    ) -> Result<String> {
        check_cancel(cancel)?;
        let mut out = Vec::new();
        for value in sample.iter().take(3) {
            check_cancel(cancel)?;
            out.push(self.format_inline(cancel, value, depth).await?);
        }
        Ok(out.join(", "))
    }

    async fn format_sample_map(
        &mut self,
        cancel: &CancellationToken,
        sample: &[(JdwpValue, JdwpValue)],
        depth: usize,
    ) -> Result<String> {
        check_cancel(cancel)?;
        let mut out = Vec::new();
        for (k, v) in sample.iter().take(3) {
            check_cancel(cancel)?;
            let key = self.format_inline(cancel, k, depth).await?;
            let val = self.format_inline(cancel, v, depth).await?;
            out.push(format!("{key}={val}"));
        }
        Ok(out.join(", "))
    }
}

fn variable_json(
    name: String,
    value: String,
    type_name: Option<String>,
    variables_reference: i64,
    evaluate_name: Option<String>,
    presentation_hint: Option<Value>,
) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("name".to_string(), json!(name));
    obj.insert("value".to_string(), json!(value));
    obj.insert("variablesReference".to_string(), json!(variables_reference));
    if let Some(type_name) = type_name {
        obj.insert("type".to_string(), json!(type_name));
    }
    if let Some(evaluate_name) = evaluate_name {
        obj.insert("evaluateName".to_string(), json!(evaluate_name));
    }
    if let Some(hint) = presentation_hint {
        obj.insert("presentationHint".to_string(), hint);
    }
    Value::Object(obj)
}

fn invalid_collected_variable() -> Value {
    variable_json(
        "<collected>".to_string(),
        "<collected>".to_string(),
        None,
        0,
        None,
        Some(json!({
            "kind": "virtual",
            "attributes": ["invalid"],
        })),
    )
}

fn signature_to_type_name(signature: &str) -> String {
    let mut sig = signature;
    let mut dims = 0usize;
    while let Some(rest) = sig.strip_prefix('[') {
        dims += 1;
        sig = rest;
    }

    let base = if let Some(class) = sig.strip_prefix('L').and_then(|s| s.strip_suffix(';')) {
        class.replace('/', ".")
    } else {
        match sig.as_bytes().first().copied() {
            Some(b'B') => "byte".to_string(),
            Some(b'C') => "char".to_string(),
            Some(b'D') => "double".to_string(),
            Some(b'F') => "float".to_string(),
            Some(b'I') => "int".to_string(),
            Some(b'J') => "long".to_string(),
            Some(b'S') => "short".to_string(),
            Some(b'Z') => "boolean".to_string(),
            Some(b'V') => "void".to_string(),
            _ => "<unknown>".to_string(),
        }
    };

    let mut out = base;
    for _ in 0..dims {
        out.push_str("[]");
    }
    out
}

fn simple_type_name(full: &str) -> &str {
    let tail = full.rsplit('.').next().unwrap_or(full);
    tail.rsplit('$').next().unwrap_or(tail)
}

fn value_type_name(value: &JdwpValue, runtime_type: Option<&str>) -> Option<String> {
    Some(match value {
        JdwpValue::Void => "void".to_string(),
        JdwpValue::Boolean(_) => "boolean".to_string(),
        JdwpValue::Byte(_) => "byte".to_string(),
        JdwpValue::Short(_) => "short".to_string(),
        JdwpValue::Int(_) => "int".to_string(),
        JdwpValue::Long(_) => "long".to_string(),
        JdwpValue::Float(_) => "float".to_string(),
        JdwpValue::Double(_) => "double".to_string(),
        JdwpValue::Char(_) => "char".to_string(),
        JdwpValue::Object { id: 0, .. } => return None,
        JdwpValue::Object { .. } => runtime_type.unwrap_or("<object>").to_string(),
    })
}

fn trim_float(value: f64) -> String {
    if value.is_nan() || value.is_infinite() {
        return value.to_string();
    }
    if value.fract() == 0.0 {
        format!("{:.0}", value)
    } else {
        value.to_string()
    }
}

fn decode_java_char(code_unit: u16) -> char {
    std::char::from_u32(code_unit as u32).unwrap_or('\u{FFFD}')
}

fn escape_java_string(input: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut chars = input.chars();
    let mut used = 0usize;
    while let Some(ch) = chars.next() {
        if used >= max_len {
            out.push('…');
            break;
        }
        match ch {
            '\\' => out.push_str("\\\\"),
            '\"' => out.push_str("\\\\\""),
            '\n' => out.push_str("\\\\n"),
            '\r' => out.push_str("\\\\r"),
            '\t' => out.push_str("\\\\t"),
            _ => out.push(ch),
        }
        used += 1;
    }
    out
}

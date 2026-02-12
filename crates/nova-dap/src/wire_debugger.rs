use std::{
    collections::{HashMap, HashSet},
    future::Future,
    hash::Hash,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use serde::Deserialize;
use serde_json::json;
use thiserror::Error;
use tokio::time::{sleep, Instant};
use tokio_util::sync::CancellationToken;

use nova_core::Line;
use nova_db::InMemoryFileStore;
use nova_jdwp::wire::{
    inspect::{Inspector, ObjectKindPreview, ERROR_INVALID_OBJECT},
    ClassInfo, EventModifier, FrameId, FrameInfo, JdwpClient, JdwpError, JdwpEvent, JdwpValue,
    LineTable, Location, MethodInfo, ObjectId, ReferenceTypeId, ThreadId, VariableInfo,
};

use crate::breakpoints::map_line_breakpoints;
use crate::eval_context::EvalOptions;
use crate::object_registry::{ObjectHandle, ObjectRegistry, OBJECT_HANDLE_BASE, PINNED_SCOPE_REF};

/// Internal representation of a DAP `SourceBreakpoint`.
///
/// This is intentionally minimal and only captures the fields we need for the wire-level adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BreakpointSpec {
    pub line: i32,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

/// Internal representation of a DAP `FunctionBreakpoint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FunctionBreakpointSpec {
    pub name: String,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

/// Internal representation of a DAP `DataBreakpoint` (watchpoint).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DataBreakpointSpec {
    pub data_id: String,
    #[serde(default)]
    pub access_type: Option<String>,
    #[serde(default)]
    pub hit_condition: Option<String>,
    #[serde(default)]
    pub condition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakpointDisposition {
    Stop,
    Continue,
    Log { message: String },
}

const ARRAY_CHILD_SAMPLE: i64 = 25;
/// Hard cap on the number of child variables we will return in one response.
///
/// DAP clients can request arbitrarily large pages via `variables` `start`/`count`.
/// Clamp to avoid OOMs when inspecting huge arrays or object graphs.
const VARIABLES_PAGE_LIMIT: i64 = 1000;

#[derive(Debug, Error)]
pub enum DebuggerError {
    #[error(transparent)]
    Jdwp(#[from] JdwpError),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Stream-debug evaluation exceeded `StreamDebugConfig::max_total_time`.
    #[error("evaluation exceeded time limit")]
    Timeout,
}

pub type Result<T> = std::result::Result<T, DebuggerError>;

/// JDWP suspend policy used for stop events (breakpoints, stepping, exceptions).
///
/// We use `EVENT_THREAD` so only the thread that triggered the event is suspended.
/// The wire DAP server reports `stopped.body.allThreadsStopped = false` for these
/// events.
const JDWP_SUSPEND_POLICY_EVENT_THREAD: u8 = 1;
const JDWP_SUSPEND_POLICY_NONE: u8 = 0;

#[derive(Debug, Clone)]
pub struct AttachArgs {
    pub host: IpAddr,
    pub port: u16,
    pub source_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub enum StepDepth {
    Into,
    Over,
    Out,
}

const SMART_STEP_MAX_RESUMES: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmartStepPhase {
    Into,
    Out,
}

#[derive(Debug, Clone)]
struct SmartStepIntoState {
    thread: ThreadId,
    origin_class_id: ReferenceTypeId,
    origin_method_id: u64,
    origin_line: i32,
    target_ordinal: i64,
    seen: i64,
    phase: SmartStepPhase,
    remaining_resumes: u32,
}

#[derive(Debug, Clone, Copy)]
struct ActiveStepRequest {
    request_id: i32,
    depth: StepDepth,
}

#[derive(Debug, Clone)]
pub(crate) enum VmStoppedValue {
    Return(JdwpValue),
    Expression(JdwpValue),
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

/// A requested DAP `SourceBreakpoint` tracked by the adapter.
///
/// DAP breakpoint IDs are allocated by the adapter and must remain stable for a breakpoint across
/// "pending → verified" transitions (e.g. when a class is loaded after `setBreakpoints`).
#[derive(Debug, Clone)]
struct RequestedSourceBreakpoint {
    id: i64,
    spec: BreakpointSpec,
}

/// A requested DAP `FunctionBreakpoint` tracked by the adapter.
#[derive(Debug, Clone)]
struct RequestedFunctionBreakpoint {
    id: i64,
    spec: FunctionBreakpointSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResolvedLocation {
    line: i32,
    location: Location,
}

#[derive(Debug, Clone)]
struct BreakpointMetadata {
    condition: Option<String>,
    hit_condition: Option<String>,
    log_message: Option<String>,
    hit_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FrameKey {
    thread: ThreadId,
    frame_id: u64,
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
    StaticFields(ReferenceTypeId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum VarKey {
    FrameLocals(FrameKey),
    StaticFields(ReferenceTypeId),
}

struct HandleTable<K, T> {
    next: i64,
    epoch: i64,
    by_id: HashMap<i64, T>,
    by_key: HashMap<K, i64>,
}

#[derive(Debug, Clone)]
struct ExceptionStopContext {
    exception: ObjectId,
    catch_location: Option<Location>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Breakpoint,
    Step,
    Exception,
}

impl<K, T> Default for HandleTable<K, T> {
    fn default() -> Self {
        Self {
            next: 0,
            epoch: 0,
            by_id: HashMap::new(),
            by_key: HashMap::new(),
        }
    }
}

impl<K, T> HandleTable<K, T>
where
    K: Eq + Hash,
{
    fn intern(&mut self, key: K, value: T) -> i64 {
        if let Some(id) = self.by_key.get(&key).copied() {
            // Refresh the stored value so callers can update fields like `location`
            // without forcing handle invalidation.
            self.by_id.insert(id, value);
            return id;
        }

        self.next += 1;
        // Encode a 1-bit epoch in the low bit of the ID so that handles can be reset each
        // stop without risking collisions with IDs from the immediately previous stop.
        //
        // This keeps IDs small (important for DAP clients that treat ids as `i32`) and
        // ensures ids remain bounded over long stepping sessions.
        let id = (self.next << 1) | self.epoch;
        self.by_id.insert(id, value);
        self.by_key.insert(key, id);
        id
    }

    fn get(&self, id: i64) -> Option<&T> {
        self.by_id.get(&id)
    }

    fn clear(&mut self) {
        let had_handles = self.next != 0;
        if had_handles {
            self.epoch ^= 1;
        }

        self.next = 0;
        self.by_id = HashMap::new();
        self.by_key = HashMap::new();
    }
}

pub struct Debugger {
    jdwp: JdwpClient,
    inspector: Inspector,
    objects: ObjectRegistry,
    internal_eval_threads: Arc<StdMutex<HashSet<ThreadId>>>,
    next_breakpoint_id: i64,
    breakpoints: HashMap<String, Vec<BreakpointEntry>>,
    requested_breakpoints: HashMap<String, Vec<RequestedSourceBreakpoint>>,
    /// Tracks source breakpoints that were returned as unverified because the class was not yet
    /// loaded.
    ///
    /// When we later observe a JDWP `ClassPrepare` for the corresponding class, we attempt to
    /// install the pending breakpoints. Successful installs are surfaced to the DAP client via
    /// `breakpoint` events (see [`Debugger::take_breakpoint_updates`]).
    pending_breakpoints: HashMap<String, HashSet<i64>>,
    function_breakpoints: Vec<BreakpointEntry>,
    requested_function_breakpoints: Vec<RequestedFunctionBreakpoint>,
    /// Tracks function breakpoints that were returned as unverified because the class was not yet
    /// loaded.
    pending_function_breakpoints: HashSet<i64>,
    breakpoint_metadata: HashMap<i32, BreakpointMetadata>,
    /// Pending DAP breakpoint updates emitted after deferred installation (class prepare).
    ///
    /// Drained by the wire DAP server event loop after [`Debugger::handle_vm_event`] to emit
    /// DAP `breakpoint` events so clients can update the breakpoint UI from unverified → verified.
    breakpoint_updates: Vec<Value>,
    class_prepare_request: Option<i32>,
    exception_requests: Vec<i32>,
    watchpoint_requests: Vec<(u8, i32)>,
    active_step_requests: HashMap<ThreadId, ActiveStepRequest>,
    active_method_exit_requests: HashMap<ThreadId, i32>,
    pending_return_values: HashMap<ThreadId, JdwpValue>,
    smart_step_into: Option<SmartStepIntoState>,
    last_stop_reason: HashMap<ThreadId, StopReason>,
    exception_stop_context: HashMap<ThreadId, ExceptionStopContext>,
    throwable_detail_message_field: Option<Option<u64>>,
    source_roots: Vec<PathBuf>,

    /// Mapping from JDWP `ReferenceType.SourceFile` (usually just `Main.java`) to the
    /// best-effort full path provided by the DAP client.
    ///
    /// The JVM typically does not expose absolute source paths over JDWP, but DAP
    /// clients expect stack frames to contain a resolvable `source.path`. We can
    /// recover that by remembering the source paths passed to `setBreakpoints`.
    source_paths: HashMap<String, String>,
    source_cache: HashMap<ReferenceTypeId, String>,
    signature_cache: HashMap<ReferenceTypeId, String>,
    resolved_source_paths: HashMap<ReferenceTypeId, PathBuf>,
    methods_cache: HashMap<ReferenceTypeId, Vec<MethodInfo>>,
    line_table_cache: HashMap<(ReferenceTypeId, u64), LineTable>,

    frame_handles: HandleTable<FrameKey, FrameHandle>,
    var_handles: HandleTable<VarKey, VarRef>,
}

#[derive(Debug)]
pub(crate) struct InternalEvaluationGuard {
    thread: ThreadId,
    threads: Arc<StdMutex<HashSet<ThreadId>>>,
}

impl Drop for InternalEvaluationGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.threads.lock() {
            guard.remove(&self.thread);
        }
    }
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
        let AttachArgs {
            host,
            port,
            source_roots,
        } = args;
        let addr = SocketAddr::new(host, port);
        let jdwp = JdwpClient::connect(addr).await?;

        let mut dbg = Self {
            inspector: Inspector::new(jdwp.clone()),
            objects: ObjectRegistry::new(),
            jdwp,
            internal_eval_threads: Arc::new(StdMutex::new(HashSet::new())),
            next_breakpoint_id: 1,
            breakpoints: HashMap::new(),
            requested_breakpoints: HashMap::new(),
            pending_breakpoints: HashMap::new(),
            function_breakpoints: Vec::new(),
            requested_function_breakpoints: Vec::new(),
            pending_function_breakpoints: HashSet::new(),
            breakpoint_metadata: HashMap::new(),
            breakpoint_updates: Vec::new(),
            class_prepare_request: None,
            exception_requests: Vec::new(),
            watchpoint_requests: Vec::new(),
            active_step_requests: HashMap::new(),
            active_method_exit_requests: HashMap::new(),
            pending_return_values: HashMap::new(),
            smart_step_into: None,
            last_stop_reason: HashMap::new(),
            exception_stop_context: HashMap::new(),
            throwable_detail_message_field: None,
            source_roots,
            source_paths: HashMap::new(),
            source_cache: HashMap::new(),
            signature_cache: HashMap::new(),
            resolved_source_paths: HashMap::new(),
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

        dbg.jdwp.event_request_set(6, 0, Vec::new()).await?;
        dbg.jdwp.event_request_set(7, 0, Vec::new()).await?;

        Ok(dbg)
    }

    /// Attach to a JDWP socket, retrying with exponential backoff until it is available.
    ///
    /// This is primarily used by `launch` flows where a build tool or JVM is spawned in
    /// debug mode and the JDWP socket is not immediately accepting connections.
    pub async fn attach_with_retry(args: AttachArgs, timeout: Duration) -> Result<Self> {
        if timeout == Duration::ZERO {
            return Self::attach(args).await;
        }

        let start = Instant::now();
        let mut backoff = Duration::from_millis(50);
        let max_backoff = Duration::from_secs(1);

        loop {
            match Self::attach(args.clone()).await {
                Ok(dbg) => return Ok(dbg),
                Err(err) => {
                    let elapsed = start.elapsed();
                    if elapsed >= timeout || !is_retryable_attach_error(&err) {
                        return Err(err);
                    }

                    let remaining = timeout.saturating_sub(elapsed);
                    sleep(backoff.min(remaining)).await;
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<JdwpEvent> {
        self.jdwp.subscribe_events()
    }

    pub fn jdwp_client(&self) -> JdwpClient {
        self.jdwp.clone()
    }

    fn alloc_breakpoint_id(&mut self) -> i64 {
        // DAP breakpoint IDs are adapter-allocated and must be stable for the lifetime of the
        // breakpoint (in particular across "pending → verified" transitions).
        let id = self.next_breakpoint_id;
        self.next_breakpoint_id = self.next_breakpoint_id.saturating_add(1);
        // Always return a positive id so clients can treat it as a stable handle.
        if id <= 0 { 1 } else { id }
    }

    pub(crate) fn begin_internal_evaluation(&self, thread: ThreadId) -> InternalEvaluationGuard {
        if let Ok(mut guard) = self.internal_eval_threads.lock() {
            guard.insert(thread);
        }
        InternalEvaluationGuard {
            thread,
            threads: self.internal_eval_threads.clone(),
        }
    }

    pub(crate) fn is_internal_evaluation_thread(&self, thread: ThreadId) -> bool {
        self.internal_eval_threads
            .lock()
            .is_ok_and(|guard| guard.contains(&thread))
    }

    /// Resolve a DAP `frameId` to the underlying JDWP thread/frame identifiers.
    ///
    /// This is intentionally synchronous (no `.await`) so call sites can snapshot
    /// the frame identity while holding the outer `Debugger` mutex, then drop the
    /// lock before performing potentially long-running JDWP operations (e.g.
    /// `InvokeMethod`).
    pub(crate) fn jdwp_frame(&self, frame_id: i64) -> Option<(ThreadId, FrameId)> {
        let handle = self.frame_handles.get(frame_id)?;
        Some((handle.thread, handle.frame_id))
    }
    pub async fn capabilities(&self) -> nova_jdwp::wire::types::JdwpCapabilitiesNew {
        self.jdwp.capabilities().await
    }

    pub fn jdwp_shutdown_token(&self) -> CancellationToken {
        self.jdwp.shutdown_token()
    }

    pub fn breakpoint_is_logpoint(&self, request_id: i32) -> bool {
        self.breakpoint_metadata
            .get(&request_id)
            .is_some_and(|meta| meta.log_message.is_some())
    }

    /// Drain any pending "breakpoint became verified" updates recorded during class-prepare
    /// handling.
    pub(crate) fn take_breakpoint_updates(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.breakpoint_updates)
    }

    fn invalidate_handles(&mut self) {
        self.frame_handles.clear();
        self.var_handles.clear();
    }

    pub async fn disconnect(&mut self) {
        self.jdwp.shutdown();
    }

    /// Detach from the target VM by issuing JDWP `VirtualMachine.Dispose` (1/6).
    ///
    /// This is distinct from [`Debugger::disconnect`], which is a local-only shutdown of the JDWP
    /// client. `Dispose` requests a clean detach from the debuggee when supported.
    pub async fn detach(&self, cancel: &CancellationToken) -> Result<()> {
        check_cancel(cancel)?;
        let res = cancellable_jdwp(cancel, self.jdwp.virtual_machine_dispose()).await;
        // Always shut down locally to unblock any pending requests/tasks, even if the dispose
        // fails because the debuggee disconnected.
        self.jdwp.shutdown();
        res?;
        Ok(())
    }

    /// Attempt to terminate the target VM via JDWP `VirtualMachine.Exit` (1/10).
    ///
    /// This is primarily used for attach sessions where the adapter does not own a spawned
    /// process handle that it can kill directly.
    pub async fn terminate_vm(&self, cancel: &CancellationToken, exit_code: i32) -> Result<()> {
        check_cancel(cancel)?;

        // Best-effort: the target VM may disconnect immediately after accepting the exit request
        // (and may not deliver a reply). Treat connection-termination errors as success.
        let exit_res = cancellable_jdwp(cancel, self.jdwp.virtual_machine_exit(exit_code)).await;

        // Ensure local shutdown even if `Exit` fails so downstream tasks observe termination.
        self.jdwp.shutdown();

        match exit_res {
            Ok(()) => Ok(()),
            Err(JdwpError::ConnectionClosed) => Ok(()),
            Err(JdwpError::Timeout) => Ok(()),
            Err(err) => Err(err.into()),
        }
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
        start_frame: Option<i64>,
        levels: Option<i64>,
    ) -> Result<(Vec<serde_json::Value>, Option<i64>)> {
        // Thread ids originate from JDWP `ObjectId` values, which are opaque 64-bit numbers.
        // Represent them in DAP as `i64` (required by the protocol) using a lossless bit-cast:
        // `u64 -> i64` and back via `as` preserves the underlying bits even if the sign flips.
        let thread = dap_thread_id as ThreadId;

        check_cancel(cancel)?;

        let start_frame = start_frame.unwrap_or(0);
        if start_frame < 0 {
            return Err(DebuggerError::InvalidRequest(format!(
                "startFrame must be >= 0 (got {start_frame})"
            )));
        }
        let Ok(start) = i32::try_from(start_frame) else {
            return Err(DebuggerError::InvalidRequest(format!(
                "startFrame is too large: {start_frame}"
            )));
        };

        let mut length = match levels {
            Some(levels) => {
                if levels < 0 {
                    return Err(DebuggerError::InvalidRequest(format!(
                        "levels must be >= 0 (got {levels})"
                    )));
                }
                let Ok(levels) = i32::try_from(levels) else {
                    return Err(DebuggerError::InvalidRequest(format!(
                        "levels is too large: {levels}"
                    )));
                };
                levels
            }
            None => -1,
        };

        // `ThreadReference.FrameCount` is significantly cheaper than fetching the full
        // `ThreadReference.Frames` list and allows us to compute an accurate DAP `totalFrames`
        // for paged stackTrace requests.
        let total_frames =
            match cancellable_jdwp(cancel, self.jdwp.thread_frame_count(thread)).await {
                Ok(count) => Some(count as i64),
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(_) => None,
            };

        // Some JVMs treat an oversized `length` as `INVALID_LENGTH` instead of clamping.
        // Clamp the requested `levels` against the known frame count when available to avoid
        // turning a benign DAP stackTrace request into an error.
        if let Some(total_frames) = total_frames {
            if start_frame >= total_frames {
                return Ok((Vec::new(), Some(total_frames)));
            }

            if length >= 0 {
                let remaining = (total_frames - start_frame).max(0);
                // `total_frames` originates from an `i32` in JDWP, so this cast is safe.
                length = length.min(remaining as i32);
            }
        }

        // Some JVMs treat an oversized `length` as `INVALID_LENGTH` instead of clamping.
        // JDWP allows `length = -1` to request all frames starting at `start`.
        let frames = if length == 0 {
            Vec::new()
        } else {
            match cancellable_jdwp(cancel, self.jdwp.frames(thread, start, length)).await {
                Ok(frames) => frames,
                Err(err @ JdwpError::VmError(_)) if length >= 0 => {
                    // Best-effort fallback: if a VM rejects the requested `length`, retry using
                    // `-1` (all frames) and then truncate locally.
                    let frames =
                        match cancellable_jdwp(cancel, self.jdwp.frames(thread, start, -1)).await {
                            Ok(frames) => frames,
                            Err(_) => return Err(err.into()),
                        };

                    frames.into_iter().take(length as usize).collect()
                }
                Err(err) => return Err(err.into()),
            }
        };
        let mut out = Vec::with_capacity(frames.len());
        for frame in frames {
            check_cancel(cancel)?;
            let frame_id = self.alloc_frame_handle(thread, &frame);
            let name = self
                .method_name(cancel, frame.location.class_id, frame.location.method_id)
                .await?
                .unwrap_or_else(|| "frame".to_string());
            let source_file = match self.source_file(cancel, frame.location.class_id).await {
                Ok(name) => Some(name),
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(_) => None,
            };
            let source = match source_file {
                Some(source_file) => {
                    let path = match self
                        .resolve_source_path(cancel, frame.location.class_id, &source_file)
                        .await
                    {
                        Ok(Some(path)) => path.to_string_lossy().to_string(),
                        Ok(None) => source_file.clone(),
                        Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                        Err(_) => source_file.clone(),
                    };
                    Some(json!({"name": source_file, "path": path}))
                }
                None => None,
            };
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

        // If `ThreadReference.FrameCount` is not available, only report `totalFrames` when we can
        // prove it's exact from the windowed `Frames` response.
        //
        // - When `levels` is omitted (`length = -1`), the VM returns all frames from `startFrame`.
        //   This makes `totalFrames = startFrame + returned` exact, except when `startFrame` is
        //   already past the end of the stack (returned=0). In that case we don't know the real
        //   total without an explicit frame count, so omit `totalFrames`.
        // - When `levels` is provided, `totalFrames` is exact only if we received fewer frames
        //   than requested *and* we received at least one frame (or we started from frame 0, in
        //   which case an empty result implies there are no frames).
        let returned = out.len() as i64;
        let total_frames = match total_frames {
            Some(total) => Some(total),
            None => match levels {
                None if returned > 0 || start_frame == 0 => Some(start_frame + returned),
                Some(levels) if returned < levels && (returned > 0 || start_frame == 0) => {
                    Some(start_frame + returned)
                }
                _ => None,
            },
        };

        Ok((out, total_frames))
    }

    pub async fn step_in_targets(
        &mut self,
        cancel: &CancellationToken,
        frame_id: i64,
    ) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;

        let Some(frame) = self.frame_handles.get(frame_id).copied() else {
            return Err(DebuggerError::InvalidRequest(format!(
                "unknown frameId {frame_id}"
            )));
        };

        let source_file = self.source_file(cancel, frame.location.class_id).await?;
        let Some(path) = self
            .resolve_source_path(cancel, frame.location.class_id, &source_file)
            .await?
        else {
            return Ok(Vec::new());
        };

        let line = self
            .line_number(
                cancel,
                frame.location.class_id,
                frame.location.method_id,
                frame.location.index,
            )
            .await
            .unwrap_or(1)
            .max(1) as u32;
        let line_idx = (line - 1) as usize;

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => return Ok(Vec::new()),
        };

        let line_text = text.lines().nth(line_idx).unwrap_or("");
        let mut targets = crate::smart_step_into::enumerate_step_in_targets_in_line(line_text);
        for target in &mut targets {
            target.line = Some(line);
            target.end_line = Some(line);
        }
        Ok(targets.into_iter().map(|t| json!(t)).collect())
    }

    pub async fn step_in_target(
        &mut self,
        cancel: &CancellationToken,
        dap_thread_id: i64,
        target_id: i64,
    ) -> Result<()> {
        self.smart_step_into = None;
        check_cancel(cancel)?;

        if target_id < 0 {
            return self.step(cancel, dap_thread_id, StepDepth::Into).await;
        }

        let thread: ThreadId = dap_thread_id as ThreadId;
        let frame = cancellable_jdwp(cancel, self.jdwp.frames(thread, 0, 1))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| DebuggerError::InvalidRequest("thread has no frames".to_string()))?;

        let origin_line = match self
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

        let frame_handle = self.alloc_frame_handle(thread, &frame);
        let targets = self.step_in_targets(cancel, frame_handle).await?;
        let Some(target_ordinal) = targets.iter().position(|target| {
            target
                .get("id")
                .and_then(|value| value.as_i64())
                .is_some_and(|id| id == target_id)
        }) else {
            return self.step(cancel, dap_thread_id, StepDepth::Into).await;
        };

        self.smart_step_into = Some(SmartStepIntoState {
            thread,
            origin_class_id: frame.location.class_id,
            origin_method_id: frame.location.method_id,
            origin_line,
            target_ordinal: target_ordinal as i64,
            seen: 0,
            phase: SmartStepPhase::Into,
            remaining_resumes: SMART_STEP_MAX_RESUMES,
        });

        let result = self.step(cancel, dap_thread_id, StepDepth::Into).await;
        if result.is_err() {
            self.smart_step_into = None;
        }
        result
    }

    pub async fn maybe_continue_smart_step(
        &mut self,
        cancel: &CancellationToken,
        event: &JdwpEvent,
    ) -> bool {
        let Some(mut state) = self.smart_step_into.take() else {
            return false;
        };

        let mut keep_state = true;
        let mut suppress_stopped_event = false;

        if state.remaining_resumes == 0 {
            keep_state = false;
        }

        match event {
            JdwpEvent::Breakpoint { thread, .. } | JdwpEvent::Exception { thread, .. }
                if *thread == state.thread =>
            {
                keep_state = false;
            }
            JdwpEvent::SingleStep {
                thread, location, ..
            } if *thread == state.thread => {
                let in_origin = location.class_id == state.origin_class_id
                    && location.method_id == state.origin_method_id;

                match state.phase {
                    SmartStepPhase::Into => {
                        if !in_origin {
                            if state.seen == state.target_ordinal {
                                keep_state = false;
                            } else {
                                state.seen = state.seen.saturating_add(1);
                                state.phase = SmartStepPhase::Out;
                                if state.remaining_resumes == 0 {
                                    keep_state = false;
                                } else {
                                    state.remaining_resumes -= 1;
                                    match self.step(cancel, *thread as i64, StepDepth::Out).await {
                                        Ok(()) => suppress_stopped_event = true,
                                        Err(_) => keep_state = false,
                                    }
                                }
                                if !suppress_stopped_event {
                                    keep_state = false;
                                }
                            }
                        } else {
                            let line = match self
                                .line_number(
                                    cancel,
                                    location.class_id,
                                    location.method_id,
                                    location.index,
                                )
                                .await
                            {
                                Ok(line) => line,
                                Err(JdwpError::Cancelled) => {
                                    keep_state = false;
                                    1
                                }
                                Err(_) => 1,
                            };

                            if line != state.origin_line {
                                keep_state = false;
                            } else if state.remaining_resumes == 0 {
                                keep_state = false;
                            } else {
                                state.remaining_resumes -= 1;
                                match self.step(cancel, *thread as i64, StepDepth::Into).await {
                                    Ok(()) => suppress_stopped_event = true,
                                    Err(_) => keep_state = false,
                                }
                            }
                        }
                    }
                    SmartStepPhase::Out => {
                        if !in_origin {
                            keep_state = false;
                        } else {
                            let line = match self
                                .line_number(
                                    cancel,
                                    location.class_id,
                                    location.method_id,
                                    location.index,
                                )
                                .await
                            {
                                Ok(line) => line,
                                Err(JdwpError::Cancelled) => {
                                    keep_state = false;
                                    1
                                }
                                Err(_) => 1,
                            };

                            if line != state.origin_line {
                                keep_state = false;
                            } else if state.remaining_resumes == 0 {
                                keep_state = false;
                            } else {
                                state.phase = SmartStepPhase::Into;
                                state.remaining_resumes -= 1;
                                match self.step(cancel, *thread as i64, StepDepth::Into).await {
                                    Ok(()) => suppress_stopped_event = true,
                                    Err(_) => keep_state = false,
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        if keep_state {
            self.smart_step_into = Some(state);
        }

        suppress_stopped_event
    }

    pub fn scopes(&mut self, frame_id: i64) -> Result<Vec<serde_json::Value>> {
        let Some(frame) = self.frame_handles.get(frame_id).copied() else {
            return Err(DebuggerError::InvalidRequest(format!(
                "unknown frameId {frame_id}"
            )));
        };

        let locals_ref = self.var_handles.intern(
            VarKey::FrameLocals(FrameKey {
                thread: frame.thread,
                frame_id: frame.frame_id,
            }),
            VarRef::FrameLocals(frame),
        );

        let static_ref = self.var_handles.intern(
            VarKey::StaticFields(frame.location.class_id),
            VarRef::StaticFields(frame.location.class_id),
        );

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
            json!({
                "name": "Static",
                "variablesReference": static_ref,
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

        if let Some(handle) = ObjectHandle::from_variables_reference(variables_reference) {
            return if self.objects.object_id(handle).is_some() {
                self.object_variables(cancel, handle, start, count).await
            } else {
                Ok(vec![evicted_variable()])
            };
        }

        let Some(var_ref) = self.var_handles.get(variables_reference).cloned() else {
            return Ok(Vec::new());
        };

        match var_ref {
            VarRef::FrameLocals(frame) => self.locals_variables(cancel, &frame).await,
            VarRef::StaticFields(class_id) => self.static_variables(cancel, class_id).await,
        }
    }

    pub(crate) fn breakpoint_locations(
        source_path: &str,
        line: i64,
        end_line: Option<i64>,
    ) -> Vec<serde_json::Value> {
        if source_path.is_empty() {
            return Vec::new();
        }

        let file_path =
            std::fs::canonicalize(source_path).unwrap_or_else(|_| PathBuf::from(source_path));
        let text = match std::fs::read_to_string(&file_path) {
            Ok(text) => text,
            Err(_) => return Vec::new(),
        };

        let mut sites = nova_ide::semantics::collect_breakpoint_sites(&text);
        sites.sort_by_key(|site| site.line);

        let mut start = line.max(1);
        let mut end = end_line.unwrap_or(line).max(1);
        if end < start {
            std::mem::swap(&mut start, &mut end);
        }

        let mut last_line: Option<Line> = None;
        sites
            .into_iter()
            .filter_map(|site| {
                let site_line = site.line as i64;
                if site_line < start || site_line > end {
                    return None;
                }

                if last_line == Some(site.line) {
                    return None;
                }
                last_line = Some(site.line);

                Some(json!({
                    "line": site_line,
                    "column": 1,
                }))
            })
            .collect()
    }

    pub(crate) async fn set_breakpoints(
        &mut self,
        cancel: &CancellationToken,
        source_path: &str,
        breakpoints: Vec<BreakpointSpec>,
    ) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;

        let file_name = Path::new(source_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(source_path)
            .to_string();

        if !source_path.is_empty() {
            let full_path =
                std::fs::canonicalize(source_path).unwrap_or_else(|_| PathBuf::from(source_path));
            self.source_paths
                .insert(file_name.clone(), full_path.to_string_lossy().to_string());
        }

        let file_path = if source_path.is_empty() {
            PathBuf::from(source_path)
        } else {
            std::fs::canonicalize(source_path).unwrap_or_else(|_| PathBuf::from(source_path))
        };
        let file_key = if source_path.is_empty() {
            file_name.clone()
        } else {
            file_path.to_string_lossy().to_string()
        };
        let file = file_key.clone();

        // Clear any pending markers from prior `setBreakpoints` calls; if we still can't resolve
        // classes after this call we'll re-populate it below.
        self.pending_breakpoints.remove(&file);

        struct BreakpointRequest {
            id: i64,
            requested_line: i32,
            spec: BreakpointSpec,
        }

        let resolved_lines = if file_path.is_file() {
            std::fs::read_to_string(&file_path)
                .ok()
                .map(|text| {
                    let mut db = InMemoryFileStore::new();
                    let file_id = db.file_id_for_path(&file_path);
                    db.set_file_text(file_id, text);

                    let requested_lines: Vec<Line> = breakpoints
                        .iter()
                        .map(|bp| bp.line.max(1) as Line)
                        .collect();
                    map_line_breakpoints(&db, file_id, &requested_lines)
                        .into_iter()
                        .map(|resolved| resolved.resolved_line as i32)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| breakpoints.iter().map(|bp| bp.line).collect())
        } else {
            breakpoints.iter().map(|bp| bp.line).collect()
        };

        let requests: Vec<BreakpointRequest> = breakpoints
            .into_iter()
            .zip(resolved_lines.into_iter())
            .map(|(bp, resolved_line)| {
                let id = self.alloc_breakpoint_id();
                BreakpointRequest {
                    id,
                    requested_line: bp.line,
                    spec: BreakpointSpec {
                        line: resolved_line,
                        ..bp
                    },
                }
            })
            .collect();

        let resolved_breakpoints: Vec<RequestedSourceBreakpoint> = requests
            .iter()
            .map(|req| RequestedSourceBreakpoint {
                id: req.id,
                spec: req.spec.clone(),
            })
            .collect();

        if let Some(existing) = self.breakpoints.remove(&file_key) {
            for bp in existing {
                check_cancel(cancel)?;
                let _ =
                    cancellable_jdwp(cancel, self.jdwp.event_request_clear(2, bp.request_id)).await;
                self.breakpoint_metadata.remove(&bp.request_id);
            }
        }

        if requests.is_empty() {
            self.requested_breakpoints.remove(&file);
        } else {
            self.requested_breakpoints
                .insert(file.clone(), resolved_breakpoints);
        }

        let mut results = Vec::with_capacity(requests.len());

        // Best-effort: attempt to apply now for already-loaded classes.
        let classes = cancellable_jdwp(cancel, self.jdwp.all_classes()).await?;
        let mut class_candidates: Vec<ClassInfo> = Vec::new();
        for class_info in classes {
            check_cancel(cancel)?;
            let source_file = match self.source_file(cancel, class_info.type_id).await {
                Ok(source_file) => source_file,
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(_) => continue,
            };

            let resolved = match self
                .resolve_source_path(cancel, class_info.type_id, &source_file)
                .await
            {
                Ok(resolved) => resolved,
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(_) => None,
            };

            let matches = if let Some(resolved) = resolved {
                resolved == file_path
            } else {
                source_file.as_str() == file_name.as_str()
            };

            if matches {
                class_candidates.push(class_info);
            }
        }

        if class_candidates.is_empty() {
            if !requests.is_empty() {
                let pending_ids: HashSet<i64> = requests.iter().map(|req| req.id).collect();
                if !pending_ids.is_empty() {
                    self.pending_breakpoints.insert(file.clone(), pending_ids);
                }
            }
            for req in requests {
                results.push(
                    json!({"verified": false, "id": req.id, "line": req.spec.line, "message": "class not loaded yet"}),
                );
            }
            return Ok(results);
        }

        let mut all_entries = Vec::new();

        for req in requests {
            check_cancel(cancel)?;
            let spec_line = req.spec.line;
            let _requested_line = req.requested_line;
            let condition = normalize_breakpoint_string(req.spec.condition);
            let mut hit_condition = normalize_breakpoint_string(req.spec.hit_condition);
            let log_message = normalize_breakpoint_string(req.spec.log_message);

            let count_modifier = hit_condition
                .as_deref()
                .and_then(|raw| raw.parse::<u32>().ok())
                .filter(|count| *count > 1);
            if count_modifier.is_some() {
                // Hit-count breakpoints are handled by JDWP's built-in `Count` filter.
                // Clear the expression so we don't try to re-evaluate it per event.
                hit_condition = None;
            }

            let suspend_policy = if log_message.is_some() {
                JDWP_SUSPEND_POLICY_NONE
            } else {
                JDWP_SUSPEND_POLICY_EVENT_THREAD
            };

            let mut verified = false;
            let mut last_error: Option<String> = None;
            let mut saw_location = false;
            let mut first_resolved_line = None;
            let mut verified_resolved_line = None;

            for class in &class_candidates {
                check_cancel(cancel)?;
                match self.location_for_line(cancel, class, spec_line).await? {
                    Some(resolved) => {
                        first_resolved_line.get_or_insert(resolved.line);
                        saw_location = true;
                        let mut modifiers = vec![EventModifier::LocationOnly {
                            location: resolved.location,
                        }];
                        if let Some(count) = count_modifier {
                            modifiers.push(EventModifier::Count { count });
                        }
                        match cancellable_jdwp(
                            cancel,
                            self.jdwp.event_request_set(2, suspend_policy, modifiers),
                        )
                        .await
                        {
                            Ok(request_id) => {
                                verified = true;
                                verified_resolved_line.get_or_insert(resolved.line);

                                all_entries.push(BreakpointEntry { request_id });
                                self.breakpoint_metadata.insert(
                                    request_id,
                                    BreakpointMetadata {
                                        condition: condition.clone(),
                                        hit_condition: hit_condition.clone(),
                                        log_message: log_message.clone(),
                                        hit_count: 0,
                                    },
                                );
                            }
                            Err(err) => {
                                if matches!(err, JdwpError::Cancelled) {
                                    return Err(JdwpError::Cancelled.into());
                                }
                                last_error = Some(err.to_string());
                            }
                        }
                    }
                    None => {}
                }
            }
            if verified {
                let line = verified_resolved_line
                    .or(first_resolved_line)
                    .unwrap_or(spec_line);
                let mut obj = serde_json::Map::new();
                obj.insert("verified".to_string(), json!(true));
                obj.insert("id".to_string(), json!(req.id));
                obj.insert("line".to_string(), json!(line));
                results.push(Value::Object(obj));
            } else if saw_location {
                let line = first_resolved_line.unwrap_or(spec_line);
                results.push(json!({
                    "verified": false,
                    "id": req.id,
                    "line": line,
                    "message": last_error.unwrap_or_else(|| "failed to set breakpoint".to_string())
                }));
            } else {
                results.push(json!({
                    "verified": false,
                    "id": req.id,
                    "line": spec_line,
                    "message": "no executable code at this line"
                }));
            }
        }

        if !all_entries.is_empty() {
            self.breakpoints.insert(file.clone(), all_entries);
        }

        Ok(results)
    }

    pub(crate) async fn set_function_breakpoints(
        &mut self,
        cancel: &CancellationToken,
        breakpoints: Vec<FunctionBreakpointSpec>,
    ) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;

        // Reset pending markers for this setFunctionBreakpoints call.
        self.pending_function_breakpoints.clear();

        // Clear existing function breakpoints.
        let existing = std::mem::take(&mut self.function_breakpoints);
        for bp in existing {
            check_cancel(cancel)?;
            let _ = cancellable_jdwp(cancel, self.jdwp.event_request_clear(2, bp.request_id)).await;
            self.breakpoint_metadata.remove(&bp.request_id);
        }

        let mut requested_breakpoints = Vec::with_capacity(breakpoints.len());
        for mut bp in breakpoints {
            bp.name = bp.name.trim().to_string();
            requested_breakpoints.push(RequestedFunctionBreakpoint {
                id: self.alloc_breakpoint_id(),
                spec: bp,
            });
        }

        let mut results = Vec::with_capacity(requested_breakpoints.len());
        let mut all_entries = Vec::new();

        for entry in &requested_breakpoints {
            check_cancel(cancel)?;
            let dap_id = entry.id;
            let bp = entry.spec.clone();
            let spec_name = bp.name.trim().to_string();
            if spec_name.is_empty() {
                results.push(json!({"verified": false, "id": dap_id, "message": "function breakpoint name must not be empty"}));
                continue;
            }

            let condition = normalize_breakpoint_string(bp.condition);
            let mut hit_condition = normalize_breakpoint_string(bp.hit_condition);
            let log_message = normalize_breakpoint_string(bp.log_message);

            let count_modifier = hit_condition
                .as_deref()
                .and_then(|raw| raw.parse::<u32>().ok())
                .filter(|count| *count > 1);
            if count_modifier.is_some() {
                hit_condition = None;
            }

            let suspend_policy = if log_message.is_some() {
                JDWP_SUSPEND_POLICY_NONE
            } else {
                JDWP_SUSPEND_POLICY_EVENT_THREAD
            };

            let Some((class_name, method_name)) = parse_function_breakpoint(&spec_name) else {
                results.push(json!({
                    "verified": false,
                    "id": dap_id,
                    "message": "unsupported function breakpoint. Use `Class.method` (optionally fully qualified)."
                }));
                continue;
            };

            let signature = class_name_to_signature(&class_name);
            let classes =
                cancellable_jdwp(cancel, self.jdwp.classes_by_signature(&signature)).await?;
            if classes.is_empty() {
                self.pending_function_breakpoints.insert(dap_id);
                results.push(json!({
                    "verified": false,
                    "id": dap_id,
                    "message": "class not loaded yet"
                }));
                continue;
            }

            let mut verified = false;
            let mut first_line: Option<i32> = None;
            let mut last_error: Option<String> = None;
            let mut saw_method = false;
            let mut saw_location = false;

            for class in classes {
                check_cancel(cancel)?;

                let methods = if let Some(methods) = self.methods_cache.get(&class.type_id) {
                    methods.clone()
                } else {
                    let methods =
                        cancellable_jdwp(cancel, self.jdwp.reference_type_methods(class.type_id))
                            .await?;
                    self.methods_cache.insert(class.type_id, methods.clone());
                    methods
                };

                for method in methods.iter().filter(|m| m.name == method_name) {
                    saw_method = true;
                    check_cancel(cancel)?;

                    let table = match cancellable_jdwp(
                        cancel,
                        self.jdwp.method_line_table(class.type_id, method.method_id),
                    )
                    .await
                    {
                        Ok(table) => Some(table),
                        Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                        Err(err) => {
                            last_error = Some(err.to_string());
                            None
                        }
                    };

                    let Some(table) = table else { continue };
                    self.line_table_cache
                        .insert((class.type_id, method.method_id), table.clone());

                    let index = table.start;
                    let line = table
                        .lines
                        .iter()
                        .filter(|entry| entry.code_index <= index)
                        .map(|entry| entry.line)
                        .last()
                        .or_else(|| table.lines.first().map(|entry| entry.line))
                        .unwrap_or(1);

                    let location = Location {
                        type_tag: class.ref_type_tag,
                        class_id: class.type_id,
                        method_id: method.method_id,
                        index,
                    };
                    saw_location = true;

                    let mut modifiers = vec![EventModifier::LocationOnly { location }];
                    if let Some(count) = count_modifier {
                        modifiers.push(EventModifier::Count { count });
                    }

                    match cancellable_jdwp(
                        cancel,
                        self.jdwp.event_request_set(2, suspend_policy, modifiers),
                    )
                    .await
                        {
                            Ok(request_id) => {
                                verified = true;
                                first_line.get_or_insert(line);

                                all_entries.push(BreakpointEntry { request_id });
                                self.breakpoint_metadata.insert(
                                request_id,
                                BreakpointMetadata {
                                    condition: condition.clone(),
                                    hit_condition: hit_condition.clone(),
                                    log_message: log_message.clone(),
                                    hit_count: 0,
                                },
                            );
                        }
                        Err(err) => {
                            if matches!(err, JdwpError::Cancelled) {
                                return Err(JdwpError::Cancelled.into());
                            }
                            last_error = Some(err.to_string());
                        }
                    }
                }
            }

            if verified {
                let mut obj = serde_json::Map::new();
                obj.insert("verified".to_string(), json!(true));
                obj.insert("id".to_string(), json!(dap_id));
                if let Some(line) = first_line {
                    obj.insert("line".to_string(), json!(line));
                }
                results.push(Value::Object(obj));
            } else if !saw_method {
                results.push(json!({
                    "verified": false,
                    "id": dap_id,
                    "message": format!("method `{method_name}` not found in {class_name}")
                }));
            } else if saw_location {
                results.push(json!({
                    "verified": false,
                    "id": dap_id,
                    "message": last_error.unwrap_or_else(|| "failed to set breakpoint".to_string())
                }));
            } else {
                results.push(json!({
                    "verified": false,
                    "id": dap_id,
                    "message": "no executable code for this function"
                }));
            }
        }

        self.requested_function_breakpoints = requested_breakpoints;

        if !all_entries.is_empty() {
            self.function_breakpoints.extend(all_entries);
        }

        Ok(results)
    }

    pub async fn continue_(
        &mut self,
        cancel: &CancellationToken,
        dap_thread_id: Option<i64>,
    ) -> Result<()> {
        self.invalidate_handles();
        self.smart_step_into = None;
        if let Some(dap_thread_id) = dap_thread_id {
            let thread: ThreadId = dap_thread_id as ThreadId;
            cancellable_jdwp(cancel, self.jdwp.thread_resume(thread)).await?;
        } else {
            cancellable_jdwp(cancel, self.jdwp.vm_resume()).await?;
        }
        Ok(())
    }

    pub async fn pause(
        &mut self,
        cancel: &CancellationToken,
        dap_thread_id: Option<i64>,
    ) -> Result<()> {
        self.invalidate_handles();
        self.smart_step_into = None;
        if let Some(dap_thread_id) = dap_thread_id {
            let thread: ThreadId = dap_thread_id as ThreadId;
            cancellable_jdwp(cancel, self.jdwp.thread_suspend(thread)).await?;
        } else {
            cancellable_jdwp(cancel, self.jdwp.vm_suspend()).await?;
        }
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

        let description = match self.exception_message(cancel, ctx.exception).await {
            Ok(message) => message,
            Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
            Err(_) => None,
        };

        Ok(Some(ExceptionInfo {
            exception_id,
            description,
            break_mode,
        }))
    }

    pub async fn step(
        &mut self,
        cancel: &CancellationToken,
        dap_thread_id: i64,
        depth: StepDepth,
    ) -> Result<()> {
        self.invalidate_handles();
        check_cancel(cancel)?;
        let thread: ThreadId = dap_thread_id as ThreadId;

        if let Some(old) = self.active_step_requests.remove(&thread) {
            let _ =
                cancellable_jdwp(cancel, self.jdwp.event_request_clear(1, old.request_id)).await;
        }
        if let Some(old) = self.active_method_exit_requests.remove(&thread) {
            let _ = cancellable_jdwp(cancel, self.jdwp.event_request_clear(42, old)).await;
        }
        self.pending_return_values.remove(&thread);

        // Best-effort: MethodExitWithReturnValue isn't guaranteed to be supported by all JVMs.
        if let Ok(req) = cancellable_jdwp(
            cancel,
            self.jdwp
                .event_request_set(42, 0, vec![EventModifier::ThreadOnly { thread }]),
        )
        .await
        {
            self.active_method_exit_requests.insert(thread, req);
        }

        let jdwp_depth = match depth {
            StepDepth::Into => 0,
            StepDepth::Over => 1,
            StepDepth::Out => 2,
        };

        let req = cancellable_jdwp(
            cancel,
            self.jdwp.event_request_set(
                1,
                JDWP_SUSPEND_POLICY_EVENT_THREAD,
                vec![EventModifier::Step {
                    thread,
                    size: 1, // line
                    depth: jdwp_depth,
                }],
            ),
        )
        .await?;
        self.active_step_requests.insert(
            thread,
            ActiveStepRequest {
                request_id: req,
                depth,
            },
        );
        cancellable_jdwp(cancel, self.jdwp.thread_resume(thread)).await?;
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
                JDWP_SUSPEND_POLICY_EVENT_THREAD,
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

    /// DAP `dataBreakpointInfo` request implementation.
    ///
    /// This currently supports watchpoints on:
    /// - instance fields of object handles surfaced via the variables UI
    /// - static fields in the `Static` scope
    ///
    /// Other targets (locals, array elements, synthetic children) return a response
    /// without a `dataId`, signalling that data breakpoints are not available.
    pub(crate) async fn data_breakpoint_info(
        &mut self,
        cancel: &CancellationToken,
        variables_reference: i64,
        name: &str,
    ) -> Result<serde_json::Value> {
        check_cancel(cancel)?;

        let name = name.trim();
        if name.is_empty() {
            return Err(DebuggerError::InvalidRequest(
                "dataBreakpointInfo.name is required".to_string(),
            ));
        }

        let caps = self.jdwp.capabilities().await;
        let access_types = if caps.can_watch_field_access && caps.can_watch_field_modification {
            vec!["read", "write", "readWrite"]
        } else if caps.can_watch_field_access {
            vec!["read"]
        } else if caps.can_watch_field_modification {
            vec!["write"]
        } else {
            Vec::new()
        };

        // --- Instance fields (object handles) ---------------------------------------------
        if let Some(handle) = self
            .objects
            .handle_from_variables_reference(variables_reference)
        {
            if self.objects.is_invalid(handle) {
                return Ok(json!({
                    "description": "<collected>",
                    "accessTypes": access_types,
                    "canPersist": false,
                }));
            }

            let runtime_type = self.objects.runtime_type(handle).unwrap_or_default();
            if runtime_type.ends_with("[]") {
                // Arrays don't have fields; only "length" and indexed elements are surfaced.
                return Ok(json!({
                    "description": format!("{name} (array element)"),
                    "accessTypes": access_types,
                    "canPersist": false,
                }));
            }

            let Some(object_id) = self.objects.object_id(handle) else {
                return Ok(json!({
                    "description": name,
                    "accessTypes": access_types,
                    "canPersist": false,
                }));
            };
            if object_id == 0 {
                return Ok(json!({
                    "description": name,
                    "accessTypes": access_types,
                    "canPersist": false,
                }));
            }

            let Some((class_id, field_id)) =
                self.resolve_instance_field(cancel, object_id, name).await?
            else {
                return Ok(json!({
                    "description": format!("{name} (not a field)"),
                    "accessTypes": access_types,
                    "canPersist": false,
                }));
            };

            let data_id = encode_field_data_id(class_id, field_id, Some(object_id));
            let description = if self.objects.is_pinned(handle) {
                format!("__novaPinned[{}].{name}", handle.as_u32())
            } else {
                self.objects
                    .evaluate_name(handle)
                    .map(|base| format!("{base}.{name}"))
                    .unwrap_or_else(|| name.to_string())
            };

            return Ok(json!({
                "dataId": data_id,
                "description": description,
                "accessTypes": access_types,
                "canPersist": false,
            }));
        }

        // --- Static fields (Static scope) ------------------------------------------------
        let Some(var_ref) = self.var_handles.get(variables_reference).cloned() else {
            return Ok(json!({
                "description": name,
                "accessTypes": access_types,
                "canPersist": false,
            }));
        };

        match var_ref {
            VarRef::StaticFields(class_id) => {
                let Some(field_id) = self.resolve_static_field(cancel, class_id, name).await?
                else {
                    return Ok(json!({
                        "description": format!("{name} (not a static field)"),
                        "accessTypes": access_types,
                        "canPersist": false,
                    }));
                };

                let class_sig = match self.signature(cancel, class_id).await {
                    Ok(sig) => sig,
                    Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                    Err(_) => String::new(),
                };
                let class_name = signature_to_type_name(&class_sig);
                let data_id = encode_field_data_id(class_id, field_id, None);

                Ok(json!({
                    "dataId": data_id,
                    "description": format!("{class_name}.{name}"),
                    "accessTypes": access_types,
                    "canPersist": false,
                }))
            }
            VarRef::FrameLocals(_) => Ok(json!({
                "description": format!("{name} (locals are not supported)"),
                "accessTypes": access_types,
                "canPersist": false,
            })),
        }
    }

    /// DAP `setDataBreakpoints` request implementation.
    pub(crate) async fn set_data_breakpoints(
        &mut self,
        cancel: &CancellationToken,
        breakpoints: Vec<DataBreakpointSpec>,
    ) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;

        // Clear prior watchpoint requests.
        let existing = std::mem::take(&mut self.watchpoint_requests);
        for (event_kind, request_id) in existing {
            match cancellable_jdwp(
                cancel,
                self.jdwp.event_request_clear(event_kind, request_id),
            )
            .await
            {
                Ok(()) => {}
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(_) => {}
            }
        }

        let caps = self.jdwp.capabilities().await;

        let mut out = Vec::with_capacity(breakpoints.len());
        for spec in breakpoints {
            check_cancel(cancel)?;

            let access_type = spec
                .access_type
                .as_deref()
                .unwrap_or("write")
                .trim()
                .to_string();

            let hit_condition = normalize_breakpoint_string(spec.hit_condition);
            let count_modifier = hit_condition
                .as_deref()
                .and_then(|raw| raw.parse::<u32>().ok())
                .filter(|count| *count > 1);

            let Some(target) = decode_field_data_id(&spec.data_id) else {
                out.push(json!({
                    "verified": false,
                    "message": "unsupported dataId (expected a field watchpoint)",
                }));
                continue;
            };

            let needs_read = matches!(access_type.as_str(), "read" | "readWrite");
            let needs_write = matches!(access_type.as_str(), "write" | "readWrite");

            if needs_read && !caps.can_watch_field_access {
                out.push(json!({
                    "verified": false,
                    "message": "target VM does not support field access watchpoints (JDWP canWatchFieldAccess=false)",
                }));
                continue;
            }
            if needs_write && !caps.can_watch_field_modification {
                out.push(json!({
                    "verified": false,
                    "message": "target VM does not support field modification watchpoints (JDWP canWatchFieldModification=false)",
                }));
                continue;
            }

            let event_kinds: Vec<u8> = match access_type.as_str() {
                "read" => vec![20],
                "write" => vec![21],
                "readWrite" => vec![20, 21],
                other => {
                    out.push(json!({
                        "verified": false,
                        "message": format!("unsupported accessType {other:?} (expected \"read\", \"write\", or \"readWrite\")"),
                    }));
                    continue;
                }
            };

            let mut installed: Vec<(u8, i32)> = Vec::new();
            let mut last_error: Option<String> = None;

            for event_kind in &event_kinds {
                match self
                    .install_watchpoint_request(cancel, *event_kind, &target, count_modifier)
                    .await
                {
                    Ok(id) => installed.push((*event_kind, id)),
                    Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                    Err(err) => {
                        last_error = Some(err.to_string());
                        break;
                    }
                }
            }

            if installed.len() != event_kinds.len() {
                // Best-effort rollback for partial installs (e.g. readWrite where one of the two
                // JDWP requests failed).
                for (event_kind, request_id) in installed {
                    let _ = cancellable_jdwp(
                        cancel,
                        self.jdwp.event_request_clear(event_kind, request_id),
                    )
                    .await;
                }
                out.push(json!({
                    "verified": false,
                    "message": last_error.unwrap_or_else(|| "failed to set watchpoint".to_string()),
                }));
                continue;
            }

            // Persist installed requests so the next `setDataBreakpoints` call can clear them.
            self.watchpoint_requests.extend(installed.iter().copied());

            let mut bp = serde_json::Map::new();
            bp.insert("verified".to_string(), json!(true));
            // DAP breakpoint IDs are opaque; expose the first JDWP request id for UX/debugging.
            bp.insert("id".to_string(), json!(installed[0].1));
            out.push(Value::Object(bp));
        }

        Ok(out)
    }

    async fn install_watchpoint_request(
        &mut self,
        cancel: &CancellationToken,
        event_kind: u8,
        target: &FieldDataId,
        count_modifier: Option<u32>,
    ) -> std::result::Result<i32, JdwpError> {
        let mut modifiers = vec![EventModifier::FieldOnly {
            class_id: target.class_id,
            field_id: target.field_id,
        }];
        if let Some(object_id) = target.object_id {
            modifiers.push(EventModifier::InstanceOnly { object_id });
        }
        if let Some(count) = count_modifier {
            modifiers.push(EventModifier::Count { count });
        }
        cancellable_jdwp(
            cancel,
            self.jdwp
                .event_request_set(event_kind, JDWP_SUSPEND_POLICY_EVENT_THREAD, modifiers),
        )
        .await
    }

    async fn resolve_instance_field(
        &mut self,
        cancel: &CancellationToken,
        object_id: ObjectId,
        field_name: &str,
    ) -> Result<Option<(ReferenceTypeId, u64)>> {
        check_cancel(cancel)?;

        const MODIFIER_STATIC: u32 = 0x0008;

        let (_ref_type_tag, mut type_id) =
            cancellable_jdwp(cancel, self.jdwp.object_reference_reference_type(object_id)).await?;

        let mut seen_types = std::collections::HashSet::new();
        while type_id != 0 && seen_types.insert(type_id) {
            let fields = cancellable_jdwp(cancel, self.jdwp.reference_type_fields(type_id)).await?;
            if let Some(field) = fields
                .into_iter()
                .find(|f| f.name == field_name && (f.mod_bits & MODIFIER_STATIC == 0))
            {
                return Ok(Some((type_id, field.field_id)));
            }

            type_id = match cancellable_jdwp(cancel, self.jdwp.class_type_superclass(type_id)).await
            {
                Ok(id) => id,
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(_) => break,
            };
        }

        Ok(None)
    }

    async fn resolve_static_field(
        &mut self,
        cancel: &CancellationToken,
        class_id: ReferenceTypeId,
        field_name: &str,
    ) -> Result<Option<u64>> {
        check_cancel(cancel)?;
        const MODIFIER_STATIC: u32 = 0x0008;

        let fields = cancellable_jdwp(cancel, self.jdwp.reference_type_fields(class_id)).await?;
        Ok(fields
            .into_iter()
            .find(|f| f.name == field_name && (f.mod_bits & MODIFIER_STATIC != 0))
            .map(|f| f.field_id))
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
            JdwpEvent::MethodExitWithReturnValue {
                request_id,
                thread,
                value,
                ..
            } => {
                let matches_active =
                    self.active_method_exit_requests.get(thread) == Some(request_id);
                if matches_active {
                    self.pending_return_values.insert(*thread, value.clone());
                }
            }
            JdwpEvent::Breakpoint { thread, .. } => {
                self.invalidate_handles();
                self.last_stop_reason
                    .insert(*thread, StopReason::Breakpoint);
                self.exception_stop_context.remove(thread);
            }
            JdwpEvent::SingleStep { thread, .. } => {
                self.invalidate_handles();
                self.last_stop_reason.insert(*thread, StopReason::Step);
                self.exception_stop_context.remove(thread);
            }
            JdwpEvent::Exception {
                thread,
                location: _,
                exception,
                catch_location,
                ..
            } => {
                self.invalidate_handles();
                self.last_stop_reason.insert(*thread, StopReason::Exception);
                self.exception_stop_context.insert(
                    *thread,
                    ExceptionStopContext {
                        exception: *exception,
                        catch_location: *catch_location,
                    },
                );
            }
            JdwpEvent::FieldAccess { thread, .. } | JdwpEvent::FieldModification { thread, .. } => {
                self.invalidate_handles();
                self.last_stop_reason
                    .insert(*thread, StopReason::Breakpoint);
                self.exception_stop_context.remove(thread);
            }
            _ => {}
        }
    }

    pub(crate) async fn take_step_output_value(
        &mut self,
        cancel: &CancellationToken,
        thread: ThreadId,
    ) -> Option<VmStoppedValue> {
        let step_depth = self.active_step_requests.get(&thread).map(|req| req.depth);

        if let Some(step_req) = self.active_step_requests.remove(&thread) {
            let _ = cancellable_jdwp(
                cancel,
                self.jdwp.event_request_clear(1, step_req.request_id),
            )
            .await;
        }
        if let Some(exit_req) = self.active_method_exit_requests.remove(&thread) {
            let _ = cancellable_jdwp(cancel, self.jdwp.event_request_clear(42, exit_req)).await;
        }

        let captured = self.pending_return_values.remove(&thread)?;
        let step_depth = step_depth?;

        Some(match step_depth {
            StepDepth::Out => VmStoppedValue::Return(captured),
            StepDepth::Into | StepDepth::Over => VmStoppedValue::Expression(captured),
        })
    }

    pub async fn evaluate(
        &mut self,
        cancel: &CancellationToken,
        frame_id: i64,
        expression: &str,
        _options: EvalOptions,
    ) -> Result<Option<serde_json::Value>> {
        let expr = expression.trim();
        if expr.is_empty() {
            return Ok(Some(json!({"result": "", "variablesReference": 0})));
        }

        let parsed = match parse_eval_expression(expr) {
            Ok(parsed) => parsed,
            Err(()) => {
                return Ok(Some(
                    json!({"result": format!("unsupported expression: {expr}"), "variablesReference": 0}),
                ))
            }
        };
        let mut segments = parsed.segments;

        let Some(frame) = self.frame_handles.get(frame_id).copied() else {
            return Ok(Some(
                json!({"result": format!("unknown frameId {frame_id}"), "variablesReference": 0}),
            ));
        };

        let (mut value, mut static_type) = match parsed.base {
            EvalBase::This => {
                let object_id = cancellable_jdwp(
                    cancel,
                    self.jdwp
                        .stack_frame_this_object(frame.thread, frame.frame_id),
                )
                .await?;
                (
                    JdwpValue::Object {
                        tag: b'L',
                        id: object_id,
                    },
                    None,
                )
            }
            EvalBase::Local(name) if name == "__novaPinned" => {
                let Some(EvalSegment::Index(raw_handle)) = segments.first().cloned() else {
                    return Ok(Some(json!({
                        "result": format!("unsupported expression: {expr}"),
                        "variablesReference": 0
                    })));
                };

                // Consume the `__novaPinned[<handle>]` segment. The remaining segments operate
                // on the pinned object value.
                segments.remove(0);

                if raw_handle < 0 {
                    return Ok(Some(json!({
                        "result": format!("unsupported expression: {expr}"),
                        "variablesReference": 0
                    })));
                }
                let Ok(handle_u32) = u32::try_from(raw_handle) else {
                    return Ok(Some(json!({
                        "result": format!("unsupported expression: {expr}"),
                        "variablesReference": 0
                    })));
                };

                let Some(handle) = self
                    .objects
                    .handle_from_variables_reference(OBJECT_HANDLE_BASE + i64::from(handle_u32))
                else {
                    return Ok(Some(
                        json!({"result": format!("not found: {expr}"), "variablesReference": 0}),
                    ));
                };

                if !self.objects.is_pinned(handle) {
                    return Ok(Some(
                        json!({"result": format!("not found: {expr}"), "variablesReference": 0}),
                    ));
                }

                let Some(object_id) = self.objects.object_id(handle) else {
                    return Ok(Some(
                        json!({"result": format!("not found: {expr}"), "variablesReference": 0}),
                    ));
                };

                let static_type = self.objects.runtime_type(handle).map(|t| t.to_string());
                (
                    JdwpValue::Object {
                        tag: b'L',
                        id: object_id,
                    },
                    static_type,
                )
            }
            EvalBase::Local(name) => {
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

                let mut found = None;
                for (var, value) in in_scope.into_iter().zip(values.into_iter()) {
                    check_cancel(cancel)?;
                    if var.name == name {
                        found = Some((value, signature_to_type_name(&var.signature)));
                        break;
                    }
                }

                let Some((value, type_name)) = found else {
                    return Ok(Some(
                        json!({"result": format!("not found: {expr}"), "variablesReference": 0}),
                    ));
                };

                (value, Some(type_name))
            }
            EvalBase::Pinned(handle) => {
                let Some(object_id) = self.objects.object_id(handle) else {
                    return Ok(Some(json!({
                        "result": format!("pinned object {handle} is not available"),
                        "variablesReference": 0,
                    })));
                };

                if self.objects.is_invalid(handle) {
                    return Ok(Some(
                        json!({"result": format!("{handle} <collected>"), "variablesReference": 0}),
                    ));
                }

                let tag = self
                    .objects
                    .runtime_type(handle)
                    .is_some_and(|ty| ty.ends_with("[]"))
                    .then_some(b'[')
                    .unwrap_or(b'L');

                (
                    JdwpValue::Object { tag, id: object_id },
                    self.objects.runtime_type(handle).map(|s| s.to_string()),
                )
            }
        };

        for segment in segments {
            check_cancel(cancel)?;
            match segment {
                EvalSegment::Field(name) => match value {
                    JdwpValue::Object { id: 0, .. } => {
                        return Ok(Some(
                            json!({"result": format!("not found: {expr}"), "variablesReference": 0}),
                        ))
                    }
                    JdwpValue::Object { tag: b'[', id } if name == "length" => {
                        let length =
                            cancellable_jdwp(cancel, self.jdwp.array_reference_length(id)).await?;
                        value = JdwpValue::Int(length.max(0));
                        static_type = Some("int".to_string());
                    }
                    JdwpValue::Object { id, .. } => {
                        let children = match cancellable_jdwp(
                            cancel,
                            self.inspector.object_children(id),
                        )
                        .await
                        {
                            Ok(children) => children,
                            Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                            Err(err) => {
                                return Ok(Some(json!({
                                    "result": format!("error: {err}"),
                                    "variablesReference": 0
                                })))
                            }
                        };
                        let Some(child) = children.into_iter().find(|child| child.name == name)
                        else {
                            return Ok(Some(
                                json!({"result": format!("not found: {expr}"), "variablesReference": 0}),
                            ));
                        };

                        value = child.value;
                        static_type = child.static_type;
                    }
                    _ => {
                        return Ok(Some(json!({
                            "result": format!("unsupported expression: {expr}"),
                            "variablesReference": 0,
                        })))
                    }
                },
                EvalSegment::Index(index) => match value {
                    JdwpValue::Object { tag: b'[', id } => {
                        if index < 0 {
                            return Ok(Some(json!({
                                "result": format!("unsupported expression: {expr}"),
                                "variablesReference": 0,
                            })));
                        }
                        let Ok(index) = i32::try_from(index) else {
                            return Ok(Some(json!({
                                "result": format!("unsupported expression: {expr}"),
                                "variablesReference": 0,
                            })));
                        };
                        let values = cancellable_jdwp(
                            cancel,
                            self.jdwp.array_reference_get_values(id, index, 1),
                        )
                        .await?;
                        let Some(value0) = values.into_iter().next() else {
                            return Ok(Some(json!({
                                "result": format!("not found: {expr}"),
                                "variablesReference": 0,
                            })));
                        };
                        value = value0;
                        static_type = None;
                    }
                    _ => {
                        return Ok(Some(json!({
                            "result": format!("unsupported expression: {expr}"),
                            "variablesReference": 0,
                        })))
                    }
                },
            }
        }

        let formatted = self
            .format_value(cancel, &value, static_type.as_deref(), 0)
            .await?;

        if let Some(handle) = ObjectHandle::from_variables_reference(formatted.variables_reference)
        {
            self.objects.set_evaluate_name(handle, expr.to_string());
        }

        let mut obj = serde_json::Map::new();
        obj.insert("result".to_string(), json!(formatted.value));
        obj.insert(
            "variablesReference".to_string(),
            json!(formatted.variables_reference),
        );
        obj.insert("evaluateName".to_string(), json!(expr));
        if let Some(type_name) = formatted.type_name {
            obj.insert("type".to_string(), json!(type_name));
        }
        if let Some(presentation_hint) = formatted.presentation_hint {
            obj.insert("presentationHint".to_string(), presentation_hint);
        }
        if let Some(named) = formatted.named_variables {
            obj.insert("namedVariables".to_string(), json!(named));
        }
        if let Some(indexed) = formatted.indexed_variables {
            obj.insert("indexedVariables".to_string(), json!(indexed));
        }

        Ok(Some(Value::Object(obj)))
    }

    pub fn stream_debug(
        &self,
        cancel: CancellationToken,
        frame_id: i64,
        expression: String,
        config: nova_stream_debug::StreamDebugConfig,
    ) -> Result<impl Future<Output = Result<crate::stream_debug::StreamDebugBody>> + Send + 'static>
    {
        check_cancel(&cancel)?;

        let Some(frame) = self.frame_handles.get(frame_id).copied() else {
            return Err(DebuggerError::InvalidRequest(format!(
                "unknown frameId {frame_id}"
            )));
        };
        let jdwp = self.jdwp.clone();

        Ok(
            async move { Debugger::stream_debug_impl(jdwp, cancel, frame, expression, config).await },
        )
    }

    async fn stream_debug_impl(
        jdwp: JdwpClient,
        cancel: CancellationToken,
        frame: FrameHandle,
        expression: String,
        config: nova_stream_debug::StreamDebugConfig,
    ) -> Result<crate::stream_debug::StreamDebugBody> {
        let cancel = &cancel;
        check_cancel(cancel)?;

        let analysis = nova_stream_debug::analyze_stream_expression(&expression)
            .map_err(|err| DebuggerError::InvalidRequest(err.to_string()))?;

        let runtime = match crate::wire_stream_debug::debug_stream_wire_in_frame(
            &jdwp,
            frame.thread,
            frame.frame_id,
            frame.location,
            &analysis,
            &config,
            cancel,
        )
        .await
        {
            Ok(runtime) => runtime,
            Err(crate::wire_stream_debug::WireStreamDebugError::Cancelled) => {
                return Err(DebuggerError::Jdwp(JdwpError::Cancelled));
            }
            Err(crate::wire_stream_debug::WireStreamDebugError::Timeout) => {
                return Err(DebuggerError::Timeout);
            }
            Err(crate::wire_stream_debug::WireStreamDebugError::Jdwp(err)) => {
                return Err(DebuggerError::Jdwp(err));
            }
            Err(err) => {
                return Err(DebuggerError::InvalidRequest(err.to_string()));
            }
        };

        Ok(crate::stream_debug::StreamDebugBody { analysis, runtime })
    }


    pub async fn set_variable(
        &mut self,
        cancel: &CancellationToken,
        variables_reference: i64,
        name: &str,
        value: &str,
    ) -> Result<Option<serde_json::Value>> {
        check_cancel(cancel)?;

        if let Some(handle) = self
            .objects
            .handle_from_variables_reference(variables_reference)
        {
            return self
                .set_object_variable(cancel, handle, name.trim(), value)
                .await;
        }

        let Some(var_ref) = self.var_handles.get(variables_reference).cloned() else {
            return Err(DebuggerError::InvalidRequest(format!(
                "unknown variablesReference {variables_reference}"
            )));
        };

        match var_ref {
            VarRef::FrameLocals(frame) => {
                self.set_local_variable(cancel, &frame, name.trim(), value)
                    .await
            }
            VarRef::StaticFields(class_id) => {
                self.set_static_variable(cancel, class_id, name.trim(), value)
                    .await
            }
        }
    }

    async fn set_local_variable(
        &mut self,
        cancel: &CancellationToken,
        frame: &FrameHandle,
        name: &str,
        value: &str,
    ) -> Result<Option<serde_json::Value>> {
        if name.is_empty() {
            return Err(DebuggerError::InvalidRequest(
                "setVariable.name is required".to_string(),
            ));
        }

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

        let Some(var) = in_scope.iter().find(|v| v.name == name) else {
            return Err(DebuggerError::InvalidRequest(format!(
                "unknown local `{name}`"
            )));
        };

        let jdwp_value = self
            .parse_set_variable_value(cancel, &var.signature, value)
            .await?;

        cancellable_jdwp(
            cancel,
            self.jdwp.stack_frame_set_values(
                frame.thread,
                frame.frame_id,
                &[(var.slot, jdwp_value)],
            ),
        )
        .await?;

        // Fetch the current value so we can present whatever the VM accepted.
        let values = cancellable_jdwp(
            cancel,
            self.jdwp.stack_frame_get_values(
                frame.thread,
                frame.frame_id,
                &[(var.slot, var.signature.clone())],
            ),
        )
        .await?;
        let new_value = values.into_iter().next().unwrap_or(JdwpValue::Void);

        let static_type = signature_to_type_name(&var.signature);
        let formatted = self
            .format_value(cancel, &new_value, Some(&static_type), 0)
            .await?;

        Ok(Some(json!({
            "value": formatted.value,
            "type": formatted.type_name,
            "variablesReference": formatted.variables_reference,
            "namedVariables": formatted.named_variables,
            "indexedVariables": formatted.indexed_variables,
        })))
    }

    async fn set_object_variable(
        &mut self,
        cancel: &CancellationToken,
        handle: ObjectHandle,
        name: &str,
        value: &str,
    ) -> Result<Option<serde_json::Value>> {
        if name.is_empty() {
            return Err(DebuggerError::InvalidRequest(
                "setVariable.name is required".to_string(),
            ));
        }

        let Some(object_id) = self.objects.object_id(handle) else {
            return Err(DebuggerError::InvalidRequest(format!(
                "unknown object handle {handle}"
            )));
        };

        let runtime_type = self
            .objects
            .runtime_type(handle)
            .unwrap_or_default()
            .to_string();

        if runtime_type.ends_with("[]") {
            return self
                .set_array_variable(cancel, handle, object_id, name, value)
                .await;
        }

        let (_ref_type_tag, mut type_id) =
            cancellable_jdwp(cancel, self.jdwp.object_reference_reference_type(object_id)).await?;

        const MODIFIER_STATIC: u32 = 0x0008;
        let mut seen_types = std::collections::HashSet::new();
        let mut field: Option<(u64, String)> = None;

        while type_id != 0 && seen_types.insert(type_id) {
            let fields = cancellable_jdwp(cancel, self.jdwp.reference_type_fields(type_id)).await?;
            if let Some(found) = fields
                .into_iter()
                .find(|f| f.name == name && (f.mod_bits & MODIFIER_STATIC == 0))
            {
                field = Some((found.field_id, found.signature));
                break;
            }

            type_id = cancellable_jdwp(cancel, self.jdwp.class_type_superclass(type_id)).await?;
        }

        let Some((field_id, signature)) = field else {
            return Err(DebuggerError::InvalidRequest(format!(
                "unknown field `{name}`"
            )));
        };

        let jdwp_value = self
            .parse_set_variable_value(cancel, &signature, value)
            .await?;

        cancellable_jdwp(
            cancel,
            self.jdwp
                .object_reference_set_values(object_id, &[(field_id, jdwp_value)]),
        )
        .await?;

        let new_value = cancellable_jdwp(
            cancel,
            self.jdwp
                .object_reference_get_values(object_id, &[field_id]),
        )
        .await?
        .into_iter()
        .next()
        .unwrap_or(JdwpValue::Void);

        let static_type = signature_to_type_name(&signature);
        let formatted = self
            .format_value(cancel, &new_value, Some(&static_type), 0)
            .await?;

        Ok(Some(json!({
            "value": formatted.value,
            "type": formatted.type_name,
            "variablesReference": formatted.variables_reference,
            "namedVariables": formatted.named_variables,
            "indexedVariables": formatted.indexed_variables,
        })))
    }

    async fn set_array_variable(
        &mut self,
        cancel: &CancellationToken,
        _handle: ObjectHandle,
        array_id: ObjectId,
        name: &str,
        value: &str,
    ) -> Result<Option<serde_json::Value>> {
        if name == "length" {
            return Err(DebuggerError::InvalidRequest(
                "cannot assign to array length".to_string(),
            ));
        }
        let Some(index) = parse_array_index_name(name) else {
            return Err(DebuggerError::InvalidRequest(format!(
                "unsupported array element name `{name}`"
            )));
        };

        let (_ref_type_tag, type_id) =
            cancellable_jdwp(cancel, self.jdwp.object_reference_reference_type(array_id)).await?;
        let sig = cancellable_jdwp(cancel, self.jdwp.reference_type_signature(type_id)).await?;
        let Some(element_sig) = sig.strip_prefix('[') else {
            return Err(DebuggerError::InvalidRequest(format!(
                "invalid array signature `{sig}`"
            )));
        };

        let element_value = self
            .parse_set_variable_value(cancel, element_sig, value)
            .await?;

        let Ok(first_index) = i32::try_from(index) else {
            return Err(DebuggerError::InvalidRequest(format!(
                "array index {index} is too large"
            )));
        };

        cancellable_jdwp(
            cancel,
            self.jdwp
                .array_reference_set_values(array_id, first_index, &[element_value]),
        )
        .await?;

        let new_value = cancellable_jdwp(
            cancel,
            self.jdwp
                .array_reference_get_values(array_id, first_index, 1),
        )
        .await?
        .into_iter()
        .next()
        .unwrap_or(JdwpValue::Void);

        let static_type = signature_to_type_name(element_sig);
        let formatted = self
            .format_value(cancel, &new_value, Some(&static_type), 0)
            .await?;

        Ok(Some(json!({
            "value": formatted.value,
            "type": formatted.type_name,
            "variablesReference": formatted.variables_reference,
            "namedVariables": formatted.named_variables,
            "indexedVariables": formatted.indexed_variables,
        })))
    }

    async fn set_static_variable(
        &mut self,
        cancel: &CancellationToken,
        class_id: ReferenceTypeId,
        name: &str,
        value: &str,
    ) -> Result<Option<serde_json::Value>> {
        if name.is_empty() {
            return Err(DebuggerError::InvalidRequest(
                "setVariable.name is required".to_string(),
            ));
        }

        const MODIFIER_STATIC: u32 = 0x0008;

        let fields = cancellable_jdwp(cancel, self.jdwp.reference_type_fields(class_id)).await?;
        let Some(field) = fields
            .into_iter()
            .find(|f| f.name == name && f.mod_bits & MODIFIER_STATIC != 0)
        else {
            return Err(DebuggerError::InvalidRequest(format!(
                "unknown static field `{name}`"
            )));
        };

        let jdwp_value = self
            .parse_set_variable_value(cancel, &field.signature, value)
            .await?;

        cancellable_jdwp(
            cancel,
            self.jdwp
                .class_type_set_values(class_id, &[(field.field_id, jdwp_value)]),
        )
        .await?;

        let new_value = cancellable_jdwp(
            cancel,
            self.jdwp
                .reference_type_get_values(class_id, &[field.field_id]),
        )
        .await?
        .into_iter()
        .next()
        .unwrap_or(JdwpValue::Void);

        let static_type = signature_to_type_name(&field.signature);
        let formatted = self
            .format_value(cancel, &new_value, Some(&static_type), 0)
            .await?;

        Ok(Some(json!({
            "value": formatted.value,
            "type": formatted.type_name,
            "variablesReference": formatted.variables_reference,
            "namedVariables": formatted.named_variables,
            "indexedVariables": formatted.indexed_variables,
        })))
    }

    async fn parse_set_variable_value(
        &mut self,
        cancel: &CancellationToken,
        signature: &str,
        raw_value: &str,
    ) -> Result<JdwpValue> {
        let value = raw_value.trim();
        if value.eq_ignore_ascii_case("null") {
            let tag = if signature.starts_with('[') {
                b'['
            } else if signature == "Ljava/lang/String;" {
                b's'
            } else {
                b'L'
            };
            return Ok(JdwpValue::Object { tag, id: 0 });
        }

        let parse_err = || {
            DebuggerError::InvalidRequest(format!(
                "could not parse `{raw_value}` as `{}`",
                signature_to_type_name(signature)
            ))
        };

        match signature.as_bytes().first().copied() {
            Some(b'Z') => {
                let lowered = value.to_ascii_lowercase();
                match lowered.as_str() {
                    "true" => Ok(JdwpValue::Boolean(true)),
                    "false" => Ok(JdwpValue::Boolean(false)),
                    "1" => Ok(JdwpValue::Boolean(true)),
                    "0" => Ok(JdwpValue::Boolean(false)),
                    _ => Err(parse_err()),
                }
            }
            Some(b'B') => Ok(JdwpValue::Byte(
                value.parse::<i8>().map_err(|_| parse_err())?,
            )),
            Some(b'S') => Ok(JdwpValue::Short(
                value.parse::<i16>().map_err(|_| parse_err())?,
            )),
            Some(b'I') => Ok(JdwpValue::Int(
                value.parse::<i32>().map_err(|_| parse_err())?,
            )),
            Some(b'J') => Ok(JdwpValue::Long(
                value.parse::<i64>().map_err(|_| parse_err())?,
            )),
            Some(b'F') => Ok(JdwpValue::Float(
                value.parse::<f32>().map_err(|_| parse_err())?,
            )),
            Some(b'D') => Ok(JdwpValue::Double(
                value.parse::<f64>().map_err(|_| parse_err())?,
            )),
            Some(b'C') => {
                let trimmed = value.trim();
                let char_value = if let Some(inner) = trimmed
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
                {
                    let mut chars = inner.chars();
                    let Some(ch) = chars.next() else {
                        return Err(parse_err());
                    };
                    if chars.next().is_some() {
                        return Err(parse_err());
                    }
                    ch as u16
                } else {
                    trimmed.parse::<u16>().map_err(|_| parse_err())?
                };
                Ok(JdwpValue::Char(char_value))
            }
            Some(b'L') | Some(b'[') => {
                if signature == "Ljava/lang/String;" {
                    let content = parse_java_string_literal(value);
                    let object_id =
                        cancellable_jdwp(cancel, self.jdwp.virtual_machine_create_string(&content))
                            .await?;
                    Ok(JdwpValue::Object {
                        tag: b's',
                        id: object_id,
                    })
                } else {
                    Err(DebuggerError::InvalidRequest(format!(
                        "setting non-String object references is not supported (type {})",
                        signature_to_type_name(signature)
                    )))
                }
            }
            _ => Err(parse_err()),
        }
    }

    pub async fn handle_breakpoint_event(
        &mut self,
        request_id: i32,
        thread: ThreadId,
        _location: Location,
    ) -> Result<BreakpointDisposition> {
        // Pull any config we need out of `breakpoint_metadata` up front so we don't hold a mutable
        // borrow across `await` points.
        let (hit_count, hit_condition, condition, log_message) = {
            let Some(meta) = self.breakpoint_metadata.get_mut(&request_id) else {
                // Unknown breakpoint request. Prefer stopping over auto-resuming to avoid silently
                // skipping user-visible pause points.
                return Ok(BreakpointDisposition::Stop);
            };

            meta.hit_count = meta.hit_count.saturating_add(1);
            (
                meta.hit_count,
                meta.hit_condition.clone(),
                meta.condition.clone(),
                meta.log_message.clone(),
            )
        };

        let is_logpoint = log_message.is_some();

        if let Some(hit_expr) = hit_condition.as_deref() {
            match hit_condition_matches(hit_expr, hit_count) {
                Ok(true) => {}
                Ok(false) => return Ok(BreakpointDisposition::Continue),
                Err(_) => {
                    return Ok(if is_logpoint {
                        BreakpointDisposition::Continue
                    } else {
                        BreakpointDisposition::Stop
                    });
                }
            }
        }

        let needs_locals = condition
            .as_deref()
            .is_some_and(|c| condition_needs_locals(c))
            || log_message
                .as_deref()
                .is_some_and(|m| log_message_needs_locals(m));

        let locals = if needs_locals {
            let frame = match self.jdwp.frames(thread, 0, 1).await {
                Ok(frames) => frames.into_iter().next(),
                Err(err) => {
                    // Logpoints are configured with `SuspendPolicy::NONE`, which means the VM
                    // may reject frame inspection with `THREAD_NOT_SUSPENDED`. In that case, we
                    // still want to emit the log message rather than turning the logpoint into a
                    // stop.
                    if is_logpoint && !matches!(err, JdwpError::Cancelled) {
                        None
                    } else {
                        return Err(err.into());
                    }
                }
            };

            match frame {
                Some(frame) => {
                    let frame = FrameHandle {
                        thread,
                        frame_id: frame.frame_id,
                        location: frame.location,
                    };
                    match self.locals_values(&frame).await {
                        Ok(values) => Some(values),
                        Err(err) => {
                            if is_logpoint {
                                None
                            } else {
                                return Err(err);
                            }
                        }
                    }
                }
                None => {
                    if is_logpoint {
                        None
                    } else {
                        return Ok(BreakpointDisposition::Stop);
                    }
                }
            }
        } else {
            None
        };

        if let Some(cond) = condition.as_deref() {
            match eval_breakpoint_condition(cond, locals.as_ref()) {
                Ok(true) => {}
                Ok(false) => return Ok(BreakpointDisposition::Continue),
                Err(_) => {
                    return Ok(if is_logpoint {
                        BreakpointDisposition::Continue
                    } else {
                        BreakpointDisposition::Stop
                    });
                }
            }
        }

        if let Some(template) = log_message.as_deref() {
            let rendered = render_log_message(template, locals.as_ref());
            return Ok(BreakpointDisposition::Log { message: rendered });
        }

        Ok(BreakpointDisposition::Stop)
    }

    fn alloc_frame_handle(&mut self, thread: ThreadId, frame: &FrameInfo) -> i64 {
        self.frame_handles.intern(
            FrameKey {
                thread,
                frame_id: frame.frame_id,
            },
            FrameHandle {
                thread,
                frame_id: frame.frame_id,
                location: frame.location,
            },
        )
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
        let file = cancellable_jdwp(cancel, self.jdwp.reference_type_source_file(class_id)).await?;
        self.source_cache.insert(class_id, file.clone());
        Ok(file)
    }

    async fn signature(
        &mut self,
        cancel: &CancellationToken,
        class_id: ReferenceTypeId,
    ) -> std::result::Result<String, JdwpError> {
        check_cancel_jdwp(cancel)?;
        if let Some(v) = self.signature_cache.get(&class_id) {
            return Ok(v.clone());
        }
        let sig = cancellable_jdwp(cancel, self.jdwp.reference_type_signature(class_id)).await?;
        self.signature_cache.insert(class_id, sig.clone());
        Ok(sig)
    }

    async fn resolve_source_path(
        &mut self,
        cancel: &CancellationToken,
        class_id: ReferenceTypeId,
        source_file: &str,
    ) -> std::result::Result<Option<PathBuf>, JdwpError> {
        check_cancel_jdwp(cancel)?;

        if let Some(cached) = self.resolved_source_paths.get(&class_id) {
            return Ok(Some(cached.clone()));
        }

        if !self.source_roots.is_empty() {
            let sig = self.signature(cancel, class_id).await?;
            if let Some(package_path) = package_path_from_signature(&sig) {
                for root in &self.source_roots {
                    let candidate = if package_path.as_os_str().is_empty() {
                        root.join(source_file)
                    } else {
                        root.join(&package_path).join(source_file)
                    };
                    if candidate.is_file() {
                        let candidate = std::fs::canonicalize(&candidate).unwrap_or(candidate);
                        self.resolved_source_paths
                            .insert(class_id, candidate.clone());
                        return Ok(Some(candidate));
                    }
                }
            }
        }

        if let Some(path) = self.source_paths.get(source_file) {
            return Ok(Some(PathBuf::from(path)));
        }

        Ok(None)
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
        let methods = cancellable_jdwp(cancel, self.jdwp.reference_type_methods(class_id)).await?;
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
            let t =
                cancellable_jdwp(cancel, self.jdwp.method_line_table(class_id, method_id)).await?;
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

        let has_this = in_scope.iter().any(|v| v.name == "this");

        let mut out = Vec::with_capacity(in_scope.len() + usize::from(!has_this));

        if !has_this {
            match cancellable_jdwp(
                cancel,
                self.jdwp
                    .stack_frame_this_object(frame.thread, frame.frame_id),
            )
            .await
            {
                Ok(object_id) if object_id != 0 => {
                    out.push(
                        self.render_variable(
                            cancel,
                            "this",
                            Some("this".to_string()),
                            JdwpValue::Object {
                                tag: b'L',
                                id: object_id,
                            },
                            None,
                        )
                        .await?,
                    );
                }
                Ok(_) => {}
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(_) => {}
            }
        }

        for (var, value) in in_scope.into_iter().zip(values.into_iter()) {
            let name = var.name;
            out.push(
                self.render_variable(
                    cancel,
                    name.clone(),
                    Some(name),
                    value,
                    Some(signature_to_type_name(&var.signature)),
                )
                .await?,
            );
        }
        Ok(out)
    }

    async fn locals_values(&mut self, frame: &FrameHandle) -> Result<HashMap<String, JdwpValue>> {
        let (_argc, vars) = self
            .jdwp
            .method_variable_table(frame.location.class_id, frame.location.method_id)
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
        let values = self
            .jdwp
            .stack_frame_get_values(frame.thread, frame.frame_id, &slots)
            .await?;

        let mut out = HashMap::new();
        for (var, value) in in_scope.into_iter().zip(values.into_iter()) {
            out.insert(var.name, value);
        }

        if !out.contains_key("this") {
            if let Ok(object_id) = self
                .jdwp
                .stack_frame_this_object(frame.thread, frame.frame_id)
                .await
            {
                if object_id != 0 {
                    out.insert(
                        "this".to_string(),
                        JdwpValue::Object {
                            tag: b'L',
                            id: object_id,
                        },
                    );
                }
            }
        }
        Ok(out)
    }

    async fn static_variables(
        &mut self,
        cancel: &CancellationToken,
        class_id: ReferenceTypeId,
    ) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;
        const MODIFIER_STATIC: u32 = 0x0008;

        let fields = cancellable_jdwp(cancel, self.jdwp.reference_type_fields(class_id)).await?;
        let static_fields: Vec<_> = fields
            .into_iter()
            .filter(|field| field.mod_bits & MODIFIER_STATIC != 0)
            .collect();

        if static_fields.is_empty() {
            return Ok(Vec::new());
        }

        let field_ids: Vec<u64> = static_fields.iter().map(|field| field.field_id).collect();
        let values = cancellable_jdwp(
            cancel,
            self.jdwp.reference_type_get_values(class_id, &field_ids),
        )
        .await?;

        let mut out = Vec::with_capacity(static_fields.len());
        for (field, value) in static_fields.into_iter().zip(values.into_iter()) {
            check_cancel(cancel)?;
            out.push(
                self.render_variable(
                    cancel,
                    field.name,
                    None,
                    value,
                    Some(signature_to_type_name(&field.signature)),
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
        start: Option<i64>,
        count: Option<i64>,
    ) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;
        let Some(object_id) = self.objects.object_id(handle) else {
            return Ok(vec![evicted_variable()]);
        };

        let runtime_type = self.objects.runtime_type(handle).unwrap_or_default();
        if runtime_type.ends_with("[]") {
            return self
                .array_variables(cancel, handle, object_id, start, count)
                .await;
        }

        let parent_eval = if self.objects.is_pinned(handle) {
            Some(format!("__novaPinned[{}]", handle.as_u32()))
        } else {
            self.objects.evaluate_name(handle).map(|s| s.to_string())
        };

        let children =
            match cancellable_jdwp(cancel, self.inspector.object_children(object_id)).await {
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
            let name = child.name;
            let eval = parent_eval
                .as_ref()
                .and_then(|base| is_identifier(&name).then(|| format!("{base}.{name}")));
            out.push(
                self.render_variable(cancel, name, eval, child.value, child.static_type)
                    .await?,
            );
        }
        Ok(out)
    }

    async fn array_variables(
        &mut self,
        cancel: &CancellationToken,
        handle: ObjectHandle,
        array_id: ObjectId,
        start: Option<i64>,
        count: Option<i64>,
    ) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;

        let length =
            match cancellable_jdwp(cancel, self.jdwp.array_reference_length(array_id)).await {
                Ok(length) => length.max(0) as i64,
                Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                Err(JdwpError::VmError(code)) if code == ERROR_INVALID_OBJECT => {
                    self.objects.mark_invalid_object_id(array_id);
                    return Ok(vec![invalid_collected_variable()]);
                }
                Err(err) => return Err(err.into()),
            };

        let element_type = self
            .objects
            .runtime_type(handle)
            .and_then(|t| t.strip_suffix("[]"))
            .unwrap_or("<unknown>")
            .to_string();

        let parent_eval = if self.objects.is_pinned(handle) {
            Some(format!("__novaPinned[{}]", handle.as_u32()))
        } else {
            self.objects.evaluate_name(handle).map(|s| s.to_string())
        };
        let paging = start.is_some() || count.is_some();

        let start_index = start.unwrap_or(0).max(0).min(length);
        let max_count = length.saturating_sub(start_index);
        let count = count
            .unwrap_or(ARRAY_CHILD_SAMPLE)
            .max(0)
            .min(max_count)
            .min(VARIABLES_PAGE_LIMIT);

        let values = match cancellable_jdwp(
            cancel,
            self.jdwp
                .array_reference_get_values(array_id, start_index as i32, count as i32),
        )
        .await
        {
            Ok(values) => values,
            Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
            Err(JdwpError::VmError(code)) if code == ERROR_INVALID_OBJECT => {
                self.objects.mark_invalid_object_id(array_id);
                return Ok(vec![invalid_collected_variable()]);
            }
            Err(err) => return Err(err.into()),
        };

        let mut out = Vec::with_capacity(values.len() + usize::from(!paging));
        if !paging {
            out.push(
                self.render_variable(
                    cancel,
                    "length",
                    parent_eval.as_ref().map(|base| format!("{base}.length")),
                    JdwpValue::Int(length as i32),
                    Some("int".to_string()),
                )
                .await?,
            );
        }

        for (idx, value) in values.into_iter().enumerate() {
            check_cancel(cancel)?;
            let idx = start_index + idx as i64;
            let eval = parent_eval.as_ref().map(|base| format!("{base}[{idx}]"));
            out.push(
                self.render_variable(
                    cancel,
                    format!("[{idx}]"),
                    eval,
                    value,
                    Some(element_type.clone()),
                )
                .await?,
            );
        }

        Ok(out)
    }

    async fn pinned_variables(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Vec<serde_json::Value>> {
        check_cancel(cancel)?;
        let pinned: Vec<_> = self.objects.pinned_handles().collect();
        let mut vars = Vec::with_capacity(pinned.len());

        for handle in pinned {
            check_cancel(cancel)?;
            let Some(object_id) = self.objects.object_id(handle) else {
                continue;
            };

            let evaluate_name = format!("__novaPinned[{}]", handle.as_u32());

            let value = JdwpValue::Object {
                tag: b'L',
                id: object_id,
            };
            let formatted = self.format_value(cancel, &value, None, 0).await?;
            vars.push(variable_json(
                handle.to_string(),
                formatted.value,
                formatted.type_name,
                formatted.variables_reference,
                Some(evaluate_name),
                Some(json!({
                    "kind": "data",
                    "attributes": ["pinned"],
                })),
                formatted.named_variables,
                formatted.indexed_variables,
            ));
        }

        Ok(vars)
    }

    async fn render_variable(
        &mut self,
        cancel: &CancellationToken,
        name: impl Into<String>,
        evaluate_name: Option<String>,
        value: JdwpValue,
        static_type: Option<String>,
    ) -> Result<serde_json::Value> {
        check_cancel(cancel)?;
        let name = name.into();
        let formatted = self
            .format_value(cancel, &value, static_type.as_deref(), 0)
            .await?;

        let evaluate_name = evaluate_name.or_else(|| Some(name.clone()));
        if let Some(eval) = evaluate_name.clone() {
            if let Some(handle) =
                ObjectHandle::from_variables_reference(formatted.variables_reference)
            {
                self.objects.set_evaluate_name(handle, eval);
            }
        }

        Ok(variable_json(
            name.clone(),
            formatted.value,
            formatted.type_name,
            formatted.variables_reference,
            evaluate_name,
            formatted.presentation_hint,
            formatted.named_variables,
            formatted.indexed_variables,
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
        let (_ref_type_tag, class_id) =
            cancellable_jdwp(cancel, self.jdwp.object_reference_reference_type(object_id)).await?;
        let sig = cancellable_jdwp(cancel, self.jdwp.reference_type_signature(class_id)).await?;
        Ok(signature_to_object_type_name(&sig))
    }

    async fn exception_message(
        &mut self,
        cancel: &CancellationToken,
        exception_id: ObjectId,
    ) -> std::result::Result<Option<String>, JdwpError> {
        let Some(field_id) = self.throwable_detail_message_field(cancel).await? else {
            return Ok(None);
        };

        let values = cancellable_jdwp(
            cancel,
            self.jdwp
                .object_reference_get_values(exception_id, &[field_id]),
        )
        .await?;
        let Some(value) = values.into_iter().next() else {
            return Ok(None);
        };

        let JdwpValue::Object { id, .. } = value else {
            return Ok(None);
        };
        if id == 0 {
            return Ok(None);
        }

        match cancellable_jdwp(cancel, self.jdwp.string_reference_value(id)).await {
            Ok(s) if s.is_empty() => Ok(None),
            Ok(s) => Ok(Some(s)),
            Err(JdwpError::Cancelled) => Err(JdwpError::Cancelled),
            Err(_) => Ok(None),
        }
    }

    async fn throwable_detail_message_field(
        &mut self,
        cancel: &CancellationToken,
    ) -> std::result::Result<Option<u64>, JdwpError> {
        if let Some(cached) = self.throwable_detail_message_field {
            return Ok(cached);
        }

        let classes = cancellable_jdwp(
            cancel,
            self.jdwp.classes_by_signature("Ljava/lang/Throwable;"),
        )
        .await?;
        let Some(throwable) = classes.first() else {
            self.throwable_detail_message_field = Some(None);
            return Ok(None);
        };

        let fields =
            cancellable_jdwp(cancel, self.jdwp.reference_type_fields(throwable.type_id)).await?;
        let field_id = fields
            .into_iter()
            .find(|field| field.name == "detailMessage")
            .map(|field| field.field_id);
        self.throwable_detail_message_field = Some(field_id);
        Ok(field_id)
    }

    async fn location_for_line(
        &mut self,
        cancel: &CancellationToken,
        class: &ClassInfo,
        line: i32,
    ) -> Result<Option<ResolvedLocation>> {
        check_cancel(cancel)?;
        let methods = if let Some(methods) = self.methods_cache.get(&class.type_id) {
            methods.clone()
        } else {
            let methods =
                cancellable_jdwp(cancel, self.jdwp.reference_type_methods(class.type_id)).await?;
            self.methods_cache.insert(class.type_id, methods.clone());
            methods
        };

        // Multiple methods (including synthetic `lambda$...` methods) can report the same
        // source line in their line tables. For DAP breakpoint mapping we generally want to
        // stop at the user-visible statement in the enclosing method, not inside a lambda
        // implementation. Prefer non-synthetic methods when a line is present in both.
        //
        // This avoids surprising behavior where a breakpoint on a line containing an inline
        // lambda (e.g. a Stream pipeline) triggers inside the lambda body instead of at the
        // statement boundary in the enclosing method.
        let mut line_candidates: HashMap<i32, (bool, u64, u64)> = HashMap::new();

        for method in methods {
            check_cancel(cancel)?;
            const MODIFIER_SYNTHETIC: u32 = 0x1000;
            let is_synthetic = (method.mod_bits & MODIFIER_SYNTHETIC) != 0
                || method.name.starts_with("lambda$")
                || method.name.contains("$$Lambda$");
            let key = (class.type_id, method.method_id);
            let table = if let Some(t) = self.line_table_cache.get(&key) {
                Some(t.clone())
            } else {
                match cancellable_jdwp(
                    cancel,
                    self.jdwp.method_line_table(class.type_id, method.method_id),
                )
                .await
                {
                    Ok(table) => {
                        self.line_table_cache.insert(key, table.clone());
                        Some(table)
                    }
                    Err(JdwpError::Cancelled) => return Err(JdwpError::Cancelled.into()),
                    Err(_) => None,
                }
            };
            let Some(table) = table else { continue };

            for entry in &table.lines {
                check_cancel(cancel)?;
                if entry.line <= 0 {
                    continue;
                }

                line_candidates
                    .entry(entry.line)
                    .and_modify(|(best_synth, best_method, best_index)| {
                        let replace = match (*best_synth, is_synthetic) {
                            (true, false) => true,
                            (false, true) => false,
                            _ => {
                                entry.code_index < *best_index
                                    || (entry.code_index == *best_index
                                        && method.method_id < *best_method)
                            }
                        };
                        if replace {
                            *best_synth = is_synthetic;
                            *best_method = method.method_id;
                            *best_index = entry.code_index;
                        }
                    })
                    .or_insert((is_synthetic, method.method_id, entry.code_index));
            }
        }

        let mut best: Option<(i64, bool, i32, u64, u64)> = None;
        for (candidate_line, (_is_synthetic, method_id, code_index)) in line_candidates {
            let distance = (candidate_line as i64 - line as i64).abs();
            let is_previous_line = candidate_line < line;

            let candidate_key = (distance, is_previous_line, candidate_line);
            let replace = match best {
                None => true,
                Some((best_distance, best_is_previous, best_line, _, _)) => {
                    candidate_key < (best_distance, best_is_previous, best_line)
                }
            };

            if replace {
                best = Some((
                    distance,
                    is_previous_line,
                    candidate_line,
                    method_id,
                    code_index,
                ));
            }
        }

        let Some((_distance, _is_previous, resolved_line, method_id, code_index)) = best else {
            return Ok(None);
        };

        Ok(Some(ResolvedLocation {
            line: resolved_line,
            location: Location {
                type_tag: class.ref_type_tag,
                class_id: class.type_id,
                method_id,
                index: code_index,
            },
        }))
    }

    async fn on_class_prepare(&mut self, ref_type_tag: u8, type_id: ReferenceTypeId) -> Result<()> {
        let cancel = CancellationToken::new();
        let source_file = self.source_file(&cancel, type_id).await?;
        let file = match self
            .resolve_source_path(&cancel, type_id, &source_file)
            .await
        {
            Ok(Some(path)) => path.to_string_lossy().to_string(),
            Ok(None) => source_file.clone(),
            Err(_) => source_file.clone(),
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

        if let Some(bps) = self.requested_breakpoints.get(&file).cloned() {
            let mut entries = Vec::new();
            for bp in bps {
                let dap_id = bp.id;
                let spec_line = bp.spec.line;
                let condition = normalize_breakpoint_string(bp.spec.condition);
                let mut hit_condition = normalize_breakpoint_string(bp.spec.hit_condition);
                let log_message = normalize_breakpoint_string(bp.spec.log_message);

                let count_modifier = hit_condition
                    .as_deref()
                    .and_then(|raw| raw.parse::<u32>().ok())
                    .filter(|count| *count > 1);
                if count_modifier.is_some() {
                    hit_condition = None;
                }

                let suspend_policy = if log_message.is_some() {
                    JDWP_SUSPEND_POLICY_NONE
                } else {
                    JDWP_SUSPEND_POLICY_EVENT_THREAD
                };

                if let Some(resolved) = self.location_for_line(&cancel, &class, spec_line).await? {
                    let mut modifiers = vec![EventModifier::LocationOnly {
                        location: resolved.location,
                    }];
                    if let Some(count) = count_modifier {
                        modifiers.push(EventModifier::Count { count });
                    }
                    if let Ok(request_id) = self
                        .jdwp
                        .event_request_set(2, suspend_policy, modifiers)
                        .await
                    {
                        let was_pending = self
                            .pending_breakpoints
                            .get(&file)
                            .is_some_and(|ids| ids.contains(&dap_id));

                        entries.push(BreakpointEntry { request_id });
                        self.breakpoint_metadata.insert(
                            request_id,
                            BreakpointMetadata {
                                condition,
                                hit_condition,
                                log_message,
                                hit_count: 0,
                            },
                        );

                        if was_pending {
                            let mut bp = serde_json::Map::new();
                            bp.insert("verified".to_string(), json!(true));
                            bp.insert("line".to_string(), json!(resolved.line));
                            bp.insert("id".to_string(), json!(dap_id));
                            bp.insert("source".to_string(), json!({ "path": file.clone() }));
                            self.breakpoint_updates.push(Value::Object(bp));

                            if let Some(ids) = self.pending_breakpoints.get_mut(&file) {
                                ids.remove(&dap_id);
                                if ids.is_empty() {
                                    self.pending_breakpoints.remove(&file);
                                }
                            }
                        }
                    }
                }
            }
            if !entries.is_empty() {
                self.breakpoints.entry(file).or_default().extend(entries);
            }
        }

        let _ = self
            .apply_function_breakpoints_for_class(&cancel, &class)
            .await;
        Ok(())
    }

    async fn apply_function_breakpoints_for_class(
        &mut self,
        cancel: &CancellationToken,
        class: &ClassInfo,
    ) -> Result<()> {
        if self.requested_function_breakpoints.is_empty() {
            return Ok(());
        }

        let source_file = match self.source_file(cancel, class.type_id).await {
            Ok(file) => file,
            Err(_) => String::new(),
        };
        let file = match self
            .resolve_source_path(cancel, class.type_id, &source_file)
            .await
        {
            Ok(Some(path)) => path.to_string_lossy().to_string(),
            Ok(None) => source_file.clone(),
            Err(_) => source_file.clone(),
        };

        let breakpoints = self.requested_function_breakpoints.clone();
        let mut entries = Vec::new();

        for bp in breakpoints {
            check_cancel(cancel)?;
            let dap_id = bp.id;
            let spec_name = bp.spec.name.trim().to_string();
            let Some((class_name, method_name)) = parse_function_breakpoint(&spec_name) else {
                continue;
            };

            if class_name_to_signature(&class_name) != class.signature {
                continue;
            }

            let is_pending = self.pending_function_breakpoints.contains(&dap_id);
            let condition = normalize_breakpoint_string(bp.spec.condition);
            let mut hit_condition = normalize_breakpoint_string(bp.spec.hit_condition);
            let log_message = normalize_breakpoint_string(bp.spec.log_message);

            let count_modifier = hit_condition
                .as_deref()
                .and_then(|raw| raw.parse::<u32>().ok())
                .filter(|count| *count > 1);
            if count_modifier.is_some() {
                hit_condition = None;
            }

            let suspend_policy = if log_message.is_some() {
                JDWP_SUSPEND_POLICY_NONE
            } else {
                JDWP_SUSPEND_POLICY_EVENT_THREAD
            };

            let methods = match self.methods_cache.get(&class.type_id) {
                Some(methods) => methods.clone(),
                None => {
                    match cancellable_jdwp(cancel, self.jdwp.reference_type_methods(class.type_id))
                        .await
                    {
                        Ok(methods) => {
                            self.methods_cache.insert(class.type_id, methods.clone());
                            methods
                        }
                        Err(_) => continue,
                    }
                }
            };

            let mut pending_first_line: Option<i32> = None;
            let mut installed_any = false;

            for method in methods.iter().filter(|m| m.name == method_name) {
                check_cancel(cancel)?;

                let table = match cancellable_jdwp(
                    cancel,
                    self.jdwp.method_line_table(class.type_id, method.method_id),
                )
                .await
                {
                    Ok(table) => table,
                    Err(_) => continue,
                };
                self.line_table_cache
                    .insert((class.type_id, method.method_id), table.clone());

                let index = table.start;
                let line = table
                    .lines
                    .iter()
                    .filter(|entry| entry.code_index <= index)
                    .map(|entry| entry.line)
                    .last()
                    .or_else(|| table.lines.first().map(|entry| entry.line))
                    .unwrap_or(1);

                let location = Location {
                    type_tag: class.ref_type_tag,
                    class_id: class.type_id,
                    method_id: method.method_id,
                    index,
                };

                let mut modifiers = vec![EventModifier::LocationOnly { location }];
                if let Some(count) = count_modifier {
                    modifiers.push(EventModifier::Count { count });
                }

                if let Ok(request_id) = self
                    .jdwp
                    .event_request_set(2, suspend_policy, modifiers)
                    .await
                {
                    installed_any = true;
                    pending_first_line.get_or_insert(line);
                    entries.push(BreakpointEntry { request_id });
                    self.breakpoint_metadata.insert(
                        request_id,
                        BreakpointMetadata {
                            condition: condition.clone(),
                            hit_condition: hit_condition.clone(),
                            log_message: log_message.clone(),
                            hit_count: 0,
                        },
                    );
                }
            }

            if is_pending && installed_any {
                let mut bp = serde_json::Map::new();
                bp.insert("verified".to_string(), json!(true));
                if let Some(line) = pending_first_line {
                    bp.insert("line".to_string(), json!(line));
                }
                bp.insert("id".to_string(), json!(dap_id));
                if !file.is_empty() {
                    bp.insert("source".to_string(), json!({ "path": file.clone() }));
                }
                self.breakpoint_updates.push(Value::Object(bp));
                self.pending_function_breakpoints.remove(&dap_id);
            }
        }

        if !entries.is_empty() {
            self.function_breakpoints.extend(entries);
        }

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

        match cancellable_jdwp(
            cancel,
            self.jdwp.object_reference_disable_collection(object_id),
        )
        .await
        {
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

    async fn unpin_object(
        &mut self,
        cancel: &CancellationToken,
        handle: ObjectHandle,
    ) -> Result<()> {
        check_cancel(cancel)?;
        if !self.objects.is_pinned(handle) {
            return Ok(());
        }

        if let Some(object_id) = self.objects.object_id(handle) {
            match cancellable_jdwp(
                cancel,
                self.jdwp.object_reference_enable_collection(object_id),
            )
            .await
            {
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

fn package_path_from_signature(sig: &str) -> Option<PathBuf> {
    let mut sig = sig.trim();
    while let Some(rest) = sig.strip_prefix('[') {
        sig = rest;
    }
    let internal = sig.strip_prefix('L').and_then(|s| s.strip_suffix(';'))?;
    if let Some((pkg, _name)) = internal.rsplit_once('/') {
        Some(PathBuf::from(pkg))
    } else {
        Some(PathBuf::new())
    }
}

fn class_name_to_signature(class_name: &str) -> String {
    let class_name = class_name.trim();
    if class_name.starts_with('L') && class_name.ends_with(';') {
        return class_name.to_string();
    }
    let internal = class_name.replace('.', "/");
    format!("L{internal};")
}

fn parse_function_breakpoint(spec: &str) -> Option<(String, String)> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }

    // Strip an optional parameter list (DAP UIs sometimes include it).
    let spec = spec.split_once('(').map(|(head, _)| head).unwrap_or(spec);
    let spec = spec.trim();

    let (class, method) = spec.rsplit_once('#').or_else(|| spec.rsplit_once('.'))?;
    let class = class.trim();
    let method = method.trim();
    if class.is_empty() || method.is_empty() {
        return None;
    }
    Some((class.to_string(), method.to_string()))
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum EvalSegment {
    Field(String),
    Index(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EvalBase {
    This,
    Local(String),
    Pinned(ObjectHandle),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedEvalExpression {
    base: EvalBase,
    segments: Vec<EvalSegment>,
}

fn parse_eval_expression(expr: &str) -> std::result::Result<ParsedEvalExpression, ()> {
    fn split_identifier_prefix(input: &str) -> Option<(&str, &str)> {
        let mut end = 0usize;
        for (idx, ch) in input.char_indices() {
            if idx == 0 {
                if !(ch == '_' || ch == '$' || ch.is_ascii_alphabetic()) {
                    return None;
                }
                end = ch.len_utf8();
                continue;
            }

            if ch == '_' || ch == '$' || ch.is_ascii_alphanumeric() {
                end = idx + ch.len_utf8();
            } else {
                break;
            }
        }
        if end == 0 {
            return None;
        }
        Some((&input[..end], &input[end..]))
    }

    let mut rest = expr.trim();
    let base = if let Some(after_pinned) = rest.strip_prefix("__novaPinned") {
        let after_pinned = after_pinned.trim_start();
        let after_bracket = after_pinned.strip_prefix('[').ok_or(())?;
        let close = after_bracket.find(']').ok_or(())?;
        let raw = after_bracket[..close].trim();
        if raw.is_empty() {
            return Err(());
        }
        let handle = raw.parse::<u32>().map_err(|_| ())?;
        let variables_reference = OBJECT_HANDLE_BASE + i64::from(handle);
        let handle = ObjectHandle::from_variables_reference(variables_reference).ok_or(())?;
        rest = &after_bracket[close + 1..];
        EvalBase::Pinned(handle)
    } else {
        let (base_ident, next) = split_identifier_prefix(rest).ok_or(())?;
        rest = next;

        if base_ident == "this" {
            EvalBase::This
        } else {
            EvalBase::Local(base_ident.to_string())
        }
    };

    let mut segments = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }

        if let Some(after_dot) = rest.strip_prefix('.') {
            let after_dot = after_dot.trim_start();
            let (field, next) = split_identifier_prefix(after_dot).ok_or(())?;
            rest = next;
            segments.push(EvalSegment::Field(field.to_string()));
            continue;
        }

        if let Some(after_bracket) = rest.strip_prefix('[') {
            let close = after_bracket.find(']').ok_or(())?;
            let raw = after_bracket[..close].trim();
            if raw.is_empty() {
                return Err(());
            }
            let index = raw.parse::<i64>().map_err(|_| ())?;
            segments.push(EvalSegment::Index(index));
            rest = &after_bracket[close + 1..];
            continue;
        }

        return Err(());
    }

    Ok(ParsedEvalExpression { base, segments })
}

fn parse_array_index_name(name: &str) -> Option<i64> {
    let trimmed = name.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    inner.trim().parse::<i64>().ok()
}

fn parse_java_string_literal(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(inner) = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
        })
    else {
        return trimmed.to_string();
    };

    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(esc) = chars.next() else {
            out.push('\\');
            break;
        };
        match esc {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '\'' => out.push('\''),
            'u' => {
                let mut code = String::new();
                for _ in 0..4 {
                    match chars.next() {
                        Some(h) => code.push(h),
                        None => break,
                    }
                }
                if let Ok(value) = u16::from_str_radix(&code, 16) {
                    if let Some(ch) = std::char::from_u32(value as u32) {
                        out.push(ch);
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
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
    named_variables: Option<i64>,
    indexed_variables: Option<i64>,
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
        let (
            display,
            variables_reference,
            presentation_hint,
            runtime_type,
            named_variables,
            indexed_variables,
        ) = self
            .format_value_display(cancel, value, static_type, depth)
            .await?;

        Ok(FormattedValue {
            value: display,
            type_name: static_type
                .map(|s| s.to_string())
                .or_else(|| value_type_name(value, runtime_type.as_deref())),
            variables_reference,
            presentation_hint,
            named_variables,
            indexed_variables,
        })
    }

    #[async_recursion::async_recursion]
    async fn format_value_display(
        &mut self,
        cancel: &CancellationToken,
        value: &JdwpValue,
        static_type: Option<&str>,
        depth: usize,
    ) -> Result<(
        String,
        i64,
        Option<Value>,
        Option<String>,
        Option<i64>,
        Option<i64>,
    )> {
        check_cancel(cancel)?;
        match value {
            JdwpValue::Void => Ok(("void".to_string(), 0, None, None, None, None)),
            JdwpValue::Boolean(v) => Ok((v.to_string(), 0, None, None, None, None)),
            JdwpValue::Byte(v) => Ok((v.to_string(), 0, None, None, None, None)),
            JdwpValue::Short(v) => Ok((v.to_string(), 0, None, None, None, None)),
            JdwpValue::Int(v) => Ok((v.to_string(), 0, None, None, None, None)),
            JdwpValue::Long(v) => Ok((v.to_string(), 0, None, None, None, None)),
            JdwpValue::Float(v) => Ok((trim_float(*v as f64), 0, None, None, None, None)),
            JdwpValue::Double(v) => Ok((trim_float(*v), 0, None, None, None, None)),
            JdwpValue::Char(v) => Ok((
                format!("'{}'", decode_java_char(*v)),
                0,
                None,
                None,
                None,
                None,
            )),
            JdwpValue::Object { id: 0, .. } => Ok(("null".to_string(), 0, None, None, None, None)),
            JdwpValue::Object { id, .. } => {
                self.format_object(cancel, *id, static_type, depth).await
            }
        }
    }

    async fn format_object(
        &mut self,
        cancel: &CancellationToken,
        object_id: ObjectId,
        static_type: Option<&str>,
        depth: usize,
    ) -> Result<(
        String,
        i64,
        Option<Value>,
        Option<String>,
        Option<i64>,
        Option<i64>,
    )> {
        check_cancel(cancel)?;
        let fallback_runtime = static_type.unwrap_or("<object>").to_string();

        let runtime_type =
            match cancellable_jdwp(cancel, self.inspector.runtime_type_name(object_id)).await {
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
                        None,
                        None,
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
                None,
                None,
            ));
        }

        let preview = match cancellable_jdwp(cancel, self.inspector.preview_object(object_id)).await
        {
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
                    None,
                    None,
                ));
            }
            Err(_err) => {
                return Ok((
                    format!("{}{handle}", simple_type_name(&runtime_type)),
                    variables_reference,
                    Some(json!({ "kind": "data" })),
                    Some(runtime_type),
                    None,
                    None,
                ));
            }
        };

        // Ensure the registry sees the most specific runtime type.
        let handle = self.objects.track_object(object_id, &preview.runtime_type);
        let variables_reference = handle.as_variables_reference();

        let runtime_simple = simple_type_name(&preview.runtime_type).to_string();
        let named_variables = None;
        let mut indexed_variables = None;
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
                indexed_variables = Some(length as i64);
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
            named_variables,
            indexed_variables,
        ))
    }

    async fn format_inline(
        &mut self,
        cancel: &CancellationToken,
        value: &JdwpValue,
        depth: usize,
    ) -> Result<String> {
        let (display, _ref, _hint, _rt, _named, _indexed) =
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
    named_variables: Option<i64>,
    indexed_variables: Option<i64>,
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
    if let Some(named) = named_variables {
        obj.insert("namedVariables".to_string(), json!(named));
    }
    if let Some(indexed) = indexed_variables {
        obj.insert("indexedVariables".to_string(), json!(indexed));
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
        None,
        None,
    )
}

fn evicted_variable() -> Value {
    variable_json(
        "<evicted>".to_string(),
        "<evicted>".to_string(),
        None,
        0,
        None,
        Some(json!({
            "kind": "virtual",
            "attributes": ["invalid"],
        })),
        None,
        None,
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
fn normalize_breakpoint_string(value: Option<String>) -> Option<String> {
    value.and_then(|v| {
        let trimmed = v.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FieldDataId {
    class_id: ReferenceTypeId,
    field_id: u64,
    object_id: Option<ObjectId>,
}

fn encode_field_data_id(
    class_id: ReferenceTypeId,
    field_id: u64,
    object_id: Option<ObjectId>,
) -> String {
    let object_id = object_id.unwrap_or(0);
    format!("nova:field:{class_id}:{field_id}:{object_id}")
}

fn decode_field_data_id(data_id: &str) -> Option<FieldDataId> {
    let mut parts = data_id.split(':');
    if parts.next()? != "nova" {
        return None;
    }
    if parts.next()? != "field" {
        return None;
    }
    let class_id: ReferenceTypeId = parts.next()?.parse().ok()?;
    let field_id: u64 = parts.next()?.parse().ok()?;
    let object_id_raw: ObjectId = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(FieldDataId {
        class_id,
        field_id,
        object_id: (object_id_raw != 0).then_some(object_id_raw),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HitConditionOp {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
}

fn hit_condition_matches(expr: &str, hit_count: u64) -> std::result::Result<bool, ()> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Ok(true);
    }

    // `%N` => break/log every N hits (1-indexed), i.e. N, 2N, 3N, ...
    if let Some(rest) = expr.strip_prefix('%') {
        let n: u64 = rest.trim().parse().map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        return Ok(hit_count % n == 0);
    }

    if expr.chars().all(|c| c.is_ascii_digit()) {
        let n: u64 = expr.parse().map_err(|_| ())?;
        return Ok(hit_count >= n);
    }

    let (op, rest) = if let Some(rest) = expr.strip_prefix("==") {
        (HitConditionOp::Eq, rest)
    } else if let Some(rest) = expr.strip_prefix(">=") {
        (HitConditionOp::Gte, rest)
    } else if let Some(rest) = expr.strip_prefix('>') {
        (HitConditionOp::Gt, rest)
    } else if let Some(rest) = expr.strip_prefix("<=") {
        (HitConditionOp::Lte, rest)
    } else if let Some(rest) = expr.strip_prefix('<') {
        (HitConditionOp::Lt, rest)
    } else {
        return Err(());
    };

    let n: u64 = rest.trim().parse().map_err(|_| ())?;
    Ok(match op {
        HitConditionOp::Eq => hit_count == n,
        HitConditionOp::Gt => hit_count > n,
        HitConditionOp::Gte => hit_count >= n,
        HitConditionOp::Lt => hit_count < n,
        HitConditionOp::Lte => hit_count <= n,
    })
}

fn condition_needs_locals(expr: &str) -> bool {
    let expr = expr.trim();
    if expr.is_empty() || expr == "true" || expr == "false" {
        return false;
    }
    if expr.parse::<i64>().is_ok() {
        return false;
    }
    // Identifiers or comparisons may require locals.
    true
}

fn log_message_needs_locals(template: &str) -> bool {
    template.contains('{')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmpOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

fn eval_breakpoint_condition(
    expr: &str,
    locals: Option<&HashMap<String, JdwpValue>>,
) -> std::result::Result<bool, ()> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Ok(true);
    }
    match expr {
        "true" => return Ok(true),
        "false" => return Ok(false),
        _ => {}
    }

    if let Ok(n) = expr.parse::<i64>() {
        return Ok(n != 0);
    }

    if let Some((lhs, op, rhs)) = parse_comparison(expr) {
        let Some(lhs) = eval_int_operand(lhs, locals) else {
            return Ok(false);
        };
        let Some(rhs) = eval_int_operand(rhs, locals) else {
            return Ok(false);
        };
        return Ok(match op {
            CmpOp::Eq => lhs == rhs,
            CmpOp::Ne => lhs != rhs,
            CmpOp::Gt => lhs > rhs,
            CmpOp::Gte => lhs >= rhs,
            CmpOp::Lt => lhs < rhs,
            CmpOp::Lte => lhs <= rhs,
        });
    }

    if is_identifier(expr) {
        let Some(locals) = locals else {
            return Err(());
        };
        let Some(value) = locals.get(expr) else {
            return Ok(false);
        };
        return Ok(jdwp_value_truthy(value));
    }

    Err(())
}

fn parse_comparison(expr: &str) -> Option<(&str, CmpOp, &str)> {
    // Try the longest operators first.
    for (tok, op) in [
        ("==", CmpOp::Eq),
        ("!=", CmpOp::Ne),
        (">=", CmpOp::Gte),
        ("<=", CmpOp::Lte),
        (">", CmpOp::Gt),
        ("<", CmpOp::Lt),
    ] {
        if let Some(idx) = expr.find(tok) {
            let (lhs, rhs_with_op) = expr.split_at(idx);
            let rhs = &rhs_with_op[tok.len()..];
            let lhs = lhs.trim();
            let rhs = rhs.trim();
            if lhs.is_empty() || rhs.is_empty() {
                return None;
            }
            return Some((lhs, op, rhs));
        }
    }
    None
}

fn eval_int_operand(op: &str, locals: Option<&HashMap<String, JdwpValue>>) -> Option<i64> {
    let op = op.trim();
    if let Ok(v) = op.parse::<i64>() {
        return Some(v);
    }
    if is_identifier(op) {
        let locals = locals?;
        let value = locals.get(op)?;
        return jdwp_value_as_i64(value);
    }
    None
}

fn jdwp_value_truthy(value: &JdwpValue) -> bool {
    match value {
        JdwpValue::Boolean(v) => *v,
        JdwpValue::Byte(v) => *v != 0,
        JdwpValue::Char(v) => *v != 0,
        JdwpValue::Short(v) => *v != 0,
        JdwpValue::Int(v) => *v != 0,
        JdwpValue::Long(v) => *v != 0,
        JdwpValue::Float(v) => *v != 0.0,
        JdwpValue::Double(v) => *v != 0.0,
        JdwpValue::Object { id, .. } => *id != 0,
        JdwpValue::Void => false,
    }
}

fn jdwp_value_as_i64(value: &JdwpValue) -> Option<i64> {
    match value {
        JdwpValue::Boolean(v) => Some(if *v { 1 } else { 0 }),
        JdwpValue::Byte(v) => Some((*v).into()),
        JdwpValue::Char(v) => Some((*v).into()),
        JdwpValue::Short(v) => Some((*v).into()),
        JdwpValue::Int(v) => Some((*v).into()),
        JdwpValue::Long(v) => Some(*v),
        _ => None,
    }
}

fn render_log_message(template: &str, locals: Option<&HashMap<String, JdwpValue>>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '{' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    out.push('{');
                    continue;
                }

                let mut expr = String::new();
                while let Some(next) = chars.next() {
                    if next == '}' {
                        break;
                    }
                    expr.push(next);
                }
                let expr = expr.trim();
                if is_identifier(expr) {
                    if let Some(locals) = locals {
                        if let Some(value) = locals.get(expr) {
                            out.push_str(&value.to_string());
                            continue;
                        }
                    }
                }

                out.push('{');
                out.push_str(expr);
                out.push('}');
            }
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                    out.push('}');
                } else {
                    out.push('}');
                }
            }
            other => out.push(other),
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use nova_jdwp::wire::mock::{DelayedReply, MockJdwpServer, MockJdwpServerConfig, THREAD_ID};

    #[test]
    fn handle_table_intern_is_stable_and_refreshes_value() {
        let mut table = HandleTable::<i32, i32>::default();
        let id1 = table.intern(42, 1);
        let id2 = table.intern(42, 2);
        assert_eq!(id1, id2);
        assert_eq!(table.get(id1), Some(&2));
    }

    #[test]
    fn handle_table_ids_do_not_grow_unbounded_across_clears() {
        let mut table = HandleTable::<i64, ()>::default();
        let mut max_id = 0;

        // Simulate repeatedly stopping and requesting (at most) 10 distinct handles per stop.
        // The ID space should be bounded by the maximum number of handles allocated in any
        // single stop, not by the number of stops.
        for stop in 0..1000 {
            for handle in 0..10 {
                let id = table.intern(stop * 100 + handle, ());
                max_id = max_id.max(id);
            }
            table.clear();
        }

        // With the current scheme of `(next << 1) | epoch`, allocating at most 10 distinct
        // handles per stop should never exceed `2 * 10 + 1 = 21`, regardless of how many
        // times we clear the table.
        assert!(
            max_id <= 21,
            "expected bounded handle ids; observed max_id={max_id}"
        );
    }

    #[test]
    fn handle_table_double_clear_does_not_reenable_previous_epoch_ids() {
        let mut table = HandleTable::<i32, ()>::default();
        let id_before_clear = table.intern(1, ());

        // First clear invalidates the previous epoch.
        table.clear();
        // Subsequent clears while the table is already empty should not flip the epoch again,
        // otherwise a later allocation could collide with ids from the immediately previous stop.
        table.clear();

        let id_after_clear = table.intern(1, ());
        assert_ne!(id_before_clear, id_after_clear);
    }

    #[test]
    fn hit_condition_digits_means_at_least() {
        assert!(!hit_condition_matches("3", 1).unwrap());
        assert!(!hit_condition_matches("3", 2).unwrap());
        assert!(hit_condition_matches("3", 3).unwrap());
        assert!(hit_condition_matches("3", 4).unwrap());
    }

    #[test]
    fn hit_condition_operators() {
        assert!(hit_condition_matches("== 2", 2).unwrap());
        assert!(!hit_condition_matches("==2", 3).unwrap());
        assert!(hit_condition_matches(">= 2", 2).unwrap());
        assert!(hit_condition_matches("> 2", 3).unwrap());
        assert!(hit_condition_matches("< 3", 2).unwrap());
        assert!(hit_condition_matches("<= 2", 2).unwrap());
    }

    #[test]
    fn hit_condition_modulo() {
        assert!(!hit_condition_matches("%2", 1).unwrap());
        assert!(hit_condition_matches("%2", 2).unwrap());
        assert!(!hit_condition_matches("%2", 3).unwrap());
        assert!(hit_condition_matches("%2", 4).unwrap());
    }

    #[test]
    fn condition_literals_and_identifier_truthiness() {
        let mut locals = HashMap::new();
        locals.insert("x".to_string(), JdwpValue::Int(42));
        locals.insert("y".to_string(), JdwpValue::Int(0));
        locals.insert("flag".to_string(), JdwpValue::Boolean(true));

        assert!(eval_breakpoint_condition("true", Some(&locals)).unwrap());
        assert!(!eval_breakpoint_condition("false", Some(&locals)).unwrap());
        assert!(eval_breakpoint_condition("1", Some(&locals)).unwrap());
        assert!(!eval_breakpoint_condition("0", Some(&locals)).unwrap());
        assert!(eval_breakpoint_condition("x", Some(&locals)).unwrap());
        assert!(!eval_breakpoint_condition("y", Some(&locals)).unwrap());
        assert!(eval_breakpoint_condition("flag", Some(&locals)).unwrap());
        assert!(!eval_breakpoint_condition("missing", Some(&locals)).unwrap());
    }

    #[test]
    fn condition_numeric_comparisons() {
        let mut locals = HashMap::new();
        locals.insert("x".to_string(), JdwpValue::Int(42));

        assert!(eval_breakpoint_condition("x == 42", Some(&locals)).unwrap());
        assert!(!eval_breakpoint_condition("x == 41", Some(&locals)).unwrap());
        assert!(eval_breakpoint_condition("x >= 42", Some(&locals)).unwrap());
        assert!(!eval_breakpoint_condition("x < 0", Some(&locals)).unwrap());
    }

    #[test]
    fn log_message_substitution_and_escapes() {
        let mut locals = HashMap::new();
        locals.insert("x".to_string(), JdwpValue::Int(42));
        locals.insert("flag".to_string(), JdwpValue::Boolean(true));

        assert_eq!(
            render_log_message("x is {x} and flag is {flag}", Some(&locals)),
            "x is 42 and flag is true"
        );
        assert_eq!(render_log_message("{{x}}", Some(&locals)), "{x}");
        assert_eq!(
            render_log_message("missing {y}", Some(&locals)),
            "missing {y}"
        );
    }

    #[tokio::test]
    async fn stream_debug_resolves_instance_field_sources() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let addr = server.addr();
        let mut dbg = Debugger::attach(AttachArgs {
            host: addr.ip(),
            port: addr.port(),
            source_roots: Vec::new(),
        })
        .await
        .unwrap();

        let cancel = CancellationToken::new();

        let (frames, _total) = dbg
            .stack_trace(&cancel, THREAD_ID as i64, None, None)
            .await
            .unwrap();
        let frame_id = frames
            .first()
            .and_then(|frame| frame.get("id"))
            .and_then(|id| id.as_i64())
            .expect("expected a stack frame id");

        // Create a new `int[]` with values [1,2,3] and attach it to the instance field `field`
        // on the frame's `this` object so stream-debug must resolve the source identifier as a
        // field (not a local variable).
        let frame = dbg.frame_handles.get(frame_id).copied().unwrap();
        let this_id = dbg
            .jdwp
            .stack_frame_this_object(frame.thread, frame.frame_id)
            .await
            .unwrap();

        let sample_array_id = server.sample_int_array_id();
        let (_tag, array_type_id) = dbg
            .jdwp
            .object_reference_reference_type(sample_array_id)
            .await
            .unwrap();
        let array_id = dbg
            .jdwp
            .array_type_new_instance(array_type_id, 3)
            .await
            .unwrap();
        dbg.jdwp
            .array_reference_set_values(
                array_id,
                0,
                &[JdwpValue::Int(1), JdwpValue::Int(2), JdwpValue::Int(3)],
            )
            .await
            .unwrap();

        let (_tag, this_type_id) = dbg
            .jdwp
            .object_reference_reference_type(this_id)
            .await
            .unwrap();
        let fields = dbg.jdwp.reference_type_fields(this_type_id).await.unwrap();
        let field_id = fields
            .iter()
            .find(|f| f.name == "field")
            .map(|f| f.field_id)
            .expect("expected mock this object to have a `field` instance field");

        dbg.jdwp
            .object_reference_set_values(
                this_id,
                &[(
                    field_id,
                    JdwpValue::Object {
                        tag: b'[',
                        id: array_id,
                    },
                )],
            )
            .await
            .unwrap();

        let cfg = nova_stream_debug::StreamDebugConfig {
            max_sample_size: 3,
            max_total_time: Duration::from_secs(1),
            allow_side_effects: false,
            allow_terminal_ops: true,
        };

        let expr = "field.stream().filter(x -> x > 1).map(x -> x * 2).count()";
        let fut = dbg
            .stream_debug(cancel, frame_id, expr.to_string(), cfg)
            .unwrap();
        let body = fut.await.unwrap();

        // The mock JDWP server does not execute injected helper bytecode, so we can't assert real
        // stream semantics here. Instead, verify that the stream debugger resolved `field` as an
        // instance-field binding (rather than a local variable) by checking that the helper
        // invocation arguments include the array object we attached to `this.field`.
        let invoke_calls = server.class_type_invoke_method_calls().await;
        assert!(
            invoke_calls.iter().any(|call| call.args.iter().any(|arg| match arg {
                JdwpValue::Object { id, .. } => *id == array_id,
                _ => false,
            })),
            "expected injected helper invocations to include instance field object id 0x{array_id:x}; calls={invoke_calls:?}"
        );

        // Still assert the request completes successfully so the full compile+inject pipeline is
        // exercised under mock JDWP.
        assert_eq!(
            body.runtime.source_sample.elements,
            vec!["10".to_string(), "20".to_string(), "30".to_string()]
        );
    }

    #[tokio::test]
    async fn stream_debug_enforces_max_total_time() {
        let mut server_config = MockJdwpServerConfig::default();
        // Delay a JDWP call that stream-debug relies on (`ArrayReference.GetValues`) so the
        // evaluation exceeds the configured `max_total_time`.
        server_config.delayed_replies = vec![DelayedReply {
            command_set: 13, // ArrayReference
            command: 2,      // GetValues
            delay: Duration::from_millis(200),
        }];

        let server = MockJdwpServer::spawn_with_config(server_config)
            .await
            .unwrap();
        let addr = server.addr();
        let mut dbg = Debugger::attach(AttachArgs {
            host: addr.ip(),
            port: addr.port(),
            source_roots: Vec::new(),
        })
        .await
        .unwrap();

        let setup_cancel = CancellationToken::new();
        let (frames, _total) = dbg
            .stack_trace(&setup_cancel, THREAD_ID as i64, None, None)
            .await
            .unwrap();
        let frame_id = frames
            .first()
            .and_then(|frame| frame.get("id"))
            .and_then(|id| id.as_i64())
            .expect("expected a stack frame id");

        // Ensure the source expression resolves to an array so `Inspector::preview_object` hits
        // `ArrayReference.GetValues` (which is delayed above).
        let frame = dbg.frame_handles.get(frame_id).copied().unwrap();
        let this_id = dbg
            .jdwp
            .stack_frame_this_object(frame.thread, frame.frame_id)
            .await
            .unwrap();
        let (_tag, this_type_id) = dbg
            .jdwp
            .object_reference_reference_type(this_id)
            .await
            .unwrap();
        let fields = dbg.jdwp.reference_type_fields(this_type_id).await.unwrap();
        let field_id = fields
            .iter()
            .find(|f| f.name == "field")
            .map(|f| f.field_id)
            .expect("expected mock this object to have a `field` instance field");

        let array_id = server.sample_int_array_id();
        dbg.jdwp
            .object_reference_set_values(
                this_id,
                &[(
                    field_id,
                    JdwpValue::Object {
                        tag: b'[',
                        id: array_id,
                    },
                )],
            )
            .await
            .unwrap();

        let cfg = nova_stream_debug::StreamDebugConfig {
            max_sample_size: 3,
            max_total_time: Duration::from_millis(10),
            allow_side_effects: false,
            allow_terminal_ops: true,
        };

        let fut = dbg
            .stream_debug(
                CancellationToken::new(),
                frame_id,
                "this.field.stream().count()".to_string(),
                cfg,
            )
            .unwrap();
        let err = fut.await.unwrap_err();
        assert_eq!(err.to_string(), "evaluation exceeded time limit");
    }
}

pub(crate) fn is_retryable_attach_error(err: &DebuggerError) -> bool {
    match err {
        DebuggerError::Timeout => false,
        DebuggerError::InvalidRequest(_) => false,
        DebuggerError::Jdwp(JdwpError::Io(io_err)) => matches!(
            io_err.kind(),
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::AddrNotAvailable
        ),
        DebuggerError::Jdwp(JdwpError::ConnectionClosed | JdwpError::Timeout) => true,
        DebuggerError::Jdwp(
            JdwpError::Cancelled | JdwpError::Protocol(_) | JdwpError::VmError(_),
        ) => false,
    }
}

use std::{
    collections::{BTreeSet, HashMap},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicI32, AtomicU16, AtomicU32, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

use super::{
    codec::{encode_command, encode_reply, JdwpReader, JdwpWriter, HANDSHAKE, HEADER_LEN},
    types::{
        FieldId, JdwpIdSizes, JdwpValue, Location, ObjectId, ReferenceTypeId, ThreadId,
        EVENT_KIND_CLASS_UNLOAD, EVENT_KIND_FIELD_ACCESS, EVENT_KIND_FIELD_MODIFICATION,
        EVENT_KIND_VM_DISCONNECT, EVENT_MODIFIER_KIND_CLASS_EXCLUDE, EVENT_MODIFIER_KIND_CLASS_MATCH,
        EVENT_MODIFIER_KIND_CLASS_ONLY, EVENT_MODIFIER_KIND_COUNT, EVENT_MODIFIER_KIND_EXCEPTION_ONLY,
        EVENT_MODIFIER_KIND_FIELD_ONLY, EVENT_MODIFIER_KIND_INSTANCE_ONLY,
        EVENT_MODIFIER_KIND_LOCATION_ONLY, EVENT_MODIFIER_KIND_SOURCE_NAME_MATCH,
        EVENT_MODIFIER_KIND_STEP, EVENT_MODIFIER_KIND_THREAD_ONLY,
    },
};

/// A tiny JDWP server used for unit/integration testing.
///
/// It intentionally supports a *small* subset of JDWP sufficient to exercise
/// nova-jdwp and nova-dap without requiring a JDK to be installed on the system.
pub struct MockJdwpServer {
    addr: SocketAddr,
    shutdown: CancellationToken,
    state: Arc<State>,
}

#[derive(Clone, Debug)]
pub struct MockJdwpServerConfig {
    /// Reply delays keyed by `(command_set, command)`.
    ///
    /// The server will still accept and respond to other commands while a delayed reply
    /// is pending.
    pub delayed_replies: Vec<DelayedReply>,
    /// Raw `VirtualMachine.CapabilitiesNew` booleans returned by the mock VM.
    ///
    /// The order must match the JDWP spec. Most HotSpot VMs return 32 booleans, so the
    /// default mirrors that for realism.
    pub capabilities: Vec<bool>,
    /// If set, the mock VM replies to `VirtualMachine.CapabilitiesNew (1/17)` with
    /// `NOT_IMPLEMENTED` (JDWP error 99).
    pub capabilities_new_not_implemented: bool,
    /// If set, the mock VM replies to `VirtualMachine.Capabilities (1/12)` with
    /// `NOT_IMPLEMENTED` (JDWP error 99).
    pub capabilities_legacy_not_implemented: bool,
    /// JDWP identifier sizes returned by `VirtualMachine.IDSizes`.
    ///
    /// Most modern JVMs use 8-byte ids; keeping this configurable lets tests
    /// exercise non-default sizes.
    pub id_sizes: JdwpIdSizes,
    /// JDWP reference type signature returned by `VirtualMachine.AllClasses` and
    /// `ReferenceType.Signature`.
    ///
    /// Example: `Lcom/example/Main;`
    pub class_signature: String,
    /// Java source file name returned by `ReferenceType.SourceFile`.
    ///
    /// Example: `Main.java`
    pub source_file: String,
    /// Maximum number of breakpoint events to emit after a `VirtualMachine.Resume`.
    ///
    /// This bounds the mock's automatic stop-event behavior so DAP tests that
    /// auto-resume ignored breakpoint hits (e.g. logpoints/conditions) don't
    /// end up in an infinite resume/stop loop.
    pub breakpoint_events: usize,
    /// Maximum number of single-step events to emit after a `VirtualMachine.Resume`.
    pub step_events: usize,
    /// When enabled, the mock will emit a composite event packet containing an
    /// `Exception`, `Breakpoint`, and `MethodExitWithReturnValue` (in that order, with the
    /// method-exit last) after a `VirtualMachine.Resume`, provided all three event requests
    /// are configured.
    ///
    /// This is useful for testing stop-event ordering semantics in the client without
    /// introducing unbounded resume/stop loops.
    pub emit_exception_breakpoint_method_exit_composite: bool,
    /// Maximum number of field-access watchpoint events to emit after a resume.
    pub field_access_events: usize,
    /// Maximum number of field-modification watchpoint events to emit after a resume.
    pub field_modification_events: usize,
    /// Maximum number of `ClassUnload` events to emit after a resume.
    pub class_unload_events: usize,
    /// Maximum number of `VmDisconnect` events to emit after a resume.
    ///
    /// When a disconnect event is emitted, the mock closes the underlying socket to
    /// simulate a debuggee terminating unexpectedly.
    pub vm_disconnect_events: usize,
}

impl Default for MockJdwpServerConfig {
    fn default() -> Self {
        Self {
            delayed_replies: Vec::new(),
            capabilities: vec![false; 32],
            capabilities_new_not_implemented: false,
            capabilities_legacy_not_implemented: false,
            id_sizes: JdwpIdSizes::default(),
            class_signature: "LMain;".to_string(),
            source_file: "Main.java".to_string(),
            // Preserve historical behavior: keep emitting stop events after every resume
            // unless tests opt into a finite budget via `spawn_with_config`.
            breakpoint_events: usize::MAX,
            step_events: usize::MAX,
            emit_exception_breakpoint_method_exit_composite: false,
            field_access_events: 0,
            field_modification_events: 0,
            class_unload_events: 0,
            vm_disconnect_events: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DelayedReply {
    pub command_set: u8,
    pub command: u8,
    pub delay: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockEventRequest {
    pub event_kind: u8,
    pub suspend_policy: u8,
    pub request_id: i32,
    pub modifiers: Vec<MockEventRequestModifier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockEventRequestModifier {
    Count {
        count: u32,
    },
    ThreadOnly {
        thread: ThreadId,
    },
    ClassOnly {
        class_id: ReferenceTypeId,
    },
    ClassMatch {
        pattern: String,
    },
    ClassExclude {
        pattern: String,
    },
    LocationOnly {
        location: Location,
    },
    ExceptionOnly {
        exception_or_null: ReferenceTypeId,
        caught: bool,
        uncaught: bool,
    },
    FieldOnly {
        class_id: ReferenceTypeId,
        field_id: FieldId,
    },
    Step {
        thread: ThreadId,
        size: u32,
        depth: u32,
    },
    InstanceOnly {
        object_id: ObjectId,
    },
    SourceNameMatch {
        pattern: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MockSimpleEventRequest {
    request_id: i32,
    suspend_policy: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MockWatchpointRequest {
    request_id: i32,
    suspend_policy: u8,
    field_only: Option<(ReferenceTypeId, FieldId)>,
    instance_only: Option<ObjectId>,
}

impl MockJdwpServer {
    pub async fn spawn() -> std::io::Result<Self> {
        Self::spawn_with_config(Default::default()).await
    }

    pub async fn spawn_with_capabilities(capabilities: Vec<bool>) -> std::io::Result<Self> {
        let mut config = MockJdwpServerConfig::default();
        config.capabilities = capabilities;
        Self::spawn_with_config(config).await
    }

    pub async fn spawn_with_config(config: MockJdwpServerConfig) -> std::io::Result<Self> {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let shutdown = CancellationToken::new();

        let state = Arc::new(State::new(config));
        let task_shutdown = shutdown.clone();
        let task_state = state.clone();

        tokio::spawn(async move {
            let _ = run(listener, task_state, task_shutdown).await;
        });

        Ok(Self {
            addr,
            shutdown,
            state,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn set_redefine_classes_error_code(&self, code: u16) {
        self.state
            .redefine_classes_error_code
            .store(code, Ordering::Relaxed);
    }

    pub async fn redefine_classes_calls(&self) -> Vec<RedefineClassesCall> {
        self.state.redefine_classes_calls.lock().await.clone()
    }

    pub async fn pinned_object_ids(&self) -> BTreeSet<ObjectId> {
        self.state.pinned_object_ids.lock().await.clone()
    }

    pub async fn exception_request(&self) -> Option<MockExceptionRequest> {
        *self.state.exception_request.lock().await
    }

    pub async fn breakpoint_suspend_policy(&self) -> Option<u8> {
        *self.state.breakpoint_suspend_policy.lock().await
    }

    pub async fn breakpoint_count_modifier(&self) -> Option<u32> {
        *self.state.breakpoint_count_modifier.lock().await
    }

    pub async fn event_requests(&self) -> Vec<MockEventRequest> {
        self.state.event_requests.lock().await.clone()
    }

    pub async fn step_suspend_policy(&self) -> Option<u8> {
        *self.state.step_suspend_policy.lock().await
    }

    pub fn vm_suspend_calls(&self) -> u32 {
        self.state.vm_suspend_calls.load(Ordering::Relaxed)
    }

    pub fn vm_resume_calls(&self) -> u32 {
        self.state.vm_resume_calls.load(Ordering::Relaxed)
    }

    pub fn thread_suspend_calls(&self) -> u32 {
        self.state.thread_suspend_calls.load(Ordering::Relaxed)
    }

    pub fn thread_resume_calls(&self) -> u32 {
        self.state.thread_resume_calls.load(Ordering::Relaxed)
    }

    /// Returns the mock `java.lang.String` object id that maps to the literal `"mock string"`.
    ///
    /// This is useful for wire-level tests that want to exercise `StringReference.Value`
    /// without depending on other mock replies (like `StackFrame.GetValues`).
    pub fn string_object_id(&self) -> u64 {
        STRING_OBJECT_ID
    }

    pub fn sample_string_id(&self) -> u64 {
        SAMPLE_STRING_OBJECT_ID
    }

    pub fn sample_int_array_id(&self) -> u64 {
        SAMPLE_INT_ARRAY_OBJECT_ID
    }

    pub fn sample_hashmap_id(&self) -> u64 {
        SAMPLE_HASHMAP_OBJECT_ID
    }

    pub fn sample_hashset_id(&self) -> u64 {
        SAMPLE_HASHSET_OBJECT_ID
    }
}

impl Drop for MockJdwpServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

struct State {
    config: MockJdwpServerConfig,
    next_request_id: AtomicI32,
    next_packet_id: AtomicU32,
    hashmap_bucket_calls: AtomicU32,
    vm_suspend_calls: AtomicU32,
    vm_resume_calls: AtomicU32,
    thread_suspend_calls: AtomicU32,
    thread_resume_calls: AtomicU32,
    breakpoint_request: tokio::sync::Mutex<Option<i32>>,
    breakpoint_count_modifier: tokio::sync::Mutex<Option<u32>>,
    step_request: tokio::sync::Mutex<Option<i32>>,
    method_exit_request: tokio::sync::Mutex<Option<i32>>,
    thread_start_request: tokio::sync::Mutex<Option<i32>>,
    thread_death_request: tokio::sync::Mutex<Option<i32>>,
    class_unload_request: tokio::sync::Mutex<Option<MockSimpleEventRequest>>,
    field_access_request: tokio::sync::Mutex<Option<MockWatchpointRequest>>,
    field_modification_request: tokio::sync::Mutex<Option<MockWatchpointRequest>>,
    vm_disconnect_request: tokio::sync::Mutex<Option<MockSimpleEventRequest>>,
    threads: tokio::sync::Mutex<Vec<u64>>,
    exception_request: tokio::sync::Mutex<Option<MockExceptionRequest>>,
    breakpoint_suspend_policy: tokio::sync::Mutex<Option<u8>>,
    step_suspend_policy: tokio::sync::Mutex<Option<u8>>,
    event_requests: tokio::sync::Mutex<Vec<MockEventRequest>>,
    redefine_classes_error_code: AtomicU16,
    redefine_classes_calls: tokio::sync::Mutex<Vec<RedefineClassesCall>>,
    pinned_object_ids: tokio::sync::Mutex<BTreeSet<ObjectId>>,
    last_classes_by_signature: tokio::sync::Mutex<Option<String>>,
    delayed_replies: HashMap<(u8, u8), Duration>,
    capabilities: Vec<bool>,
    breakpoint_events_remaining: AtomicUsize,
    step_events_remaining: AtomicUsize,
    field_access_events_remaining: AtomicUsize,
    field_modification_events_remaining: AtomicUsize,
    class_unload_events_remaining: AtomicUsize,
    vm_disconnect_events_remaining: AtomicUsize,
}

impl Default for State {
    fn default() -> Self {
        Self::new(MockJdwpServerConfig::default())
    }
}

impl State {
    fn new(config: MockJdwpServerConfig) -> Self {
        let breakpoint_events = config.breakpoint_events;
        let step_events = config.step_events;
        let field_access_events = config.field_access_events;
        let field_modification_events = config.field_modification_events;
        let class_unload_events = config.class_unload_events;
        let vm_disconnect_events = config.vm_disconnect_events;

        let mut delayed_replies = HashMap::new();
        for entry in &config.delayed_replies {
            delayed_replies.insert((entry.command_set, entry.command), entry.delay);
        }

        let capabilities = config.capabilities.clone();

        Self {
            config,
            next_request_id: AtomicI32::new(0),
            next_packet_id: AtomicU32::new(0),
            hashmap_bucket_calls: AtomicU32::new(0),
            vm_suspend_calls: AtomicU32::new(0),
            vm_resume_calls: AtomicU32::new(0),
            thread_suspend_calls: AtomicU32::new(0),
            thread_resume_calls: AtomicU32::new(0),
            breakpoint_request: tokio::sync::Mutex::new(None),
            breakpoint_count_modifier: tokio::sync::Mutex::new(None),
            step_request: tokio::sync::Mutex::new(None),
            method_exit_request: tokio::sync::Mutex::new(None),
            thread_start_request: tokio::sync::Mutex::new(None),
            thread_death_request: tokio::sync::Mutex::new(None),
            class_unload_request: tokio::sync::Mutex::new(None),
            field_access_request: tokio::sync::Mutex::new(None),
            field_modification_request: tokio::sync::Mutex::new(None),
            vm_disconnect_request: tokio::sync::Mutex::new(None),
            threads: tokio::sync::Mutex::new(vec![THREAD_ID]),
            exception_request: tokio::sync::Mutex::new(None),
            breakpoint_suspend_policy: tokio::sync::Mutex::new(None),
            step_suspend_policy: tokio::sync::Mutex::new(None),
            event_requests: tokio::sync::Mutex::new(Vec::new()),
            redefine_classes_error_code: AtomicU16::new(0),
            redefine_classes_calls: tokio::sync::Mutex::new(Vec::new()),
            pinned_object_ids: tokio::sync::Mutex::new(BTreeSet::new()),
            last_classes_by_signature: tokio::sync::Mutex::new(None),
            delayed_replies,
            capabilities,
            breakpoint_events_remaining: AtomicUsize::new(breakpoint_events),
            step_events_remaining: AtomicUsize::new(step_events),
            field_access_events_remaining: AtomicUsize::new(field_access_events),
            field_modification_events_remaining: AtomicUsize::new(field_modification_events),
            class_unload_events_remaining: AtomicUsize::new(class_unload_events),
            vm_disconnect_events_remaining: AtomicUsize::new(vm_disconnect_events),
        }
    }

    fn alloc_request_id(&self) -> i32 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn alloc_packet_id(&self) -> u32 {
        self.next_packet_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn reply_delay(&self, command_set: u8, command: u8) -> Option<Duration> {
        self.delayed_replies.get(&(command_set, command)).copied()
    }

    fn take_breakpoint_event(&self) -> bool {
        self.breakpoint_events_remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }

    fn take_step_event(&self) -> bool {
        self.step_events_remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }

    fn take_field_access_event(&self) -> bool {
        self.field_access_events_remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }

    fn take_field_modification_event(&self) -> bool {
        self.field_modification_events_remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }

    fn take_class_unload_event(&self) -> bool {
        self.class_unload_events_remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }

    fn take_vm_disconnect_event(&self) -> bool {
        self.vm_disconnect_events_remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedefineClassesCall {
    pub class_count: u32,
    pub classes: Vec<(ReferenceTypeId, Vec<u8>)>,
}

// Use a thread object id with the high bit set so DAP implementations that
// (correctly) bit-cast `u64 <-> i64` are exercised by integration tests.
//
// JDWP error codes (subset).
const ERROR_THREAD_NOT_SUSPENDED: u16 = 13;
const ERROR_NOT_IMPLEMENTED: u16 = 99;
const THREAD_ID: u64 = 0x8000_0000_0000_1001;
const WORKER_THREAD_ID: u64 = 0x1002;
const FRAME_ID: u64 = 0x2001;
const CLASS_ID: u64 = 0x3001;
const FOO_CLASS_ID: u64 = 0x3002;
const METHOD_ID: u64 = 0x4001;
const OBJECT_ID: u64 = 0x5001;
const EXCEPTION_ID: u64 = 0x5002;
const STRING_OBJECT_ID: u64 = 0x5003;
const ARRAY_OBJECT_ID: u64 = 0x5004;
const OBJECT_CLASS_ID: u64 = 0x6001;
const STRING_CLASS_ID: u64 = 0x6002;
const ARRAY_CLASS_ID: u64 = 0x6003;
const EXCEPTION_CLASS_ID: u64 = 0x6004;
const THROWABLE_CLASS_ID: u64 = 0x6005;
const FIELD_ID: u64 = 0x7001;
const DETAIL_MESSAGE_FIELD_ID: u64 = 0x7002;

// Sample objects used by `nova-dap`'s wire formatter tests.
const SAMPLE_STRING_OBJECT_ID: u64 = 0x5101;
const SAMPLE_INT_ARRAY_OBJECT_ID: u64 = 0x5102;
const SAMPLE_HASHMAP_OBJECT_ID: u64 = 0x5103;
const SAMPLE_HASHSET_OBJECT_ID: u64 = 0x5104;

const HASHMAP_TABLE_ARRAY_OBJECT_ID: u64 = 0x5105;
const HASHMAP_NODE_A_OBJECT_ID: u64 = 0x5106;
const HASHMAP_NODE_B_OBJECT_ID: u64 = 0x5107;

const HASHMAP_KEY_A_OBJECT_ID: u64 = 0x5108;
const HASHMAP_KEY_B_OBJECT_ID: u64 = 0x5109;

const HASHMAP_CLASS_ID: u64 = 0x6010;
const HASHSET_CLASS_ID: u64 = 0x6011;
const HASHMAP_NODE_CLASS_ID: u64 = 0x6012;
const HASHMAP_TABLE_ARRAY_CLASS_ID: u64 = 0x6013;

const HASHMAP_FIELD_SIZE_ID: u64 = 0x7010;
const HASHMAP_FIELD_TABLE_ID: u64 = 0x7011;
const HASHSET_FIELD_MAP_ID: u64 = 0x7012;

const HASHMAP_NODE_FIELD_KEY_ID: u64 = 0x7013;
const HASHMAP_NODE_FIELD_VALUE_ID: u64 = 0x7014;
const HASHMAP_NODE_FIELD_NEXT_ID: u64 = 0x7015;

// Monitor objects used by thread/lock introspection commands.
const OWNED_MONITOR_A_OBJECT_ID: u64 = 0x5201;
const OWNED_MONITOR_B_OBJECT_ID: u64 = 0x5202;
const CONTENDED_MONITOR_OBJECT_ID: u64 = 0x5203;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MockExceptionRequest {
    pub request_id: i32,
    pub caught: bool,
    pub uncaught: bool,
}

const CLASS_LOADER_ID: u64 = 0x8001;
const DEFINED_CLASS_ID: u64 = 0x9001;

fn default_location() -> Location {
    Location {
        type_tag: 1,
        class_id: CLASS_ID,
        method_id: METHOD_ID,
        index: 0,
    }
}

async fn run(
    listener: TcpListener,
    state: Arc<State>,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    tokio::select! {
        _ = shutdown.cancelled() => return Ok(()),
        accept = listener.accept() => {
            let (mut socket, _) = accept?;

            // Handshake: debugger -> "JDWP-Handshake", server echoes back.
            let mut hs = [0u8; HANDSHAKE.len()];
            socket.read_exact(&mut hs).await?;
            if hs != *HANDSHAKE {
                return Ok(());
            }
            socket.write_all(HANDSHAKE).await?;

            let id_sizes = state.config.id_sizes;
            let (mut reader, writer) = socket.into_split();
            let writer = Arc::new(tokio::sync::Mutex::new(writer));

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return Ok(()),
                    res = read_packet(&mut reader) => {
                        let Some(packet) = res? else {
                            return Ok(());
                        };
                        handle_packet(&writer, &state, &id_sizes, packet, shutdown.clone()).await?;
                    }
                }
            }
        }
    }
}

struct Packet {
    id: u32,
    command_set: u8,
    command: u8,
    payload: Vec<u8>,
}

async fn read_packet(
    socket: &mut tokio::net::tcp::OwnedReadHalf,
) -> std::io::Result<Option<Packet>> {
    let mut header = [0u8; HEADER_LEN];
    match socket.read_exact(&mut header).await {
        Ok(_n) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }

    let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if length < HEADER_LEN {
        return Ok(None);
    }
    let id = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let flags = header[8];
    if flags != 0 {
        // The mock only expects commands from the debugger.
        return Ok(None);
    }
    let command_set = header[9];
    let command = header[10];
    let mut payload = vec![0u8; length - HEADER_LEN];
    socket.read_exact(&mut payload).await?;
    Ok(Some(Packet {
        id,
        command_set,
        command,
        payload,
    }))
}

async fn handle_packet(
    writer: &Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    state: &State,
    id_sizes: &JdwpIdSizes,
    packet: Packet,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    let sizes = id_sizes;
    let mut r = JdwpReader::new(&packet.payload);
    let cap = |idx: usize| state.capabilities.get(idx).copied().unwrap_or(false);

    let (reply_error_code, reply_payload) = match (packet.command_set, packet.command) {
        // VirtualMachine.IDSizes
        (1, 7) => {
            let mut w = JdwpWriter::new();
            w.write_u32(sizes.field_id as u32);
            w.write_u32(sizes.method_id as u32);
            w.write_u32(sizes.object_id as u32);
            w.write_u32(sizes.reference_type_id as u32);
            w.write_u32(sizes.frame_id as u32);
            (0, w.into_vec())
        }
        // VirtualMachine.Capabilities (legacy)
        (1, 12) => {
            if state.config.capabilities_legacy_not_implemented {
                (ERROR_NOT_IMPLEMENTED, Vec::new())
            } else {
                let mut w = JdwpWriter::new();
                // Legacy Capabilities reply contains 7 booleans.
                for idx in 0..7 {
                    w.write_bool(state.capabilities.get(idx).copied().unwrap_or(false));
                }
                (0, w.into_vec())
            }
        }
        // VirtualMachine.CapabilitiesNew
        (1, 17) => {
            if state.config.capabilities_new_not_implemented {
                (ERROR_NOT_IMPLEMENTED, Vec::new())
            } else {
                let mut w = JdwpWriter::new();
                for cap in &state.capabilities {
                    w.write_bool(*cap);
                }
                (0, w.into_vec())
            }
        }
        // VirtualMachine.AllThreads
        (1, 4) => {
            let threads = state.threads.lock().await;
            let mut w = JdwpWriter::new();
            w.write_u32(threads.len() as u32);
            for thread in threads.iter().copied() {
                w.write_object_id(thread, sizes);
            }
            (0, w.into_vec())
        }
        // VirtualMachine.ClassesBySignature
        (1, 2) => {
            let signature = r.read_string().unwrap_or_default();
            *state.last_classes_by_signature.lock().await = Some(signature.clone());

            let mut w = JdwpWriter::new();
            match signature.as_str() {
                sig if sig == state.config.class_signature => {
                    w.write_u32(1);
                    w.write_u8(1); // class
                    w.write_reference_type_id(CLASS_ID, sizes);
                    w.write_u32(1);
                }
                "Lcom/example/Foo;" => {
                    w.write_u32(1);
                    w.write_u8(1); // class
                    w.write_reference_type_id(FOO_CLASS_ID, sizes);
                    w.write_u32(1);
                }
                "Ljava/lang/Throwable;" => {
                    w.write_u32(1);
                    w.write_u8(1); // class
                    w.write_reference_type_id(THROWABLE_CLASS_ID, sizes);
                    w.write_u32(1);
                }
                _ => {
                    w.write_u32(0);
                }
            }
            (0, w.into_vec())
        }
        // VirtualMachine.AllClasses
        (1, 3) => {
            let mut w = JdwpWriter::new();
            w.write_u32(1);
            w.write_u8(1); // class
            w.write_reference_type_id(CLASS_ID, sizes);
            w.write_string(&state.config.class_signature);
            w.write_u32(1);
            (0, w.into_vec())
        }
        // VirtualMachine.RedefineClasses
        (1, 18) => {
            let class_count = r.read_u32().unwrap_or(0);
            let mut classes = Vec::with_capacity(class_count as usize);
            for _ in 0..class_count {
                let type_id = r.read_reference_type_id(sizes).unwrap_or(0);
                let len = r.read_u32().unwrap_or(0) as usize;
                let bytes = r.read_bytes(len).unwrap_or(&[]).to_vec();
                classes.push((type_id, bytes));
            }

            state
                .redefine_classes_calls
                .lock()
                .await
                .push(RedefineClassesCall {
                    class_count,
                    classes,
                });

            let err = state.redefine_classes_error_code.load(Ordering::Relaxed);
            (err, Vec::new())
        }
        // VirtualMachine.Suspend
        (1, 8) => {
            state.vm_suspend_calls.fetch_add(1, Ordering::Relaxed);
            (0, Vec::new())
        }
        // VirtualMachine.Resume
        (1, 9) => {
            state.vm_resume_calls.fetch_add(1, Ordering::Relaxed);
            (0, Vec::new())
        }
        // VirtualMachine.ClassPaths
        (1, 13) => {
            let mut w = JdwpWriter::new();
            w.write_string("/mock");
            w.write_u32(1);
            w.write_string("/mock/classes");
            w.write_u32(1);
            w.write_string("/mock/boot");
            (0, w.into_vec())
        }
        // ThreadReference.Name
        (11, 1) => {
            let _thread_id = r.read_object_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_string("main");
            (0, w.into_vec())
        }
        // ThreadReference.Suspend
        (11, 2) => {
            let _thread_id = r.read_object_id(sizes).unwrap_or(0);
            state.thread_suspend_calls.fetch_add(1, Ordering::Relaxed);
            (0, Vec::new())
        }
        // ThreadReference.Resume
        (11, 3) => {
            let _thread_id = r.read_object_id(sizes).unwrap_or(0);
            state.thread_resume_calls.fetch_add(1, Ordering::Relaxed);
            (0, Vec::new())
        }
        // ThreadReference.Status
        (11, 4) => {
            let _thread_id = r.read_object_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            // `ThreadStatus.MONITOR` + `SuspendStatus.SUSPENDED`.
            w.write_u32(3);
            w.write_u32(1);
            (0, w.into_vec())
        }
        // ThreadReference.Frames
        (11, 6) => {
            if *state.breakpoint_suspend_policy.lock().await == Some(0) {
                // JDWP `Error.THREAD_NOT_SUSPENDED` (the thread is running, so stack frames are unavailable).
                (ERROR_THREAD_NOT_SUSPENDED, Vec::new())
            } else {
                let _thread_id = r.read_object_id(sizes).unwrap_or(0);
                let _start = r.read_i32().unwrap_or(0);
                let _length = r.read_i32().unwrap_or(0);
                let mut w = JdwpWriter::new();
                w.write_u32(1);
                w.write_id(FRAME_ID, sizes.frame_id);
                w.write_location(&default_location(), sizes);
                (0, w.into_vec())
            }
        }
        // ThreadReference.FrameCount
        (11, 7) => {
            let _thread_id = r.read_object_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_u32(1);
            (0, w.into_vec())
        }
        // ThreadReference.OwnedMonitors
        (11, 8) => {
            if !cap(4) {
                (ERROR_NOT_IMPLEMENTED, Vec::new())
            } else {
                let _thread_id = r.read_object_id(sizes).unwrap_or(0);
                let mut w = JdwpWriter::new();
                w.write_u32(2);
                w.write_object_id(OWNED_MONITOR_A_OBJECT_ID, sizes);
                w.write_object_id(OWNED_MONITOR_B_OBJECT_ID, sizes);
                (0, w.into_vec())
            }
        }
        // ThreadReference.CurrentContendedMonitor
        (11, 9) => {
            if !cap(5) {
                (ERROR_NOT_IMPLEMENTED, Vec::new())
            } else {
                let _thread_id = r.read_object_id(sizes).unwrap_or(0);
                let mut w = JdwpWriter::new();
                w.write_object_id(CONTENDED_MONITOR_OBJECT_ID, sizes);
                (0, w.into_vec())
            }
        }
        // ThreadReference.SuspendCount
        (11, 12) => {
            let _thread_id = r.read_object_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_u32(2);
            (0, w.into_vec())
        }
        // ThreadReference.OwnedMonitorsStackDepthInfo
        (11, 13) => {
            if !cap(21) {
                (ERROR_NOT_IMPLEMENTED, Vec::new())
            } else {
                let _thread_id = r.read_object_id(sizes).unwrap_or(0);
                let mut w = JdwpWriter::new();
                w.write_u32(2);
                w.write_object_id(OWNED_MONITOR_A_OBJECT_ID, sizes);
                w.write_i32(0);
                w.write_object_id(OWNED_MONITOR_B_OBJECT_ID, sizes);
                w.write_i32(2);
                (0, w.into_vec())
            }
        }
        // ReferenceType.SourceFile
        (2, 7) => {
            let _class_id = r.read_reference_type_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_string(&state.config.source_file);
            (0, w.into_vec())
        }
        // ReferenceType.Signature
        (2, 1) => {
            let class_id = r.read_reference_type_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            let sig = match class_id {
                CLASS_ID => state.config.class_signature.as_str(),
                FOO_CLASS_ID => "Lcom/example/Foo;",
                OBJECT_CLASS_ID => "LObject;",
                STRING_CLASS_ID => "Ljava/lang/String;",
                ARRAY_CLASS_ID => "[I",
                EXCEPTION_CLASS_ID => "Ljava/lang/RuntimeException;",
                THROWABLE_CLASS_ID => "Ljava/lang/Throwable;",
                HASHMAP_CLASS_ID => "Ljava/util/HashMap;",
                HASHSET_CLASS_ID => "Ljava/util/HashSet;",
                HASHMAP_NODE_CLASS_ID => "Ljava/util/HashMap$Node;",
                HASHMAP_TABLE_ARRAY_CLASS_ID => "[Ljava/util/HashMap$Node;",
                _ => "LObject;",
            };
            w.write_string(sig);
            (0, w.into_vec())
        }
        // ReferenceType.ClassLoader
        (2, 2) => {
            let _class_id = r.read_reference_type_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_object_id(CLASS_LOADER_ID, sizes);
            (0, w.into_vec())
        }
        // ReferenceType.Methods
        (2, 5) => {
            let _class_id = r.read_reference_type_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_u32(1);
            w.write_id(METHOD_ID, sizes.method_id);
            w.write_string("main");
            w.write_string("()V");
            w.write_u32(1);
            (0, w.into_vec())
        }
        // ReferenceType.Fields (for object inspection)
        (2, 4) => {
            let class_id = r.read_reference_type_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            match class_id {
                OBJECT_CLASS_ID => {
                    w.write_u32(1);
                    w.write_id(FIELD_ID, sizes.field_id);
                    w.write_string("field");
                    w.write_string("I");
                    w.write_u32(1);
                }
                THROWABLE_CLASS_ID => {
                    w.write_u32(1);
                    w.write_id(DETAIL_MESSAGE_FIELD_ID, sizes.field_id);
                    w.write_string("detailMessage");
                    w.write_string("Ljava/lang/String;");
                    w.write_u32(0);
                }
                HASHMAP_CLASS_ID => {
                    w.write_u32(2);
                    w.write_id(HASHMAP_FIELD_SIZE_ID, sizes.field_id);
                    w.write_string("size");
                    w.write_string("I");
                    w.write_u32(1);

                    w.write_id(HASHMAP_FIELD_TABLE_ID, sizes.field_id);
                    w.write_string("table");
                    w.write_string("[Ljava/util/HashMap$Node;");
                    w.write_u32(1);
                }
                HASHSET_CLASS_ID => {
                    w.write_u32(1);
                    w.write_id(HASHSET_FIELD_MAP_ID, sizes.field_id);
                    w.write_string("map");
                    w.write_string("Ljava/util/HashMap;");
                    w.write_u32(1);
                }
                HASHMAP_NODE_CLASS_ID => {
                    w.write_u32(3);
                    w.write_id(HASHMAP_NODE_FIELD_KEY_ID, sizes.field_id);
                    w.write_string("key");
                    w.write_string("Ljava/lang/String;");
                    w.write_u32(1);

                    w.write_id(HASHMAP_NODE_FIELD_VALUE_ID, sizes.field_id);
                    w.write_string("value");
                    w.write_string("Ljava/lang/Object;");
                    w.write_u32(1);

                    w.write_id(HASHMAP_NODE_FIELD_NEXT_ID, sizes.field_id);
                    w.write_string("next");
                    w.write_string("Ljava/util/HashMap$Node;");
                    w.write_u32(1);
                }
                _ => {
                    w.write_u32(0);
                }
            }
            (0, w.into_vec())
        }
        // ReferenceType.GetValues (static field access)
        (2, 6) => {
            let type_id = r.read_reference_type_id(sizes).unwrap_or(0);
            let count = r.read_u32().unwrap_or(0) as usize;
            let mut field_ids = Vec::with_capacity(count);
            for _ in 0..count {
                field_ids.push(r.read_id(sizes.field_id).unwrap_or(0));
            }
            let mut w = JdwpWriter::new();
            w.write_u32(field_ids.len() as u32);
            for field_id in field_ids {
                match (type_id, field_id) {
                    (OBJECT_CLASS_ID, FIELD_ID) => {
                        w.write_u8(b'I');
                        w.write_i32(7);
                    }
                    (THROWABLE_CLASS_ID, DETAIL_MESSAGE_FIELD_ID) => {
                        // String values are tagged as `s` (JDWP Tag.STRING) in replies.
                        w.write_u8(b's');
                        w.write_object_id(STRING_OBJECT_ID, sizes);
                    }
                    _ => {
                        w.write_u8(b'V');
                    }
                }
            }
            (0, w.into_vec())
        }
        // Method.LineTable
        (6, 1) => {
            let _class_id = r.read_reference_type_id(sizes).unwrap_or(0);
            let _method_id = r.read_id(sizes.method_id).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_u64(0);
            w.write_u64(10);
            w.write_u32(1);
            w.write_u64(0);
            w.write_i32(3);
            (0, w.into_vec())
        }
        // Method.VariableTable
        (6, 2) => {
            let _class_id = r.read_reference_type_id(sizes).unwrap_or(0);
            let _method_id = r.read_id(sizes.method_id).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_u32(0); // arg count
            w.write_u32(4); // slots

            // int x (slot 0)
            w.write_u64(0);
            w.write_string("x");
            w.write_string("I");
            w.write_u32(10);
            w.write_u32(0);

            // Object obj (slot 1)
            w.write_u64(0);
            w.write_string("obj");
            w.write_string("LObject;");
            w.write_u32(10);
            w.write_u32(1);

            // String s (slot 2)
            w.write_u64(0);
            w.write_string("s");
            w.write_string("Ljava/lang/String;");
            w.write_u32(10);
            w.write_u32(2);

            // int[] arr (slot 3)
            w.write_u64(0);
            w.write_string("arr");
            w.write_string("[I");
            w.write_u32(10);
            w.write_u32(3);

            (0, w.into_vec())
        }
        // StackFrame.GetValues
        (16, 1) => {
            if *state.breakpoint_suspend_policy.lock().await == Some(0) {
                // JDWP `Error.THREAD_NOT_SUSPENDED` (no suspension means frames/locals are unavailable).
                (ERROR_THREAD_NOT_SUSPENDED, Vec::new())
            } else {
                let _thread_id = r.read_object_id(sizes).unwrap_or(0);
                let _frame_id = r.read_id(sizes.frame_id).unwrap_or(0);
                let count = r.read_u32().unwrap_or(0) as usize;
                let mut slots = Vec::with_capacity(count);
                for _ in 0..count {
                    let slot = r.read_u32().unwrap_or(0);
                    let tag = r.read_u8().unwrap_or(0);
                    slots.push((slot, tag));
                }
                let mut w = JdwpWriter::new();
                w.write_u32(slots.len() as u32);
                for (slot, tag) in slots {
                    match (slot, tag) {
                        (0, b'I') => {
                            w.write_u8(b'I');
                            w.write_i32(42);
                        }
                        (1, _) => {
                            w.write_u8(b'L');
                            w.write_object_id(OBJECT_ID, sizes);
                        }
                        (2, _) => {
                            // String values are tagged as `s` (JDWP Tag.STRING) in replies.
                            w.write_u8(b's');
                            w.write_object_id(STRING_OBJECT_ID, sizes);
                        }
                        (3, _) => {
                            w.write_u8(b'[');
                            w.write_object_id(ARRAY_OBJECT_ID, sizes);
                        }
                        _ => {
                            w.write_u8(b'V');
                        }
                    }
                }
                (0, w.into_vec())
            }
        }
        // StackFrame.ThisObject
        (16, 3) => {
            let _thread_id = r.read_object_id(sizes).unwrap_or(0);
            let _frame_id = r.read_id(sizes.frame_id).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_object_id(OBJECT_ID, sizes);
            (0, w.into_vec())
        }
        // ObjectReference.ReferenceType
        (9, 1) => {
            let object_id = r.read_object_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            match object_id {
                OBJECT_ID => {
                    w.write_u8(1); // TypeTag.CLASS
                    w.write_reference_type_id(OBJECT_CLASS_ID, sizes);
                }
                EXCEPTION_ID => {
                    w.write_u8(1); // TypeTag.CLASS
                    w.write_reference_type_id(EXCEPTION_CLASS_ID, sizes);
                }
                STRING_OBJECT_ID => {
                    w.write_u8(1); // TypeTag.CLASS
                    w.write_reference_type_id(STRING_CLASS_ID, sizes);
                }
                SAMPLE_STRING_OBJECT_ID | HASHMAP_KEY_A_OBJECT_ID | HASHMAP_KEY_B_OBJECT_ID => {
                    w.write_u8(1); // TypeTag.CLASS
                    w.write_reference_type_id(STRING_CLASS_ID, sizes);
                }
                ARRAY_OBJECT_ID => {
                    w.write_u8(3); // TypeTag.ARRAY
                    w.write_reference_type_id(ARRAY_CLASS_ID, sizes);
                }
                SAMPLE_INT_ARRAY_OBJECT_ID => {
                    w.write_u8(3); // TypeTag.ARRAY
                    w.write_reference_type_id(ARRAY_CLASS_ID, sizes);
                }
                SAMPLE_HASHMAP_OBJECT_ID => {
                    w.write_u8(1); // TypeTag.CLASS
                    w.write_reference_type_id(HASHMAP_CLASS_ID, sizes);
                }
                SAMPLE_HASHSET_OBJECT_ID => {
                    w.write_u8(1); // TypeTag.CLASS
                    w.write_reference_type_id(HASHSET_CLASS_ID, sizes);
                }
                HASHMAP_TABLE_ARRAY_OBJECT_ID => {
                    w.write_u8(3); // TypeTag.ARRAY
                    w.write_reference_type_id(HASHMAP_TABLE_ARRAY_CLASS_ID, sizes);
                }
                HASHMAP_NODE_A_OBJECT_ID | HASHMAP_NODE_B_OBJECT_ID => {
                    w.write_u8(1); // TypeTag.CLASS
                    w.write_reference_type_id(HASHMAP_NODE_CLASS_ID, sizes);
                }
                _ => {
                    // Default to a generic class reference type for unknown object ids.
                    w.write_u8(1);
                    w.write_reference_type_id(OBJECT_CLASS_ID, sizes);
                }
            }
            (0, w.into_vec())
        }
        // ObjectReference.MonitorInfo
        (9, 5) => {
            if !cap(6) {
                (ERROR_NOT_IMPLEMENTED, Vec::new())
            } else {
                let object_id = r.read_object_id(sizes).unwrap_or(0);
                let mut w = JdwpWriter::new();
                match object_id {
                    CONTENDED_MONITOR_OBJECT_ID => {
                        w.write_object_id(WORKER_THREAD_ID, sizes); // owner
                        w.write_i32(1); // entry count
                        w.write_u32(1); // waiter count
                        w.write_object_id(THREAD_ID, sizes); // waiters
                    }
                    OWNED_MONITOR_A_OBJECT_ID | OWNED_MONITOR_B_OBJECT_ID => {
                        w.write_object_id(THREAD_ID, sizes); // owner
                        w.write_i32(2); // entry count
                        w.write_u32(0); // waiter count
                    }
                    _ => {
                        // Unknown objects: report a "no owner" monitor with no waiters.
                        w.write_object_id(0, sizes);
                        w.write_i32(0);
                        w.write_u32(0);
                    }
                }
                (0, w.into_vec())
            }
        }
        // ObjectReference.InvokeMethod
        (9, 6) => {
            let res = (|| {
                let _object_id = r.read_object_id(sizes)?;
                let _thread_id = r.read_object_id(sizes)?;
                let _class_id = r.read_reference_type_id(sizes)?;
                let _method_id = r.read_id(sizes.method_id)?;
                let arg_count = r.read_u32()? as usize;
                let mut args = Vec::with_capacity(arg_count);
                for _ in 0..arg_count {
                    args.push(r.read_tagged_value(sizes)?);
                }
                let _options = r.read_u32()?;
                Ok::<_, super::types::JdwpError>(args)
            })();

            match res {
                Ok(args) => {
                    let return_value = args.first().cloned().unwrap_or(JdwpValue::Void);
                    let mut w = JdwpWriter::new();
                    w.write_tagged_value(&return_value, sizes);
                    w.write_object_id(0, sizes); // exception
                    (0, w.into_vec())
                }
                Err(_) => (1, Vec::new()),
            }
        }
        // ObjectReference.GetValues
        (9, 2) => {
            let object_id = r.read_object_id(sizes).unwrap_or(0);
            let count = r.read_u32().unwrap_or(0) as usize;
            let mut field_ids = Vec::with_capacity(count);
            for _ in 0..count {
                field_ids.push(r.read_id(sizes.field_id).unwrap_or(0));
            }
            let mut w = JdwpWriter::new();
            w.write_u32(count as u32);
            for field_id in field_ids {
                match (object_id, field_id) {
                    (EXCEPTION_ID, DETAIL_MESSAGE_FIELD_ID) => {
                        w.write_u8(b's');
                        w.write_object_id(STRING_OBJECT_ID, sizes);
                    }
                    (SAMPLE_HASHMAP_OBJECT_ID, HASHMAP_FIELD_SIZE_ID) => {
                        w.write_u8(b'I');
                        w.write_i32(2);
                    }
                    (SAMPLE_HASHMAP_OBJECT_ID, HASHMAP_FIELD_TABLE_ID) => {
                        w.write_u8(b'[');
                        w.write_object_id(HASHMAP_TABLE_ARRAY_OBJECT_ID, sizes);
                    }
                    (SAMPLE_HASHSET_OBJECT_ID, HASHSET_FIELD_MAP_ID) => {
                        w.write_u8(b'L');
                        w.write_object_id(SAMPLE_HASHMAP_OBJECT_ID, sizes);
                    }
                    (HASHMAP_NODE_A_OBJECT_ID, HASHMAP_NODE_FIELD_KEY_ID) => {
                        w.write_u8(b's');
                        w.write_object_id(HASHMAP_KEY_A_OBJECT_ID, sizes);
                    }
                    (HASHMAP_NODE_B_OBJECT_ID, HASHMAP_NODE_FIELD_KEY_ID) => {
                        w.write_u8(b's');
                        w.write_object_id(HASHMAP_KEY_B_OBJECT_ID, sizes);
                    }
                    (
                        HASHMAP_NODE_A_OBJECT_ID | HASHMAP_NODE_B_OBJECT_ID,
                        HASHMAP_NODE_FIELD_VALUE_ID,
                    )
                    | (
                        HASHMAP_NODE_A_OBJECT_ID | HASHMAP_NODE_B_OBJECT_ID,
                        HASHMAP_NODE_FIELD_NEXT_ID,
                    ) => {
                        w.write_u8(b'L');
                        w.write_object_id(0, sizes);
                    }
                    _ => {
                        w.write_u8(b'I');
                        w.write_i32(7);
                    }
                }
            }
            (0, w.into_vec())
        }
        // ObjectReference.DisableCollection
        (9, 7) => {
            let object_id = r.read_object_id(sizes).unwrap_or(0);
            state.pinned_object_ids.lock().await.insert(object_id);
            (0, Vec::new())
        }
        // ObjectReference.EnableCollection
        (9, 8) => {
            let object_id = r.read_object_id(sizes).unwrap_or(0);
            state.pinned_object_ids.lock().await.remove(&object_id);
            (0, Vec::new())
        }
        // StringReference.Value
        (10, 1) => {
            let object_id = r.read_object_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            let value = match object_id {
                STRING_OBJECT_ID => "mock string".to_string(),
                SAMPLE_STRING_OBJECT_ID => {
                    // Include characters that require escaping and exceed the formatter's default length.
                    let mut out = "hello\\world\n\"quoted\" and a long tail: ".repeat(3);
                    out.push_str(&"x".repeat(80));
                    out
                }
                HASHMAP_KEY_A_OBJECT_ID => "a".to_string(),
                HASHMAP_KEY_B_OBJECT_ID => "b".to_string(),
                _ => "mock string".to_string(),
            };
            w.write_string(&value);
            (0, w.into_vec())
        }
        // ArrayReference.Length
        (13, 1) => {
            let array_id = r.read_object_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            let len = match array_id {
                ARRAY_OBJECT_ID => 3,
                SAMPLE_INT_ARRAY_OBJECT_ID => 5,
                HASHMAP_TABLE_ARRAY_OBJECT_ID => 2,
                _ => 0,
            };
            w.write_i32(len);
            (0, w.into_vec())
        }
        // ArrayReference.GetValues
        (13, 2) => {
            let array_id = r.read_object_id(sizes).unwrap_or(0);
            let first_index = r.read_i32().unwrap_or(0);
            let length = r.read_i32().unwrap_or(0);
            let mut w = JdwpWriter::new();
            match array_id {
                ARRAY_OBJECT_ID => {
                    w.write_u8(b'I'); // element tag
                    w.write_u32(length.max(0) as u32);
                    for idx in 0..length.max(0) {
                        w.write_i32(first_index + idx);
                    }
                }
                SAMPLE_INT_ARRAY_OBJECT_ID => {
                    let values = [10i32, 20, 30, 40, 50];
                    let start = first_index.max(0) as usize;
                    let req = length.max(0) as usize;
                    let end = start.saturating_add(req).min(values.len());
                    let slice = if start < end {
                        &values[start..end]
                    } else {
                        &[]
                    };
                    w.write_u8(b'I');
                    w.write_u32(slice.len() as u32);
                    for value in slice {
                        w.write_i32(*value);
                    }
                }
                HASHMAP_TABLE_ARRAY_OBJECT_ID => {
                    let buckets_a = [HASHMAP_NODE_B_OBJECT_ID, HASHMAP_NODE_A_OBJECT_ID];
                    let buckets_b = [HASHMAP_NODE_A_OBJECT_ID, HASHMAP_NODE_B_OBJECT_ID];
                    let call = state.hashmap_bucket_calls.fetch_add(1, Ordering::Relaxed);
                    let buckets = if call % 2 == 0 {
                        &buckets_a
                    } else {
                        &buckets_b
                    };

                    let start = first_index.max(0) as usize;
                    let req = length.max(0) as usize;
                    let end = start.saturating_add(req).min(buckets.len());
                    let slice = if start < end {
                        &buckets[start..end]
                    } else {
                        &[]
                    };

                    w.write_u8(b'L');
                    w.write_u32(slice.len() as u32);
                    for object_id in slice {
                        w.write_object_id(*object_id, sizes);
                    }
                }
                _ => {
                    w.write_u8(b'V');
                    w.write_u32(0);
                }
            }
            (0, w.into_vec())
        }
        // ClassLoaderReference.DefineClass
        (14, 2) => {
            let res = (|| {
                let _loader = r.read_object_id(sizes)?;
                let _name = r.read_string()?;
                let len = r.read_u32()? as usize;
                let _bytes = r.read_bytes(len)?;
                Ok::<_, super::types::JdwpError>(())
            })();

            if res.is_err() {
                (1, Vec::new())
            } else {
                let mut w = JdwpWriter::new();
                w.write_reference_type_id(DEFINED_CLASS_ID, sizes);
                (0, w.into_vec())
            }
        }
        // ClassType.InvokeMethod
        (3, 3) => {
            let res = (|| {
                let _class_id = r.read_reference_type_id(sizes)?;
                let _thread_id = r.read_object_id(sizes)?;
                let _method_id = r.read_id(sizes.method_id)?;
                let arg_count = r.read_u32()? as usize;
                let mut args = Vec::with_capacity(arg_count);
                for _ in 0..arg_count {
                    args.push(r.read_tagged_value(sizes)?);
                }
                let _options = r.read_u32()?;
                Ok::<_, super::types::JdwpError>(args)
            })();

            match res {
                Ok(args) => {
                    let return_value = args.first().cloned().unwrap_or(JdwpValue::Void);
                    let mut w = JdwpWriter::new();
                    w.write_tagged_value(&return_value, sizes);
                    w.write_object_id(0, sizes); // exception
                    (0, w.into_vec())
                }
                Err(_) => (1, Vec::new()),
            }
        }
        // EventRequest.Set
        (15, 1) => {
            let event_kind = r.read_u8().unwrap_or(0);
            let suspend_policy = r.read_u8().unwrap_or(0);
            let modifier_count = r.read_u32().unwrap_or(0) as usize;
            let mut count_modifier: Option<u32> = None;
            let mut exception_caught = false;
            let mut exception_uncaught = false;
            let mut field_only: Option<(ReferenceTypeId, FieldId)> = None;
            let mut instance_only: Option<ObjectId> = None;
            let mut modifiers = Vec::with_capacity(modifier_count);
            for _ in 0..modifier_count {
                let mod_kind = r.read_u8().unwrap_or(0);
                match mod_kind {
                    EVENT_MODIFIER_KIND_COUNT => {
                        let count = r.read_u32().unwrap_or(0);
                        count_modifier = Some(count);
                        modifiers.push(MockEventRequestModifier::Count { count });
                    }
                    EVENT_MODIFIER_KIND_THREAD_ONLY => {
                        let thread = r.read_object_id(sizes).unwrap_or(0);
                        modifiers.push(MockEventRequestModifier::ThreadOnly { thread });
                    }
                    EVENT_MODIFIER_KIND_CLASS_ONLY => {
                        let class_id = r.read_reference_type_id(sizes).unwrap_or(0);
                        modifiers.push(MockEventRequestModifier::ClassOnly { class_id });
                    }
                    EVENT_MODIFIER_KIND_CLASS_MATCH => {
                        let pattern = r.read_string().unwrap_or_default();
                        modifiers.push(MockEventRequestModifier::ClassMatch { pattern });
                    }
                    EVENT_MODIFIER_KIND_CLASS_EXCLUDE => {
                        let pattern = r.read_string().unwrap_or_default();
                        modifiers.push(MockEventRequestModifier::ClassExclude { pattern });
                    }
                    EVENT_MODIFIER_KIND_LOCATION_ONLY => {
                        let location =
                            r.read_location(sizes).unwrap_or_else(|_| default_location());
                        modifiers.push(MockEventRequestModifier::LocationOnly { location });
                    }
                    EVENT_MODIFIER_KIND_EXCEPTION_ONLY => {
                        let exception_or_null = r.read_reference_type_id(sizes).unwrap_or(0);
                        exception_caught = r.read_bool().unwrap_or(false);
                        exception_uncaught = r.read_bool().unwrap_or(false);
                        modifiers.push(MockEventRequestModifier::ExceptionOnly {
                            exception_or_null,
                            caught: exception_caught,
                            uncaught: exception_uncaught,
                        });
                    }
                    EVENT_MODIFIER_KIND_FIELD_ONLY => {
                        let class_id = r.read_reference_type_id(sizes).unwrap_or(0);
                        let field_id = r.read_id(sizes.field_id).unwrap_or(0);
                        field_only = Some((class_id, field_id));
                        modifiers.push(MockEventRequestModifier::FieldOnly { class_id, field_id });
                    }
                    EVENT_MODIFIER_KIND_STEP => {
                        let thread = r.read_object_id(sizes).unwrap_or(0);
                        let size = r.read_u32().unwrap_or(0);
                        let depth = r.read_u32().unwrap_or(0);
                        modifiers.push(MockEventRequestModifier::Step {
                            thread,
                            size,
                            depth,
                        });
                    }
                    EVENT_MODIFIER_KIND_INSTANCE_ONLY => {
                        let object_id = r.read_object_id(sizes).unwrap_or(0);
                        instance_only = Some(object_id);
                        modifiers.push(MockEventRequestModifier::InstanceOnly { object_id });
                    }
                    EVENT_MODIFIER_KIND_SOURCE_NAME_MATCH => {
                        let pattern = r.read_string().unwrap_or_default();
                        modifiers.push(MockEventRequestModifier::SourceNameMatch { pattern });
                    }
                    _ => {}
                }
            }
            let request_id = state.alloc_request_id();
            state.event_requests.lock().await.push(MockEventRequest {
                event_kind,
                suspend_policy,
                request_id,
                modifiers,
            });
            match event_kind {
                1 => {
                    *state.step_request.lock().await = Some(request_id);
                    *state.step_suspend_policy.lock().await = Some(suspend_policy);
                }
                2 => {
                    *state.breakpoint_request.lock().await = Some(request_id);
                    *state.breakpoint_suspend_policy.lock().await = Some(suspend_policy);
                    *state.breakpoint_count_modifier.lock().await = count_modifier;
                }
                4 => {
                    *state.exception_request.lock().await = Some(MockExceptionRequest {
                        request_id,
                        caught: exception_caught,
                        uncaught: exception_uncaught,
                    })
                }
                6 => *state.thread_start_request.lock().await = Some(request_id),
                7 => *state.thread_death_request.lock().await = Some(request_id),
                42 => *state.method_exit_request.lock().await = Some(request_id),
                EVENT_KIND_CLASS_UNLOAD => {
                    *state.class_unload_request.lock().await = Some(MockSimpleEventRequest {
                        request_id,
                        suspend_policy,
                    })
                }
                EVENT_KIND_FIELD_ACCESS => {
                    *state.field_access_request.lock().await = Some(MockWatchpointRequest {
                        request_id,
                        suspend_policy,
                        field_only,
                        instance_only,
                    })
                }
                EVENT_KIND_FIELD_MODIFICATION => {
                    *state.field_modification_request.lock().await = Some(MockWatchpointRequest {
                        request_id,
                        suspend_policy,
                        field_only,
                        instance_only,
                    })
                }
                EVENT_KIND_VM_DISCONNECT => {
                    *state.vm_disconnect_request.lock().await = Some(MockSimpleEventRequest {
                        request_id,
                        suspend_policy,
                    })
                }
                _ => {}
            }
            let mut w = JdwpWriter::new();
            w.write_i32(request_id);
            (0, w.into_vec())
        }
        // EventRequest.Clear
        (15, 2) => {
            let event_kind = r.read_u8().unwrap_or(0);
            let request_id = r.read_i32().unwrap_or(0);
            match event_kind {
                1 => {
                    let mut guard = state.step_request.lock().await;
                    if guard.map(|v| v == request_id).unwrap_or(false) {
                        *guard = None;
                    }
                }
                2 => {
                    let mut guard = state.breakpoint_request.lock().await;
                    if guard.map(|v| v == request_id).unwrap_or(false) {
                        *guard = None;
                    }
                }
                4 => {
                    let mut guard = state.exception_request.lock().await;
                    if guard.map(|v| v.request_id == request_id).unwrap_or(false) {
                        *guard = None;
                    }
                }
                6 => {
                    let mut guard = state.thread_start_request.lock().await;
                    if guard.map(|v| v == request_id).unwrap_or(false) {
                        *guard = None;
                    }
                }
                7 => {
                    let mut guard = state.thread_death_request.lock().await;
                    if guard.map(|v| v == request_id).unwrap_or(false) {
                        *guard = None;
                    }
                }
                42 => {
                    let mut guard = state.method_exit_request.lock().await;
                    if guard.map(|v| v == request_id).unwrap_or(false) {
                        *guard = None;
                    }
                }
                EVENT_KIND_CLASS_UNLOAD => {
                    let mut guard = state.class_unload_request.lock().await;
                    if guard
                        .map(|v| v.request_id == request_id)
                        .unwrap_or(false)
                    {
                        *guard = None;
                    }
                }
                EVENT_KIND_FIELD_ACCESS => {
                    let mut guard = state.field_access_request.lock().await;
                    if guard
                        .map(|v| v.request_id == request_id)
                        .unwrap_or(false)
                    {
                        *guard = None;
                    }
                }
                EVENT_KIND_FIELD_MODIFICATION => {
                    let mut guard = state.field_modification_request.lock().await;
                    if guard
                        .map(|v| v.request_id == request_id)
                        .unwrap_or(false)
                    {
                        *guard = None;
                    }
                }
                EVENT_KIND_VM_DISCONNECT => {
                    let mut guard = state.vm_disconnect_request.lock().await;
                    if guard
                        .map(|v| v.request_id == request_id)
                        .unwrap_or(false)
                    {
                        *guard = None;
                    }
                }
                _ => {}
            }
            (0, Vec::new())
        }
        _ => {
            // Unknown command: reply with a generic error.
            let _ = r;
            let reply = encode_reply(packet.id, 1, &[]);
            return write_reply(
                writer,
                reply,
                None,
                state.reply_delay(packet.command_set, packet.command),
                shutdown,
                false,
            )
            .await;
        }
    };

    let (follow_up, close_after) = if reply_error_code == 0
        && ((packet.command_set == 1 && packet.command == 9)
            || (packet.command_set == 11 && packet.command == 3))
    {
        // After a resume, immediately emit a stop event if a request is configured.
        let breakpoint_request = { *state.breakpoint_request.lock().await };
        let breakpoint_suspend_policy = { *state.breakpoint_suspend_policy.lock().await };
        let step_request = { *state.step_request.lock().await };
        let step_suspend_policy = { *state.step_suspend_policy.lock().await };
        let method_exit_request = { *state.method_exit_request.lock().await };
        let exception_request = { *state.exception_request.lock().await };
        let thread_start_request = { *state.thread_start_request.lock().await };
        let thread_death_request = { *state.thread_death_request.lock().await };
        let class_unload_request = { *state.class_unload_request.lock().await };
        let field_access_request = { *state.field_access_request.lock().await };
        let field_modification_request = { *state.field_modification_request.lock().await };
        let vm_disconnect_request = { *state.vm_disconnect_request.lock().await };

        let mut follow_up = Vec::new();
        let mut close_after = false;

        if let Some(request_id) = thread_start_request {
            {
                let mut threads = state.threads.lock().await;
                if !threads.contains(&WORKER_THREAD_ID) {
                    threads.push(WORKER_THREAD_ID);
                }
            }
            follow_up.extend(make_thread_event_packet(
                state,
                id_sizes,
                6,
                request_id,
                WORKER_THREAD_ID,
            ));
        }

        if let Some(request_id) = thread_death_request {
            {
                let mut threads = state.threads.lock().await;
                threads.retain(|t| *t != WORKER_THREAD_ID);
            }
            follow_up.extend(make_thread_event_packet(
                state,
                id_sizes,
                7,
                request_id,
                WORKER_THREAD_ID,
                ));
        }

        if let Some(request) = class_unload_request {
            if state.take_class_unload_event() {
                follow_up.extend(make_class_unload_event_packet(
                    state,
                    id_sizes,
                    request.suspend_policy,
                    request.request_id,
                    &state.config.class_signature,
                ));
            }
        }

        if let Some(request) = field_access_request {
            if state.take_field_access_event() {
                follow_up.extend(make_field_access_event_packet(state, id_sizes, request));
            }
        }

        if let Some(request) = field_modification_request {
            if state.take_field_modification_event() {
                follow_up.extend(make_field_modification_event_packet(state, id_sizes, request));
            }
        }

        if let Some(stop_packet) = make_stop_event_packet(
            state,
            id_sizes,
            breakpoint_request,
            breakpoint_suspend_policy,
            step_request,
            step_suspend_policy,
            method_exit_request,
            exception_request,
        ) {
            follow_up.extend(stop_packet);
        }

        if let Some(request) = vm_disconnect_request {
            if state.take_vm_disconnect_event() {
                follow_up.extend(make_vm_disconnect_event_packet(
                    state,
                    id_sizes,
                    request.suspend_policy,
                    request.request_id,
                ));
                close_after = true;
            }
        }

        let follow_up = if follow_up.is_empty() {
            None
        } else {
            Some(follow_up)
        };
        (follow_up, close_after)
    } else {
        (None, false)
    };

    write_reply(
        writer,
        encode_reply(packet.id, reply_error_code, &reply_payload),
        follow_up,
        state.reply_delay(packet.command_set, packet.command),
        shutdown,
        close_after,
    )
    .await
}

async fn write_reply(
    writer: &Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    reply: Vec<u8>,
    follow_up: Option<Vec<u8>>,
    delay: Option<Duration>,
    shutdown: CancellationToken,
    close_after: bool,
) -> std::io::Result<()> {
    let delay = delay.filter(|d| !d.is_zero());
    if let Some(delay) = delay {
        let writer = writer.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = shutdown.cancelled() => {}
                _ = tokio::time::sleep(delay) => {
                    let mut guard = writer.lock().await;
                    let _ = guard.write_all(&reply).await;
                    if let Some(follow_up) = follow_up {
                        let _ = guard.write_all(&follow_up).await;
                    }

                    if close_after {
                        let _ = guard.shutdown().await;
                        shutdown.cancel();
                    }
                }
            }
        });
        return Ok(());
    }

    let mut guard = writer.lock().await;
    guard.write_all(&reply).await?;
    if let Some(follow_up) = follow_up {
        guard.write_all(&follow_up).await?;
    }

    if close_after {
        guard.shutdown().await?;
        shutdown.cancel();
    }

    Ok(())
}

fn make_thread_event_packet(
    state: &State,
    id_sizes: &JdwpIdSizes,
    event_kind: u8,
    request_id: i32,
    thread_id: u64,
) -> Vec<u8> {
    let mut w = JdwpWriter::new();
    w.write_u8(0); // suspend policy: none
    w.write_u32(1); // event count
    w.write_u8(event_kind);
    w.write_i32(request_id);
    w.write_object_id(thread_id, id_sizes);
    let payload = w.into_vec();
    let packet_id = state.alloc_packet_id();
    encode_command(packet_id, 64, 100, &payload)
}

fn make_class_unload_event_packet(
    state: &State,
    _id_sizes: &JdwpIdSizes,
    suspend_policy: u8,
    request_id: i32,
    signature: &str,
) -> Vec<u8> {
    let mut w = JdwpWriter::new();
    w.write_u8(suspend_policy);
    w.write_u32(1); // event count
    w.write_u8(EVENT_KIND_CLASS_UNLOAD);
    w.write_i32(request_id);
    w.write_string(signature);

    let payload = w.into_vec();
    let packet_id = state.alloc_packet_id();
    encode_command(packet_id, 64, 100, &payload)
}

fn make_field_access_event_packet(
    state: &State,
    id_sizes: &JdwpIdSizes,
    request: MockWatchpointRequest,
) -> Vec<u8> {
    let (type_id, field_id) = request
        .field_only
        .unwrap_or((CLASS_ID, FIELD_ID));
    let object_id = request.instance_only.unwrap_or(OBJECT_ID);
    let location = Location {
        type_tag: 1,
        class_id: type_id,
        method_id: METHOD_ID,
        index: 0,
    };

    let mut w = JdwpWriter::new();
    w.write_u8(request.suspend_policy);
    w.write_u32(1); // event count
    w.write_u8(EVENT_KIND_FIELD_ACCESS);
    w.write_i32(request.request_id);
    w.write_object_id(THREAD_ID, id_sizes);
    w.write_location(&location, id_sizes);
    w.write_u8(1); // TypeTag.CLASS
    w.write_reference_type_id(type_id, id_sizes);
    w.write_id(field_id, id_sizes.field_id);
    w.write_object_id(object_id, id_sizes);
    // Value being accessed.
    w.write_u8(b'I');
    w.write_i32(7);

    let payload = w.into_vec();
    let packet_id = state.alloc_packet_id();
    encode_command(packet_id, 64, 100, &payload)
}

fn make_field_modification_event_packet(
    state: &State,
    id_sizes: &JdwpIdSizes,
    request: MockWatchpointRequest,
) -> Vec<u8> {
    let (type_id, field_id) = request
        .field_only
        .unwrap_or((CLASS_ID, FIELD_ID));
    let object_id = request.instance_only.unwrap_or(OBJECT_ID);
    let location = Location {
        type_tag: 1,
        class_id: type_id,
        method_id: METHOD_ID,
        index: 0,
    };

    let mut w = JdwpWriter::new();
    w.write_u8(request.suspend_policy);
    w.write_u32(1); // event count
    w.write_u8(EVENT_KIND_FIELD_MODIFICATION);
    w.write_i32(request.request_id);
    w.write_object_id(THREAD_ID, id_sizes);
    w.write_location(&location, id_sizes);
    w.write_u8(1); // TypeTag.CLASS
    w.write_reference_type_id(type_id, id_sizes);
    w.write_id(field_id, id_sizes.field_id);
    w.write_object_id(object_id, id_sizes);
    // Value about to be written.
    w.write_u8(b'I');
    w.write_i32(8);

    let payload = w.into_vec();
    let packet_id = state.alloc_packet_id();
    encode_command(packet_id, 64, 100, &payload)
}

fn make_vm_disconnect_event_packet(
    state: &State,
    _id_sizes: &JdwpIdSizes,
    suspend_policy: u8,
    request_id: i32,
) -> Vec<u8> {
    let mut w = JdwpWriter::new();
    w.write_u8(suspend_policy);
    w.write_u32(1); // event count
    w.write_u8(EVENT_KIND_VM_DISCONNECT);
    w.write_i32(request_id);

    let payload = w.into_vec();
    let packet_id = state.alloc_packet_id();
    encode_command(packet_id, 64, 100, &payload)
}

fn make_stop_event_packet(
    state: &State,
    id_sizes: &JdwpIdSizes,
    breakpoint_request: Option<i32>,
    breakpoint_suspend_policy: Option<u8>,
    step_request: Option<i32>,
    step_suspend_policy: Option<u8>,
    method_exit_request: Option<i32>,
    exception_request: Option<MockExceptionRequest>,
) -> Option<Vec<u8>> {
    if let (Some(step_request), Some(method_exit_request)) = (step_request, method_exit_request) {
        if !state.take_step_event() {
            return None;
        }

        let mut w = JdwpWriter::new();
        w.write_u8(step_suspend_policy.unwrap_or(1)); // suspend policy
        w.write_u32(2); // event count

        // Step event first.
        w.write_u8(1); // SingleStep
        w.write_i32(step_request);
        w.write_object_id(THREAD_ID, id_sizes);
        w.write_location(&default_location(), id_sizes);

        // MethodExitWithReturnValue event after the stop event to validate that
        // the client reorders events before broadcasting.
        w.write_u8(42);
        w.write_i32(method_exit_request);
        w.write_object_id(THREAD_ID, id_sizes);
        w.write_location(&default_location(), id_sizes);
        w.write_u8(b'I');
        w.write_i32(123);

        let payload = w.into_vec();
        let packet_id = state.alloc_packet_id();
        return Some(encode_command(packet_id, 64, 100, &payload));
    }

    if state.config.emit_exception_breakpoint_method_exit_composite {
        if let (Some(exception_request), Some(breakpoint_request), Some(method_exit_request)) =
            (exception_request, breakpoint_request, method_exit_request)
        {
            if !state.take_breakpoint_event() {
                return None;
            }

            let suspend_policy = breakpoint_suspend_policy.unwrap_or(1);
            let mut w = JdwpWriter::new();
            w.write_u8(suspend_policy); // suspend policy
            w.write_u32(3); // event count

            // Exception event first.
            w.write_u8(4);
            w.write_i32(exception_request.request_id);
            w.write_object_id(THREAD_ID, id_sizes);
            w.write_location(&default_location(), id_sizes);
            w.write_object_id(EXCEPTION_ID, id_sizes);
            let catch_location = if exception_request.caught {
                default_location()
            } else {
                Location {
                    type_tag: 0,
                    class_id: 0,
                    method_id: 0,
                    index: 0,
                }
            };
            w.write_location(&catch_location, id_sizes);

            // Breakpoint stop event second.
            w.write_u8(2);
            w.write_i32(breakpoint_request);
            w.write_object_id(THREAD_ID, id_sizes);
            w.write_location(&default_location(), id_sizes);

            // MethodExitWithReturnValue event last to validate that the client reorders
            // events before broadcasting.
            w.write_u8(42);
            w.write_i32(method_exit_request);
            w.write_object_id(THREAD_ID, id_sizes);
            w.write_location(&default_location(), id_sizes);
            w.write_u8(b'I');
            w.write_i32(123);

            let payload = w.into_vec();
            let packet_id = state.alloc_packet_id();
            return Some(encode_command(packet_id, 64, 100, &payload));
        }
    }

    let mut kind = None;
    let mut request_id = 0;

    if let Some(id) = breakpoint_request {
        if state.take_breakpoint_event() {
            kind = Some(2);
            request_id = id;
        }
    }

    if kind.is_none() {
        if let Some(id) = step_request {
            if state.take_step_event() {
                kind = Some(1);
                request_id = id;
            }
        }
    }

    if kind.is_none() {
        if let Some(request) = exception_request {
            kind = Some(4);
            request_id = request.request_id;
        }
    }

    let Some(kind) = kind else {
        return None;
    };

    let suspend_policy = match kind {
        2 => breakpoint_suspend_policy.unwrap_or(1),
        1 => step_suspend_policy.unwrap_or(1),
        _ => 1,
    };

    let mut w = JdwpWriter::new();
    w.write_u8(suspend_policy); // suspend policy
    w.write_u32(1); // event count
    w.write_u8(kind);
    w.write_i32(request_id);
    w.write_object_id(THREAD_ID, id_sizes);
    w.write_location(&default_location(), id_sizes);
    if kind == 4 {
        w.write_object_id(EXCEPTION_ID, id_sizes);
        let catch_location = if exception_request.map(|r| r.caught).unwrap_or(false) {
            default_location()
        } else {
            Location {
                type_tag: 0,
                class_id: 0,
                method_id: 0,
                index: 0,
            }
        };
        w.write_location(&catch_location, id_sizes);
    }

    let payload = w.into_vec();
    let packet_id = state.alloc_packet_id();
    Some(encode_command(packet_id, 64, 100, &payload))
}

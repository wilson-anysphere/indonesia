use std::{
    collections::BTreeSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicI32, AtomicU16, AtomicU32, Ordering},
        Arc,
    },
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;

use super::{
    codec::{encode_command, encode_reply, JdwpReader, JdwpWriter, HANDSHAKE, HEADER_LEN},
    types::{JdwpIdSizes, Location, ObjectId, ReferenceTypeId},
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

impl MockJdwpServer {
    pub async fn spawn() -> std::io::Result<Self> {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let shutdown = CancellationToken::new();

        let state = Arc::new(State::default());
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
        self.state.redefine_classes_error_code.store(code, Ordering::Relaxed);
    }

    pub async fn redefine_classes_calls(&self) -> Vec<RedefineClassesCall> {
        self.state.redefine_classes_calls.lock().await.clone()
    }

    pub async fn pinned_object_ids(&self) -> BTreeSet<ObjectId> {
        self.state.pinned_object_ids.lock().await.clone()
    }
}

impl Drop for MockJdwpServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

struct State {
    next_request_id: AtomicI32,
    next_packet_id: AtomicU32,
    breakpoint_request: tokio::sync::Mutex<Option<i32>>,
    step_request: tokio::sync::Mutex<Option<i32>>,
    redefine_classes_error_code: AtomicU16,
    redefine_classes_calls: tokio::sync::Mutex<Vec<RedefineClassesCall>>,
    pinned_object_ids: tokio::sync::Mutex<BTreeSet<ObjectId>>,
    last_classes_by_signature: tokio::sync::Mutex<Option<String>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            next_request_id: AtomicI32::new(0),
            next_packet_id: AtomicU32::new(0),
            breakpoint_request: tokio::sync::Mutex::new(None),
            step_request: tokio::sync::Mutex::new(None),
            redefine_classes_error_code: AtomicU16::new(0),
            redefine_classes_calls: tokio::sync::Mutex::new(Vec::new()),
            pinned_object_ids: tokio::sync::Mutex::new(BTreeSet::new()),
            last_classes_by_signature: tokio::sync::Mutex::new(None),
        }
    }
}

impl State {
    fn alloc_request_id(&self) -> i32 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn alloc_packet_id(&self) -> u32 {
        self.next_packet_id.fetch_add(1, Ordering::Relaxed) + 1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedefineClassesCall {
    pub class_count: u32,
    pub classes: Vec<(ReferenceTypeId, Vec<u8>)>,
}

const THREAD_ID: u64 = 0x1001;
const FRAME_ID: u64 = 0x2001;
const CLASS_ID: u64 = 0x3001;
const FOO_CLASS_ID: u64 = 0x3002;
const METHOD_ID: u64 = 0x4001;
const OBJECT_ID: u64 = 0x5001;
const OBJECT_CLASS_ID: u64 = 0x6001;
const FIELD_ID: u64 = 0x7001;

fn default_location() -> Location {
    Location {
        type_tag: 1,
        class_id: CLASS_ID,
        method_id: METHOD_ID,
        index: 0,
    }
}

async fn run(listener: TcpListener, state: Arc<State>, shutdown: CancellationToken) -> std::io::Result<()> {
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

            let id_sizes = JdwpIdSizes::default();

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return Ok(()),
                    res = read_packet(&mut socket) => {
                        let Some(packet) = res? else {
                            return Ok(());
                        };
                        handle_packet(&mut socket, &state, &id_sizes, packet).await?;
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

async fn read_packet(socket: &mut tokio::net::TcpStream) -> std::io::Result<Option<Packet>> {
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
    socket: &mut tokio::net::TcpStream,
    state: &State,
    id_sizes: &JdwpIdSizes,
    packet: Packet,
) -> std::io::Result<()> {
    let sizes = id_sizes;
    let mut r = JdwpReader::new(&packet.payload);

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
        // VirtualMachine.CapabilitiesNew
        (1, 17) => {
            // 32 capabilities is common in older JDWP; we keep it simple.
            let mut w = JdwpWriter::new();
            for _ in 0..32 {
                w.write_bool(false);
            }
            (0, w.into_vec())
        }
        // VirtualMachine.AllThreads
        (1, 4) => {
            let mut w = JdwpWriter::new();
            w.write_u32(1);
            w.write_object_id(THREAD_ID, sizes);
            (0, w.into_vec())
        }
        // VirtualMachine.ClassesBySignature
        (1, 2) => {
            let signature = r.read_string().unwrap_or_default();
            *state.last_classes_by_signature.lock().await = Some(signature.clone());

            let mut w = JdwpWriter::new();
            match signature.as_str() {
                "LMain;" => {
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
            w.write_string("LMain;");
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
                .push(RedefineClassesCall { class_count, classes });

            let err = state.redefine_classes_error_code.load(Ordering::Relaxed);
            (err, Vec::new())
        }
        // VirtualMachine.Suspend
        (1, 8) => Vec::new(),
        // VirtualMachine.Resume
        (1, 9) => {
            (0, Vec::new())
        }
        // ThreadReference.Name
        (11, 1) => {
            let _thread_id = r.read_object_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_string("main");
            (0, w.into_vec())
        }
        // ThreadReference.Frames
        (11, 6) => {
            let _thread_id = r.read_object_id(sizes).unwrap_or(0);
            let _start = r.read_i32().unwrap_or(0);
            let _length = r.read_i32().unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_u32(1);
            w.write_id(FRAME_ID, sizes.frame_id);
            w.write_location(&default_location(), sizes);
            (0, w.into_vec())
        }
        // ReferenceType.SourceFile
        (2, 7) => {
            let _class_id = r.read_reference_type_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_string("Main.java");
            (0, w.into_vec())
        }
        // ReferenceType.Signature
        (2, 1) => {
            let _class_id = r.read_reference_type_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_string("LMain;");
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
            if class_id == OBJECT_CLASS_ID {
                w.write_u32(1);
                w.write_id(FIELD_ID, sizes.field_id);
                w.write_string("field");
                w.write_string("I");
                w.write_u32(1);
            } else {
                w.write_u32(0);
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
            w.write_u32(2); // slots

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

            (0, w.into_vec())
        }
        // StackFrame.GetValues
        (16, 1) => {
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
                    _ => {
                        w.write_u8(b'V');
                    }
                }
            }
            (0, w.into_vec())
        }
        // ObjectReference.ReferenceType
        (9, 1) => {
            let _object_id = r.read_object_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_reference_type_id(OBJECT_CLASS_ID, sizes);
            (0, w.into_vec())
        }
        // ObjectReference.GetValues
        (9, 2) => {
            let _object_id = r.read_object_id(sizes).unwrap_or(0);
            let count = r.read_u32().unwrap_or(0) as usize;
            for _ in 0..count {
                let _field_id = r.read_id(sizes.field_id).unwrap_or(0);
            }
            let mut w = JdwpWriter::new();
            w.write_u32(count as u32);
            for _ in 0..count {
                w.write_u8(b'I');
                w.write_i32(7);
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
            let _object_id = r.read_object_id(sizes).unwrap_or(0);
            let mut w = JdwpWriter::new();
            w.write_string("mock string");
            (0, w.into_vec())
        }
        // EventRequest.Set
        (15, 1) => {
            let event_kind = r.read_u8().unwrap_or(0);
            let _suspend = r.read_u8().unwrap_or(0);
            let modifiers = r.read_u32().unwrap_or(0) as usize;
            for _ in 0..modifiers {
                let mod_kind = r.read_u8().unwrap_or(0);
                match mod_kind {
                    3 => {
                        let _thread = r.read_object_id(sizes).unwrap_or(0);
                    }
                    5 => {
                        let _pattern = r.read_string().unwrap_or_default();
                    }
                    7 => {
                        let _ = r.read_location(sizes);
                    }
                    8 => {
                        let _ = r.read_reference_type_id(sizes);
                        let _ = r.read_bool();
                        let _ = r.read_bool();
                    }
                    10 => {
                        let _ = r.read_object_id(sizes);
                        let _ = r.read_u32();
                        let _ = r.read_u32();
                    }
                    _ => {}
                }
            }
            let request_id = state.alloc_request_id();
            match event_kind {
                1 => *state.step_request.lock().await = Some(request_id),
                2 => *state.breakpoint_request.lock().await = Some(request_id),
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
                _ => {}
            }
            (0, Vec::new())
        }
        _ => {
            // Unknown command: reply with a generic error.
            let _ = r;
            return socket
                .write_all(&encode_reply(packet.id, 1, &[]))
                .await;
        }
    };

    socket
        .write_all(&encode_reply(packet.id, reply_error_code, &reply_payload))
        .await?;
    if reply_error_code == 0 && packet.command_set == 1 && packet.command == 9 {
        // After a resume, immediately emit a stop event if a request is configured.
        let breakpoint_request = { *state.breakpoint_request.lock().await };
        let step_request = { *state.step_request.lock().await };
        emit_stop_event(socket, state, id_sizes, breakpoint_request, step_request).await?;
    }
    Ok(())
}

async fn emit_stop_event(
    socket: &mut tokio::net::TcpStream,
    state: &State,
    id_sizes: &JdwpIdSizes,
    breakpoint_request: Option<i32>,
    step_request: Option<i32>,
) -> std::io::Result<()> {
    let request_id = breakpoint_request.or(step_request);
    let Some(request_id) = request_id else {
        return Ok(());
    };

    let mut w = JdwpWriter::new();
    w.write_u8(1); // suspend policy: event thread
    w.write_u32(1); // event count
    w.write_u8(if breakpoint_request.is_some() { 2 } else { 1 });
    w.write_i32(request_id);
    w.write_object_id(THREAD_ID, id_sizes);
    w.write_location(&default_location(), id_sizes);

    let payload = w.into_vec();
    let packet_id = state.alloc_packet_id();
    socket
        .write_all(&encode_command(packet_id, 64, 100, &payload))
        .await?;
    Ok(())
}

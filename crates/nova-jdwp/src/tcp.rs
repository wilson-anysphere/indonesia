use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::{
    JdwpClient, JdwpError, JdwpEvent, StackFrameInfo, StopReason, StoppedEvent, ThreadId, ThreadInfo,
};

/// A very small JDWP client.
///
/// Currently this implements only the initial JDWP handshake plus a handful of
/// commands needed for early DAP integration. Higher-level commands (value
/// inspection, object pinning) are left as [`JdwpError::NotImplemented`] while
/// the wire protocol is filled out.
pub struct TcpJdwpClient {
    stream: Option<TcpStream>,
    next_packet_id: u32,
    id_sizes: IdSizes,
    cache: Cache,
    pending_events: VecDeque<JdwpEvent>,
}

impl TcpJdwpClient {
    pub fn new() -> Self {
        Self {
            stream: None,
            next_packet_id: 1,
            id_sizes: IdSizes::default(),
            cache: Cache::default(),
            pending_events: VecDeque::new(),
        }
    }

    fn stream_mut(&mut self) -> Result<&mut TcpStream, JdwpError> {
        self.stream.as_mut().ok_or(JdwpError::NotConnected)
    }

    fn perform_handshake(stream: &mut TcpStream) -> Result<(), JdwpError> {
        const HANDSHAKE: &[u8] = b"JDWP-Handshake";

        stream.write_all(HANDSHAKE)?;
        stream.flush()?;

        let mut reply = [0u8; HANDSHAKE.len()];
        stream.read_exact(&mut reply)?;
        if reply != HANDSHAKE {
            return Err(JdwpError::HandshakeFailed);
        }
        Ok(())
    }

    fn send_command(&mut self, command_set: u8, command: u8, data: &[u8]) -> Result<Vec<u8>, JdwpError> {
        let id = self.next_packet_id;
        self.next_packet_id = self.next_packet_id.wrapping_add(1);

        let length = 11usize
            .checked_add(data.len())
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;

        let mut buf = Vec::with_capacity(length);
        buf.extend_from_slice(&(length as u32).to_be_bytes());
        buf.extend_from_slice(&id.to_be_bytes());
        buf.push(0); // flags
        buf.push(command_set);
        buf.push(command);
        buf.extend_from_slice(data);

        {
            let stream = self.stream_mut()?;
            stream.write_all(&buf)?;
            stream.flush()?;
        }

        loop {
            let packet = {
                let stream = self.stream_mut()?;
                read_packet(stream)?
            };

            match packet {
                Packet::Reply {
                    id: reply_id,
                    error_code,
                    data,
                } => {
                    if reply_id != id {
                        return Err(JdwpError::Protocol(format!(
                            "unexpected reply id {reply_id}, expected {id}"
                        )));
                    }
                    if error_code != 0 {
                        return Err(JdwpError::CommandFailed { error_code });
                    }
                    return Ok(data);
                }
                Packet::Command {
                    command_set,
                    command,
                    data,
                    ..
                } => {
                    // The JVM can deliver asynchronous events (e.g. breakpoint hits) as
                    // command packets. Queue them so the DAP side can surface them.
                    self.handle_command_packet(command_set, command, &data)?;
                    continue;
                }
            }
        }
    }

    fn id_sizes(&mut self) -> Result<IdSizes, JdwpError> {
        let reply = self.send_command(1, 7, &[])?;
        let mut cursor = Cursor::new(&reply);
        Ok(IdSizes {
            field_id: cursor.read_u32()? as usize,
            method_id: cursor.read_u32()? as usize,
            object_id: cursor.read_u32()? as usize,
            reference_type_id: cursor.read_u32()? as usize,
            frame_id: cursor.read_u32()? as usize,
        })
    }

    fn class_by_signature(&mut self, signature: &str) -> Result<Option<ClassInfo>, JdwpError> {
        if let Some(info) = self.cache.class_by_signature.get(signature) {
            return Ok(Some(*info));
        }

        let mut body = Vec::new();
        write_string(&mut body, signature);

        let reply = self.send_command(1, 2, &body)?;
        let mut cursor = Cursor::new(&reply);
        let count = cursor.read_u32()? as usize;
        if count == 0 {
            return Ok(None);
        }

        let tag = cursor.read_u8()?;
        let type_id = cursor.read_id(self.id_sizes.reference_type_id)?;
        let _status = cursor.read_u32()?;

        let info = ClassInfo { tag, type_id };
        self.cache.class_by_signature.insert(signature.to_string(), info);
        Ok(Some(info))
    }

    /// Replace the bytecode for an already-loaded class.
    ///
    /// This uses JDWP's `VirtualMachine/RedefineClasses` command. The JVM will
    /// reject changes that are not supported by HotSwap (for example schema
    /// changes like adding fields/methods).
    pub fn redefine_class(&mut self, class: &str, bytecode: &[u8]) -> Result<(), JdwpError> {
        let signature = class_name_to_signature(class);
        let Some(class_info) = self.class_by_signature(&signature)? else {
            return Err(JdwpError::Protocol(format!(
                "class {class} is not loaded in target JVM"
            )));
        };

        let mut body = Vec::new();
        body.extend_from_slice(&(1u32).to_be_bytes()); // classCount
        write_id(&mut body, self.id_sizes.reference_type_id, class_info.type_id);
        body.extend_from_slice(&(bytecode.len() as u32).to_be_bytes());
        body.extend_from_slice(bytecode);

        // VirtualMachine/RedefineClasses
        let _ = self.send_command(1, 18, &body)?;
        Ok(())
    }

    fn methods_for_type(&mut self, type_id: u64) -> Result<&HashMap<u64, MethodInfo>, JdwpError> {
        if !self.cache.methods.contains_key(&type_id) {
            let mut body = Vec::new();
            write_id(&mut body, self.id_sizes.reference_type_id, type_id);

            let reply = self.send_command(2, 5, &body)?;
            let mut cursor = Cursor::new(&reply);
            let count = cursor.read_u32()? as usize;

            let mut methods = HashMap::new();
            for _ in 0..count {
                let method_id = cursor.read_id(self.id_sizes.method_id)?;
                let name = cursor.read_string()?;
                let signature = cursor.read_string()?;
                let _mod_bits = cursor.read_u32()?;
                methods.insert(
                    method_id,
                    MethodInfo {
                        id: method_id,
                        name,
                        signature,
                    },
                );
            }

            self.cache.methods.insert(type_id, methods);
        }

        Ok(self.cache.methods.get(&type_id).unwrap())
    }

    fn line_table_for_method(&mut self, type_id: u64, method_id: u64) -> Result<&Vec<(u64, u32)>, JdwpError> {
        if !self.cache.line_tables.contains_key(&(type_id, method_id)) {
            let mut body = Vec::new();
            write_id(&mut body, self.id_sizes.reference_type_id, type_id);
            write_id(&mut body, self.id_sizes.method_id, method_id);

            let reply = self.send_command(6, 1, &body)?;
            let mut cursor = Cursor::new(&reply);
            let _start = cursor.read_i64()?;
            let _end = cursor.read_i64()?;
            let count = cursor.read_u32()? as usize;

            let mut entries = Vec::new();
            for _ in 0..count {
                let code_index = cursor.read_i64()? as u64;
                let line = cursor.read_u32()?;
                entries.push((code_index, line));
            }
            self.cache.line_tables.insert((type_id, method_id), entries);
        }

        Ok(self.cache.line_tables.get(&(type_id, method_id)).unwrap())
    }

    fn source_file_for_type(&mut self, type_id: u64) -> Result<Option<String>, JdwpError> {
        if let Some(path) = self.cache.source_files.get(&type_id) {
            return Ok(Some(path.clone()));
        }

        let mut body = Vec::new();
        write_id(&mut body, self.id_sizes.reference_type_id, type_id);

        let reply = self.send_command(2, 7, &body)?;
        let mut cursor = Cursor::new(&reply);
        let file = cursor.read_string()?;
        self.cache.source_files.insert(type_id, file.clone());
        Ok(Some(file))
    }

    fn resume_vm(&mut self) -> Result<(), JdwpError> {
        let _ = self.send_command(1, 9, &[])?;
        Ok(())
    }

    fn clear_event_request(&mut self, event_kind: u8, request_id: u32) -> Result<(), JdwpError> {
        let mut body = Vec::new();
        body.push(event_kind);
        body.extend_from_slice(&request_id.to_be_bytes());
        let _ = self.send_command(15, 2, &body)?;
        Ok(())
    }

    fn set_step_request(&mut self, thread_id: u64, depth: i32) -> Result<u32, JdwpError> {
        // EventRequest.Set(STEP)
        let mut body = Vec::new();
        body.push(1); // EventKind.STEP
        body.push(1); // SuspendPolicy.EVENT_THREAD
        body.extend_from_slice(&(2u32).to_be_bytes()); // modifiers

        // Modifier.Count (1) - trigger once.
        body.push(1);
        body.extend_from_slice(&(1u32).to_be_bytes());

        // Modifier.Step (10)
        body.push(10);
        write_id(&mut body, self.id_sizes.object_id, thread_id);
        body.extend_from_slice(&(1i32).to_be_bytes()); // StepSize.LINE
        body.extend_from_slice(&(depth).to_be_bytes());

        let reply = self.send_command(15, 1, &body)?;
        let mut cursor = Cursor::new(&reply);
        let request_id = cursor.read_u32()?;
        Ok(request_id)
    }

    fn set_breakpoint_request(&mut self, location: Location) -> Result<u32, JdwpError> {
        // EventRequest.Set(BREAKPOINT)
        let mut body = Vec::new();
        body.push(2); // EventKind.BREAKPOINT
        body.push(1); // SuspendPolicy.EVENT_THREAD
        body.extend_from_slice(&(1u32).to_be_bytes()); // modifiers

        // Modifier.LocationOnly (7)
        body.push(7);
        body.push(1); // TypeTag.CLASS
        write_id(&mut body, self.id_sizes.reference_type_id, location.type_id);
        write_id(&mut body, self.id_sizes.method_id, location.method_id);
        body.extend_from_slice(&(location.index as i64).to_be_bytes());

        let reply = self.send_command(15, 1, &body)?;
        let mut cursor = Cursor::new(&reply);
        let request_id = cursor.read_u32()?;
        Ok(request_id)
    }

    fn resolve_location(
        &mut self,
        class: &str,
        method_name: Option<&str>,
        line: u32,
    ) -> Result<Option<Location>, JdwpError> {
        let signature = class_name_to_signature(class);
        let Some(info) = self.class_by_signature(&signature)? else {
            return Ok(None);
        };

        let mut best: Option<(u32, Location)> = None;

        // Clone to avoid holding a mutable borrow of `self.cache.methods` while
        // also looking up line tables (which may populate caches).
        let methods = self.methods_for_type(info.type_id)?.clone();
        for method in methods.values() {
            if let Some(filter) = method_name {
                if method.name != filter {
                    continue;
                }
            }

            let table = self.line_table_for_method(info.type_id, method.id)?;
            if table.is_empty() {
                continue;
            }
            let mut best_entry: Option<(u32, u64)> = None;
            for &(code_index, entry_line) in table {
                let dist = if entry_line >= line {
                    entry_line - line
                } else {
                    line - entry_line
                };
                match best_entry {
                    None => best_entry = Some((dist, code_index)),
                    Some((best_dist, _)) => {
                        if dist < best_dist || (dist == best_dist && entry_line >= line) {
                            best_entry = Some((dist, code_index));
                        }
                    }
                }
            }

            if let Some((dist, code_index)) = best_entry {
                let loc = Location {
                    type_id: info.type_id,
                    method_id: method.id,
                    index: code_index as i64,
                };
                match &best {
                    None => best = Some((dist, loc)),
                    Some((best_dist, _)) if dist < *best_dist => best = Some((dist, loc)),
                    _ => {}
                }
            }
        }

        Ok(best.map(|(_, loc)| loc))
    }

    fn handle_command_packet(&mut self, command_set: u8, command: u8, data: &[u8]) -> Result<(), JdwpError> {
        // Event.Composite
        if command_set != 64 || command != 100 {
            return Ok(());
        }

        let mut cursor = Cursor::new(data);
        let _suspend_policy = cursor.read_u8()?;
        let events = cursor.read_u32()? as usize;

        for _ in 0..events {
            let kind = cursor.read_u8()?;
            let request_id = cursor.read_u32()?;

            match kind {
                1 => {
                    // Step
                    let thread_id = cursor.read_id(self.id_sizes.object_id)?;
                    // location
                    let _type_tag = cursor.read_u8()?;
                    let _type_id = cursor.read_id(self.id_sizes.reference_type_id)?;
                    let _method_id = cursor.read_id(self.id_sizes.method_id)?;
                    let _index = cursor.read_i64()?;

                    self.pending_events.push_back(JdwpEvent::Stopped(StoppedEvent {
                        reason: StopReason::Step,
                        thread_id,
                        request_id,
                        return_value: None,
                        expression_value: None,
                    }));
                }
                2 => {
                    // Breakpoint
                    let thread_id = cursor.read_id(self.id_sizes.object_id)?;
                    // location
                    let _type_tag = cursor.read_u8()?;
                    let _type_id = cursor.read_id(self.id_sizes.reference_type_id)?;
                    let _method_id = cursor.read_id(self.id_sizes.method_id)?;
                    let _index = cursor.read_i64()?;

                    self.pending_events.push_back(JdwpEvent::Stopped(StoppedEvent {
                        reason: StopReason::Breakpoint,
                        thread_id,
                        request_id,
                        return_value: None,
                        expression_value: None,
                    }));
                }
                other => {
                    return Err(JdwpError::Protocol(format!(
                        "unsupported JDWP event kind {other}"
                    )));
                }
            }
        }

        Ok(())
    }
}

impl Default for TcpJdwpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl JdwpClient for TcpJdwpClient {
    fn connect(&mut self, host: &str, port: u16) -> Result<(), JdwpError> {
        let addr = (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid JDWP address"))?;

        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;

        Self::perform_handshake(&mut stream.try_clone()?)?;

        self.stream = Some(stream);
        self.id_sizes = self.id_sizes()?;
        Ok(())
    }

    fn set_line_breakpoint(
        &mut self,
        class: &str,
        method: Option<&str>,
        line: u32,
    ) -> Result<(), JdwpError> {
        let Some(location) = self.resolve_location(class, method, line)? else {
            return Err(JdwpError::Protocol(format!(
                "unable to resolve breakpoint location for {class}:{line}"
            )));
        };

        let _ = self.set_breakpoint_request(location)?;
        Ok(())
    }

    fn threads(&mut self) -> Result<Vec<ThreadInfo>, JdwpError> {
        let reply = self.send_command(1, 4, &[])?;
        let mut cursor = Cursor::new(&reply);
        let count = cursor.read_u32()? as usize;

        let mut threads = Vec::with_capacity(count);
        for _ in 0..count {
            let id = cursor.read_id(self.id_sizes.object_id)?;

            let mut name_body = Vec::new();
            write_id(&mut name_body, self.id_sizes.object_id, id);
            let name_reply = self.send_command(11, 1, &name_body)?;
            let mut name_cursor = Cursor::new(&name_reply);
            let name = name_cursor.read_string()?;

            threads.push(ThreadInfo { id, name });
        }

        Ok(threads)
    }

    fn stack_frames(&mut self, thread_id: ThreadId) -> Result<Vec<StackFrameInfo>, JdwpError> {
        let mut body = Vec::new();
        write_id(&mut body, self.id_sizes.object_id, thread_id);
        body.extend_from_slice(&(0i32).to_be_bytes()); // startFrame
        body.extend_from_slice(&(20i32).to_be_bytes()); // length

        let reply = self.send_command(11, 6, &body)?;
        let mut cursor = Cursor::new(&reply);
        let count = cursor.read_u32()? as usize;

        let mut frames = Vec::with_capacity(count);
        for _ in 0..count {
            let frame_id = cursor.read_id(self.id_sizes.frame_id)?;
            let _type_tag = cursor.read_u8()?;
            let type_id = cursor.read_id(self.id_sizes.reference_type_id)?;
            let method_id = cursor.read_id(self.id_sizes.method_id)?;
            let index = cursor.read_i64()?;

            let methods = self.methods_for_type(type_id)?;
            let method_name = methods
                .get(&method_id)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| "<unknown>".to_string());

            let source_file = self.source_file_for_type(type_id)?;

            let line = self
                .line_table_for_method(type_id, method_id)
                .ok()
                .and_then(|table| line_for_index(table, index as u64))
                .unwrap_or(0);

            frames.push(StackFrameInfo {
                id: frame_id,
                name: method_name,
                source_path: source_file,
                line,
            });
        }

        Ok(frames)
    }

    fn r#continue(&mut self, _thread_id: ThreadId) -> Result<(), JdwpError> {
        self.resume_vm()
    }

    fn next(&mut self, thread_id: ThreadId) -> Result<(), JdwpError> {
        let _ = self.set_step_request(thread_id, 1)?; // StepDepth.OVER
        self.resume_vm()
    }

    fn step_in(&mut self, thread_id: ThreadId) -> Result<(), JdwpError> {
        let _ = self.set_step_request(thread_id, 0)?; // StepDepth.INTO
        self.resume_vm()
    }

    fn step_out(&mut self, thread_id: ThreadId) -> Result<(), JdwpError> {
        let _ = self.set_step_request(thread_id, 2)?; // StepDepth.OUT
        self.resume_vm()
    }

    fn pause(&mut self, _thread_id: ThreadId) -> Result<(), JdwpError> {
        let _ = self.send_command(1, 8, &[])?;
        Ok(())
    }

    fn wait_for_event(&mut self) -> Result<Option<JdwpEvent>, JdwpError> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(Some(event));
        }

        loop {
            let packet = {
                let stream = match self.stream_mut() {
                    Ok(stream) => stream,
                    Err(JdwpError::NotConnected) => return Ok(None),
                    Err(err) => return Err(err),
                };
                read_packet(stream)?
            };

            match packet {
                Packet::Reply { .. } => continue,
                Packet::Command {
                    command_set,
                    command,
                    data,
                    ..
                } => {
                    self.handle_command_packet(command_set, command, &data)?;
                    if let Some(event) = self.pending_events.pop_front() {
                        // Step requests are one-shot; clear to avoid accumulating disabled requests.
                        let JdwpEvent::Stopped(stopped) = &event;
                        if stopped.reason == StopReason::Step {
                            let _ = self.clear_event_request(1, stopped.request_id);
                        }
                        return Ok(Some(event));
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ClassInfo {
    tag: u8,
    type_id: u64,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct IdSizes {
    field_id: usize,
    method_id: usize,
    object_id: usize,
    reference_type_id: usize,
    frame_id: usize,
}

impl Default for IdSizes {
    fn default() -> Self {
        // Most modern JVMs use 8-byte ids, but the JDWP protocol allows targets
        // to choose. We query sizes at runtime and overwrite these defaults.
        Self {
            field_id: 8,
            method_id: 8,
            object_id: 8,
            reference_type_id: 8,
            frame_id: 8,
        }
    }
}

#[derive(Debug, Default)]
struct Cache {
    class_by_signature: HashMap<String, ClassInfo>,
    methods: HashMap<u64, HashMap<u64, MethodInfo>>,
    line_tables: HashMap<(u64, u64), Vec<(u64, u32)>>,
    source_files: HashMap<u64, String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct MethodInfo {
    id: u64,
    name: String,
    signature: String,
}

#[derive(Debug, Clone, Copy)]
struct Location {
    type_id: u64,
    method_id: u64,
    index: i64,
}

#[derive(Debug)]
enum Packet {
    Reply {
        id: u32,
        error_code: u16,
        data: Vec<u8>,
    },
    #[allow(dead_code)]
    Command {
        id: u32,
        command_set: u8,
        command: u8,
        data: Vec<u8>,
    },
}

fn read_packet(reader: &mut impl Read) -> Result<Packet, JdwpError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let length = u32::from_be_bytes(len_buf) as usize;
    if length < 11 {
        return Err(JdwpError::Protocol(format!(
            "invalid packet length {length}"
        )));
    }

    let mut rest = vec![0u8; length - 4];
    reader.read_exact(&mut rest)?;

    let id = u32::from_be_bytes(rest[0..4].try_into().unwrap());
    let flags = rest[4];

    if flags & 0x80 != 0 {
        let error_code = u16::from_be_bytes(rest[5..7].try_into().unwrap());
        Ok(Packet::Reply {
            id,
            error_code,
            data: rest[7..].to_vec(),
        })
    } else {
        Ok(Packet::Command {
            id,
            command_set: rest[5],
            command: rest[6],
            data: rest[7..].to_vec(),
        })
    }
}

fn class_name_to_signature(class: &str) -> String {
    let internal = class.replace('.', "/");
    format!("L{internal};")
}

fn write_id(buf: &mut Vec<u8>, size: usize, value: u64) {
    let bytes = value.to_be_bytes();
    let start = bytes.len().saturating_sub(size);
    buf.extend_from_slice(&bytes[start..]);
}

fn write_string(buf: &mut Vec<u8>, value: &str) {
    buf.extend_from_slice(&(value.len() as u32).to_be_bytes());
    buf.extend_from_slice(value.as_bytes());
}

fn line_for_index(table: &[(u64, u32)], index: u64) -> Option<u32> {
    // Entries are sorted by code index. Choose the last entry with code_index <= index.
    table
        .iter()
        .filter(|(code_index, _)| *code_index <= index)
        .max_by_key(|(code_index, _)| *code_index)
        .map(|(_, line)| *line)
        .or_else(|| table.first().map(|(_, line)| *line))
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], JdwpError> {
        if self.pos + len > self.buf.len() {
            return Err(JdwpError::Protocol("unexpected end of packet".to_string()));
        }
        let slice = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, JdwpError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32, JdwpError> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_i64(&mut self) -> Result<i64, JdwpError> {
        let bytes = self.read_exact(8)?;
        Ok(i64::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_id(&mut self, size: usize) -> Result<u64, JdwpError> {
        let bytes = self.read_exact(size)?;
        let mut value = 0u64;
        for b in bytes {
            value = (value << 8) | (*b as u64);
        }
        Ok(value)
    }

    fn read_string(&mut self) -> Result<String, JdwpError> {
        let len = self.read_u32()? as usize;
        let bytes = self.read_exact(len)?;
        Ok(String::from_utf8(bytes.to_vec())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_signature_conversion() {
        assert_eq!(class_name_to_signature("com.example.Foo"), "Lcom/example/Foo;");
        assert_eq!(class_name_to_signature("Foo"), "LFoo;");
    }

    #[test]
    fn parses_reply_packet() {
        // Construct a minimal reply packet: id=42, error=0, data="ok"
        let data = b"ok";
        let length = 11 + data.len();
        let mut packet = Vec::new();
        packet.extend_from_slice(&(length as u32).to_be_bytes());
        packet.extend_from_slice(&42u32.to_be_bytes());
        packet.push(0x80);
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(data);

        let mut cursor = std::io::Cursor::new(packet);
        match read_packet(&mut cursor).unwrap() {
            Packet::Reply {
                id,
                error_code,
                data,
            } => {
                assert_eq!(id, 42);
                assert_eq!(error_code, 0);
                assert_eq!(data, b"ok");
            }
            other => panic!("expected reply, got {other:?}"),
        }
    }
}

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;
use std::{collections::BTreeSet, net::SocketAddr};

use crate::{
    FrameId, JdwpClient, JdwpError, JdwpEvent, JdwpValue, JdwpVariable, ObjectId,
    ObjectKindPreview, ObjectPreview, ObjectRef, StackFrameInfo, StepKind, StopReason,
    StoppedEvent, ThreadId, ThreadInfo,
};

const ERROR_INVALID_OBJECT: u16 = 20;
const EVENT_KIND_STEP: u8 = 1;
const EVENT_KIND_BREAKPOINT: u8 = 2;
const EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE: u8 = 42;
const SUSPEND_POLICY_NONE: u8 = 0;
const MODIFIER_THREAD_ONLY: u8 = 3;
const ARRAY_PREVIEW_SAMPLE: usize = 3;
const ARRAY_CHILD_SAMPLE: usize = 25;
const FIELD_MODIFIER_STATIC: u32 = 0x0008;

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
    frame_contexts: HashMap<FrameId, FrameContext>,
    pending_events: VecDeque<JdwpEvent>,
    pending_return_values: HashMap<ThreadId, JdwpValue>,
    active_step_requests: HashMap<ThreadId, ActiveStepRequest>,
    active_method_exit_requests: HashMap<ThreadId, u32>,
}

#[derive(Clone, Copy, Debug)]
struct ActiveStepRequest {
    request_id: u32,
    kind: StepKind,
}

#[derive(Clone, Copy, Debug)]
struct FrameContext {
    thread_id: ThreadId,
    type_id: u64,
    method_id: u64,
    code_index: i64,
}

impl TcpJdwpClient {
    pub fn new() -> Self {
        Self {
            stream: None,
            next_packet_id: 1,
            id_sizes: IdSizes::default(),
            cache: Cache::default(),
            frame_contexts: HashMap::new(),
            pending_events: VecDeque::new(),
            pending_return_values: HashMap::new(),
            active_step_requests: HashMap::new(),
            active_method_exit_requests: HashMap::new(),
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

    fn send_command(
        &mut self,
        command_set: u8,
        command: u8,
        data: &[u8],
    ) -> Result<Vec<u8>, JdwpError> {
        let id = self.next_packet_id;
        self.next_packet_id = self.next_packet_id.wrapping_add(1);

        let length = crate::JDWP_HEADER_LEN
            .checked_add(data.len())
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        if length > crate::MAX_JDWP_PACKET_BYTES {
            return Err(JdwpError::Protocol(format!(
                "packet too large ({length} bytes, max {})",
                crate::MAX_JDWP_PACKET_BYTES
            )));
        }

        {
            let stream = self.stream_mut()?;
            let mut header = [0u8; crate::JDWP_HEADER_LEN];
            header[0..4].copy_from_slice(&(length as u32).to_be_bytes());
            header[4..8].copy_from_slice(&id.to_be_bytes());
            header[8] = 0; // flags
            header[9] = command_set;
            header[10] = command;

            stream.write_all(&header)?;
            stream.write_all(data)?;
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
        self.cache
            .class_by_signature
            .insert(signature.to_string(), info);
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

        let body_len = 4usize // classCount
            .checked_add(self.id_sizes.reference_type_id)
            .and_then(|v| v.checked_add(4)) // bytecodeLen
            .and_then(|v| v.checked_add(bytecode.len()))
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        let length = crate::JDWP_HEADER_LEN
            .checked_add(body_len)
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        if length > crate::MAX_JDWP_PACKET_BYTES {
            return Err(JdwpError::Protocol(format!(
                "packet too large ({length} bytes, max {})",
                crate::MAX_JDWP_PACKET_BYTES
            )));
        }

        let mut body = Vec::new();
        body.extend_from_slice(&(1u32).to_be_bytes()); // classCount
        write_id(
            &mut body,
            self.id_sizes.reference_type_id,
            class_info.type_id,
        );
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

    fn signature_for_type(&mut self, type_id: u64) -> Result<&str, JdwpError> {
        if !self.cache.signatures.contains_key(&type_id) {
            let mut body = Vec::new();
            write_id(&mut body, self.id_sizes.reference_type_id, type_id);

            // ReferenceType/Signature
            let reply = self.send_command(2, 1, &body)?;
            let mut cursor = Cursor::new(&reply);
            let signature = cursor.read_string()?;
            self.cache.signatures.insert(type_id, signature);
        }

        Ok(self
            .cache
            .signatures
            .get(&type_id)
            .map(String::as_str)
            .expect("just inserted"))
    }

    fn fields_for_type(&mut self, type_id: u64) -> Result<&[FieldInfo], JdwpError> {
        if !self.cache.fields.contains_key(&type_id) {
            let mut body = Vec::new();
            write_id(&mut body, self.id_sizes.reference_type_id, type_id);

            // ReferenceType/Fields
            let reply = self.send_command(2, 4, &body)?;
            let mut cursor = Cursor::new(&reply);
            let count = cursor.read_u32()? as usize;

            let mut fields = Vec::new();
            for _ in 0..count {
                let field_id = cursor.read_id(self.id_sizes.field_id)?;
                let name = cursor.read_string()?;
                let signature = cursor.read_string()?;
                let mod_bits = cursor.read_u32()?;
                fields.push(FieldInfo {
                    id: field_id,
                    name,
                    signature,
                    mod_bits,
                });
            }

            self.cache.fields.insert(type_id, fields);
        }

        Ok(self
            .cache
            .fields
            .get(&type_id)
            .map(Vec::as_slice)
            .expect("just inserted"))
    }

    fn reference_type_for_object(&mut self, object_id: ObjectId) -> Result<(u8, u64), JdwpError> {
        let mut body = Vec::new();
        write_id(&mut body, self.id_sizes.object_id, object_id);

        // ObjectReference/ReferenceType
        let reply = match self.send_command(9, 1, &body) {
            Ok(reply) => reply,
            Err(JdwpError::CommandFailed { error_code }) if error_code == ERROR_INVALID_OBJECT => {
                return Err(JdwpError::InvalidObjectId(object_id));
            }
            Err(err) => return Err(err),
        };

        let mut cursor = Cursor::new(&reply);
        let type_tag = cursor.read_u8()?;
        let type_id = cursor.read_id(self.id_sizes.reference_type_id)?;
        Ok((type_tag, type_id))
    }

    fn set_method_exit_with_return_value_request(
        &mut self,
        thread_id: ThreadId,
    ) -> Result<u32, JdwpError> {
        // EventRequest.Set(METHOD_EXIT_WITH_RETURN_VALUE)
        let mut body = Vec::new();
        body.push(EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE);
        body.push(SUSPEND_POLICY_NONE);
        body.extend_from_slice(&(1u32).to_be_bytes()); // modifiers

        // Modifier.ThreadOnly (3)
        body.push(MODIFIER_THREAD_ONLY);
        write_id(&mut body, self.id_sizes.object_id, thread_id);

        let reply = self.send_command(15, 1, &body)?;
        let mut cursor = Cursor::new(&reply);
        cursor.read_u32()
    }

    fn string_value(&mut self, object_id: ObjectId) -> Result<String, JdwpError> {
        let mut body = Vec::new();
        write_id(&mut body, self.id_sizes.object_id, object_id);

        // StringReference/Value
        let reply = match self.send_command(10, 1, &body) {
            Ok(reply) => reply,
            Err(JdwpError::CommandFailed { error_code }) if error_code == ERROR_INVALID_OBJECT => {
                return Err(JdwpError::InvalidObjectId(object_id));
            }
            Err(err) => return Err(err),
        };
        let mut cursor = Cursor::new(&reply);
        cursor.read_string()
    }

    fn array_length(&mut self, object_id: ObjectId) -> Result<usize, JdwpError> {
        let mut body = Vec::new();
        write_id(&mut body, self.id_sizes.object_id, object_id);

        // ArrayReference/Length
        let reply = match self.send_command(13, 1, &body) {
            Ok(reply) => reply,
            Err(JdwpError::CommandFailed { error_code }) if error_code == ERROR_INVALID_OBJECT => {
                return Err(JdwpError::InvalidObjectId(object_id));
            }
            Err(err) => return Err(err),
        };
        let mut cursor = Cursor::new(&reply);
        Ok(cursor.read_u32()? as usize)
    }

    fn array_get_values(
        &mut self,
        object_id: ObjectId,
        first_index: i32,
        length: i32,
    ) -> Result<Vec<JdwpValue>, JdwpError> {
        let mut body = Vec::new();
        write_id(&mut body, self.id_sizes.object_id, object_id);
        body.extend_from_slice(&first_index.to_be_bytes());
        body.extend_from_slice(&length.to_be_bytes());

        // ArrayReference/GetValues
        let reply = match self.send_command(13, 2, &body) {
            Ok(reply) => reply,
            Err(JdwpError::CommandFailed { error_code }) if error_code == ERROR_INVALID_OBJECT => {
                return Err(JdwpError::InvalidObjectId(object_id));
            }
            Err(err) => return Err(err),
        };

        let mut cursor = Cursor::new(&reply);
        let tag = cursor.read_u8()?;
        let count = cursor.read_u32()? as usize;
        let mut values = Vec::new();
        for _ in 0..count {
            values.push(self.read_value_with_tag(&mut cursor, tag)?);
        }
        Ok(values)
    }

    fn object_get_values(
        &mut self,
        object_id: ObjectId,
        fields: &[FieldInfo],
    ) -> Result<Vec<JdwpValue>, JdwpError> {
        let mut body = Vec::new();
        write_id(&mut body, self.id_sizes.object_id, object_id);
        body.extend_from_slice(&(fields.len() as u32).to_be_bytes());
        for field in fields {
            write_id(&mut body, self.id_sizes.field_id, field.id);
        }

        // ObjectReference/GetValues
        let reply = match self.send_command(9, 2, &body) {
            Ok(reply) => reply,
            Err(JdwpError::CommandFailed { error_code }) if error_code == ERROR_INVALID_OBJECT => {
                return Err(JdwpError::InvalidObjectId(object_id));
            }
            Err(err) => return Err(err),
        };

        let mut cursor = Cursor::new(&reply);
        let count = cursor.read_u32()? as usize;
        let mut values = Vec::new();
        for _ in 0..count {
            values.push(self.read_tagged_value(&mut cursor)?);
        }
        Ok(values)
    }

    fn read_tagged_value(&mut self, cursor: &mut Cursor<'_>) -> Result<JdwpValue, JdwpError> {
        let tag = cursor.read_u8()?;
        self.read_value_with_tag(cursor, tag)
    }

    fn read_value_with_tag(
        &mut self,
        cursor: &mut Cursor<'_>,
        tag: u8,
    ) -> Result<JdwpValue, JdwpError> {
        match tag {
            b'V' => Ok(JdwpValue::Void),
            b'Z' => Ok(JdwpValue::Boolean(cursor.read_u8()? != 0)),
            b'B' => Ok(JdwpValue::Byte(cursor.read_u8()? as i8)),
            b'S' => Ok(JdwpValue::Short(cursor.read_i16()?)),
            b'I' => Ok(JdwpValue::Int(cursor.read_i32()?)),
            b'J' => Ok(JdwpValue::Long(cursor.read_i64()?)),
            b'F' => Ok(JdwpValue::Float(cursor.read_f32()?)),
            b'D' => Ok(JdwpValue::Double(cursor.read_f64()?)),
            b'C' => Ok(JdwpValue::Char(cursor.read_java_char()?)),
            b's' => {
                let id = cursor.read_id(self.id_sizes.object_id)?;
                if id == 0 {
                    Ok(JdwpValue::Null)
                } else {
                    Ok(JdwpValue::Object(ObjectRef {
                        id,
                        runtime_type: "java.lang.String".to_string(),
                    }))
                }
            }
            b'[' => {
                let id = cursor.read_id(self.id_sizes.object_id)?;
                if id == 0 {
                    Ok(JdwpValue::Null)
                } else {
                    Ok(JdwpValue::Object(ObjectRef {
                        id,
                        runtime_type: "java.lang.Object[]".to_string(),
                    }))
                }
            }
            b'L' => {
                let id = cursor.read_id(self.id_sizes.object_id)?;
                if id == 0 {
                    Ok(JdwpValue::Null)
                } else {
                    Ok(JdwpValue::Object(ObjectRef {
                        id,
                        runtime_type: "java.lang.Object".to_string(),
                    }))
                }
            }
            _other => {
                // JDWP defines additional object-like tags beyond `L`/`[`/`s` (e.g. thread
                // references). For the synchronous façade we treat any unknown tag as an
                // object reference so higher-level code still receives a usable object id.
                let id = cursor.read_id(self.id_sizes.object_id)?;
                if id == 0 {
                    Ok(JdwpValue::Null)
                } else {
                    Ok(JdwpValue::Object(ObjectRef {
                        id,
                        runtime_type: "java.lang.Object".to_string(),
                    }))
                }
            }
        }
    }

    fn line_table_for_method(
        &mut self,
        type_id: u64,
        method_id: u64,
    ) -> Result<&Vec<(u64, u32)>, JdwpError> {
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

    fn variable_table_for_method(
        &mut self,
        type_id: u64,
        method_id: u64,
    ) -> Result<&[VariableInfo], JdwpError> {
        if !self
            .cache
            .variable_tables
            .contains_key(&(type_id, method_id))
        {
            let mut body = Vec::new();
            write_id(&mut body, self.id_sizes.reference_type_id, type_id);
            write_id(&mut body, self.id_sizes.method_id, method_id);
            let reply = self.send_command(6, 2, &body)?;
            let mut cursor = Cursor::new(&reply);
            let _arg_count = cursor.read_u32()?;
            let count = cursor.read_u32()? as usize;
            let mut vars = Vec::new();
            for _ in 0..count {
                let code_index = cursor.read_i64()?;
                let name = cursor.read_string()?;
                let signature = cursor.read_string()?;
                let length = cursor.read_u32()? as i64;
                let slot = cursor.read_u32()?;
                vars.push(VariableInfo {
                    code_index,
                    name,
                    signature,
                    length,
                    slot,
                });
            }
            self.cache
                .variable_tables
                .insert((type_id, method_id), vars);
        }

        Ok(self
            .cache
            .variable_tables
            .get(&(type_id, method_id))
            .map(Vec::as_slice)
            .expect("just inserted"))
    }

    fn stack_frame_get_values(
        &mut self,
        thread_id: ThreadId,
        frame_id: FrameId,
        slots: &[(u32, &str)],
    ) -> Result<Vec<JdwpValue>, JdwpError> {
        let mut body = Vec::new();
        write_id(&mut body, self.id_sizes.object_id, thread_id);
        write_id(&mut body, self.id_sizes.frame_id, frame_id);
        body.extend_from_slice(&(slots.len() as u32).to_be_bytes());
        for (slot, signature) in slots {
            body.extend_from_slice(&slot.to_be_bytes());
            body.push(signature_to_tag(signature));
        }

        let reply = self.send_command(16, 1, &body)?;
        let mut cursor = Cursor::new(&reply);
        let count = cursor.read_u32()? as usize;
        let mut values = Vec::new();
        for _ in 0..count {
            values.push(self.read_tagged_value(&mut cursor)?);
        }
        Ok(values)
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
        body.extend_from_slice(&location.index.to_be_bytes());

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
                let dist = entry_line.abs_diff(line);
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

    fn handle_command_packet(
        &mut self,
        command_set: u8,
        command: u8,
        data: &[u8],
    ) -> Result<(), JdwpError> {
        // Event.Composite
        if command_set != 64 || command != 100 {
            return Ok(());
        }

        let mut cursor = Cursor::new(data);
        let _suspend_policy = cursor.read_u8()?;
        let events = cursor.read_u32()? as usize;

        let mut stopped_events = Vec::new();
        for _ in 0..events {
            let kind = cursor.read_u8()?;
            let request_id = cursor.read_u32()?;

            match kind {
                EVENT_KIND_STEP => {
                    // Step
                    let thread_id = cursor.read_id(self.id_sizes.object_id)?;
                    // location
                    let _type_tag = cursor.read_u8()?;
                    let _type_id = cursor.read_id(self.id_sizes.reference_type_id)?;
                    let _method_id = cursor.read_id(self.id_sizes.method_id)?;
                    let _index = cursor.read_i64()?;

                    stopped_events.push((StopReason::Step, thread_id, request_id));
                }
                EVENT_KIND_BREAKPOINT => {
                    // Breakpoint
                    let thread_id = cursor.read_id(self.id_sizes.object_id)?;
                    // location
                    let _type_tag = cursor.read_u8()?;
                    let _type_id = cursor.read_id(self.id_sizes.reference_type_id)?;
                    let _method_id = cursor.read_id(self.id_sizes.method_id)?;
                    let _index = cursor.read_i64()?;

                    stopped_events.push((StopReason::Breakpoint, thread_id, request_id));
                }
                EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE => {
                    // MethodExitWithReturnValue
                    let thread_id = cursor.read_id(self.id_sizes.object_id)?;
                    // location
                    let _type_tag = cursor.read_u8()?;
                    let _type_id = cursor.read_id(self.id_sizes.reference_type_id)?;
                    let _method_id = cursor.read_id(self.id_sizes.method_id)?;
                    let _index = cursor.read_i64()?;

                    let value = self.read_tagged_value(&mut cursor)?;
                    self.pending_return_values.insert(thread_id, value);
                }
                other => {
                    return Err(JdwpError::Protocol(format!(
                        "unsupported JDWP event kind {other}"
                    )));
                }
            }
        }

        for (reason, thread_id, request_id) in stopped_events {
            let captured = self.pending_return_values.remove(&thread_id);
            let (return_value, expression_value) = match captured {
                None => (None, None),
                Some(value) => match self
                    .active_step_requests
                    .get(&thread_id)
                    .map(|req| req.kind)
                {
                    Some(StepKind::Out) => (Some(value), None),
                    Some(StepKind::Over) | Some(StepKind::Into) => (None, Some(value)),
                    None => (Some(value), None),
                },
            };
            self.pending_events
                .push_back(JdwpEvent::Stopped(StoppedEvent {
                    reason,
                    thread_id,
                    request_id,
                    return_value,
                    expression_value,
                }));
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
        let mut unique = BTreeSet::new();
        for addr in (host, port).to_socket_addrs()? {
            unique.insert(addr);
        }

        if unique.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid JDWP address").into());
        }

        // `SocketAddr` orders IPv4 before IPv6, so `localhost` prefers `127.0.0.1` over `::1`.
        // Still try all candidates to avoid getting stuck on an address family the debuggee isn't
        // listening on.
        let addrs: Vec<SocketAddr> = unique.into_iter().collect();

        let mut last_err: Option<JdwpError> = None;
        for addr in addrs {
            let stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
                Ok(stream) => stream,
                Err(err) => {
                    last_err = Some(err.into());
                    continue;
                }
            };

            if let Err(err) = stream.set_read_timeout(Some(Duration::from_secs(3))) {
                last_err = Some(err.into());
                continue;
            }
            if let Err(err) = stream.set_write_timeout(Some(Duration::from_secs(3))) {
                last_err = Some(err.into());
                continue;
            }

            if let Err(err) = Self::perform_handshake(&mut stream.try_clone()?) {
                last_err = Some(err);
                continue;
            }

            self.stream = Some(stream);
            match self.id_sizes() {
                Ok(id_sizes) => {
                    self.id_sizes = id_sizes;
                    return Ok(());
                }
                Err(err) => {
                    // Best-effort: try other resolved addresses before giving up.
                    self.stream = None;
                    last_err = Some(err);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid JDWP address").into()
        }))
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

        let mut threads = Vec::new();
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

        let mut frames = Vec::new();
        for _ in 0..count {
            let frame_id = cursor.read_id(self.id_sizes.frame_id)?;
            let _type_tag = cursor.read_u8()?;
            let type_id = cursor.read_id(self.id_sizes.reference_type_id)?;
            let method_id = cursor.read_id(self.id_sizes.method_id)?;
            let index = cursor.read_i64()?;

            self.frame_contexts.insert(
                frame_id,
                FrameContext {
                    thread_id,
                    type_id,
                    method_id,
                    code_index: index,
                },
            );

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
        if let Some(prev) = self.active_step_requests.remove(&thread_id) {
            let _ = self.clear_event_request(EVENT_KIND_STEP, prev.request_id);
        }
        if let Some(prev) = self.active_method_exit_requests.remove(&thread_id) {
            let _ = self.clear_event_request(EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE, prev);
        }
        self.pending_return_values.remove(&thread_id);

        // Best-effort: if METHOD_EXIT_WITH_RETURN_VALUE isn't supported, still step.
        if let Ok(request_id) = self.set_method_exit_with_return_value_request(thread_id) {
            self.active_method_exit_requests
                .insert(thread_id, request_id);
        }

        let request_id = self.set_step_request(thread_id, 1)?; // StepDepth.OVER
        self.active_step_requests.insert(
            thread_id,
            ActiveStepRequest {
                request_id,
                kind: StepKind::Over,
            },
        );
        self.resume_vm()
    }

    fn step_in(&mut self, thread_id: ThreadId) -> Result<(), JdwpError> {
        if let Some(prev) = self.active_step_requests.remove(&thread_id) {
            let _ = self.clear_event_request(EVENT_KIND_STEP, prev.request_id);
        }
        if let Some(prev) = self.active_method_exit_requests.remove(&thread_id) {
            let _ = self.clear_event_request(EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE, prev);
        }
        self.pending_return_values.remove(&thread_id);

        if let Ok(request_id) = self.set_method_exit_with_return_value_request(thread_id) {
            self.active_method_exit_requests
                .insert(thread_id, request_id);
        }

        let request_id = self.set_step_request(thread_id, 0)?; // StepDepth.INTO
        self.active_step_requests.insert(
            thread_id,
            ActiveStepRequest {
                request_id,
                kind: StepKind::Into,
            },
        );
        self.resume_vm()
    }

    fn step_out(&mut self, thread_id: ThreadId) -> Result<(), JdwpError> {
        if let Some(prev) = self.active_step_requests.remove(&thread_id) {
            let _ = self.clear_event_request(EVENT_KIND_STEP, prev.request_id);
        }
        if let Some(prev) = self.active_method_exit_requests.remove(&thread_id) {
            let _ = self.clear_event_request(EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE, prev);
        }
        self.pending_return_values.remove(&thread_id);

        if let Ok(request_id) = self.set_method_exit_with_return_value_request(thread_id) {
            self.active_method_exit_requests
                .insert(thread_id, request_id);
        }

        let request_id = self.set_step_request(thread_id, 2)?; // StepDepth.OUT
        self.active_step_requests.insert(
            thread_id,
            ActiveStepRequest {
                request_id,
                kind: StepKind::Out,
            },
        );
        self.resume_vm()
    }

    fn pause(&mut self, _thread_id: ThreadId) -> Result<(), JdwpError> {
        let _ = self.send_command(1, 8, &[])?;
        Ok(())
    }

    fn evaluate(&mut self, expression: &str, frame_id: FrameId) -> Result<JdwpValue, JdwpError> {
        let expression = expression.trim();
        if !is_java_identifier(expression) {
            return Err(JdwpError::Other(format!(
                "unsupported expression: {expression}"
            )));
        }

        let context = self
            .frame_contexts
            .get(&frame_id)
            .copied()
            .ok_or_else(|| JdwpError::Other("unknown frameId".to_string()))?;

        let (slot, signature) = {
            let vars = self.variable_table_for_method(context.type_id, context.method_id)?;
            let var = vars
                .iter()
                .filter(|var| {
                    if var.name != expression {
                        return false;
                    }
                    let Some(end) = var.code_index.checked_add(var.length) else {
                        return false;
                    };
                    var.code_index <= context.code_index && context.code_index < end
                })
                // If the same identifier is present in multiple scopes, prefer the one
                // that starts latest (inner-most scope).
                .max_by_key(|var| var.code_index)
                .ok_or_else(|| JdwpError::Other(format!("not found: {expression}")))?;

            (var.slot, var.signature.clone())
        };

        let mut values = self.stack_frame_get_values(
            context.thread_id,
            frame_id,
            &[(slot, signature.as_str())],
        )?;

        let value = values
            .pop()
            .ok_or_else(|| JdwpError::Protocol("missing StackFrame.GetValues reply".to_string()))?;

        Ok(match value {
            JdwpValue::Object(mut obj) => {
                if let Ok((_tag, type_id)) = self.reference_type_for_object(obj.id) {
                    if let Ok(signature) = self.signature_for_type(type_id) {
                        obj.runtime_type = signature_to_type_name(signature);
                    }
                }
                JdwpValue::Object(obj)
            }
            other => other,
        })
    }

    fn preview_object(&mut self, object_id: ObjectId) -> Result<ObjectPreview, JdwpError> {
        let (_tag, type_id) = self.reference_type_for_object(object_id)?;
        let signature = self.signature_for_type(type_id)?.to_string();
        let runtime_type = signature_to_type_name(&signature);

        if signature == "Ljava/lang/String;" {
            return Ok(ObjectPreview {
                runtime_type,
                kind: ObjectKindPreview::String {
                    value: self.string_value(object_id)?,
                },
            });
        }

        if signature.starts_with('[') {
            let length = self.array_length(object_id)?;
            let sample_len = length.min(ARRAY_PREVIEW_SAMPLE);
            let sample = if sample_len == 0 {
                Vec::new()
            } else {
                self.array_get_values(object_id, 0, sample_len as i32)?
            };

            let element_sig = signature.strip_prefix('[').unwrap_or(&signature);
            let element_type = signature_to_type_name(element_sig);
            return Ok(ObjectPreview {
                runtime_type,
                kind: ObjectKindPreview::Array {
                    element_type,
                    length,
                    sample,
                },
            });
        }

        // Primitive wrapper previews (Integer, Long, etc.) by reading their `value` field.
        if matches!(
            runtime_type.as_str(),
            "java.lang.Boolean"
                | "java.lang.Byte"
                | "java.lang.Character"
                | "java.lang.Double"
                | "java.lang.Float"
                | "java.lang.Integer"
                | "java.lang.Long"
                | "java.lang.Short"
        ) {
            if let Ok(children) = self.object_children(object_id) {
                if let Some(value) = children
                    .iter()
                    .find(|v| v.name == "value")
                    .map(|v| v.value.clone())
                {
                    return Ok(ObjectPreview {
                        runtime_type,
                        kind: ObjectKindPreview::PrimitiveWrapper {
                            value: Box::new(value),
                        },
                    });
                }
            }
        }

        // Optional preview by reading its `value` field.
        if runtime_type == "java.util.Optional" {
            if let Ok(children) = self.object_children(object_id) {
                if let Some(value) = children
                    .iter()
                    .find(|v| v.name == "value")
                    .map(|v| v.value.clone())
                {
                    return Ok(ObjectPreview {
                        runtime_type,
                        kind: ObjectKindPreview::Optional {
                            value: match value {
                                JdwpValue::Null => None,
                                other => Some(Box::new(other)),
                            },
                        },
                    });
                }
            }
        }

        // Collection previews via best-effort field introspection for common JDK implementations.
        if runtime_type == "java.util.ArrayList" {
            if let Ok(children) = self.object_children(object_id) {
                let size = children.iter().find_map(|v| match (&v.name[..], &v.value) {
                    ("size", JdwpValue::Int(size)) => Some(*size as usize),
                    _ => None,
                });
                let element_data = children.iter().find_map(|v| match (&v.name[..], &v.value) {
                    ("elementData", JdwpValue::Object(obj)) => Some(obj.id),
                    _ => None,
                });

                if let (Some(size), Some(array_id)) = (size, element_data) {
                    let sample_len = size.min(ARRAY_PREVIEW_SAMPLE);
                    let sample = if sample_len == 0 {
                        Vec::new()
                    } else {
                        self.array_get_values(array_id, 0, sample_len as i32)
                            .unwrap_or_default()
                    };

                    return Ok(ObjectPreview {
                        runtime_type,
                        kind: ObjectKindPreview::List { size, sample },
                    });
                }
            }
        }

        if runtime_type == "java.util.HashMap" {
            if let Ok(children) = self.object_children(object_id) {
                let size = children.iter().find_map(|v| match (&v.name[..], &v.value) {
                    ("size", JdwpValue::Int(size)) => Some(*size as usize),
                    _ => None,
                });
                let table = children.iter().find_map(|v| match (&v.name[..], &v.value) {
                    ("table", JdwpValue::Object(obj)) => Some(obj.id),
                    _ => None,
                });

                if let (Some(size), Some(table_id)) = (size, table) {
                    let mut sample = Vec::new();
                    if let Ok(table_len) = self.array_length(table_id) {
                        let scan = table_len.min(64);
                        if scan > 0 {
                            if let Ok(buckets) = self.array_get_values(table_id, 0, scan as i32) {
                                for bucket in buckets {
                                    if sample.len() >= ARRAY_PREVIEW_SAMPLE {
                                        break;
                                    }
                                    let JdwpValue::Object(mut node) = bucket else {
                                        continue;
                                    };
                                    // Traverse collision chain (bounded).
                                    for _ in 0..16 {
                                        if sample.len() >= ARRAY_PREVIEW_SAMPLE {
                                            break;
                                        }
                                        let Ok(node_fields) = self.object_children(node.id) else {
                                            break;
                                        };
                                        let key = node_fields
                                            .iter()
                                            .find(|v| v.name == "key")
                                            .map(|v| v.value.clone())
                                            .unwrap_or(JdwpValue::Null);
                                        let value = node_fields
                                            .iter()
                                            .find(|v| v.name == "value")
                                            .map(|v| v.value.clone())
                                            .unwrap_or(JdwpValue::Null);
                                        sample.push((key, value));
                                        match node_fields
                                            .iter()
                                            .find(|v| v.name == "next")
                                            .map(|v| &v.value)
                                        {
                                            Some(JdwpValue::Object(next)) => node = next.clone(),
                                            _ => break,
                                        }
                                    }
                                }
                            }
                        }
                    }

                    return Ok(ObjectPreview {
                        runtime_type,
                        kind: ObjectKindPreview::Map { size, sample },
                    });
                }
            }
        }

        if runtime_type == "java.util.HashSet" {
            if let Ok(children) = self.object_children(object_id) {
                let map = children.iter().find_map(|v| match (&v.name[..], &v.value) {
                    ("map", JdwpValue::Object(obj)) => Some(obj.id),
                    _ => None,
                });

                if let Some(map_id) = map {
                    // Reuse HashMap's preview by pulling the keys out of the sampled entries.
                    if let Ok(ObjectPreview {
                        kind: ObjectKindPreview::Map { size, sample },
                        ..
                    }) = self.preview_object(map_id)
                    {
                        let sample = sample.into_iter().map(|(k, _)| k).collect();
                        return Ok(ObjectPreview {
                            runtime_type,
                            kind: ObjectKindPreview::Set { size, sample },
                        });
                    }
                }
            }
        }

        Ok(ObjectPreview {
            runtime_type,
            kind: ObjectKindPreview::Plain,
        })
    }

    fn object_children(&mut self, object_id: ObjectId) -> Result<Vec<JdwpVariable>, JdwpError> {
        let (_tag, type_id) = self.reference_type_for_object(object_id)?;
        let signature = self.signature_for_type(type_id)?.to_string();

        if signature.starts_with('[') {
            let length = self.array_length(object_id)?;
            let sample_len = length.min(ARRAY_CHILD_SAMPLE);
            let element_sig = signature.strip_prefix('[').unwrap_or(&signature);
            let element_type = signature_to_type_name(element_sig);
            let mut vars = Vec::new();
            vars.push(JdwpVariable {
                name: "length".to_string(),
                value: JdwpValue::Int(length as i32),
                static_type: Some("int".to_string()),
                evaluate_name: None,
            });
            if sample_len > 0 {
                let values = self.array_get_values(object_id, 0, sample_len as i32)?;
                for (idx, value) in values.into_iter().enumerate() {
                    vars.push(JdwpVariable {
                        name: format!("[{idx}]"),
                        value,
                        static_type: Some(element_type.clone()),
                        evaluate_name: None,
                    });
                }
            }
            return Ok(vars);
        }

        let fields: Vec<_> = self
            .fields_for_type(type_id)?
            .iter()
            .filter(|field| field.mod_bits & FIELD_MODIFIER_STATIC == 0)
            .cloned()
            .collect();
        if fields.is_empty() {
            return Ok(Vec::new());
        }

        let values = self.object_get_values(object_id, &fields)?;
        Ok(fields
            .into_iter()
            .zip(values)
            .map(|(field, value)| JdwpVariable {
                name: field.name,
                value,
                static_type: Some(signature_to_type_name(&field.signature)),
                evaluate_name: None,
            })
            .collect())
    }

    fn disable_collection(&mut self, object_id: ObjectId) -> Result<(), JdwpError> {
        let mut body = Vec::new();
        write_id(&mut body, self.id_sizes.object_id, object_id);

        // ObjectReference/DisableCollection
        match self.send_command(9, 7, &body) {
            Ok(_) => Ok(()),
            Err(JdwpError::CommandFailed { error_code }) if error_code == ERROR_INVALID_OBJECT => {
                Err(JdwpError::InvalidObjectId(object_id))
            }
            Err(err) => Err(err),
        }
    }

    fn enable_collection(&mut self, object_id: ObjectId) -> Result<(), JdwpError> {
        let mut body = Vec::new();
        write_id(&mut body, self.id_sizes.object_id, object_id);

        // ObjectReference/EnableCollection
        match self.send_command(9, 8, &body) {
            Ok(_) => Ok(()),
            Err(JdwpError::CommandFailed { error_code }) if error_code == ERROR_INVALID_OBJECT => {
                Err(JdwpError::InvalidObjectId(object_id))
            }
            Err(err) => Err(err),
        }
    }

    fn wait_for_event(&mut self) -> Result<Option<JdwpEvent>, JdwpError> {
        if let Some(event) = self.pending_events.pop_front() {
            let JdwpEvent::Stopped(stopped) = &event;
            if let Some(step_request) = self.active_step_requests.remove(&stopped.thread_id) {
                let _ = self.clear_event_request(EVENT_KIND_STEP, step_request.request_id);
            }
            if let Some(exit_request) = self.active_method_exit_requests.remove(&stopped.thread_id)
            {
                let _ = self
                    .clear_event_request(EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE, exit_request);
                self.pending_return_values.remove(&stopped.thread_id);
            }
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
                        let JdwpEvent::Stopped(stopped) = &event;
                        if let Some(step_request) =
                            self.active_step_requests.remove(&stopped.thread_id)
                        {
                            let _ =
                                self.clear_event_request(EVENT_KIND_STEP, step_request.request_id);
                        }
                        if let Some(exit_request) =
                            self.active_method_exit_requests.remove(&stopped.thread_id)
                        {
                            let _ = self.clear_event_request(
                                EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE,
                                exit_request,
                            );
                            self.pending_return_values.remove(&stopped.thread_id);
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
    #[allow(dead_code)]
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
    signatures: HashMap<u64, String>,
    fields: HashMap<u64, Vec<FieldInfo>>,
    variable_tables: HashMap<(u64, u64), Vec<VariableInfo>>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct MethodInfo {
    id: u64,
    name: String,
    signature: String,
}

#[derive(Debug, Clone)]
struct FieldInfo {
    id: u64,
    name: String,
    signature: String,
    mod_bits: u32,
}

#[derive(Debug, Clone)]
struct VariableInfo {
    code_index: i64,
    name: String,
    signature: String,
    length: i64,
    slot: u32,
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
    crate::validate_jdwp_packet_length(length).map_err(JdwpError::Protocol)?;

    // Read the fixed header remainder after the length field:
    //   u32 id, u8 flags, u16 error_code | (u8 command_set, u8 command)
    let mut header = [0u8; 7];
    reader.read_exact(&mut header)?;

    let id = u32::from_be_bytes(header[0..4].try_into().unwrap());
    let flags = header[4];

    let payload_len = length - crate::JDWP_HEADER_LEN;
    let mut payload = Vec::new();
    payload.try_reserve_exact(payload_len).map_err(|_| {
        JdwpError::Protocol(format!(
            "unable to allocate packet buffer ({payload_len} bytes)"
        ))
    })?;
    payload.resize(payload_len, 0);
    reader.read_exact(&mut payload)?;

    if flags & 0x80 != 0 {
        let error_code = u16::from_be_bytes(header[5..7].try_into().unwrap());
        Ok(Packet::Reply {
            id,
            error_code,
            data: payload,
        })
    } else {
        Ok(Packet::Command {
            id,
            command_set: header[5],
            command: header[6],
            data: payload,
        })
    }
}

fn class_name_to_signature(class: &str) -> String {
    let internal = class.replace('.', "/");
    format!("L{internal};")
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

fn signature_to_tag(signature: &str) -> u8 {
    signature.as_bytes().first().copied().unwrap_or(b'V')
}

fn is_java_identifier(expression: &str) -> bool {
    fn is_start(c: char) -> bool {
        c == '_' || c == '$' || c.is_ascii_alphabetic()
    }

    fn is_part(c: char) -> bool {
        is_start(c) || c.is_ascii_digit()
    }

    let mut chars = expression.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_start(first) && chars.all(is_part)
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
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| JdwpError::Protocol("unexpected end of packet".to_string()))?;
        if end > self.buf.len() {
            return Err(JdwpError::Protocol("unexpected end of packet".to_string()));
        }
        let slice = &self.buf[self.pos..end];
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

    fn read_u16(&mut self) -> Result<u16, JdwpError> {
        let bytes = self.read_exact(2)?;
        Ok(u16::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_i16(&mut self) -> Result<i16, JdwpError> {
        Ok(self.read_u16()? as i16)
    }

    fn read_i32(&mut self) -> Result<i32, JdwpError> {
        let bytes = self.read_exact(4)?;
        Ok(i32::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_i64(&mut self) -> Result<i64, JdwpError> {
        let bytes = self.read_exact(8)?;
        Ok(i64::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_f32(&mut self) -> Result<f32, JdwpError> {
        let bits = self.read_u32()?;
        Ok(f32::from_bits(bits))
    }

    fn read_f64(&mut self) -> Result<f64, JdwpError> {
        let bytes = self.read_exact(8)?;
        Ok(f64::from_bits(u64::from_be_bytes(
            bytes.try_into().unwrap(),
        )))
    }

    fn read_java_char(&mut self) -> Result<char, JdwpError> {
        let code_unit = self.read_u16()? as u32;
        Ok(std::char::from_u32(code_unit).unwrap_or('\u{FFFD}'))
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
        let mut out = Vec::new();
        out.try_reserve_exact(len).map_err(|_| {
            JdwpError::Protocol(format!("unable to allocate string buffer ({len} bytes)"))
        })?;
        out.extend_from_slice(bytes);
        Ok(String::from_utf8(out)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::mock::MockJdwpServer;
    use std::net::TcpListener;
    use std::thread;

    #[tokio::test]
    async fn tcp_jdwp_client_connect_accepts_hostname_localhost() {
        let jdwp = MockJdwpServer::spawn().await.unwrap();
        let port = jdwp.addr().port();

        tokio::task::spawn_blocking(move || {
            let mut client = TcpJdwpClient::new();
            client.connect("localhost", port).unwrap();
        })
        .await
        .unwrap();
    }

    #[test]
    fn class_signature_conversion() {
        assert_eq!(
            class_name_to_signature("com.example.Foo"),
            "Lcom/example/Foo;"
        );
        assert_eq!(class_name_to_signature("Foo"), "LFoo;");
    }

    #[test]
    fn method_exit_return_value_is_attached_to_next_step_stop() {
        let mut client = TcpJdwpClient::new();

        // Composite event packet with:
        //  - Step
        //  - MethodExitWithReturnValue(int 123)
        //
        // Order is intentionally "step first" to ensure we attach return values
        // after reading all events in the composite.
        let thread_id = 99u64;
        let type_id = 1u64;
        let method_id = 2u64;
        let index = 0i64;

        let mut data = Vec::new();
        data.push(0); // SuspendPolicy.NONE
        data.extend_from_slice(&(2u32).to_be_bytes()); // events

        // Step event.
        data.push(EVENT_KIND_STEP);
        data.extend_from_slice(&(11u32).to_be_bytes()); // request id
        write_id(&mut data, client.id_sizes.object_id, thread_id);
        data.push(1); // type tag
        write_id(&mut data, client.id_sizes.reference_type_id, type_id);
        write_id(&mut data, client.id_sizes.method_id, method_id);
        data.extend_from_slice(&index.to_be_bytes());

        // MethodExitWithReturnValue event.
        data.push(EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE);
        data.extend_from_slice(&(12u32).to_be_bytes()); // request id
        write_id(&mut data, client.id_sizes.object_id, thread_id);
        data.push(1); // type tag
        write_id(&mut data, client.id_sizes.reference_type_id, type_id);
        write_id(&mut data, client.id_sizes.method_id, method_id);
        data.extend_from_slice(&index.to_be_bytes());
        data.push(b'I');
        data.extend_from_slice(&(123i32).to_be_bytes());

        client.handle_command_packet(64, 100, &data).unwrap();

        let event = client.pending_events.pop_front().unwrap();
        let JdwpEvent::Stopped(stopped) = event;
        assert_eq!(stopped.reason, StopReason::Step);
        assert_eq!(stopped.return_value, Some(JdwpValue::Int(123)));
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

    #[test]
    fn read_packet_rejects_oversized_length_prefix_without_reading_rest() {
        let length = (crate::MAX_JDWP_PACKET_BYTES + 1) as u32;
        let mut cursor = std::io::Cursor::new(length.to_be_bytes());

        let err = read_packet(&mut cursor).unwrap_err();
        match err {
            JdwpError::Protocol(msg) => {
                assert_eq!(
                    msg,
                    format!(
                        "JDWP packet length {} exceeds maximum allowed ({} bytes); refusing to allocate",
                        crate::MAX_JDWP_PACKET_BYTES + 1,
                        crate::MAX_JDWP_PACKET_BYTES
                    )
                );
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn read_packet_rejects_invalid_length_prefix_without_reading_rest() {
        let length = (crate::JDWP_HEADER_LEN - 1) as u32;
        let mut cursor = std::io::Cursor::new(length.to_be_bytes());

        let err = read_packet(&mut cursor).unwrap_err();
        match err {
            JdwpError::Protocol(msg) => {
                assert_eq!(
                    msg,
                    format!("invalid packet length {}", crate::JDWP_HEADER_LEN - 1)
                );
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_supports_identifier_locals() {
        const THREAD_ID: u64 = 0x1001;
        const FRAME_ID: u64 = 0x2001;
        const CLASS_ID: u64 = 0x3001;
        const METHOD_ID: u64 = 0x4001;
        const OBJECT_ID: u64 = 0x5001;
        const OBJECT_CLASS_ID: u64 = 0x6001;

        fn write_reply(stream: &mut TcpStream, id: u32, error_code: u16, payload: &[u8]) {
            let length = 11usize + payload.len();
            let mut header = [0u8; crate::JDWP_HEADER_LEN];
            header[0..4].copy_from_slice(&(length as u32).to_be_bytes());
            header[4..8].copy_from_slice(&id.to_be_bytes());
            header[8] = 0x80;
            header[9..11].copy_from_slice(&error_code.to_be_bytes());
            stream.write_all(&header).unwrap();
            stream.write_all(payload).unwrap();
            stream.flush().unwrap();
        }

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();

            // Handshake: debugger -> "JDWP-Handshake", server echoes back.
            const HANDSHAKE: &[u8] = b"JDWP-Handshake";
            let mut hs = [0u8; HANDSHAKE.len()];
            stream.read_exact(&mut hs).unwrap();
            assert_eq!(&hs, HANDSHAKE);
            stream.write_all(HANDSHAKE).unwrap();
            stream.flush().unwrap();

            loop {
                let packet = match read_packet(&mut stream) {
                    Ok(packet) => packet,
                    Err(JdwpError::Io(err))
                        if matches!(
                            err.kind(),
                            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                        ) =>
                    {
                        break;
                    }
                    Err(err) => panic!("unexpected server read error: {err:?}"),
                };

                let Packet::Command {
                    id,
                    command_set,
                    command,
                    data,
                } = packet
                else {
                    panic!("unexpected reply packet from client");
                };

                let mut reply = Vec::new();
                match (command_set, command) {
                    // VirtualMachine.IDSizes
                    (1, 7) => {
                        reply.extend_from_slice(&8u32.to_be_bytes()); // field id size
                        reply.extend_from_slice(&8u32.to_be_bytes()); // method id size
                        reply.extend_from_slice(&8u32.to_be_bytes()); // object id size
                        reply.extend_from_slice(&8u32.to_be_bytes()); // reference type id size
                        reply.extend_from_slice(&8u32.to_be_bytes()); // frame id size
                    }
                    // ThreadReference.Frames
                    (11, 6) => {
                        let mut cursor = Cursor::new(&data);
                        let _thread_id = cursor.read_id(8).unwrap();
                        let _start = cursor.read_i32().unwrap();
                        let _length = cursor.read_i32().unwrap();

                        reply.extend_from_slice(&1u32.to_be_bytes());
                        write_id(&mut reply, 8, FRAME_ID);
                        reply.push(1); // type tag
                        write_id(&mut reply, 8, CLASS_ID);
                        write_id(&mut reply, 8, METHOD_ID);
                        reply.extend_from_slice(&0i64.to_be_bytes()); // code index
                    }
                    // ReferenceType.Methods
                    (2, 5) => {
                        let mut cursor = Cursor::new(&data);
                        let _type_id = cursor.read_id(8).unwrap();

                        reply.extend_from_slice(&1u32.to_be_bytes());
                        write_id(&mut reply, 8, METHOD_ID);
                        write_string(&mut reply, "main");
                        write_string(&mut reply, "()V");
                        reply.extend_from_slice(&0u32.to_be_bytes());
                    }
                    // ReferenceType.SourceFile
                    (2, 7) => {
                        let mut cursor = Cursor::new(&data);
                        let _type_id = cursor.read_id(8).unwrap();
                        write_string(&mut reply, "Main.java");
                    }
                    // Method.LineTable
                    (6, 1) => {
                        let mut cursor = Cursor::new(&data);
                        let _type_id = cursor.read_id(8).unwrap();
                        let _method_id = cursor.read_id(8).unwrap();
                        reply.extend_from_slice(&0i64.to_be_bytes());
                        reply.extend_from_slice(&10i64.to_be_bytes());
                        reply.extend_from_slice(&1u32.to_be_bytes());
                        reply.extend_from_slice(&0i64.to_be_bytes()); // code index
                        reply.extend_from_slice(&3u32.to_be_bytes()); // line
                    }
                    // Method.VariableTable
                    (6, 2) => {
                        let mut cursor = Cursor::new(&data);
                        let _type_id = cursor.read_id(8).unwrap();
                        let _method_id = cursor.read_id(8).unwrap();

                        reply.extend_from_slice(&0u32.to_be_bytes()); // arg count
                        reply.extend_from_slice(&2u32.to_be_bytes()); // slots

                        // int x (slot 0)
                        reply.extend_from_slice(&0i64.to_be_bytes());
                        write_string(&mut reply, "x");
                        write_string(&mut reply, "I");
                        reply.extend_from_slice(&10u32.to_be_bytes());
                        reply.extend_from_slice(&0u32.to_be_bytes());

                        // String obj (slot 1)
                        reply.extend_from_slice(&0i64.to_be_bytes());
                        write_string(&mut reply, "obj");
                        write_string(&mut reply, "Ljava/lang/String;");
                        reply.extend_from_slice(&10u32.to_be_bytes());
                        reply.extend_from_slice(&1u32.to_be_bytes());
                    }
                    // StackFrame.GetValues
                    (16, 1) => {
                        let mut cursor = Cursor::new(&data);
                        let _thread_id = cursor.read_id(8).unwrap();
                        let _frame_id = cursor.read_id(8).unwrap();
                        let count = cursor.read_u32().unwrap() as usize;
                        let mut requests = Vec::new();
                        for _ in 0..count {
                            let slot = cursor.read_u32().unwrap();
                            let tag = cursor.read_u8().unwrap();
                            requests.push((slot, tag));
                        }

                        reply.extend_from_slice(&(requests.len() as u32).to_be_bytes());
                        for (slot, tag) in requests {
                            match (slot, tag) {
                                (0, b'I') => {
                                    reply.push(b'I');
                                    reply.extend_from_slice(&42i32.to_be_bytes());
                                }
                                (1, _) => {
                                    reply.push(b'L');
                                    write_id(&mut reply, 8, OBJECT_ID);
                                }
                                _ => {
                                    reply.push(b'V');
                                }
                            }
                        }
                    }
                    // ObjectReference.ReferenceType
                    (9, 1) => {
                        let mut cursor = Cursor::new(&data);
                        let _object_id = cursor.read_id(8).unwrap();
                        reply.push(1); // ref type tag
                        write_id(&mut reply, 8, OBJECT_CLASS_ID);
                    }
                    // ReferenceType.Signature
                    (2, 1) => {
                        let mut cursor = Cursor::new(&data);
                        let type_id = cursor.read_id(8).unwrap();
                        if type_id == OBJECT_CLASS_ID {
                            write_string(&mut reply, "Ljava/lang/String;");
                        } else {
                            write_string(&mut reply, "LMain;");
                        }
                    }
                    _ => {
                        write_reply(&mut stream, id, 1, &[]);
                        continue;
                    }
                }

                write_reply(&mut stream, id, 0, &reply);
            }
        });

        let mut client = TcpJdwpClient::new();
        client.connect("127.0.0.1", port).unwrap();

        let frames = client.stack_frames(THREAD_ID).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, FRAME_ID);

        let context = client.frame_contexts.get(&FRAME_ID).copied().unwrap();
        assert_eq!(context.thread_id, THREAD_ID);
        assert_eq!(context.type_id, CLASS_ID);
        assert_eq!(context.method_id, METHOD_ID);
        assert_eq!(context.code_index, 0);

        assert_eq!(client.evaluate("x", FRAME_ID).unwrap(), JdwpValue::Int(42));

        match client.evaluate("obj", FRAME_ID).unwrap() {
            JdwpValue::Object(obj) => {
                assert_eq!(obj.id, OBJECT_ID);
                assert_eq!(obj.runtime_type, "java.lang.String");
            }
            other => panic!("expected object ref, got {other:?}"),
        }

        let err = client.evaluate("x+1", FRAME_ID).unwrap_err();
        assert!(matches!(err, JdwpError::Other(msg) if msg.contains("unsupported expression")));

        drop(client);
        server.join().unwrap();
    }
}

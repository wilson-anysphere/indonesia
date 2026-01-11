use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{broadcast, oneshot, Mutex},
};
use tokio_util::sync::CancellationToken;

use super::{
    codec::{
        encode_command, signature_to_tag, JdwpReader, JdwpWriter, FLAG_REPLY, HANDSHAKE, HEADER_LEN,
    },
    types::{
        ClassInfo, FieldInfo, FrameId, FrameInfo, JdwpError, JdwpEvent, JdwpIdSizes, JdwpValue,
        LineTable, LineTableEntry, Location, MethodId, MethodInfo, ObjectId, ReferenceTypeId,
        Result, ThreadId, VariableInfo,
    },
};

#[derive(Debug, Clone)]
pub struct JdwpClientConfig {
    pub handshake_timeout: Duration,
    pub reply_timeout: Duration,
    pub pending_channel_size: usize,
    pub event_channel_size: usize,
}

impl Default for JdwpClientConfig {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(5),
            reply_timeout: Duration::from_secs(10),
            pending_channel_size: 256,
            event_channel_size: 64,
        }
    }
}

#[derive(Debug)]
struct Reply {
    error_code: u16,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct Inner {
    writer: Mutex<tokio::net::tcp::OwnedWriteHalf>,
    pending: Mutex<HashMap<u32, oneshot::Sender<std::result::Result<Reply, JdwpError>>>>,
    next_id: AtomicU32,
    id_sizes: Mutex<JdwpIdSizes>,
    events: broadcast::Sender<JdwpEvent>,
    shutdown: CancellationToken,
    config: JdwpClientConfig,
}

#[derive(Clone)]
pub struct JdwpClient {
    inner: Arc<Inner>,
}

impl JdwpClient {
    pub async fn connect(addr: SocketAddr) -> Result<Self> {
        Self::connect_with_config(addr, JdwpClientConfig::default()).await
    }

    pub async fn connect_with_config(addr: SocketAddr, config: JdwpClientConfig) -> Result<Self> {
        let mut stream = TcpStream::connect(addr).await?;
        let _ = stream.set_nodelay(true);

        tokio::time::timeout(config.handshake_timeout, stream.write_all(HANDSHAKE))
            .await
            .map_err(|_| JdwpError::Timeout)??;

        let mut handshake = [0u8; HANDSHAKE.len()];
        tokio::time::timeout(config.handshake_timeout, stream.read_exact(&mut handshake))
            .await
            .map_err(|_| JdwpError::Timeout)??;

        if handshake != *HANDSHAKE {
            return Err(JdwpError::Protocol(format!(
                "invalid handshake reply: {:?}",
                String::from_utf8_lossy(&handshake)
            )));
        }

        let (reader, writer) = stream.into_split();
        let (events, _) = broadcast::channel(config.event_channel_size);

        let inner = Arc::new(Inner {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::with_capacity(config.pending_channel_size)),
            next_id: AtomicU32::new(1),
            id_sizes: Mutex::new(JdwpIdSizes::default()),
            events,
            shutdown: CancellationToken::new(),
            config,
        });

        tokio::spawn(read_loop(reader, inner.clone()));

        let client = Self { inner };
        // ID sizes are required for correct parsing of most replies/events.
        let _ = client.idsizes().await?;
        // Capabilities are not strictly required but help feature detection.
        let _ = client.capabilities_new().await?;

        Ok(client)
    }

    pub fn shutdown(&self) {
        self.inner.shutdown.cancel();
    }

    /// A token that is cancelled when the JDWP client is shut down, either
    /// explicitly via [`JdwpClient::shutdown`] or implicitly when the underlying
    /// TCP connection closes.
    ///
    /// This is useful for higher-level adapters (e.g. DAP) that need to exit
    /// cleanly when the debuggee disconnects.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.inner.shutdown.clone()
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<JdwpEvent> {
        self.inner.events.subscribe()
    }

    async fn send_command_raw(
        &self,
        command_set: u8,
        command: u8,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.inner.pending.lock().await;
            pending.insert(id, tx);
        }

        let packet = encode_command(id, command_set, command, &payload);
        {
            let mut writer = self.inner.writer.lock().await;
            writer.write_all(&packet).await?;
        }

        let reply = tokio::select! {
            _ = self.inner.shutdown.cancelled() => {
                self.remove_pending(id).await;
                return Err(JdwpError::Cancelled);
            }
            res = tokio::time::timeout(self.inner.config.reply_timeout, rx) => {
                match res {
                    Ok(Ok(r)) => r,
                    Ok(Err(_closed)) => return Err(JdwpError::ConnectionClosed),
                    Err(_elapsed) => {
                        self.remove_pending(id).await;
                        return Err(JdwpError::Timeout);
                    }
                }
            }
        }?;

        if reply.error_code != 0 {
            return Err(JdwpError::VmError(reply.error_code));
        }

        Ok(reply.payload)
    }

    async fn remove_pending(&self, id: u32) {
        let mut pending = self.inner.pending.lock().await;
        pending.remove(&id);
    }

    async fn id_sizes(&self) -> JdwpIdSizes {
        *self.inner.id_sizes.lock().await
    }

    async fn set_id_sizes(&self, sizes: JdwpIdSizes) {
        *self.inner.id_sizes.lock().await = sizes;
    }

    pub async fn idsizes(&self) -> Result<JdwpIdSizes> {
        let payload = self.send_command_raw(1, 7, Vec::new()).await?;
        let mut r = JdwpReader::new(&payload);
        let sizes = JdwpIdSizes {
            field_id: r.read_u32()? as usize,
            method_id: r.read_u32()? as usize,
            object_id: r.read_u32()? as usize,
            reference_type_id: r.read_u32()? as usize,
            frame_id: r.read_u32()? as usize,
        };
        self.set_id_sizes(sizes).await;
        Ok(sizes)
    }

    /// VirtualMachine.CapabilitiesNew (1, 17)
    ///
    /// Returns the raw set of capability booleans in the order defined by the JDWP spec.
    /// The client currently uses this for feature detection but does not expose a stable
    /// typed struct yet (this project is still early).
    pub async fn capabilities_new(&self) -> Result<Vec<bool>> {
        let payload = self.send_command_raw(1, 17, Vec::new()).await?;
        let mut r = JdwpReader::new(&payload);
        let mut caps = Vec::with_capacity(r.remaining());
        while r.remaining() > 0 {
            caps.push(r.read_bool()?);
        }
        Ok(caps)
    }

    pub async fn all_threads(&self) -> Result<Vec<ThreadId>> {
        let payload = self.send_command_raw(1, 4, Vec::new()).await?;
        let sizes = self.id_sizes().await;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut threads = Vec::with_capacity(count);
        for _ in 0..count {
            threads.push(r.read_object_id(&sizes)?);
        }
        Ok(threads)
    }

    pub async fn thread_name(&self, thread: ThreadId) -> Result<String> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        let payload = self.send_command_raw(11, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_string()
    }

    pub async fn frames(
        &self,
        thread: ThreadId,
        start: i32,
        length: i32,
    ) -> Result<Vec<FrameInfo>> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        w.write_i32(start);
        w.write_i32(length);
        let payload = self.send_command_raw(11, 6, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut frames = Vec::with_capacity(count);
        for _ in 0..count {
            let frame_id = r.read_id(sizes.frame_id)?;
            let location = r.read_location(&sizes)?;
            frames.push(FrameInfo { frame_id, location });
        }
        Ok(frames)
    }

    pub async fn all_classes(&self) -> Result<Vec<ClassInfo>> {
        let payload = self.send_command_raw(1, 3, Vec::new()).await?;
        let sizes = self.id_sizes().await;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut classes = Vec::with_capacity(count);
        for _ in 0..count {
            classes.push(ClassInfo {
                ref_type_tag: r.read_u8()?,
                type_id: r.read_reference_type_id(&sizes)?,
                signature: r.read_string()?,
                status: r.read_u32()?,
            });
        }
        Ok(classes)
    }

    /// VirtualMachine.ClassesBySignature (1, 2)
    pub async fn classes_by_signature(&self, signature: &str) -> Result<Vec<ClassInfo>> {
        let mut w = JdwpWriter::new();
        w.write_string(signature);
        let payload = self.send_command_raw(1, 2, w.into_vec()).await?;
        let sizes = self.id_sizes().await;
        let mut r = JdwpReader::new(&payload);

        let count = r.read_u32()? as usize;
        let signature = signature.to_string();
        let mut classes = Vec::with_capacity(count);
        for _ in 0..count {
            classes.push(ClassInfo {
                ref_type_tag: r.read_u8()?,
                type_id: r.read_reference_type_id(&sizes)?,
                signature: signature.clone(),
                status: r.read_u32()?,
            });
        }
        Ok(classes)
    }

    /// VirtualMachine.RedefineClasses (1, 18)
    ///
    /// Payload encoding follows the JDWP spec:
    /// - `u32 classCount`
    /// - repeated `referenceTypeId`, `u32 bytecodeLen`, `byte[bytecodeLen]`
    pub async fn redefine_classes(&self, classes: &[(ReferenceTypeId, Vec<u8>)]) -> Result<()> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_u32(classes.len() as u32);
        for (type_id, bytecode) in classes {
            w.write_reference_type_id(*type_id, &sizes);
            w.write_u32(bytecode.len() as u32);
            w.write_bytes(bytecode);
        }
        let _ = self.send_command_raw(1, 18, w.into_vec()).await?;
        Ok(())
    }

    /// Convenience wrapper for `VirtualMachine.RedefineClasses` that resolves a reference type by name.
    ///
    /// `com.example.Foo` is translated to the JDWP signature `Lcom/example/Foo;`.
    /// Returns an error when the class is not currently loaded in the target JVM.
    pub async fn redefine_class_by_name(&self, class_name: &str, bytecode: &[u8]) -> Result<()> {
        let signature = class_name_to_signature(class_name);
        let infos = self.classes_by_signature(&signature).await?;
        if infos.is_empty() {
            return Err(JdwpError::Protocol(format!(
                "class {class_name} is not loaded in target JVM"
            )));
        }

        let classes: Vec<(ReferenceTypeId, Vec<u8>)> = infos
            .into_iter()
            .map(|info| (info.type_id, bytecode.to_vec()))
            .collect();
        self.redefine_classes(&classes).await
    }

    pub async fn reference_type_source_file(&self, class_id: ReferenceTypeId) -> Result<String> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        let payload = self.send_command_raw(2, 7, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_string()
    }

    pub async fn reference_type_signature(&self, class_id: ReferenceTypeId) -> Result<String> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        let payload = self.send_command_raw(2, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_string()
    }

    pub async fn reference_type_methods(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<Vec<MethodInfo>> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        let payload = self.send_command_raw(2, 5, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut methods = Vec::with_capacity(count);
        for _ in 0..count {
            methods.push(MethodInfo {
                method_id: r.read_id(sizes.method_id)?,
                name: r.read_string()?,
                signature: r.read_string()?,
                mod_bits: r.read_u32()?,
            });
        }
        Ok(methods)
    }

    pub async fn method_line_table(
        &self,
        class_id: ReferenceTypeId,
        method_id: MethodId,
    ) -> Result<LineTable> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        w.write_id(method_id, sizes.method_id);
        let payload = self.send_command_raw(6, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let start = r.read_u64()?;
        let end = r.read_u64()?;
        let count = r.read_u32()? as usize;
        let mut lines = Vec::with_capacity(count);
        for _ in 0..count {
            lines.push(LineTableEntry {
                code_index: r.read_u64()?,
                line: r.read_i32()?,
            });
        }
        Ok(LineTable { start, end, lines })
    }

    pub async fn method_variable_table(
        &self,
        class_id: ReferenceTypeId,
        method_id: MethodId,
    ) -> Result<(u32, Vec<VariableInfo>)> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        w.write_id(method_id, sizes.method_id);
        let payload = self.send_command_raw(6, 2, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let arg_count = r.read_u32()?;
        let count = r.read_u32()? as usize;
        let mut vars = Vec::with_capacity(count);
        for _ in 0..count {
            vars.push(VariableInfo {
                code_index: r.read_u64()?,
                name: r.read_string()?,
                signature: r.read_string()?,
                length: r.read_u32()?,
                slot: r.read_u32()?,
            });
        }
        Ok((arg_count, vars))
    }

    pub async fn stack_frame_get_values(
        &self,
        thread: ThreadId,
        frame_id: FrameId,
        slots: &[(u32, String)],
    ) -> Result<Vec<JdwpValue>> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        w.write_id(frame_id, sizes.frame_id);
        w.write_u32(slots.len() as u32);
        for (slot, signature) in slots {
            w.write_u32(*slot);
            w.write_u8(signature_to_tag(signature));
        }
        let payload = self.send_command_raw(16, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            let tag = r.read_u8()?;
            values.push(r.read_value(tag, &sizes)?);
        }
        Ok(values)
    }

    pub async fn object_reference_reference_type(
        &self,
        object_id: ObjectId,
    ) -> Result<ReferenceTypeId> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(object_id, &sizes);
        let payload = self.send_command_raw(9, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        // JDWP spec: ObjectReference.ReferenceType reply starts with a `refTypeTag` byte.
        let _ref_type_tag = r.read_u8()?;
        r.read_reference_type_id(&sizes)
    }

    /// StringReference.Value (10, 1)
    pub async fn string_reference_value(&self, string_id: ObjectId) -> Result<String> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(string_id, &sizes);
        let payload = self.send_command_raw(10, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_string()
    }

    /// ObjectReference.DisableCollection (9, 7)
    pub async fn object_reference_disable_collection(&self, object_id: ObjectId) -> Result<()> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(object_id, &sizes);
        let _ = self.send_command_raw(9, 7, w.into_vec()).await?;
        Ok(())
    }

    /// ObjectReference.EnableCollection (9, 8)
    pub async fn object_reference_enable_collection(&self, object_id: ObjectId) -> Result<()> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(object_id, &sizes);
        let _ = self.send_command_raw(9, 8, w.into_vec()).await?;
        Ok(())
    }

    pub async fn reference_type_fields(&self, class_id: ReferenceTypeId) -> Result<Vec<FieldInfo>> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        let payload = self.send_command_raw(2, 4, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut fields = Vec::with_capacity(count);
        for _ in 0..count {
            fields.push(FieldInfo {
                field_id: r.read_id(sizes.field_id)?,
                name: r.read_string()?,
                signature: r.read_string()?,
                mod_bits: r.read_u32()?,
            });
        }
        Ok(fields)
    }

    pub async fn object_reference_get_values(
        &self,
        object_id: ObjectId,
        field_ids: &[u64],
    ) -> Result<Vec<JdwpValue>> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(object_id, &sizes);
        w.write_u32(field_ids.len() as u32);
        for field_id in field_ids {
            w.write_id(*field_id, sizes.field_id);
        }
        let payload = self.send_command_raw(9, 2, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            let tag = r.read_u8()?;
            values.push(r.read_value(tag, &sizes)?);
        }
        Ok(values)
    }

    pub async fn array_reference_length(&self, array_id: ObjectId) -> Result<i32> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(array_id, &sizes);
        let payload = self.send_command_raw(13, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_i32()
    }

    pub async fn array_reference_get_values(
        &self,
        array_id: ObjectId,
        first_index: i32,
        length: i32,
    ) -> Result<Vec<JdwpValue>> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(array_id, &sizes);
        w.write_i32(first_index);
        w.write_i32(length);
        let payload = self.send_command_raw(13, 2, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        // JDWP spec: ArrayReference.GetValues reply contains a single element tag, followed by the values.
        let tag = r.read_u8()?;
        let count = r.read_u32()? as usize;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(r.read_value(tag, &sizes)?);
        }
        Ok(values)
    }

    pub async fn vm_resume(&self) -> Result<()> {
        let _ = self.send_command_raw(1, 9, Vec::new()).await?;
        Ok(())
    }

    pub async fn vm_suspend(&self) -> Result<()> {
        let _ = self.send_command_raw(1, 8, Vec::new()).await?;
        Ok(())
    }

    /// EventRequest.Set (15, 1)
    pub async fn event_request_set(
        &self,
        event_kind: u8,
        suspend_policy: u8,
        modifiers: Vec<EventModifier>,
    ) -> Result<i32> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_u8(event_kind);
        w.write_u8(suspend_policy);
        w.write_u32(modifiers.len() as u32);
        for modifier in modifiers {
            modifier.encode(&mut w, &sizes);
        }
        let payload = self.send_command_raw(15, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_i32()
    }

    /// EventRequest.Clear (15, 2)
    pub async fn event_request_clear(&self, event_kind: u8, request_id: i32) -> Result<()> {
        let mut w = JdwpWriter::new();
        w.write_u8(event_kind);
        w.write_i32(request_id);
        let _ = self.send_command_raw(15, 2, w.into_vec()).await?;
        Ok(())
    }
}

fn class_name_to_signature(class_name: &str) -> String {
    if class_name.starts_with('L') && class_name.ends_with(';') {
        return class_name.to_string();
    }
    let internal = class_name.replace('.', "/");
    format!("L{internal};")
}

#[derive(Debug)]
pub enum EventModifier {
    ThreadOnly {
        thread: ThreadId,
    },
    ClassMatch {
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
    Step {
        thread: ThreadId,
        size: u32,
        depth: u32,
    },
}

impl EventModifier {
    fn encode(self, w: &mut JdwpWriter, sizes: &JdwpIdSizes) {
        match self {
            EventModifier::ThreadOnly { thread } => {
                w.write_u8(3);
                w.write_object_id(thread, sizes);
            }
            EventModifier::ClassMatch { pattern } => {
                w.write_u8(5);
                w.write_string(&pattern);
            }
            EventModifier::LocationOnly { location } => {
                w.write_u8(7);
                w.write_location(&location, sizes);
            }
            EventModifier::ExceptionOnly {
                exception_or_null,
                caught,
                uncaught,
            } => {
                w.write_u8(8);
                w.write_reference_type_id(exception_or_null, sizes);
                w.write_bool(caught);
                w.write_bool(uncaught);
            }
            EventModifier::Step {
                thread,
                size,
                depth,
            } => {
                w.write_u8(10);
                w.write_object_id(thread, sizes);
                w.write_u32(size);
                w.write_u32(depth);
            }
        }
    }
}

async fn read_loop(mut reader: tokio::net::tcp::OwnedReadHalf, inner: Arc<Inner>) {
    let mut terminated_with_error = false;

    loop {
        let mut header = [0u8; HEADER_LEN];
        let header_read = tokio::select! {
            _ = inner.shutdown.cancelled() => break,
            res = reader.read_exact(&mut header) => res,
        };
        if let Err(err) = header_read {
            terminated_with_error = true;
            let _ = err;
            break;
        }

        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        if length < HEADER_LEN {
            terminated_with_error = true;
            break;
        }

        let id = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        let flags = header[8];
        let mut payload = vec![0u8; length - HEADER_LEN];
        let payload_read = tokio::select! {
            _ = inner.shutdown.cancelled() => break,
            res = reader.read_exact(&mut payload) => res,
        };
        if payload_read.is_err() {
            terminated_with_error = true;
            break;
        }

        if (flags & FLAG_REPLY) != 0 {
            let error_code = u16::from_be_bytes([header[9], header[10]]);
            let tx = {
                let mut pending = inner.pending.lock().await;
                pending.remove(&id)
            };

            if let Some(tx) = tx {
                let _ = tx.send(Ok(Reply {
                    error_code,
                    payload,
                }));
            }
        } else {
            let command_set = header[9];
            let command = header[10];
            if command_set == 64 && command == 100 {
                // Composite event packet.
                if handle_event_packet(&inner, &payload).await.is_err() {
                    terminated_with_error = true;
                    break;
                }
            } else {
                // Unknown command packets are ignored (we don't implement VM->debugger commands other than events).
                let _ = (id, command_set, command, payload);
            }
        }
    }

    inner.shutdown.cancel();

    if terminated_with_error {
        let pending = {
            let mut pending = inner.pending.lock().await;
            std::mem::take(&mut *pending)
        };
        for (_id, tx) in pending {
            let _ = tx.send(Err(JdwpError::ConnectionClosed));
        }
    }
}

async fn handle_event_packet(inner: &Inner, payload: &[u8]) -> Result<()> {
    let sizes = *inner.id_sizes.lock().await;
    let mut r = JdwpReader::new(payload);
    let _suspend_policy = r.read_u8()?;
    let event_count = r.read_u32()? as usize;
    for _ in 0..event_count {
        let kind = r.read_u8()?;
        let request_id = r.read_i32()?;
        match kind {
            1 => {
                let thread = r.read_object_id(&sizes)?;
                let location = r.read_location(&sizes)?;
                let _ = inner.events.send(JdwpEvent::SingleStep {
                    request_id,
                    thread,
                    location,
                });
            }
            2 => {
                let thread = r.read_object_id(&sizes)?;
                let location = r.read_location(&sizes)?;
                let _ = inner.events.send(JdwpEvent::Breakpoint {
                    request_id,
                    thread,
                    location,
                });
            }
            4 => {
                let thread = r.read_object_id(&sizes)?;
                let location = r.read_location(&sizes)?;
                let exception = r.read_object_id(&sizes)?;
                let catch_location = {
                    let catch_loc = r.read_location(&sizes)?;
                    if catch_loc.type_tag == 0
                        && catch_loc.class_id == 0
                        && catch_loc.method_id == 0
                        && catch_loc.index == 0
                    {
                        None
                    } else {
                        Some(catch_loc)
                    }
                };
                let _ = inner.events.send(JdwpEvent::Exception {
                    request_id,
                    thread,
                    location,
                    exception,
                    catch_location,
                });
            }
            8 => {
                let thread = r.read_object_id(&sizes)?;
                let ref_type_tag = r.read_u8()?;
                let type_id = r.read_reference_type_id(&sizes)?;
                let signature = r.read_string()?;
                let status = r.read_u32()?;
                let _ = inner.events.send(JdwpEvent::ClassPrepare {
                    request_id,
                    thread,
                    ref_type_tag,
                    type_id,
                    signature,
                    status,
                });
            }
            90 => {
                let thread = r.read_object_id(&sizes)?;
                let _ = inner.events.send(JdwpEvent::VmStart { request_id, thread });
            }
            99 => {
                let _ = request_id;
                let _ = inner.events.send(JdwpEvent::VmDeath);
            }
            _ => {
                // Unknown event kind: ignore the remainder of this composite packet.
                return Ok(());
            }
        }
    }
    Ok(())
}

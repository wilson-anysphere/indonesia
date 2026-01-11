use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex as StdMutex,
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
    inspect::InspectCache,
    types::{
        ClassInfo, FieldId, FieldInfo, FrameId, FrameInfo, JdwpCapabilitiesNew, JdwpError,
        JdwpEvent, JdwpEventEnvelope, JdwpIdSizes, JdwpValue, LineTable, LineTableEntry, Location,
        MethodId, MethodInfo, MonitorInfo, ObjectId, ReferenceTypeId, Result, ThreadId,
        VariableInfo, VmClassPaths, EVENT_KIND_BREAKPOINT, EVENT_KIND_CLASS_PREPARE,
        EVENT_KIND_CLASS_UNLOAD, EVENT_KIND_EXCEPTION, EVENT_KIND_FIELD_ACCESS,
        EVENT_KIND_FIELD_MODIFICATION, EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE,
        EVENT_KIND_SINGLE_STEP, EVENT_KIND_VM_DEATH, EVENT_KIND_VM_DISCONNECT, EVENT_KIND_VM_START,
        EVENT_MODIFIER_KIND_CLASS_EXCLUDE, EVENT_MODIFIER_KIND_CLASS_MATCH,
        EVENT_MODIFIER_KIND_CLASS_ONLY, EVENT_MODIFIER_KIND_COUNT, EVENT_MODIFIER_KIND_EXCEPTION_ONLY,
        EVENT_MODIFIER_KIND_FIELD_ONLY, EVENT_MODIFIER_KIND_INSTANCE_ONLY,
        EVENT_MODIFIER_KIND_LOCATION_ONLY, EVENT_MODIFIER_KIND_SOURCE_NAME_MATCH,
        EVENT_MODIFIER_KIND_STEP, EVENT_MODIFIER_KIND_THREAD_ONLY,
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
struct PendingGuard {
    inner: Arc<Inner>,
    id: u32,
    disarmed: bool,
}

impl PendingGuard {
    fn new(inner: Arc<Inner>, id: u32) -> Self {
        Self {
            inner,
            id,
            disarmed: false,
        }
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }

        let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.remove(&self.id);
    }
}

#[derive(Debug)]
struct Inner {
    writer: Mutex<tokio::net::tcp::OwnedWriteHalf>,
    // `pending` uses a std mutex so we can eagerly clean up entries when a request future
    // is dropped (e.g. higher-level cancellation). The critical sections are tiny and we never
    // hold the lock across `.await`, so blocking the runtime thread is not a concern.
    pending: StdMutex<HashMap<u32, oneshot::Sender<std::result::Result<Reply, JdwpError>>>>,
    next_id: AtomicU32,
    id_sizes: Mutex<JdwpIdSizes>,
    capabilities: Mutex<JdwpCapabilitiesNew>,
    events: broadcast::Sender<JdwpEvent>,
    event_envelopes: broadcast::Sender<JdwpEventEnvelope>,
    shutdown: CancellationToken,
    config: JdwpClientConfig,
    inspect_cache: Mutex<InspectCache>,
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
        let (event_envelopes, _) = broadcast::channel(config.event_channel_size);

        let inner = Arc::new(Inner {
            writer: Mutex::new(writer),
            pending: StdMutex::new(HashMap::with_capacity(config.pending_channel_size)),
            next_id: AtomicU32::new(1),
            id_sizes: Mutex::new(JdwpIdSizes::default()),
            capabilities: Mutex::new(JdwpCapabilitiesNew::default()),
            events,
            event_envelopes,
            shutdown: CancellationToken::new(),
            config,
            inspect_cache: Mutex::new(InspectCache::default()),
        });

        tokio::spawn(read_loop(reader, inner.clone()));

        let client = Self { inner };
        // ID sizes are required for correct parsing of most replies/events.
        let _ = client.idsizes().await?;
        // Capabilities are used for feature detection (hot swap, watchpoints, etc.).
        //
        // `VirtualMachine.CapabilitiesNew` was introduced after the legacy
        // `VirtualMachine.Capabilities` command and is not implemented by all
        // VMs (especially older/embedded JDWP stacks). Treat this as best-effort
        // and fall back to the legacy capability list when possible.
        match client.refresh_capabilities().await {
            Ok(_) => {}
            Err(err) if is_unsupported_command_error(&err) => match client
                .refresh_capabilities_legacy()
                .await
            {
                Ok(_) => {}
                Err(err) if is_unsupported_command_error(&err) => {
                    // Both capability queries are unsupported; keep the default
                    // all-false capability struct and continue connecting.
                }
                Err(err) => return Err(err),
            },
            Err(err) => return Err(err),
        }

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

    pub fn subscribe_event_envelopes(&self) -> broadcast::Receiver<JdwpEventEnvelope> {
        self.inner.event_envelopes.subscribe()
    }

    async fn send_command_raw(
        &self,
        command_set: u8,
        command: u8,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let mut pending_guard = PendingGuard::new(self.inner.clone(), id);

        {
            let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.insert(id, tx);
        }

        let packet = encode_command(id, command_set, command, &payload);
        {
            let mut writer = self.inner.writer.lock().await;
            writer.write_all(&packet).await?;
        }

        let reply = tokio::select! {
            _ = self.inner.shutdown.cancelled() => {
                self.remove_pending(id);
                pending_guard.disarm();
                return Err(JdwpError::Cancelled);
            }
            res = tokio::time::timeout(self.inner.config.reply_timeout, rx) => {
                match res {
                    Ok(Ok(r)) => r,
                    Ok(Err(_closed)) => return Err(JdwpError::ConnectionClosed),
                    Err(_elapsed) => {
                        self.remove_pending(id);
                        pending_guard.disarm();
                        return Err(JdwpError::Timeout);
                    }
                }
            }
        }?;

        pending_guard.disarm();

        if reply.error_code != 0 {
            return Err(JdwpError::VmError(reply.error_code));
        }

        Ok(reply.payload)
    }

    fn remove_pending(&self, id: u32) {
        let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.remove(&id);
    }

    async fn id_sizes(&self) -> JdwpIdSizes {
        *self.inner.id_sizes.lock().await
    }

    async fn set_id_sizes(&self, sizes: JdwpIdSizes) {
        *self.inner.id_sizes.lock().await = sizes;
    }

    async fn set_capabilities(&self, caps: JdwpCapabilitiesNew) {
        *self.inner.capabilities.lock().await = caps;
    }

    /// Returns the cached set of JDWP capabilities reported by the target VM.
    pub async fn capabilities(&self) -> JdwpCapabilitiesNew {
        *self.inner.capabilities.lock().await
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
    pub async fn refresh_capabilities(&self) -> Result<JdwpCapabilitiesNew> {
        let payload = self.send_command_raw(1, 17, Vec::new()).await?;
        let mut r = JdwpReader::new(&payload);
        let mut caps = Vec::with_capacity(r.remaining());
        while r.remaining() > 0 {
            caps.push(r.read_bool()?);
        }
        let caps = JdwpCapabilitiesNew::from_vec(caps);
        self.set_capabilities(caps).await;
        Ok(caps)
    }

    /// VirtualMachine.Capabilities (1, 12)
    pub async fn refresh_capabilities_legacy(&self) -> Result<JdwpCapabilitiesNew> {
        let payload = self.send_command_raw(1, 12, Vec::new()).await?;
        let mut r = JdwpReader::new(&payload);
        let mut caps = Vec::with_capacity(r.remaining());
        while r.remaining() > 0 {
            caps.push(r.read_bool()?);
        }
        let caps = JdwpCapabilitiesNew::from_legacy_vec(caps);
        self.set_capabilities(caps).await;
        Ok(caps)
    }

    /// VirtualMachine.ClassPaths (1, 13)
    pub async fn virtual_machine_class_paths(&self) -> Result<VmClassPaths> {
        let payload = self.send_command_raw(1, 13, Vec::new()).await?;
        let mut r = JdwpReader::new(&payload);

        let base_dir = r.read_string()?;
        let classpath_count = r.read_u32()? as usize;
        let mut classpaths = Vec::with_capacity(classpath_count);
        for _ in 0..classpath_count {
            classpaths.push(r.read_string()?);
        }

        let boot_classpath_count = r.read_u32()? as usize;
        let mut boot_classpaths = Vec::with_capacity(boot_classpath_count);
        for _ in 0..boot_classpath_count {
            boot_classpaths.push(r.read_string()?);
        }

        Ok(VmClassPaths {
            base_dir,
            classpaths,
            boot_classpaths,
        })
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

    /// ThreadReference.Suspend (11, 2)
    pub async fn thread_suspend(&self, thread: ThreadId) -> Result<()> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        let _ = self.send_command_raw(11, 2, w.into_vec()).await?;
        Ok(())
    }

    /// ThreadReference.Resume (11, 3)
    pub async fn thread_resume(&self, thread: ThreadId) -> Result<()> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        let _ = self.send_command_raw(11, 3, w.into_vec()).await?;
        Ok(())
    }

    /// ThreadReference.Status (11, 4)
    pub async fn thread_status(&self, thread: ThreadId) -> Result<(u32, u32)> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        let payload = self.send_command_raw(11, 4, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let thread_status = r.read_u32()?;
        let suspend_status = r.read_u32()?;
        Ok((thread_status, suspend_status))
    }

    /// ThreadReference.FrameCount (11, 7)
    pub async fn thread_frame_count(&self, thread: ThreadId) -> Result<u32> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        let payload = self.send_command_raw(11, 7, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_u32()
    }

    /// ThreadReference.OwnedMonitors (11, 8)
    pub async fn thread_owned_monitors(&self, thread: ThreadId) -> Result<Vec<ObjectId>> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        let payload = self.send_command_raw(11, 8, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut monitors = Vec::with_capacity(count);
        for _ in 0..count {
            monitors.push(r.read_object_id(&sizes)?);
        }
        Ok(monitors)
    }

    /// ThreadReference.CurrentContendedMonitor (11, 9)
    pub async fn thread_current_contended_monitor(&self, thread: ThreadId) -> Result<ObjectId> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        let payload = self.send_command_raw(11, 9, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_object_id(&sizes)
    }

    /// ThreadReference.SuspendCount (11, 12)
    pub async fn thread_suspend_count(&self, thread: ThreadId) -> Result<u32> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        let payload = self.send_command_raw(11, 12, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_u32()
    }

    /// ThreadReference.OwnedMonitorsStackDepthInfo (11, 13)
    pub async fn thread_owned_monitors_stack_depth_info(
        &self,
        thread: ThreadId,
    ) -> Result<Vec<(ObjectId, i32)>> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        let payload = self.send_command_raw(11, 13, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut monitors = Vec::with_capacity(count);
        for _ in 0..count {
            let monitor = r.read_object_id(&sizes)?;
            let stack_depth = r.read_i32()?;
            monitors.push((monitor, stack_depth));
        }
        Ok(monitors)
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

    pub(crate) async fn reference_type_signature_cached(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<String> {
        {
            let cache = self.inner.inspect_cache.lock().await;
            if let Some(sig) = cache.signatures.get(&class_id) {
                return Ok(sig.clone());
            }
        }

        let sig = self.reference_type_signature(class_id).await?;
        let mut cache = self.inner.inspect_cache.lock().await;
        cache.signatures.insert(class_id, sig.clone());
        Ok(sig)
    }

    pub async fn reference_type_class_loader(&self, class_id: ReferenceTypeId) -> Result<ObjectId> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        let payload = self.send_command_raw(2, 2, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_object_id(&sizes)
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

    pub(crate) async fn reference_type_methods_cached(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<Vec<MethodInfo>> {
        {
            let cache = self.inner.inspect_cache.lock().await;
            if let Some(methods) = cache.methods.get(&class_id) {
                return Ok(methods.clone());
            }
        }

        let methods = self.reference_type_methods(class_id).await?;
        let mut cache = self.inner.inspect_cache.lock().await;
        cache.methods.insert(class_id, methods.clone());
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

    pub async fn class_loader_define_class(
        &self,
        loader: ObjectId,
        name: &str,
        bytecode: &[u8],
    ) -> Result<ReferenceTypeId> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(loader, &sizes);
        w.write_string(name);
        w.write_u32(bytecode.len() as u32);
        w.write_bytes(bytecode);
        let payload = self.send_command_raw(14, 2, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_reference_type_id(&sizes)
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

    pub async fn class_type_invoke_method(
        &self,
        class_id: ReferenceTypeId,
        thread: ThreadId,
        method_id: MethodId,
        args: &[JdwpValue],
        options: u32,
    ) -> Result<(JdwpValue, ObjectId)> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        w.write_object_id(thread, &sizes);
        w.write_id(method_id, sizes.method_id);
        w.write_u32(args.len() as u32);
        for arg in args {
            w.write_tagged_value(arg, &sizes);
        }
        w.write_u32(options);
        let payload = self.send_command_raw(3, 3, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let return_value = r.read_tagged_value(&sizes)?;
        let exception = r.read_object_id(&sizes)?;
        Ok((return_value, exception))
    }

    /// StackFrame.ThisObject (16, 3)
    ///
    /// Returns the `this` object for the given stack frame (or 0 if the frame has no `this`).
    pub async fn stack_frame_this_object(
        &self,
        thread: ThreadId,
        frame_id: FrameId,
    ) -> Result<ObjectId> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        w.write_id(frame_id, sizes.frame_id);
        let payload = self.send_command_raw(16, 3, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_object_id(&sizes)
    }

    pub async fn object_reference_reference_type(
        &self,
        object_id: ObjectId,
    ) -> Result<(u8, ReferenceTypeId)> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(object_id, &sizes);
        let payload = self.send_command_raw(9, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        // JDWP spec: ObjectReference.ReferenceType reply starts with a `refTypeTag` byte.
        let ref_type_tag = r.read_u8()?;
        let type_id = r.read_reference_type_id(&sizes)?;
        Ok((ref_type_tag, type_id))
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

    pub(crate) async fn reference_type_fields_cached(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<Vec<FieldInfo>> {
        {
            let cache = self.inner.inspect_cache.lock().await;
            if let Some(fields) = cache.fields.get(&class_id) {
                return Ok(fields.clone());
            }
        }

        let fields = self.reference_type_fields(class_id).await?;
        let mut cache = self.inner.inspect_cache.lock().await;
        cache.fields.insert(class_id, fields.clone());
        Ok(fields)
    }

    /// ReferenceType.GetValues (2, 6)
    ///
    /// Fetches the values of the given (static) fields for a reference type.
    pub async fn reference_type_get_values(
        &self,
        type_id: ReferenceTypeId,
        field_ids: &[FieldId],
    ) -> Result<Vec<JdwpValue>> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(type_id, &sizes);
        w.write_u32(field_ids.len() as u32);
        for field_id in field_ids {
            w.write_id(*field_id, sizes.field_id);
        }
        let payload = self.send_command_raw(2, 6, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            let tag = r.read_u8()?;
            values.push(r.read_value(tag, &sizes)?);
        }
        Ok(values)
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

    pub async fn object_reference_invoke_method(
        &self,
        object_id: ObjectId,
        thread: ThreadId,
        class_id: ReferenceTypeId,
        method_id: MethodId,
        args: &[JdwpValue],
        options: u32,
    ) -> Result<(JdwpValue, ObjectId)> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(object_id, &sizes);
        w.write_object_id(thread, &sizes);
        w.write_reference_type_id(class_id, &sizes);
        w.write_id(method_id, sizes.method_id);
        w.write_u32(args.len() as u32);
        for arg in args {
            w.write_tagged_value(arg, &sizes);
        }
        w.write_u32(options);
        let payload = self.send_command_raw(9, 6, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let return_value = r.read_tagged_value(&sizes)?;
        let exception = r.read_object_id(&sizes)?;
        Ok((return_value, exception))
    }

    /// ObjectReference.MonitorInfo (9, 5)
    pub async fn object_reference_monitor_info(&self, object: ObjectId) -> Result<MonitorInfo> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(object, &sizes);
        let payload = self.send_command_raw(9, 5, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);

        let owner = r.read_object_id(&sizes)?;
        let entry_count = r.read_i32()?;
        let waiter_count = r.read_u32()? as usize;
        let mut waiters = Vec::with_capacity(waiter_count);
        for _ in 0..waiter_count {
            waiters.push(r.read_object_id(&sizes)?);
        }

        Ok(MonitorInfo {
            owner,
            entry_count,
            waiters,
        })
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

fn is_unsupported_command_error(err: &JdwpError) -> bool {
    const ERROR_NOT_FOUND: u16 = 41;
    const ERROR_NOT_IMPLEMENTED: u16 = 99;

    matches!(
        err,
        JdwpError::VmError(ERROR_NOT_FOUND | ERROR_NOT_IMPLEMENTED)
    )
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
    /// JDWP `EventRequest` modifier kind 1.
    ///
    /// Report the event after it has occurred `count` times.
    Count {
        count: u32,
    },
    ThreadOnly {
        thread: ThreadId,
    },
    /// JDWP `EventRequest` modifier kind 4.
    ///
    /// Limit the event to a specific reference type.
    ClassOnly {
        class_id: ReferenceTypeId,
    },
    ClassMatch {
        pattern: String,
    },
    /// JDWP `EventRequest` modifier kind 6.
    ///
    /// Exclude reference types that match the given class name pattern.
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
    /// JDWP `EventRequest` modifier kind 9.
    ///
    /// Limit the event to accesses/modifications of a specific field.
    FieldOnly {
        class_id: ReferenceTypeId,
        field_id: FieldId,
    },
    Step {
        thread: ThreadId,
        size: u32,
        depth: u32,
    },
    /// JDWP `EventRequest` modifier kind 11.
    ///
    /// Limit the event to a specific object instance.
    InstanceOnly {
        object_id: ObjectId,
    },
    /// JDWP `EventRequest` modifier kind 12.
    ///
    /// Limit the event to a source file name pattern.
    SourceNameMatch {
        pattern: String,
    },
}

impl EventModifier {
    fn encode(self, w: &mut JdwpWriter, sizes: &JdwpIdSizes) {
        match self {
            EventModifier::Count { count } => {
                w.write_u8(EVENT_MODIFIER_KIND_COUNT);
                w.write_u32(count);
            }
            EventModifier::ThreadOnly { thread } => {
                w.write_u8(EVENT_MODIFIER_KIND_THREAD_ONLY);
                w.write_object_id(thread, sizes);
            }
            EventModifier::ClassOnly { class_id } => {
                w.write_u8(EVENT_MODIFIER_KIND_CLASS_ONLY);
                w.write_reference_type_id(class_id, sizes);
            }
            EventModifier::ClassMatch { pattern } => {
                w.write_u8(EVENT_MODIFIER_KIND_CLASS_MATCH);
                w.write_string(&pattern);
            }
            EventModifier::ClassExclude { pattern } => {
                w.write_u8(EVENT_MODIFIER_KIND_CLASS_EXCLUDE);
                w.write_string(&pattern);
            }
            EventModifier::LocationOnly { location } => {
                w.write_u8(EVENT_MODIFIER_KIND_LOCATION_ONLY);
                w.write_location(&location, sizes);
            }
            EventModifier::ExceptionOnly {
                exception_or_null,
                caught,
                uncaught,
            } => {
                w.write_u8(EVENT_MODIFIER_KIND_EXCEPTION_ONLY);
                w.write_reference_type_id(exception_or_null, sizes);
                w.write_bool(caught);
                w.write_bool(uncaught);
            }
            EventModifier::FieldOnly { class_id, field_id } => {
                w.write_u8(EVENT_MODIFIER_KIND_FIELD_ONLY);
                w.write_reference_type_id(class_id, sizes);
                w.write_id(field_id, sizes.field_id);
            }
            EventModifier::Step {
                thread,
                size,
                depth,
            } => {
                w.write_u8(EVENT_MODIFIER_KIND_STEP);
                w.write_object_id(thread, sizes);
                w.write_u32(size);
                w.write_u32(depth);
            }
            EventModifier::InstanceOnly { object_id } => {
                w.write_u8(EVENT_MODIFIER_KIND_INSTANCE_ONLY);
                w.write_object_id(object_id, sizes);
            }
            EventModifier::SourceNameMatch { pattern } => {
                w.write_u8(EVENT_MODIFIER_KIND_SOURCE_NAME_MATCH);
                w.write_string(&pattern);
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
                let mut pending = inner.pending.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut pending = inner.pending.lock().unwrap_or_else(|e| e.into_inner());
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
    let suspend_policy = r.read_u8()?;
    let event_count = r.read_u32()? as usize;

    // Composite packets can include both stop events (SingleStep/Breakpoint/Exception) and
    // `MethodExitWithReturnValue`. The legacy TCP client parses the whole composite and
    // attaches method-exit values to the stop that follows. To preserve that behavior
    // for async consumers, we must emit `MethodExitWithReturnValue` (and all other
    // non-stop events) before stop events, even if the VM sends them in the opposite
    // order.
    let mut stop_events = Vec::new();
    let mut non_stop_events = Vec::new();

    fn is_stop_event(event: &JdwpEvent) -> bool {
        matches!(
            event,
            JdwpEvent::SingleStep { .. }
                | JdwpEvent::Breakpoint { .. }
                | JdwpEvent::Exception { .. }
                | JdwpEvent::FieldAccess { .. }
                | JdwpEvent::FieldModification { .. }
        )
    }

    for _ in 0..event_count {
        let kind = r.read_u8()?;
        let request_id = r.read_i32()?;

        let event = match kind {
            EVENT_KIND_SINGLE_STEP => {
                let thread = r.read_object_id(&sizes)?;
                let location = r.read_location(&sizes)?;
                Some(JdwpEvent::SingleStep {
                    request_id,
                    thread,
                    location,
                })
            }
            EVENT_KIND_BREAKPOINT => {
                let thread = r.read_object_id(&sizes)?;
                let location = r.read_location(&sizes)?;
                Some(JdwpEvent::Breakpoint {
                    request_id,
                    thread,
                    location,
                })
            }
            EVENT_KIND_EXCEPTION => {
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
                Some(JdwpEvent::Exception {
                    request_id,
                    thread,
                    location,
                    exception,
                    catch_location,
                })
            }
            6 => {
                let thread = r.read_object_id(&sizes)?;
                Some(JdwpEvent::ThreadStart { request_id, thread })
            }
            7 => {
                let thread = r.read_object_id(&sizes)?;
                Some(JdwpEvent::ThreadDeath { request_id, thread })
            }
            EVENT_KIND_CLASS_PREPARE => {
                let thread = r.read_object_id(&sizes)?;
                let ref_type_tag = r.read_u8()?;
                let type_id = r.read_reference_type_id(&sizes)?;
                let signature = r.read_string()?;
                let status = r.read_u32()?;
                Some(JdwpEvent::ClassPrepare {
                    request_id,
                    thread,
                    ref_type_tag,
                    type_id,
                    signature,
                    status,
                })
            }
            EVENT_KIND_CLASS_UNLOAD => {
                let signature = r.read_string()?;
                Some(JdwpEvent::ClassUnload {
                    request_id,
                    signature,
                })
            }
            EVENT_KIND_FIELD_ACCESS => {
                let thread = r.read_object_id(&sizes)?;
                let location = r.read_location(&sizes)?;
                let ref_type_tag = r.read_u8()?;
                let type_id = r.read_reference_type_id(&sizes)?;
                let field_id = r.read_id(sizes.field_id)?;
                let object = r.read_object_id(&sizes)?;
                let tag = r.read_u8()?;
                let value = r.read_value(tag, &sizes)?;
                Some(JdwpEvent::FieldAccess {
                    request_id,
                    thread,
                    location,
                    ref_type_tag,
                    type_id,
                    field_id,
                    object,
                    value,
                })
            }
            EVENT_KIND_FIELD_MODIFICATION => {
                let thread = r.read_object_id(&sizes)?;
                let location = r.read_location(&sizes)?;
                let ref_type_tag = r.read_u8()?;
                let type_id = r.read_reference_type_id(&sizes)?;
                let field_id = r.read_id(sizes.field_id)?;
                let object = r.read_object_id(&sizes)?;
                let tag = r.read_u8()?;
                let value_to_be = r.read_value(tag, &sizes)?;
                Some(JdwpEvent::FieldModification {
                    request_id,
                    thread,
                    location,
                    ref_type_tag,
                    type_id,
                    field_id,
                    object,
                    value_current: None,
                    value_to_be,
                })
            }
            EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE => {
                let thread = r.read_object_id(&sizes)?;
                let location = r.read_location(&sizes)?;
                let tag = r.read_u8()?;
                let value = r.read_value(tag, &sizes)?;
                Some(JdwpEvent::MethodExitWithReturnValue {
                    request_id,
                    thread,
                    location,
                    value,
                })
            }
            EVENT_KIND_VM_START => {
                let thread = r.read_object_id(&sizes)?;
                Some(JdwpEvent::VmStart { request_id, thread })
            }
            EVENT_KIND_VM_DEATH => {
                let _ = request_id;
                Some(JdwpEvent::VmDeath)
            }
            EVENT_KIND_VM_DISCONNECT => {
                let _ = request_id;
                inner.shutdown.cancel();
                Some(JdwpEvent::VmDisconnect)
            }
            _ => {
                // Unknown event kind: ignore the remainder of this composite packet.
                None
            }
        };

        let Some(event) = event else {
            break;
        };

        if is_stop_event(&event) {
            stop_events.push(event);
        } else {
            non_stop_events.push(event);
        }
    }

    for event in non_stop_events {
        let _ = inner.events.send(event.clone());
        let _ = inner
            .event_envelopes
            .send(JdwpEventEnvelope { suspend_policy, event });
    }
    for event in stop_events {
        let _ = inner.events.send(event.clone());
        let _ = inner
            .event_envelopes
            .send(JdwpEventEnvelope { suspend_policy, event });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{EventModifier, JdwpClient, JdwpClientConfig};
    use crate::wire::mock::{DelayedReply, MockEventRequestModifier, MockJdwpServer, MockJdwpServerConfig};
    use crate::wire::types::{
        EVENT_KIND_BREAKPOINT, EVENT_KIND_CLASS_UNLOAD, EVENT_KIND_EXCEPTION, EVENT_KIND_FIELD_ACCESS,
        EVENT_KIND_FIELD_MODIFICATION, EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE, EVENT_KIND_VM_DISCONNECT,
        JdwpCapabilitiesNew, JdwpError, JdwpEvent, JdwpIdSizes, Location, SUSPEND_POLICY_ALL,
        SUSPEND_POLICY_EVENT_THREAD, SUSPEND_POLICY_NONE,
    };
    use crate::wire::JdwpValue;

    fn monitor_capabilities() -> Vec<bool> {
        let mut caps = vec![false; 32];
        caps[4] = true; // canGetOwnedMonitorInfo
        caps[5] = true; // canGetCurrentContendedMonitor
        caps[6] = true; // canGetMonitorInfo
        caps[21] = true; // canGetOwnedMonitorStackDepthInfo
        caps
    }

    #[tokio::test]
    async fn pending_entries_are_removed_when_request_future_is_dropped() {
        // Delay `VirtualMachine.AllThreads (1, 4)` so the request stays in-flight long enough
        // for us to abort it without racing a reply/timeout path.
        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            delayed_replies: vec![DelayedReply {
                command_set: 1,
                command: 4,
                delay: Duration::from_secs(60),
            }],
            ..Default::default()
        })
        .await
        .unwrap();

        let client = JdwpClient::connect_with_config(
            server.addr(),
            JdwpClientConfig {
                reply_timeout: Duration::from_secs(60),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(client.inner.pending.lock().unwrap().len(), 0);

        let client_for_task = client.clone();
        let task = tokio::spawn(async move {
            let _ = client_for_task.all_threads().await;
        });

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if client.inner.pending.lock().unwrap().len() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("request never became pending");

        task.abort();
        let _ = task.await;

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if client.inner.pending.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("pending entry was not cleaned up");
    }

    #[tokio::test]
    async fn connect_falls_back_to_legacy_capabilities_when_capabilities_new_is_not_implemented() {
        let mut capabilities = vec![false; 32];
        capabilities[0] = true; // canWatchFieldModification
        capabilities[6] = true; // canGetMonitorInfo
        capabilities[7] = true; // canRedefineClasses (not representable by legacy list)

        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            capabilities,
            capabilities_new_not_implemented: true,
            ..Default::default()
        })
        .await
        .unwrap();

        let client = JdwpClient::connect(server.addr()).await.unwrap();
        let caps = client.capabilities().await;

        let mut expected = JdwpCapabilitiesNew::default();
        expected.can_watch_field_modification = true;
        expected.can_get_monitor_info = true;
        // Legacy `VirtualMachine.Capabilities` cannot report `can_redefine_classes`.
        expected.can_redefine_classes = false;

        assert_eq!(caps, expected);
    }

    #[tokio::test]
    async fn connect_succeeds_when_both_capability_commands_are_unsupported() {
        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            capabilities_new_not_implemented: true,
            capabilities_legacy_not_implemented: true,
            ..Default::default()
        })
        .await
        .unwrap();

        let client = JdwpClient::connect(server.addr()).await.unwrap();
        assert_eq!(client.capabilities().await, JdwpCapabilitiesNew::default());
    }

    #[tokio::test]
    async fn capabilities_new_parses_short_boolean_list() {
        // Some older/embedded JDWP implementations return fewer than the 32 booleans
        // typically produced by HotSpot.
        let server = MockJdwpServer::spawn_with_capabilities(vec![true, false, true])
            .await
            .unwrap();

        let client = JdwpClient::connect(server.addr()).await.unwrap();
        let caps = client.capabilities().await;

        assert!(caps.can_watch_field_modification);
        assert!(!caps.can_watch_field_access);
        assert!(caps.can_get_bytecodes);
        // Missing booleans should be treated as false.
        assert!(!caps.can_get_monitor_info);
    }

    #[tokio::test]
    async fn thread_status_and_suspension_accounting_are_decoded() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let thread = client.all_threads().await.unwrap()[0];

        let (thread_status, suspend_status) = client.thread_status(thread).await.unwrap();
        assert_eq!(thread_status, 3);
        assert_eq!(suspend_status, 1);

        let suspend_count = client.thread_suspend_count(thread).await.unwrap();
        assert_eq!(suspend_count, 2);

        let frame_count = client.thread_frame_count(thread).await.unwrap();
        assert_eq!(frame_count, 1);
    }

    #[tokio::test]
    async fn monitor_and_lock_information_are_decoded() {
        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            capabilities: monitor_capabilities(),
            ..Default::default()
        })
        .await
        .unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let caps = client.capabilities().await;
        assert!(caps.supports_monitor_info());
        assert!(caps.supports_owned_monitor_info());
        assert!(caps.supports_current_contended_monitor());
        assert!(caps.supports_owned_monitor_stack_depth_info());

        let thread = client.all_threads().await.unwrap()[0];

        let owned = client.thread_owned_monitors(thread).await.unwrap();
        assert_eq!(owned, vec![0x5201, 0x5202]);

        let owned_depth = client
            .thread_owned_monitors_stack_depth_info(thread)
            .await
            .unwrap();
        assert_eq!(owned_depth, vec![(0x5201, 0), (0x5202, 2)]);

        let contended = client.thread_current_contended_monitor(thread).await.unwrap();
        assert_eq!(contended, 0x5203);

        let info = client.object_reference_monitor_info(contended).await.unwrap();
        assert_eq!(info.owner, 0x1002);
        assert_eq!(info.entry_count, 1);
        assert_eq!(info.waiters, vec![thread]);
    }

    #[tokio::test]
    async fn monitor_commands_propagate_not_implemented() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let thread = client.all_threads().await.unwrap()[0];
        let err = client.thread_owned_monitors(thread).await.unwrap_err();

        match err {
            JdwpError::VmError(code) => assert_eq!(code, 99),
            other => panic!("expected VmError(99), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn monitor_introspection_respects_non_default_id_sizes() {
        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            capabilities: monitor_capabilities(),
            id_sizes: JdwpIdSizes {
                field_id: 4,
                method_id: 4,
                object_id: 4,
                reference_type_id: 4,
                frame_id: 4,
            },
            ..Default::default()
        })
        .await
        .unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let thread = client.all_threads().await.unwrap()[0];
        assert_eq!(thread, 0x1001);

        let contended = client
            .thread_current_contended_monitor(thread)
            .await
            .unwrap();
        assert_eq!(contended, 0x5203);

        let info = client.object_reference_monitor_info(contended).await.unwrap();
        assert_eq!(info.owner, 0x1002);
        assert_eq!(info.waiters, vec![thread]);
    }

    #[tokio::test]
    async fn event_envelopes_preserve_suspend_policy() {
        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            breakpoint_events: 1,
            ..Default::default()
        })
        .await
        .unwrap();

        let client = JdwpClient::connect(server.addr()).await.unwrap();
        let mut envelopes = client.subscribe_event_envelopes();

        let request_id = client
            .event_request_set(
                EVENT_KIND_BREAKPOINT,
                SUSPEND_POLICY_ALL,
                vec![EventModifier::LocationOnly {
                    location: Location {
                        type_tag: 1,
                        class_id: 0,
                        method_id: 0,
                        index: 0,
                    },
                }],
            )
            .await
            .unwrap();

        client.vm_resume().await.unwrap();

        let envelope = tokio::time::timeout(Duration::from_secs(5), envelopes.recv())
            .await
            .expect("timed out waiting for breakpoint event envelope")
            .expect("failed to receive breakpoint event envelope");

        assert_eq!(envelope.suspend_policy, SUSPEND_POLICY_ALL);
        match envelope.event {
            JdwpEvent::Breakpoint { request_id: rid, .. } => assert_eq!(rid, request_id),
            other => panic!("expected breakpoint event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn method_exit_is_emitted_before_stop_events_in_same_composite_packet() {
        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            breakpoint_events: 1,
            emit_exception_breakpoint_method_exit_composite: true,
            ..Default::default()
        })
        .await
        .unwrap();

        let client = JdwpClient::connect(server.addr()).await.unwrap();
        let mut events = client.subscribe_events();

        let exception_request_id = client
            .event_request_set(
                EVENT_KIND_EXCEPTION,
                SUSPEND_POLICY_EVENT_THREAD,
                vec![EventModifier::ExceptionOnly {
                    exception_or_null: 0,
                    caught: false,
                    uncaught: true,
                }],
            )
            .await
            .unwrap();

        let breakpoint_request_id = client
            .event_request_set(
                EVENT_KIND_BREAKPOINT,
                SUSPEND_POLICY_EVENT_THREAD,
                vec![EventModifier::LocationOnly {
                    location: Location {
                        type_tag: 1,
                        class_id: 0,
                        method_id: 0,
                        index: 0,
                    },
                }],
            )
            .await
            .unwrap();

        let method_exit_request_id = client
            .event_request_set(
                EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE,
                SUSPEND_POLICY_NONE,
                Vec::new(),
            )
            .await
            .unwrap();

        client.vm_resume().await.unwrap();

        let first = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for composite events (first)")
            .expect("failed to receive composite event (first)");
        let second = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for composite events (second)")
            .expect("failed to receive composite event (second)");
        let third = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for composite events (third)")
            .expect("failed to receive composite event (third)");

        match first {
            JdwpEvent::MethodExitWithReturnValue { request_id, .. } => {
                assert_eq!(request_id, method_exit_request_id)
            }
            other => panic!("expected MethodExitWithReturnValue first, got {other:?}"),
        }
        match second {
            JdwpEvent::Exception { request_id, .. } => assert_eq!(request_id, exception_request_id),
            other => panic!("expected Exception second, got {other:?}"),
        }
        match third {
            JdwpEvent::Breakpoint { request_id, .. } => assert_eq!(request_id, breakpoint_request_id),
            other => panic!("expected Breakpoint third, got {other:?}"),
        }
    }


    #[tokio::test]
    async fn event_request_set_encodes_new_event_modifiers() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let class_id = 0x1111_u64;
        let field_id = 0x2222_u64;
        let object_id = 0x3333_u64;

        let _request_id = client
            .event_request_set(
                EVENT_KIND_BREAKPOINT,
                SUSPEND_POLICY_NONE,
                vec![
                    EventModifier::ClassOnly { class_id },
                    EventModifier::ClassExclude {
                        pattern: "java.*".to_string(),
                    },
                    EventModifier::FieldOnly { class_id, field_id },
                    EventModifier::InstanceOnly { object_id },
                    EventModifier::SourceNameMatch {
                        pattern: "Main.java".to_string(),
                    },
                ],
            )
            .await
            .unwrap();

        let requests = server.event_requests().await;
        let last = requests.last().cloned().expect("no EventRequest.Set observed");

        assert_eq!(
            last.modifiers,
            vec![
                MockEventRequestModifier::ClassOnly { class_id },
                MockEventRequestModifier::ClassExclude {
                    pattern: "java.*".to_string()
                },
                MockEventRequestModifier::FieldOnly { class_id, field_id },
                MockEventRequestModifier::InstanceOnly { object_id },
                MockEventRequestModifier::SourceNameMatch {
                    pattern: "Main.java".to_string()
                },
            ]
        );
    }

    #[tokio::test]
    async fn parses_watchpoint_and_class_unload_events() {
        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            field_access_events: 1,
            field_modification_events: 1,
            class_unload_events: 1,
            ..Default::default()
        })
        .await
        .unwrap();

        let client = JdwpClient::connect(server.addr()).await.unwrap();
        let mut events = client.subscribe_events();

        let thread = client.all_threads().await.unwrap()[0];
        let class_id = client.all_classes().await.unwrap()[0].type_id;
        let field_id = 0x7777_u64;
        let object_id = 0x8888_u64;

        let class_unload_request = client
            .event_request_set(EVENT_KIND_CLASS_UNLOAD, SUSPEND_POLICY_NONE, Vec::new())
            .await
            .unwrap();
        let field_access_request = client
            .event_request_set(
                EVENT_KIND_FIELD_ACCESS,
                SUSPEND_POLICY_NONE,
                vec![
                    EventModifier::FieldOnly { class_id, field_id },
                    EventModifier::InstanceOnly { object_id },
                ],
            )
            .await
            .unwrap();
        let field_modification_request = client
            .event_request_set(
                EVENT_KIND_FIELD_MODIFICATION,
                SUSPEND_POLICY_NONE,
                vec![
                    EventModifier::FieldOnly { class_id, field_id },
                    EventModifier::InstanceOnly { object_id },
                ],
            )
            .await
            .unwrap();

        client.vm_resume().await.unwrap();

        let class_unload = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("timed out waiting for ClassUnload")
            .unwrap();
        match class_unload {
            JdwpEvent::ClassUnload {
                request_id,
                signature,
            } => {
                assert_eq!(request_id, class_unload_request);
                assert_eq!(signature, "LMain;");
            }
            other => panic!("expected ClassUnload, got {other:?}"),
        }

        let field_access = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("timed out waiting for FieldAccess")
            .unwrap();
        match field_access {
            JdwpEvent::FieldAccess {
                request_id,
                thread: event_thread,
                location,
                ref_type_tag,
                type_id,
                field_id: event_field_id,
                object,
                value,
            } => {
                assert_eq!(request_id, field_access_request);
                assert_eq!(event_thread, thread);
                assert_eq!(location.class_id, class_id);
                assert_eq!(ref_type_tag, 1);
                assert_eq!(type_id, class_id);
                assert_eq!(event_field_id, field_id);
                assert_eq!(object, object_id);
                assert_eq!(value, JdwpValue::Int(7));
            }
            other => panic!("expected FieldAccess, got {other:?}"),
        }

        let field_modification = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("timed out waiting for FieldModification")
            .unwrap();
        match field_modification {
            JdwpEvent::FieldModification {
                request_id,
                thread: event_thread,
                location,
                ref_type_tag,
                type_id,
                field_id: event_field_id,
                object,
                value_current,
                value_to_be,
            } => {
                assert_eq!(request_id, field_modification_request);
                assert_eq!(event_thread, thread);
                assert_eq!(location.class_id, class_id);
                assert_eq!(ref_type_tag, 1);
                assert_eq!(type_id, class_id);
                assert_eq!(event_field_id, field_id);
                assert_eq!(object, object_id);
                assert_eq!(value_current, None);
                assert_eq!(value_to_be, JdwpValue::Int(8));
            }
            other => panic!("expected FieldModification, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn vm_disconnect_event_cancels_shutdown_token() {
        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            vm_disconnect_events: 1,
            ..Default::default()
        })
        .await
        .unwrap();

        let client = JdwpClient::connect(server.addr()).await.unwrap();
        let token = client.shutdown_token();
        let mut events = client.subscribe_events();

        let _request_id = client
            .event_request_set(EVENT_KIND_VM_DISCONNECT, SUSPEND_POLICY_NONE, Vec::new())
            .await
            .unwrap();
        client.vm_resume().await.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("timed out waiting for VmDisconnect")
            .unwrap();
        assert!(matches!(event, JdwpEvent::VmDisconnect));

        tokio::time::timeout(Duration::from_secs(2), token.cancelled())
            .await
            .expect("shutdown token was not cancelled after VmDisconnect");
    }
}


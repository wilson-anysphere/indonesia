use std::{
    collections::{HashMap, HashSet},
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
        class_name_to_signature, signature_to_tag, JdwpReader, JdwpWriter, FLAG_REPLY, HANDSHAKE,
        HEADER_LEN,
    },
    inspect::InspectCache,
    types::{
        ClassInfo, FieldId, FieldInfo, FieldInfoWithGeneric, FrameId, FrameInfo,
        JdwpCapabilitiesNew, JdwpError, JdwpEvent, JdwpEventEnvelope, JdwpIdSizes, JdwpValue,
        LineTable, LineTableEntry, Location, MethodId, MethodInfo, MethodInfoWithGeneric,
        MonitorInfo, ObjectId, ReferenceTypeId, Result, ThreadGroupId, ThreadId, VariableInfo,
        VariableInfoWithGeneric, VmClassPaths, EVENT_KIND_BREAKPOINT, EVENT_KIND_CLASS_PREPARE,
        EVENT_KIND_CLASS_UNLOAD, EVENT_KIND_EXCEPTION, EVENT_KIND_FIELD_ACCESS,
        EVENT_KIND_FIELD_MODIFICATION, EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE,
        EVENT_KIND_SINGLE_STEP, EVENT_KIND_VM_DEATH, EVENT_KIND_VM_DISCONNECT, EVENT_KIND_VM_START,
        EVENT_MODIFIER_KIND_CLASS_EXCLUDE, EVENT_MODIFIER_KIND_CLASS_MATCH,
        EVENT_MODIFIER_KIND_CLASS_ONLY, EVENT_MODIFIER_KIND_COUNT,
        EVENT_MODIFIER_KIND_EXCEPTION_ONLY, EVENT_MODIFIER_KIND_FIELD_ONLY,
        EVENT_MODIFIER_KIND_INSTANCE_ONLY, EVENT_MODIFIER_KIND_LOCATION_ONLY,
        EVENT_MODIFIER_KIND_SOURCE_NAME_MATCH, EVENT_MODIFIER_KIND_STEP,
        EVENT_MODIFIER_KIND_THREAD_ONLY,
    },
};

const FIELD_MODIFIER_STATIC: u32 = 0x0008;
// Capabilities replies are spec-defined fixed-size boolean lists (HotSpot returns 32).
// Parse only a small bounded prefix to avoid allocating/iterating on a maliciously large reply.
const MAX_CAPABILITIES_BOOL_COUNT: usize = 256;

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
            Err(err) if is_unsupported_command_error(&err) => {
                match client.refresh_capabilities_legacy().await {
                    Ok(_) => {}
                    Err(err) if is_unsupported_command_error(&err) => {
                        // Both capability queries are unsupported; keep the default
                        // all-false capability struct and continue connecting.
                    }
                    Err(err) => return Err(err),
                }
            }
            Err(err) => return Err(err),
        }

        Ok(client)
    }

    /// Shut down the client locally without sending any JDWP command to the target VM.
    ///
    /// This is primarily a cancellation mechanism for higher-level adapters that need to
    /// stop all in-flight JDWP requests and event processing.
    ///
    /// To explicitly detach from or terminate the target VM, use [`JdwpClient::virtual_machine_dispose`]
    /// (detach) or [`JdwpClient::virtual_machine_exit`] (terminate).
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
        let length = HEADER_LEN
            .checked_add(payload.len())
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        if length > crate::MAX_JDWP_PACKET_BYTES {
            return Err(JdwpError::Protocol(format!(
                "packet too large ({length} bytes, max {})",
                crate::MAX_JDWP_PACKET_BYTES
            )));
        }

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let mut pending_guard = PendingGuard::new(self.inner.clone(), id);

        {
            let mut pending = self.inner.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.insert(id, tx);
        }

        {
            let mut writer = self.inner.writer.lock().await;
            let mut header = [0u8; HEADER_LEN];
            header[0..4].copy_from_slice(&(length as u32).to_be_bytes());
            header[4..8].copy_from_slice(&id.to_be_bytes());
            header[8] = 0; // flags
            header[9] = command_set;
            header[10] = command;

            writer.write_all(&header).await?;
            writer.write_all(&payload).await?;
        }

        // Prefer delivering a reply over treating a concurrently-cancelled shutdown token
        // as an error. This avoids racy `Cancelled` results when the VM sends a terminal
        // event (e.g. VMDisconnect) immediately after replying to a command.
        let reply = tokio::select! {
            biased;
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
            _ = self.inner.shutdown.cancelled() => {
                self.remove_pending(id);
                pending_guard.disarm();
                return Err(JdwpError::Cancelled);
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
        let count = r.remaining().min(MAX_CAPABILITIES_BOOL_COUNT);
        let mut caps = Vec::with_capacity(count);
        for _ in 0..count {
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
        let count = r.remaining().min(MAX_CAPABILITIES_BOOL_COUNT);
        let mut caps = Vec::with_capacity(count);
        for _ in 0..count {
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
        let mut classpaths = Vec::new();
        classpaths.try_reserve_exact(classpath_count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate classpath list ({classpath_count} entries)"
            ))
        })?;
        for _ in 0..classpath_count {
            classpaths.push(r.read_string()?);
        }

        let boot_classpath_count = r.read_u32()? as usize;
        let mut boot_classpaths = Vec::new();
        boot_classpaths
            .try_reserve_exact(boot_classpath_count)
            .map_err(|_| {
                JdwpError::Protocol(format!(
                    "unable to allocate boot classpath list ({boot_classpath_count} entries)"
                ))
            })?;
        for _ in 0..boot_classpath_count {
            boot_classpaths.push(r.read_string()?);
        }

        Ok(VmClassPaths {
            base_dir,
            classpaths,
            boot_classpaths,
        })
    }

    /// VirtualMachine.CreateString (1, 11)
    pub async fn virtual_machine_create_string(&self, value: &str) -> Result<ObjectId> {
        let length = HEADER_LEN
            .checked_add(4usize.saturating_add(value.len()))
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        if length > crate::MAX_JDWP_PACKET_BYTES {
            return Err(JdwpError::Protocol(format!(
                "packet too large ({length} bytes, max {})",
                crate::MAX_JDWP_PACKET_BYTES
            )));
        }

        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_string(value);
        let payload = self.send_command_raw(1, 11, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_object_id(&sizes)
    }

    /// VirtualMachine.SetDefaultStratum (1, 19)
    pub async fn virtual_machine_set_default_stratum(&self, stratum: &str) -> Result<()> {
        let length = HEADER_LEN
            .checked_add(4usize.saturating_add(stratum.len()))
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        if length > crate::MAX_JDWP_PACKET_BYTES {
            return Err(JdwpError::Protocol(format!(
                "packet too large ({length} bytes, max {})",
                crate::MAX_JDWP_PACKET_BYTES
            )));
        }

        let mut w = JdwpWriter::new();
        w.write_string(stratum);
        let _ = self.send_command_raw(1, 19, w.into_vec()).await?;
        Ok(())
    }

    pub async fn all_threads(&self) -> Result<Vec<ThreadId>> {
        let payload = self.send_command_raw(1, 4, Vec::new()).await?;
        let sizes = self.id_sizes().await;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut threads = Vec::new();
        threads.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate thread id list ({count} entries)"
            ))
        })?;
        for _ in 0..count {
            threads.push(r.read_object_id(&sizes)?);
        }
        Ok(threads)
    }

    /// VirtualMachine.TopLevelThreadGroups (1, 5)
    pub async fn virtual_machine_top_level_thread_groups(&self) -> Result<Vec<ThreadGroupId>> {
        let payload = self.send_command_raw(1, 5, Vec::new()).await?;
        let sizes = self.id_sizes().await;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut groups = Vec::new();
        groups.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate thread group id list ({count} entries)"
            ))
        })?;
        for _ in 0..count {
            groups.push(r.read_object_id(&sizes)?);
        }
        Ok(groups)
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

    /// ThreadReference.ThreadGroup (11, 5)
    pub async fn thread_reference_thread_group(&self, thread: ThreadId) -> Result<ThreadGroupId> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        let payload = self.send_command_raw(11, 5, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_object_id(&sizes)
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
        let mut monitors = Vec::new();
        monitors.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate owned monitor list ({count} entries)"
            ))
        })?;
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
        let mut monitors = Vec::new();
        monitors.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate owned monitor list ({count} entries)"
            ))
        })?;
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
        let mut frames = Vec::new();
        frames.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!("unable to allocate frame list ({count} entries)"))
        })?;
        for _ in 0..count {
            let frame_id = r.read_id(sizes.frame_id)?;
            let location = r.read_location(&sizes)?;
            frames.push(FrameInfo { frame_id, location });
        }
        Ok(frames)
    }

    /// ThreadGroupReference.Name (12, 1)
    pub async fn thread_group_reference_name(&self, group: ThreadGroupId) -> Result<String> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(group, &sizes);
        let payload = self.send_command_raw(12, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_string()
    }

    /// ThreadGroupReference.Parent (12, 2)
    pub async fn thread_group_reference_parent(
        &self,
        group: ThreadGroupId,
    ) -> Result<ThreadGroupId> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(group, &sizes);
        let payload = self.send_command_raw(12, 2, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_object_id(&sizes)
    }

    /// ThreadGroupReference.Children (12, 3)
    pub async fn thread_group_reference_children(
        &self,
        group: ThreadGroupId,
    ) -> Result<(Vec<ThreadGroupId>, Vec<ThreadId>)> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(group, &sizes);
        let payload = self.send_command_raw(12, 3, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);

        let group_count = r.read_u32()? as usize;
        let mut groups = Vec::new();
        groups.try_reserve_exact(group_count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate thread group list ({group_count} entries)"
            ))
        })?;
        for _ in 0..group_count {
            groups.push(r.read_object_id(&sizes)?);
        }

        let thread_count = r.read_u32()? as usize;
        let mut threads = Vec::new();
        threads.try_reserve_exact(thread_count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate thread id list ({thread_count} entries)"
            ))
        })?;
        for _ in 0..thread_count {
            threads.push(r.read_object_id(&sizes)?);
        }

        Ok((groups, threads))
    }

    pub async fn all_classes(&self) -> Result<Vec<ClassInfo>> {
        let payload = self.send_command_raw(1, 3, Vec::new()).await?;
        let sizes = self.id_sizes().await;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut classes = Vec::new();
        classes.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!("unable to allocate class list ({count} entries)"))
        })?;
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
        let mut classes = Vec::new();
        classes.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!("unable to allocate class list ({count} entries)"))
        })?;
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

        let mut payload_len = 4usize; // classCount
        for (_type_id, bytecode) in classes {
            payload_len = payload_len
                .checked_add(sizes.reference_type_id)
                .and_then(|v| v.checked_add(4)) // bytecodeLen
                .and_then(|v| v.checked_add(bytecode.len()))
                .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        }
        let length = HEADER_LEN
            .checked_add(payload_len)
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        if length > crate::MAX_JDWP_PACKET_BYTES {
            return Err(JdwpError::Protocol(format!(
                "packet too large ({length} bytes, max {})",
                crate::MAX_JDWP_PACKET_BYTES
            )));
        }

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

        // Avoid infallible allocations for attacker-controlled bytecode blobs (and avoid copying
        // the bytecode at all when the resulting packet would be rejected as oversized).
        let sizes = self.id_sizes().await;
        let per_class_len = sizes
            .reference_type_id
            .checked_add(4) // bytecodeLen
            .and_then(|v| v.checked_add(bytecode.len()))
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        let payload_len = 4usize
            .checked_add(
                infos
                    .len()
                    .checked_mul(per_class_len)
                    .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?,
            )
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        let length = HEADER_LEN
            .checked_add(payload_len)
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        if length > crate::MAX_JDWP_PACKET_BYTES {
            return Err(JdwpError::Protocol(format!(
                "packet too large ({length} bytes, max {})",
                crate::MAX_JDWP_PACKET_BYTES
            )));
        }

        let mut classes = Vec::new();
        classes.try_reserve_exact(infos.len()).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate redefine class list ({} entries)",
                infos.len()
            ))
        })?;
        for info in infos {
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(bytecode.len()).map_err(|_| {
                JdwpError::Protocol(format!(
                    "unable to allocate redefine class bytecode buffer ({} bytes)",
                    bytecode.len()
                ))
            })?;
            bytes.extend_from_slice(bytecode);
            classes.push((info.type_id, bytes));
        }
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

    /// ReferenceType.SourceDebugExtension (2, 12)
    pub async fn reference_type_source_debug_extension(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<String> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        let payload = self.send_command_raw(2, 12, w.into_vec()).await?;
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

    /// ReferenceType.SignatureWithGeneric (2, 13)
    pub async fn reference_type_signature_with_generic(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<(String, Option<String>)> {
        {
            let cache = self.inner.inspect_cache.lock().await;
            if let Some(sig) = cache.signatures_with_generic.get(&class_id) {
                return Ok(sig.clone());
            }
        }

        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        let payload = self.send_command_raw(2, 13, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let signature = r.read_string()?;
        let generic = r.read_string()?;
        let generic = (!generic.is_empty()).then_some(generic);
        let mut cache = self.inner.inspect_cache.lock().await;
        cache.signatures.insert(class_id, signature.clone());
        let value = (signature, generic);
        cache
            .signatures_with_generic
            .insert(class_id, value.clone());
        Ok(value)
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

    #[allow(dead_code)]
    pub(crate) async fn reference_type_signature_with_generic_cached(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<(String, Option<String>)> {
        {
            let cache = self.inner.inspect_cache.lock().await;
            if let Some(sig) = cache.signatures_with_generic.get(&class_id) {
                return Ok(sig.clone());
            }
        }

        let sig = self.reference_type_signature_with_generic(class_id).await?;
        let mut cache = self.inner.inspect_cache.lock().await;
        cache.signatures_with_generic.insert(class_id, sig.clone());
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

    /// ReferenceType.Interfaces (2, 10)
    pub async fn reference_type_interfaces(
        &self,
        type_id: ReferenceTypeId,
    ) -> Result<Vec<ReferenceTypeId>> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(type_id, &sizes);
        let payload = self.send_command_raw(2, 10, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut interfaces = Vec::new();
        interfaces.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate interface list ({count} entries)"
            ))
        })?;
        for _ in 0..count {
            interfaces.push(r.read_reference_type_id(&sizes)?);
        }
        Ok(interfaces)
    }

    #[allow(dead_code)]
    pub(crate) async fn reference_type_interfaces_cached(
        &self,
        type_id: ReferenceTypeId,
    ) -> Result<Vec<ReferenceTypeId>> {
        {
            let cache = self.inner.inspect_cache.lock().await;
            if let Some(interfaces) = cache.interfaces.get(&type_id) {
                return Ok(interfaces.clone());
            }
        }

        let interfaces = self.reference_type_interfaces(type_id).await?;
        let mut cache = self.inner.inspect_cache.lock().await;
        cache.interfaces.insert(type_id, interfaces.clone());
        Ok(interfaces)
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
        let mut methods = Vec::new();
        methods.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!("unable to allocate method list ({count} entries)"))
        })?;
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

    /// ReferenceType.MethodsWithGeneric (2, 15)
    pub async fn reference_type_methods_with_generic(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<Vec<MethodInfoWithGeneric>> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        let payload = self.send_command_raw(2, 15, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut methods = Vec::new();
        methods.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!("unable to allocate method list ({count} entries)"))
        })?;
        for _ in 0..count {
            let method_id = r.read_id(sizes.method_id)?;
            let name = r.read_string()?;
            let signature = r.read_string()?;
            let generic = r.read_string()?;
            let mod_bits = r.read_u32()?;
            methods.push(MethodInfoWithGeneric {
                method_id,
                name,
                signature,
                generic_signature: (!generic.is_empty()).then_some(generic),
                mod_bits,
            });
        }
        Ok(methods)
    }

    #[allow(dead_code)]
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

    #[allow(dead_code)]
    pub(crate) async fn reference_type_methods_with_generic_cached(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<Vec<MethodInfoWithGeneric>> {
        {
            let cache = self.inner.inspect_cache.lock().await;
            if let Some(methods) = cache.methods_with_generic.get(&class_id) {
                return Ok(methods.clone());
            }
        }

        let methods = self.reference_type_methods_with_generic(class_id).await?;
        let mut cache = self.inner.inspect_cache.lock().await;
        cache.methods_with_generic.insert(class_id, methods.clone());
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
        let mut lines = Vec::new();
        lines.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!("unable to allocate line table ({count} entries)"))
        })?;
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
        let mut vars = Vec::new();
        vars.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate variable table ({count} entries)"
            ))
        })?;
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

    /// Method.VariableTableWithGeneric (6, 5)
    pub async fn method_variable_table_with_generic(
        &self,
        class_id: ReferenceTypeId,
        method_id: MethodId,
    ) -> Result<(u32, Vec<VariableInfoWithGeneric>)> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        w.write_id(method_id, sizes.method_id);
        let payload = self.send_command_raw(6, 5, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let arg_count = r.read_u32()?;
        let count = r.read_u32()? as usize;
        let mut vars = Vec::new();
        vars.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate variable table ({count} entries)"
            ))
        })?;
        for _ in 0..count {
            let code_index = r.read_u64()?;
            let name = r.read_string()?;
            let signature = r.read_string()?;
            let generic = r.read_string()?;
            let length = r.read_u32()?;
            let slot = r.read_u32()?;
            vars.push(VariableInfoWithGeneric {
                code_index,
                name,
                signature,
                generic_signature: (!generic.is_empty()).then_some(generic),
                length,
                slot,
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

        let payload_len = sizes
            .object_id
            .checked_add(4usize.saturating_add(name.len()))
            .and_then(|v| v.checked_add(4)) // bytecodeLen
            .and_then(|v| v.checked_add(bytecode.len()))
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        let length = HEADER_LEN
            .checked_add(payload_len)
            .ok_or_else(|| JdwpError::Protocol("packet too large".to_string()))?;
        if length > crate::MAX_JDWP_PACKET_BYTES {
            return Err(JdwpError::Protocol(format!(
                "packet too large ({length} bytes, max {})",
                crate::MAX_JDWP_PACKET_BYTES
            )));
        }

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
        let mut values = Vec::new();
        values.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate stack frame value list ({count} entries)"
            ))
        })?;
        for _ in 0..count {
            let tag = r.read_u8()?;
            values.push(r.read_value(tag, &sizes)?);
        }
        Ok(values)
    }

    /// StackFrame.SetValues (16, 2)
    pub async fn stack_frame_set_values(
        &self,
        thread: ThreadId,
        frame_id: FrameId,
        values: &[(u32, JdwpValue)],
    ) -> Result<()> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(thread, &sizes);
        w.write_id(frame_id, sizes.frame_id);
        w.write_u32(values.len() as u32);
        for (slot, value) in values {
            w.write_u32(*slot);
            w.write_tagged_value(value, &sizes);
        }
        let _ = self.send_command_raw(16, 2, w.into_vec()).await?;
        Ok(())
    }

    /// ClassType.Superclass (3, 1)
    pub async fn class_type_superclass(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<ReferenceTypeId> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        let payload = self.send_command_raw(3, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_reference_type_id(&sizes)
    }

    pub(crate) async fn class_type_superclass_cached(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<ReferenceTypeId> {
        {
            let cache = self.inner.inspect_cache.lock().await;
            if let Some(superclass) = cache.superclasses.get(&class_id) {
                return Ok(*superclass);
            }
        }

        let superclass = self.class_type_superclass(class_id).await?;
        let mut cache = self.inner.inspect_cache.lock().await;
        cache.superclasses.insert(class_id, superclass);
        Ok(superclass)
    }

    /// ClassType.SetValues (3, 2)
    pub async fn class_type_set_values(
        &self,
        class_id: ReferenceTypeId,
        values: &[(FieldId, JdwpValue)],
    ) -> Result<()> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        w.write_u32(values.len() as u32);
        for (field_id, value) in values {
            w.write_id(*field_id, sizes.field_id);
            w.write_tagged_value(value, &sizes);
        }
        let _ = self.send_command_raw(3, 2, w.into_vec()).await?;
        Ok(())
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
        // The JDWP specification (and HotSpot) represent invoke exceptions as a tagged-objectID
        // (`tag` byte followed by an object ID). Some embedded implementations return the legacy
        // untagged object ID. Decode both formats for compatibility.
        let exception = match r.remaining() {
            rem if rem == sizes.object_id => r.read_object_id(&sizes)?,
            rem if rem == sizes.object_id + 1 => {
                let (_tag, id) = r.read_tagged_object_id(&sizes)?;
                id
            }
            rem if rem >= sizes.object_id + 1 => {
                let (_tag, id) = r.read_tagged_object_id(&sizes)?;
                id
            }
            rem => {
                return Err(JdwpError::Protocol(format!(
                    "invalid ClassType.InvokeMethod exception payload length: {rem}"
                )));
            }
        };
        Ok((return_value, exception))
    }

    /// ClassType.NewInstance (3, 4)
    pub async fn class_type_new_instance(
        &self,
        class_id: ReferenceTypeId,
        thread: ThreadId,
        ctor_method: MethodId,
        args: &[JdwpValue],
        options: u32,
    ) -> Result<(ObjectId, ObjectId)> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        w.write_object_id(thread, &sizes);
        w.write_id(ctor_method, sizes.method_id);
        w.write_u32(args.len() as u32);
        for arg in args {
            w.write_tagged_value(arg, &sizes);
        }
        w.write_u32(options);
        let payload = self.send_command_raw(3, 4, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let new_object = r.read_object_id(&sizes)?;
        let exception = match r.remaining() {
            rem if rem == sizes.object_id => r.read_object_id(&sizes)?,
            rem if rem == sizes.object_id + 1 => {
                let (_tag, id) = r.read_tagged_object_id(&sizes)?;
                id
            }
            rem if rem >= sizes.object_id + 1 => {
                let (_tag, id) = r.read_tagged_object_id(&sizes)?;
                id
            }
            rem => {
                return Err(JdwpError::Protocol(format!(
                    "invalid ClassType.NewInstance exception payload length: {rem}"
                )));
            }
        };
        Ok((new_object, exception))
    }

    /// ArrayType.NewInstance (4, 1)
    pub async fn array_type_new_instance(
        &self,
        array_type_id: ReferenceTypeId,
        length: i32,
    ) -> Result<ObjectId> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(array_type_id, &sizes);
        w.write_i32(length);
        let payload = self.send_command_raw(4, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        r.read_object_id(&sizes)
    }

    /// InterfaceType.InvokeMethod (5, 1)
    pub async fn interface_type_invoke_method(
        &self,
        interface_id: ReferenceTypeId,
        thread: ThreadId,
        method_id: MethodId,
        args: &[JdwpValue],
        options: u32,
    ) -> Result<(JdwpValue, ObjectId)> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(interface_id, &sizes);
        w.write_object_id(thread, &sizes);
        w.write_id(method_id, sizes.method_id);
        w.write_u32(args.len() as u32);
        for arg in args {
            w.write_tagged_value(arg, &sizes);
        }
        w.write_u32(options);
        let payload = self.send_command_raw(5, 1, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let return_value = r.read_tagged_value(&sizes)?;
        let exception = match r.remaining() {
            rem if rem == sizes.object_id => r.read_object_id(&sizes)?,
            rem if rem == sizes.object_id + 1 => {
                let (_tag, id) = r.read_tagged_object_id(&sizes)?;
                id
            }
            rem if rem >= sizes.object_id + 1 => {
                let (_tag, id) = r.read_tagged_object_id(&sizes)?;
                id
            }
            rem => {
                return Err(JdwpError::Protocol(format!(
                    "invalid InterfaceType.InvokeMethod exception payload length: {rem}"
                )));
            }
        };
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
        let mut fields = Vec::new();
        fields.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!("unable to allocate field list ({count} entries)"))
        })?;
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

    /// ReferenceType.FieldsWithGeneric (2, 14)
    pub async fn reference_type_fields_with_generic(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<Vec<FieldInfoWithGeneric>> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_reference_type_id(class_id, &sizes);
        let payload = self.send_command_raw(2, 14, w.into_vec()).await?;
        let mut r = JdwpReader::new(&payload);
        let count = r.read_u32()? as usize;
        let mut fields = Vec::new();
        fields.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!("unable to allocate field list ({count} entries)"))
        })?;
        for _ in 0..count {
            let field_id = r.read_id(sizes.field_id)?;
            let name = r.read_string()?;
            let signature = r.read_string()?;
            let generic = r.read_string()?;
            let mod_bits = r.read_u32()?;
            fields.push(FieldInfoWithGeneric {
                field_id,
                name,
                signature,
                generic_signature: (!generic.is_empty()).then_some(generic),
                mod_bits,
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

    #[allow(dead_code)]
    pub(crate) async fn reference_type_fields_with_generic_cached(
        &self,
        class_id: ReferenceTypeId,
    ) -> Result<Vec<FieldInfoWithGeneric>> {
        {
            let cache = self.inner.inspect_cache.lock().await;
            if let Some(fields) = cache.fields_with_generic.get(&class_id) {
                return Ok(fields.clone());
            }
        }

        let fields = self.reference_type_fields_with_generic(class_id).await?;
        let mut cache = self.inner.inspect_cache.lock().await;
        cache.fields_with_generic.insert(class_id, fields.clone());
        Ok(fields)
    }

    /// Returns all non-static fields declared on `type_id` and its superclasses.
    ///
    /// Ordering: fields declared on the most-derived class come first, followed by each superclass
    /// up to (but excluding) the null superclass (`0`).
    ///
    /// If multiple classes declare a field with the same name, the most-derived field wins.
    pub(crate) async fn reference_type_all_instance_fields_cached(
        &self,
        type_id: ReferenceTypeId,
    ) -> Result<Vec<FieldInfo>> {
        {
            let cache = self.inner.inspect_cache.lock().await;
            if let Some(fields) = cache.all_instance_fields.get(&type_id) {
                return Ok(fields.clone());
            }
        }

        let mut hierarchy = Vec::new();
        let mut seen_types = HashSet::new();
        let mut current = type_id;
        loop {
            if current == 0 || !seen_types.insert(current) {
                break;
            }
            hierarchy.push(current);
            let superclass = self.class_type_superclass_cached(current).await?;
            if superclass == 0 {
                break;
            }
            current = superclass;
        }

        let mut seen_names = HashSet::new();
        let mut out = Vec::new();
        for class_id in hierarchy {
            for field in self.reference_type_fields_cached(class_id).await? {
                if field.mod_bits & FIELD_MODIFIER_STATIC != 0 {
                    continue;
                }
                if seen_names.insert(field.name.clone()) {
                    out.push(field);
                }
            }
        }

        let mut cache = self.inner.inspect_cache.lock().await;
        cache.all_instance_fields.insert(type_id, out.clone());
        Ok(out)
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
        let mut values = Vec::new();
        values.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate static field value list ({count} entries)"
            ))
        })?;
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
        let mut values = Vec::new();
        values.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate object field value list ({count} entries)"
            ))
        })?;
        for _ in 0..count {
            let tag = r.read_u8()?;
            values.push(r.read_value(tag, &sizes)?);
        }
        Ok(values)
    }

    /// ObjectReference.SetValues (9, 3)
    pub async fn object_reference_set_values(
        &self,
        object_id: ObjectId,
        values: &[(FieldId, JdwpValue)],
    ) -> Result<()> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(object_id, &sizes);
        w.write_u32(values.len() as u32);
        for (field_id, value) in values {
            w.write_id(*field_id, sizes.field_id);
            w.write_tagged_value(value, &sizes);
        }
        let _ = self.send_command_raw(9, 3, w.into_vec()).await?;
        Ok(())
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
        let exception = match r.remaining() {
            rem if rem == sizes.object_id => r.read_object_id(&sizes)?,
            rem if rem == sizes.object_id + 1 => {
                let (_tag, id) = r.read_tagged_object_id(&sizes)?;
                id
            }
            rem if rem >= sizes.object_id + 1 => {
                let (_tag, id) = r.read_tagged_object_id(&sizes)?;
                id
            }
            rem => {
                return Err(JdwpError::Protocol(format!(
                    "invalid ObjectReference.InvokeMethod exception payload length: {rem}"
                )));
            }
        };
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
        let mut waiters = Vec::new();
        waiters.try_reserve_exact(waiter_count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate monitor waiter list ({waiter_count} entries)"
            ))
        })?;
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
        // The JDWP spec includes an element tag in the reply. For primitive arrays, the
        // elements are encoded without per-value tags (the element tag determines the wire
        // encoding). For object arrays, VMs (including HotSpot) still encode each element as a
        // tagged value so the debugger can distinguish Strings/arrays/etc.
        let element_tag = r.read_u8()?;
        let count = r.read_u32()? as usize;
        let mut values = Vec::new();
        values.try_reserve_exact(count).map_err(|_| {
            JdwpError::Protocol(format!(
                "unable to allocate array value list ({count} entries)"
            ))
        })?;

        match element_tag {
            // Primitive arrays return untagged primitives.
            b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => {
                for _ in 0..count {
                    values.push(r.read_value(element_tag, &sizes)?);
                }
            }
            // Object arrays vary between implementations:
            // - HotSpot uses `tagged-objectID` values (each element carries its own tag).
            // - The mock server (and some embedded stacks) use raw object IDs.
            _ => {
                let remaining = r.remaining();
                let tagged_len = count.saturating_mul(1usize.saturating_add(sizes.object_id));
                let untagged_len = count.saturating_mul(sizes.object_id);

                if remaining == tagged_len || (remaining >= tagged_len && tagged_len != 0) {
                    for _ in 0..count {
                        let (elem_tag, elem_id) = r.read_tagged_object_id(&sizes)?;
                        values.push(JdwpValue::Object {
                            tag: elem_tag,
                            id: elem_id,
                        });
                    }
                } else if remaining == untagged_len
                    || (remaining >= untagged_len && untagged_len != 0)
                {
                    for _ in 0..count {
                        values.push(r.read_value(element_tag, &sizes)?);
                    }
                } else {
                    return Err(JdwpError::Protocol(format!(
                        "unexpected ArrayReference.GetValues payload length: tag={element_tag} count={count} remaining={remaining}"
                    )));
                }
            }
        }

        Ok(values)
    }

    /// ArrayReference.SetValues (13, 3)
    pub async fn array_reference_set_values(
        &self,
        array_id: ObjectId,
        first_index: i32,
        values: &[JdwpValue],
    ) -> Result<()> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_object_id(array_id, &sizes);
        w.write_i32(first_index);
        w.write_u32(values.len() as u32);
        for value in values {
            w.write_tagged_value(value, &sizes);
        }
        let _ = self.send_command_raw(13, 3, w.into_vec()).await?;
        Ok(())
    }

    pub async fn vm_resume(&self) -> Result<()> {
        let _ = self.send_command_raw(1, 9, Vec::new()).await?;
        Ok(())
    }

    pub async fn vm_suspend(&self) -> Result<()> {
        let _ = self.send_command_raw(1, 8, Vec::new()).await?;
        Ok(())
    }

    /// VirtualMachine.Dispose (1, 6)
    ///
    /// Detach from the target VM and dispose the JDWP session.
    ///
    /// Note: this is distinct from [`JdwpClient::shutdown`], which is a local-only cancellation.
    pub async fn virtual_machine_dispose(&self) -> Result<()> {
        let _ = self.send_command_raw(1, 6, Vec::new()).await?;
        // After a successful dispose the JDWP session is no longer usable, so shut down the
        // local client to unblock pending requests and stop the reader task.
        self.shutdown();
        Ok(())
    }

    /// VirtualMachine.Exit (1, 10)
    ///
    /// Request that the target VM terminates with the given exit code.
    pub async fn virtual_machine_exit(&self, exit_code: i32) -> Result<()> {
        let mut w = JdwpWriter::new();
        w.write_i32(exit_code);
        let _ = self.send_command_raw(1, 10, w.into_vec()).await?;
        Ok(())
    }

    /// VirtualMachine.DisposeObjects (1, 14)
    ///
    /// Dispose of object IDs held by the debugger. Each tuple in `objects` contains
    /// `(objectId, refCnt)`.
    pub async fn virtual_machine_dispose_objects(&self, objects: &[(ObjectId, u32)]) -> Result<()> {
        let sizes = self.id_sizes().await;
        let mut w = JdwpWriter::new();
        w.write_u32(objects.len() as u32);
        for (object_id, ref_cnt) in objects {
            w.write_object_id(*object_id, &sizes);
            w.write_u32(*ref_cnt);
        }
        let _ = self.send_command_raw(1, 14, w.into_vec()).await?;
        Ok(())
    }

    /// VirtualMachine.HoldEvents (1, 15)
    ///
    /// Temporarily stops event delivery from the target VM.
    pub async fn virtual_machine_hold_events(&self) -> Result<()> {
        let _ = self.send_command_raw(1, 15, Vec::new()).await?;
        Ok(())
    }

    /// VirtualMachine.ReleaseEvents (1, 16)
    ///
    /// Re-enables event delivery after a prior [`JdwpClient::virtual_machine_hold_events`].
    pub async fn virtual_machine_release_events(&self) -> Result<()> {
        let _ = self.send_command_raw(1, 16, Vec::new()).await?;
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

    /// EventRequest.ClearAllBreakpoints (15, 3)
    pub async fn event_request_clear_all_breakpoints(&self) -> Result<()> {
        let _ = self.send_command_raw(15, 3, Vec::new()).await?;
        Ok(())
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
    const ERROR_UNSUPPORTED_VERSION: u16 = 68;
    const ERROR_NOT_IMPLEMENTED: u16 = 99;

    matches!(
        err,
        JdwpError::VmError(ERROR_NOT_FOUND | ERROR_UNSUPPORTED_VERSION | ERROR_NOT_IMPLEMENTED)
    )
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
    // If the peer sends malformed JDWP frames (e.g. invalid length prefixes), wake all pending
    // requests with a protocol error to preserve an actionable reason for the disconnect.
    //
    // For IO errors / graceful disconnects we keep returning `ConnectionClosed`, which is what most
    // callers expect for remote shutdowns.
    let mut terminated_with_error: Option<JdwpError> = None;

    loop {
        let mut header = [0u8; HEADER_LEN];
        let header_read = tokio::select! {
            _ = inner.shutdown.cancelled() => break,
            res = reader.read_exact(&mut header) => res,
        };
        if let Err(err) = header_read {
            terminated_with_error = Some(JdwpError::Io(err));
            break;
        }

        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        if let Err(msg) = crate::validate_jdwp_packet_length(length) {
            terminated_with_error = Some(JdwpError::Protocol(msg));
            break;
        }

        let id = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        let flags = header[8];
        let payload_len = length - HEADER_LEN;
        let mut payload = Vec::new();
        if payload.try_reserve_exact(payload_len).is_err() {
            terminated_with_error = Some(JdwpError::Protocol(format!(
                "unable to allocate packet buffer ({payload_len} bytes)"
            )));
            break;
        }
        payload.resize(payload_len, 0);
        let payload_read = tokio::select! {
            _ = inner.shutdown.cancelled() => break,
            res = reader.read_exact(&mut payload) => res,
        };
        if let Err(err) = payload_read {
            terminated_with_error = Some(JdwpError::Io(err));
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
                if let Err(err) = handle_event_packet(&inner, &payload).await {
                    terminated_with_error = Some(err);
                    break;
                }
            } else {
                // Unknown command packets are ignored (we don't implement VM->debugger commands other than events).
                let _ = (id, command_set, command, payload);
            }
        }
    }

    if let Some(err) = terminated_with_error {
        let pending = {
            let mut pending = inner.pending.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *pending)
        };
        for (_id, tx) in pending {
            let err = match &err {
                JdwpError::Protocol(msg) => JdwpError::Protocol(msg.clone()),
                _ => JdwpError::ConnectionClosed,
            };
            let _ = tx.send(Err(err));
        }
    }

    // Cancel the shutdown token after waking any pending requests. This avoids racy `Cancelled`
    // results for in-flight request futures when the connection terminates unexpectedly.
    inner.shutdown.cancel();
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
            // `Vec::push` can abort the process on OOM when it needs to grow the backing buffer.
            // Use fallible reservation so a malicious VM cannot trigger an allocation abort by
            // stuffing a large number of stop events into a single composite packet.
            if stop_events.len() == stop_events.capacity() {
                stop_events.try_reserve(1).map_err(|_| {
                    JdwpError::Protocol("unable to allocate stop event buffer".to_string())
                })?;
            }
            stop_events.push(event);
        } else {
            let _ = inner.events.send(event.clone());
            let _ = inner.event_envelopes.send(JdwpEventEnvelope {
                suspend_policy,
                event,
            });
        }
    }

    for event in stop_events {
        let _ = inner.events.send(event.clone());
        let _ = inner.event_envelopes.send(JdwpEventEnvelope {
            suspend_policy,
            event,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{EventModifier, JdwpClient, JdwpClientConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::wire::mock::{
        DelayedReply, MockEventRequestModifier, MockJdwpServer, MockJdwpServerConfig,
        NESTED_THREAD_GROUP_ID, THREAD_ID, TOP_LEVEL_THREAD_GROUP_ID, TOP_LEVEL_THREAD_GROUP_NAME,
        WORKER_THREAD_ID,
    };
    use crate::wire::types::{
        JdwpCapabilitiesNew, JdwpError, JdwpEvent, JdwpIdSizes, Location, EVENT_KIND_BREAKPOINT,
        EVENT_KIND_CLASS_PREPARE, EVENT_KIND_CLASS_UNLOAD, EVENT_KIND_EXCEPTION,
        EVENT_KIND_FIELD_ACCESS, EVENT_KIND_FIELD_MODIFICATION,
        EVENT_KIND_METHOD_EXIT_WITH_RETURN_VALUE, EVENT_KIND_VM_DISCONNECT, INVOKE_NONVIRTUAL,
        INVOKE_SINGLE_THREADED, SUSPEND_POLICY_ALL, SUSPEND_POLICY_EVENT_THREAD,
        SUSPEND_POLICY_NONE,
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
    async fn connect_aborts_on_oversized_packet_length_prefix() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            // JDWP handshake.
            let mut hs = [0u8; crate::wire::codec::HANDSHAKE.len()];
            socket.read_exact(&mut hs).await.unwrap();
            socket
                .write_all(crate::wire::codec::HANDSHAKE)
                .await
                .unwrap();

            // Read the first command packet from the debugger (VirtualMachine.IDSizes).
            // This ensures the client has a pending request before we inject the oversized
            // packet that should terminate the read loop.
            let mut header = [0u8; crate::wire::codec::HEADER_LEN];
            socket.read_exact(&mut header).await.unwrap();
            let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
            assert!(
                (crate::wire::codec::HEADER_LEN..=crate::MAX_JDWP_PACKET_BYTES).contains(&length)
            );
            let payload_len = length - crate::wire::codec::HEADER_LEN;
            if payload_len > 0 {
                let mut payload = Vec::new();
                payload.try_reserve_exact(payload_len).unwrap();
                payload.resize(payload_len, 0);
                socket.read_exact(&mut payload).await.unwrap();
            }

            // Inject an oversized JDWP packet header.
            let oversize = (crate::MAX_JDWP_PACKET_BYTES + 1) as u32;
            let mut header = [0u8; crate::wire::codec::HEADER_LEN];
            header[0..4].copy_from_slice(&oversize.to_be_bytes());
            socket.write_all(&header).await.unwrap();
        });

        let config = JdwpClientConfig {
            // Keep the test fast even if something regresses.
            reply_timeout: Duration::from_secs(1),
            ..Default::default()
        };

        let res = tokio::time::timeout(
            Duration::from_secs(2),
            JdwpClient::connect_with_config(addr, config),
        )
        .await
        .expect("connect should not hang");

        let err = match res {
            Ok(_client) => panic!("expected connect to fail"),
            Err(err) => err,
        };
        let expected = format!(
            "JDWP packet length {} exceeds maximum allowed ({} bytes); refusing to allocate",
            crate::MAX_JDWP_PACKET_BYTES + 1,
            crate::MAX_JDWP_PACKET_BYTES
        );
        match err {
            JdwpError::Protocol(msg) => assert_eq!(msg, expected),
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_aborts_on_invalid_packet_length_prefix() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            // JDWP handshake.
            let mut hs = [0u8; crate::wire::codec::HANDSHAKE.len()];
            socket.read_exact(&mut hs).await.unwrap();
            socket
                .write_all(crate::wire::codec::HANDSHAKE)
                .await
                .unwrap();

            // Read the first command packet from the debugger (VirtualMachine.IDSizes).
            // This ensures the client has a pending request before we inject the malformed
            // packet that should terminate the read loop.
            let mut header = [0u8; crate::wire::codec::HEADER_LEN];
            socket.read_exact(&mut header).await.unwrap();
            let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
            assert!(
                (crate::wire::codec::HEADER_LEN..=crate::MAX_JDWP_PACKET_BYTES).contains(&length)
            );
            let payload_len = length - crate::wire::codec::HEADER_LEN;
            if payload_len > 0 {
                let mut payload = Vec::new();
                payload.try_reserve_exact(payload_len).unwrap();
                payload.resize(payload_len, 0);
                socket.read_exact(&mut payload).await.unwrap();
            }

            // Inject an invalid JDWP packet header (`length < HEADER_LEN`).
            let invalid_len = (crate::wire::codec::HEADER_LEN - 1) as u32;
            let mut header = [0u8; crate::wire::codec::HEADER_LEN];
            header[0..4].copy_from_slice(&invalid_len.to_be_bytes());
            socket.write_all(&header).await.unwrap();
        });

        let config = JdwpClientConfig {
            // Keep the test fast even if something regresses.
            reply_timeout: Duration::from_secs(1),
            ..Default::default()
        };

        let res = tokio::time::timeout(
            Duration::from_secs(2),
            JdwpClient::connect_with_config(addr, config),
        )
        .await
        .expect("connect should not hang");

        let err = match res {
            Ok(_client) => panic!("expected connect to fail"),
            Err(err) => err,
        };

        let expected = format!(
            "invalid packet length {}",
            crate::wire::codec::HEADER_LEN - 1
        );
        match err {
            JdwpError::Protocol(msg) => assert_eq!(msg, expected),
            other => panic!("expected Protocol error, got {other:?}"),
        }
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

        let expected = JdwpCapabilitiesNew {
            can_watch_field_modification: true,
            can_get_monitor_info: true,
            // Legacy `VirtualMachine.Capabilities` cannot report `can_redefine_classes`.
            can_redefine_classes: false,
            ..Default::default()
        };

        assert_eq!(caps, expected);
    }

    #[tokio::test]
    async fn connect_falls_back_to_legacy_capabilities_when_capabilities_new_is_not_found() {
        let mut capabilities = vec![false; 32];
        capabilities[2] = true; // canGetBytecodes
        capabilities[3] = true; // canGetSyntheticAttribute

        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            capabilities,
            capabilities_new_error_code: Some(41), // NOT_FOUND
            ..Default::default()
        })
        .await
        .unwrap();

        let client = JdwpClient::connect(server.addr()).await.unwrap();
        let caps = client.capabilities().await;

        let expected = JdwpCapabilitiesNew {
            can_get_bytecodes: true,
            can_get_synthetic_attribute: true,
            ..Default::default()
        };

        assert_eq!(caps, expected);
    }

    #[tokio::test]
    async fn connect_falls_back_to_legacy_capabilities_when_capabilities_new_is_unsupported_version(
    ) {
        let mut capabilities = vec![false; 32];
        capabilities[1] = true; // canWatchFieldAccess
        capabilities[5] = true; // canGetCurrentContendedMonitor

        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            capabilities,
            capabilities_new_error_code: Some(68), // UNSUPPORTED_VERSION
            ..Default::default()
        })
        .await
        .unwrap();

        let client = JdwpClient::connect(server.addr()).await.unwrap();
        let caps = client.capabilities().await;

        let expected = JdwpCapabilitiesNew {
            can_watch_field_access: true,
            can_get_current_contended_monitor: true,
            ..Default::default()
        };

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
    async fn connect_succeeds_when_both_capability_commands_are_not_found() {
        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            capabilities_new_error_code: Some(41),    // NOT_FOUND
            capabilities_legacy_error_code: Some(41), // NOT_FOUND
            ..Default::default()
        })
        .await
        .unwrap();

        let client = JdwpClient::connect(server.addr()).await.unwrap();
        assert_eq!(client.capabilities().await, JdwpCapabilitiesNew::default());
    }

    #[tokio::test]
    async fn connect_succeeds_when_both_capability_commands_report_unsupported_version() {
        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            capabilities_new_error_code: Some(68),    // UNSUPPORTED_VERSION
            capabilities_legacy_error_code: Some(68), // UNSUPPORTED_VERSION
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

        let contended = client
            .thread_current_contended_monitor(thread)
            .await
            .unwrap();
        assert_eq!(contended, 0x5203);

        let info = client
            .object_reference_monitor_info(contended)
            .await
            .unwrap();
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

        let info = client
            .object_reference_monitor_info(contended)
            .await
            .unwrap();
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
            JdwpEvent::Breakpoint {
                request_id: rid, ..
            } => assert_eq!(rid, request_id),
            other => panic!("expected breakpoint event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn class_prepare_event_marks_main_class_as_loaded() {
        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            class_prepare_events: 1,
            all_classes_initially_loaded: false,
            ..Default::default()
        })
        .await
        .unwrap();

        let client = JdwpClient::connect(server.addr()).await.unwrap();
        let mut events = client.subscribe_events();

        assert_eq!(client.all_classes().await.unwrap().len(), 0);

        let request_id = client
            .event_request_set(EVENT_KIND_CLASS_PREPARE, SUSPEND_POLICY_NONE, Vec::new())
            .await
            .unwrap();

        client.vm_resume().await.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("timed out waiting for ClassPrepare")
            .expect("failed to receive ClassPrepare");

        let prepared_type_id = match event {
            JdwpEvent::ClassPrepare {
                request_id: rid,
                thread,
                ref_type_tag,
                type_id,
                signature,
                status,
            } => {
                assert_eq!(rid, request_id);
                assert_eq!(thread, THREAD_ID);
                assert_eq!(ref_type_tag, 1);
                assert_eq!(signature, "LMain;");
                assert_ne!(status, 0);
                type_id
            }
            other => panic!("expected ClassPrepare, got {other:?}"),
        };

        let classes = client.all_classes().await.unwrap();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].type_id, prepared_type_id);
        assert_eq!(classes[0].signature, "LMain;");
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
            JdwpEvent::Breakpoint { request_id, .. } => {
                assert_eq!(request_id, breakpoint_request_id)
            }
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
        let last = requests
            .last()
            .cloned()
            .expect("no EventRequest.Set observed");

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

    #[tokio::test]
    async fn virtual_machine_create_string_round_trips_through_string_reference_value() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let string_id = client.virtual_machine_create_string("hello").await.unwrap();
        let value = client.string_reference_value(string_id).await.unwrap();
        assert_eq!(value, "hello");

        let calls = server.create_string_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].value, "hello");
        assert_eq!(calls[0].returned_id, string_id);
    }

    #[tokio::test]
    async fn set_values_commands_are_encoded_and_mutate_mock_state() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let thread = client.all_threads().await.unwrap()[0];
        let frame = client.frames(thread, 0, 1).await.unwrap()[0];

        // StackFrame.SetValues
        client
            .stack_frame_set_values(
                thread,
                frame.frame_id,
                &[
                    (0, JdwpValue::Int(123)),
                    (2, JdwpValue::Object { tag: b's', id: 0 }),
                ],
            )
            .await
            .unwrap();

        let calls = server.stack_frame_set_values_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].thread, thread);
        assert_eq!(calls[0].frame_id, frame.frame_id);
        assert_eq!(
            calls[0].values,
            vec![
                (0, JdwpValue::Int(123)),
                (2, JdwpValue::Object { tag: b's', id: 0 }),
            ]
        );

        // Ensure SetValues actually affects StackFrame.GetValues in the mock.
        let values = client
            .stack_frame_get_values(
                thread,
                frame.frame_id,
                &[(0, "I".to_string()), (2, "Ljava/lang/String;".to_string())],
            )
            .await
            .unwrap();
        assert_eq!(values[0], JdwpValue::Int(123));
        assert_eq!(values[1], JdwpValue::Object { tag: b's', id: 0 });

        // ObjectReference.SetValues round-trip via ObjectReference.GetValues.
        let this_object = client
            .stack_frame_this_object(thread, frame.frame_id)
            .await
            .unwrap();
        let (_ref_type_tag, class_id) = client
            .object_reference_reference_type(this_object)
            .await
            .unwrap();
        let fields = client.reference_type_fields(class_id).await.unwrap();
        let field_id = fields[0].field_id;

        let before = client
            .object_reference_get_values(this_object, &[field_id])
            .await
            .unwrap();
        assert_eq!(before, vec![JdwpValue::Int(7)]);

        client
            .object_reference_set_values(this_object, &[(field_id, JdwpValue::Int(99))])
            .await
            .unwrap();

        let after = client
            .object_reference_get_values(this_object, &[field_id])
            .await
            .unwrap();
        assert_eq!(after, vec![JdwpValue::Int(99)]);

        let calls = server.object_reference_set_values_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].object_id, this_object);
        assert_eq!(calls[0].values, vec![(field_id, JdwpValue::Int(99))]);

        // ClassType.SetValues round-trip via ReferenceType.GetValues.
        client
            .class_type_set_values(class_id, &[(field_id, JdwpValue::Int(77))])
            .await
            .unwrap();
        let values = client
            .reference_type_get_values(class_id, &[field_id])
            .await
            .unwrap();
        assert_eq!(values, vec![JdwpValue::Int(77)]);

        let calls = server.class_type_set_values_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].class_id, class_id);
        assert_eq!(calls[0].values, vec![(field_id, JdwpValue::Int(77))]);

        // ArrayReference.SetValues round-trip via ArrayReference.GetValues.
        let array_id = server.sample_int_array_id();
        client
            .array_reference_set_values(array_id, 1, &[JdwpValue::Int(999)])
            .await
            .unwrap();
        let values = client
            .array_reference_get_values(array_id, 0, 5)
            .await
            .unwrap();
        assert_eq!(
            values,
            vec![
                JdwpValue::Int(10),
                JdwpValue::Int(999),
                JdwpValue::Int(30),
                JdwpValue::Int(40),
                JdwpValue::Int(50)
            ]
        );

        let calls = server.array_reference_set_values_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].array_id, array_id);
        assert_eq!(calls[0].first_index, 1);
        assert_eq!(calls[0].values, vec![JdwpValue::Int(999)]);
    }

    #[tokio::test]
    async fn expression_eval_primitives_record_payloads() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let thread = client.all_threads().await.unwrap()[0];
        let frame = client.frames(thread, 0, 1).await.unwrap()[0];
        let class_id = frame.location.class_id;
        let ctor_method = frame.location.method_id;

        let (new_object, exception) = client
            .class_type_new_instance(
                class_id,
                thread,
                ctor_method,
                &[JdwpValue::Int(1), JdwpValue::Object { tag: b'L', id: 0 }],
                INVOKE_SINGLE_THREADED,
            )
            .await
            .unwrap();
        assert_ne!(new_object, 0);
        assert_eq!(exception, 0);

        let calls = server.class_type_new_instance_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].class_id, class_id);
        assert_eq!(calls[0].thread, thread);
        assert_eq!(calls[0].ctor_method, ctor_method);
        assert_eq!(
            calls[0].args,
            vec![JdwpValue::Int(1), JdwpValue::Object { tag: b'L', id: 0 }]
        );
        assert_eq!(calls[0].options, INVOKE_SINGLE_THREADED);
        assert_eq!(calls[0].returned_id, new_object);

        let (_tag, array_type_id) = client
            .object_reference_reference_type(server.sample_int_array_id())
            .await
            .unwrap();
        let new_array = client
            .array_type_new_instance(array_type_id, 4)
            .await
            .unwrap();
        assert_ne!(new_array, 0);
        assert_eq!(client.array_reference_length(new_array).await.unwrap(), 4);

        let calls = server.array_type_new_instance_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].array_type_id, array_type_id);
        assert_eq!(calls[0].length, 4);
        assert_eq!(calls[0].returned_id, new_array);

        let arg = JdwpValue::Int(5);
        let (return_value, exception) = client
            .interface_type_invoke_method(
                0xDEAD_BEEF,
                thread,
                ctor_method,
                std::slice::from_ref(&arg),
                INVOKE_SINGLE_THREADED | INVOKE_NONVIRTUAL,
            )
            .await
            .unwrap();
        assert_eq!(return_value, arg);
        assert_eq!(exception, 0);

        let calls = server.interface_type_invoke_method_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].interface_id, 0xDEAD_BEEF);
        assert_eq!(calls[0].thread, thread);
        assert_eq!(calls[0].method_id, ctor_method);
        assert_eq!(calls[0].args, vec![arg]);
        assert_eq!(calls[0].options, INVOKE_SINGLE_THREADED | INVOKE_NONVIRTUAL);
    }

    #[tokio::test]
    async fn signature_with_generic_decodes_generic_signature() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let main_id = client
            .classes_by_signature("LMain;")
            .await
            .unwrap()
            .first()
            .unwrap()
            .type_id;

        let (signature, generic) = client
            .reference_type_signature_with_generic(main_id)
            .await
            .unwrap();
        assert_eq!(signature, "LMain;");
        assert_eq!(
            generic.as_deref(),
            Some("Ljava/util/List<Ljava/lang/String;>;")
        );

        let foo_id = client
            .classes_by_signature("Lcom/example/Foo;")
            .await
            .unwrap()
            .first()
            .unwrap()
            .type_id;
        let (_signature, generic) = client
            .reference_type_signature_with_generic(foo_id)
            .await
            .unwrap();
        assert_eq!(generic, None);
    }

    #[tokio::test]
    async fn fields_with_generic_include_generic_signature() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let main_id = client
            .classes_by_signature("LMain;")
            .await
            .unwrap()
            .first()
            .unwrap()
            .type_id;

        let fields = client
            .reference_type_fields_with_generic(main_id)
            .await
            .unwrap();

        assert_eq!(fields.len(), 3);

        let strings = fields.iter().find(|field| field.name == "strings").unwrap();
        assert_eq!(strings.signature, "Ljava/util/List;");
        assert_eq!(
            strings.generic_signature.as_deref(),
            Some("Ljava/util/List<Ljava/lang/String;>;")
        );

        let count = fields.iter().find(|field| field.name == "count").unwrap();
        assert_eq!(count.signature, "I");
        assert_eq!(count.generic_signature, None);

        let static_field = fields
            .iter()
            .find(|field| field.name == "staticField")
            .unwrap();
        assert_eq!(static_field.signature, "I");
        assert_eq!(static_field.generic_signature, None);
    }

    #[tokio::test]
    async fn methods_with_generic_include_generic_signature() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let main_id = client
            .classes_by_signature("LMain;")
            .await
            .unwrap()
            .first()
            .unwrap()
            .type_id;

        let methods = client
            .reference_type_methods_with_generic(main_id)
            .await
            .unwrap();

        let accept_list = methods.iter().find(|m| m.name == "acceptList").unwrap();
        assert_eq!(accept_list.signature, "(Ljava/util/List;)V");
        assert_eq!(
            accept_list.generic_signature.as_deref(),
            Some("(Ljava/util/List<Ljava/lang/String;>;)V")
        );
    }

    #[tokio::test]
    async fn variable_table_with_generic_includes_generic_signature() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let main_id = client
            .classes_by_signature("LMain;")
            .await
            .unwrap()
            .first()
            .unwrap()
            .type_id;

        let methods = client
            .reference_type_methods_with_generic(main_id)
            .await
            .unwrap();
        let method_id = methods
            .iter()
            .find(|m| m.name == "acceptList")
            .unwrap()
            .method_id;

        let (arg_count, vars) = client
            .method_variable_table_with_generic(main_id, method_id)
            .await
            .unwrap();
        assert_eq!(arg_count, 0);

        let list_var = vars.iter().find(|v| v.name == "list").unwrap();
        assert_eq!(list_var.signature, "Ljava/util/List;");
        assert_eq!(
            list_var.generic_signature.as_deref(),
            Some("Ljava/util/List<Ljava/lang/String;>;")
        );
    }

    #[tokio::test]
    async fn source_debug_extension_reads_smap() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let main_id = client
            .classes_by_signature("LMain;")
            .await
            .unwrap()
            .first()
            .unwrap()
            .type_id;

        let smap = client
            .reference_type_source_debug_extension(main_id)
            .await
            .unwrap();
        assert_eq!(smap, "SMAP\nMain.java\nJava\n*E\n");
    }

    #[tokio::test]
    async fn set_default_stratum_is_recorded_by_mock() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        client
            .virtual_machine_set_default_stratum("Kotlin")
            .await
            .unwrap();

        assert_eq!(
            server.last_default_stratum().await.as_deref(),
            Some("Kotlin")
        );
    }

    #[tokio::test]
    async fn signature_with_generic_is_cached() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let main_id = client
            .classes_by_signature("LMain;")
            .await
            .unwrap()
            .first()
            .unwrap()
            .type_id;

        assert_eq!(server.signature_with_generic_calls(), 0);

        let _ = client
            .reference_type_signature_with_generic(main_id)
            .await
            .unwrap();
        let _ = client
            .reference_type_signature_with_generic(main_id)
            .await
            .unwrap();

        assert_eq!(server.signature_with_generic_calls(), 1);
    }

    #[tokio::test]
    async fn virtual_machine_dispose_cancels_shutdown_token() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let token = client.shutdown_token();
        assert!(!token.is_cancelled());

        client.virtual_machine_dispose().await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), token.cancelled())
            .await
            .expect("shutdown token was not cancelled after VM dispose");

        assert_eq!(server.virtual_machine_dispose_calls(), 1);
    }

    #[tokio::test]
    async fn event_request_clear_all_breakpoints_clears_mock_state() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let request_id = client
            .event_request_set(
                EVENT_KIND_BREAKPOINT,
                SUSPEND_POLICY_EVENT_THREAD,
                Vec::new(),
            )
            .await
            .unwrap();

        assert_eq!(server.breakpoint_request().await, Some(request_id));

        client.event_request_clear_all_breakpoints().await.unwrap();

        assert_eq!(server.breakpoint_request().await, None);
        assert_eq!(server.clear_all_breakpoints_calls(), 1);
    }

    #[tokio::test]
    async fn virtual_machine_dispose_objects_uses_negotiated_id_sizes() {
        let server = MockJdwpServer::spawn_with_config(MockJdwpServerConfig {
            id_sizes: JdwpIdSizes {
                object_id: 4,
                ..JdwpIdSizes::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();

        let client = JdwpClient::connect(server.addr()).await.unwrap();
        client
            .virtual_machine_dispose_objects(&[(0x1122_3344, 1), (0x5566_7788, 2)])
            .await
            .unwrap();

        let calls = server.dispose_objects_calls().await;
        assert_eq!(calls, vec![vec![(0x1122_3344, 1), (0x5566_7788, 2)]]);
    }

    #[tokio::test]
    async fn jdwp_client_can_fetch_thread_groups() {
        let server = MockJdwpServer::spawn().await.unwrap();
        let client = JdwpClient::connect(server.addr()).await.unwrap();

        let groups = client
            .virtual_machine_top_level_thread_groups()
            .await
            .unwrap();
        assert!(!groups.is_empty());
        assert_eq!(groups, vec![TOP_LEVEL_THREAD_GROUP_ID]);

        let group = groups[0];
        let (child_groups, child_threads) =
            client.thread_group_reference_children(group).await.unwrap();
        assert!(child_groups.contains(&NESTED_THREAD_GROUP_ID));
        assert!(child_threads.contains(&THREAD_ID));
        assert!(child_threads.contains(&WORKER_THREAD_ID));

        let thread_group = client
            .thread_reference_thread_group(THREAD_ID)
            .await
            .unwrap();
        assert_eq!(thread_group, group);

        let name = client.thread_group_reference_name(group).await.unwrap();
        assert_eq!(name, TOP_LEVEL_THREAD_GROUP_NAME);
    }
}

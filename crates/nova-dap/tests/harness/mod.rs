#![allow(dead_code)]

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{broadcast, Mutex, Notify},
};

use nova_dap::dap_tokio::{DapReader, DapWriter};

pub mod transcript;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_BUFFERED_MESSAGES: usize = 256;
const MAX_TRANSCRIPT_ENTRIES: usize = 1024;

#[derive(Debug, Clone)]
pub struct StoppedEvent {
    pub thread_id: Option<i64>,
    pub reason: Option<String>,
    pub raw: Value,
}

#[derive(Clone)]
pub struct DapClient {
    inner: Arc<Inner>,
}

struct Inner {
    next_seq: AtomicI64,
    default_timeout: Duration,
    writer: Mutex<DapWriter<Box<dyn AsyncWrite + Unpin + Send>>>,
    inbox: Mutex<VecDeque<Value>>,
    notify: Notify,
    events: broadcast::Sender<Value>,
    transcript: Mutex<Vec<transcript::Entry>>,
}

impl DapClient {
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (events, _) = broadcast::channel(64);

        let inner = Arc::new(Inner {
            next_seq: AtomicI64::new(1),
            default_timeout: DEFAULT_TIMEOUT,
            writer: Mutex::new(DapWriter::new(Box::new(writer))),
            inbox: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            events,
            transcript: Mutex::new(Vec::new()),
        });

        let reader_inner = inner.clone();
        tokio::spawn(async move {
            let mut reader = DapReader::new(reader);
            loop {
                let msg = match reader.read_value().await {
                    Ok(Some(msg)) => msg,
                    Ok(None) => break,
                    Err(_) => break,
                };

                if msg.get("type").and_then(|v| v.as_str()) == Some("event") {
                    // Best-effort publish; tests primarily consume via `wait_for_event`.
                    let _ = reader_inner.events.send(msg.clone());
                }

                let mut inbox = reader_inner.inbox.lock().await;
                if inbox.len() >= MAX_BUFFERED_MESSAGES {
                    inbox.pop_front();
                }
                inbox.push_back(msg);
                drop(inbox);
                reader_inner.notify.notify_waiters();
            }
            reader_inner.notify.notify_waiters();
        });

        Self { inner }
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<Value> {
        self.inner.events.subscribe()
    }

    pub async fn send_request(&self, command: &str, arguments: Value) -> i64 {
        let seq = self.inner.next_seq.fetch_add(1, Ordering::Relaxed);
        let msg = json!({
            "seq": seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        });

        self.record(transcript::Direction::ClientToServer, msg.clone()).await;

        let mut writer = self.inner.writer.lock().await;
        writer
            .write_value(&msg)
            .await
            .unwrap_or_else(|err| panic!("failed to write request {command:?}: {err}"));

        seq
    }

    pub async fn request(&self, command: &str, arguments: Value) -> Value {
        let seq = self.send_request(command, arguments).await;
        self.wait_for_response(seq).await
    }

    pub async fn wait_for_response(&self, request_seq: i64) -> Value {
        self.wait_for_response_with_timeout(request_seq, self.inner.default_timeout)
            .await
    }

    pub async fn wait_for_response_with_timeout(&self, request_seq: i64, timeout: Duration) -> Value {
        self.wait_for_message_with_timeout(
            &format!("response(request_seq={request_seq})"),
            timeout,
            |msg| {
                msg.get("type").and_then(|v| v.as_str()) == Some("response")
                    && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(request_seq)
            },
        )
        .await
    }

    pub async fn wait_for_event(&self, event: &str) -> Value {
        self.wait_for_event_with_timeout(event, self.inner.default_timeout).await
    }

    pub async fn wait_for_event_with_timeout(&self, event: &str, timeout: Duration) -> Value {
        self.wait_for_message_with_timeout(
            &format!("event({event})"),
            timeout,
            |msg| msg.get("type").and_then(|v| v.as_str()) == Some("event") && msg.get("event").and_then(|v| v.as_str()) == Some(event),
        )
        .await
    }

    pub async fn wait_for_event_matching<F>(&self, description: &str, timeout: Duration, predicate: F) -> Value
    where
        F: Fn(&Value) -> bool,
    {
        self.wait_for_message_with_timeout(description, timeout, predicate).await
    }

    async fn wait_for_message_with_timeout<F>(&self, description: &str, timeout: Duration, predicate: F) -> Value
    where
        F: Fn(&Value) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            if let Some(msg) = self.try_take_message(&predicate).await {
                self.record(transcript::Direction::ServerToClient, msg.clone()).await;
                return msg;
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                self.panic_timeout(description, timeout).await;
            }

            let remaining = deadline - now;
            let notified = self.inner.notify.notified();
            let _ = tokio::time::timeout(remaining, notified).await;
        }
    }

    async fn try_take_message<F>(&self, predicate: &F) -> Option<Value>
    where
        F: Fn(&Value) -> bool,
    {
        let mut inbox = self.inner.inbox.lock().await;
        let idx = inbox.iter().position(|msg| predicate(msg))?;
        inbox.remove(idx)
    }

    async fn record(&self, direction: transcript::Direction, message: Value) {
        let mut transcript = self.inner.transcript.lock().await;
        transcript.push(transcript::Entry { direction, message });
        if transcript.len() > MAX_TRANSCRIPT_ENTRIES {
            let overflow = transcript.len() - MAX_TRANSCRIPT_ENTRIES;
            transcript.drain(0..overflow);
        }
    }

    async fn panic_timeout(&self, description: &str, timeout: Duration) -> ! {
        let transcript = self.inner.transcript.lock().await.clone();
        let inbox = self.inner.inbox.lock().await.clone();

        panic!(
            "timeout after {timeout:?} waiting for {description}\n\ntranscript:\n{}\n\nbuffered messages ({}):\n{}",
            transcript::format_entries(&transcript),
            inbox.len(),
            inbox
                .iter()
                .enumerate()
                .map(|(idx, msg)| {
                    let msg = serde_json::to_string(msg).unwrap_or_else(|_| "<invalid json>".to_string());
                    format!("{idx:03} {msg}")
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    pub async fn take_transcript(&self) -> Vec<transcript::Entry> {
        let mut t = self.inner.transcript.lock().await;
        std::mem::take(&mut *t)
    }

    pub async fn assert_transcript(&self, expected: &[transcript::ExpectedEntry]) {
        let actual = self.take_transcript().await;
        transcript::assert_matches(&actual, expected);
    }

    pub async fn assert_no_pending_messages(&self, grace: Duration) {
        tokio::time::sleep(grace).await;
        let inbox = self.inner.inbox.lock().await;
        if !inbox.is_empty() {
            panic!(
                "expected no pending messages, but buffered {}:\n{}",
                inbox.len(),
                inbox
                    .iter()
                    .enumerate()
                    .map(|(idx, msg)| {
                        let msg = serde_json::to_string(msg).unwrap_or_else(|_| "<invalid json>".to_string());
                        format!("{idx:03} {msg}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }

    pub async fn initialize_handshake(&self) -> Value {
        let resp = self.request("initialize", json!({})).await;
        assert_success(&resp, "initialize");
        let _ = self.wait_for_event("initialized").await;
        resp
    }

    pub async fn attach(&self, host: &str, port: u16) -> Value {
        let resp = self
            .request(
                "attach",
                json!({
                    "host": host,
                    "port": port,
                }),
            )
            .await;
        assert_success(&resp, "attach");
        resp
    }

    pub async fn attach_mock_jdwp(&self) -> nova_jdwp::wire::mock::MockJdwpServer {
        self.attach_mock_jdwp_with_config(nova_jdwp::wire::mock::MockJdwpServerConfig::default())
            .await
    }

    pub async fn attach_mock_jdwp_with_config(
        &self,
        config: nova_jdwp::wire::mock::MockJdwpServerConfig,
    ) -> nova_jdwp::wire::mock::MockJdwpServer {
        let jdwp = nova_jdwp::wire::mock::MockJdwpServer::spawn_with_config(config)
            .await
            .expect("failed to spawn MockJdwpServer");
        self.attach("127.0.0.1", jdwp.addr().port()).await;
        jdwp
    }

    pub async fn set_breakpoints(&self, source_path: &str, lines: &[i64]) -> Value {
        let breakpoints: Vec<Value> = lines.iter().map(|line| json!({ "line": line })).collect();
        let resp = self
            .request(
                "setBreakpoints",
                json!({
                    "source": { "path": source_path },
                    "breakpoints": breakpoints,
                }),
            )
            .await;
        assert_success(&resp, "setBreakpoints");
        resp
    }

    pub async fn continue_(&self) -> Value {
        let (resp, _continued) = self.continue_with_thread_id(None).await;
        resp
    }

    pub async fn continue_with_thread_id(&self, thread_id: Option<i64>) -> (Value, Value) {
        let arguments = match thread_id {
            Some(thread_id) => json!({ "threadId": thread_id }),
            None => json!({}),
        };

        let resp = self.request("continue", arguments).await;
        assert_success(&resp, "continue");
        let continued = self.wait_for_event("continued").await;
        (resp, continued)
    }

    pub async fn pause(&self, thread_id: Option<i64>) -> (Value, StoppedEvent) {
        let arguments = match thread_id {
            Some(thread_id) => json!({ "threadId": thread_id }),
            None => json!({}),
        };

        let resp = self.request("pause", arguments).await;
        assert_success(&resp, "pause");
        let stopped = self.wait_for_stopped_reason("pause").await;
        (resp, stopped)
    }

    pub async fn wait_for_stopped(&self) -> StoppedEvent {
        let raw = self.wait_for_event("stopped").await;
        StoppedEvent {
            thread_id: raw.pointer("/body/threadId").and_then(|v| v.as_i64()),
            reason: raw
                .pointer("/body/reason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            raw,
        }
    }

    pub async fn wait_for_stopped_reason(&self, reason: &str) -> StoppedEvent {
        let stopped = self.wait_for_stopped().await;
        let Some(actual) = stopped.reason.as_deref() else {
            panic!("stopped event missing body.reason: {}", stopped.raw);
        };
        assert_eq!(actual, reason, "unexpected stopped reason: {}", stopped.raw);
        stopped
    }

    pub async fn disconnect(&self) -> Value {
        let resp = self.request("disconnect", json!({})).await;
        assert_success(&resp, "disconnect");
        let _ = self.wait_for_event("terminated").await;
        resp
    }

    pub async fn next(&self, thread_id: i64) -> Value {
        let resp = self.request("next", json!({ "threadId": thread_id })).await;
        assert_success(&resp, "next");
        resp
    }

    pub async fn step_in(&self, thread_id: i64) -> Value {
        let resp = self.request("stepIn", json!({ "threadId": thread_id })).await;
        assert_success(&resp, "stepIn");
        resp
    }

    pub async fn step_out(&self, thread_id: i64) -> Value {
        let resp = self.request("stepOut", json!({ "threadId": thread_id })).await;
        assert_success(&resp, "stepOut");
        resp
    }

    pub async fn first_thread_id(&self) -> i64 {
        let threads = self.request("threads", json!({})).await;
        assert_success(&threads, "threads");
        threads
            .pointer("/body/threads/0/id")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| panic!("threads response missing body.threads[0].id: {threads}"))
    }

    pub async fn first_frame_id(&self, thread_id: i64) -> i64 {
        let stack = self.request("stackTrace", json!({ "threadId": thread_id })).await;
        assert_success(&stack, "stackTrace");
        stack
            .pointer("/body/stackFrames/0/id")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| panic!("stackTrace response missing body.stackFrames[0].id: {stack}"))
    }

    pub async fn first_scope_variables_reference(&self, frame_id: i64) -> i64 {
        let scopes = self.request("scopes", json!({ "frameId": frame_id })).await;
        assert_success(&scopes, "scopes");
        scopes
            .pointer("/body/scopes/0/variablesReference")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| panic!("scopes response missing body.scopes[0].variablesReference: {scopes}"))
    }

    pub async fn variables(&self, variables_reference: i64) -> Value {
        let vars = self
            .request(
                "variables",
                json!({
                    "variablesReference": variables_reference,
                }),
            )
            .await;
        assert_success(&vars, "variables");
        vars
    }
}

pub fn spawn_wire_server(
) -> (
    DapClient,
    tokio::task::JoinHandle<std::result::Result<(), nova_dap::wire_server::WireServerError>>,
) {
    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task = tokio::spawn(async move { nova_dap::wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let client = DapClient::new(client_read, client_write);

    (client, server_task)
}

fn assert_success(resp: &Value, context: &str) {
    let ok = resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
    assert!(ok, "{context} response was not successful: {resp}");
}

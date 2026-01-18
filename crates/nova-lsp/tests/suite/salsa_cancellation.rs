use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nova_lsp::{
    INTERNAL_INTERRUPTIBLE_WORK_METHOD, INTERNAL_INTERRUPTIBLE_WORK_STARTED_NOTIFICATION,
};

use crate::support::{
    exit_notification, initialize_request_empty, initialized_notification, jsonrpc_notification,
    jsonrpc_request, shutdown_request, stdio_server_lock, write_jsonrpc_message,
};
use lsp_types::{CancelParams, NumberOrString};

fn try_write_jsonrpc_message(writer: &mut impl Write, message: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

fn read_jsonrpc_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader
            .read_line(&mut line)
            .expect("read JSON-RPC message header line");
        if bytes_read == 0 {
            return None;
        }

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                let len = value.trim().parse::<usize>().unwrap_or_else(|err| {
                    panic!("invalid Content-Length header: {err}\nheader={line:?}")
                });
                content_length = Some(len);
            }
        }
    }

    let len = content_length.expect("missing Content-Length header");
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .expect("read JSON-RPC message body");
    Some(
        serde_json::from_slice(&buf)
            .unwrap_or_else(|err| panic!("failed to parse JSON-RPC message body: {err}")),
    )
}

fn spawn_message_reader(stdout: ChildStdout) -> (mpsc::Receiver<Value>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Value>();
    let handle = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        while let Some(msg) = read_jsonrpc_message(&mut reader) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    (rx, handle)
}

struct MessagePump {
    rx: mpsc::Receiver<Value>,
    pending: VecDeque<Value>,
}

impl MessagePump {
    fn new(rx: mpsc::Receiver<Value>) -> Self {
        Self {
            rx,
            pending: VecDeque::new(),
        }
    }

    fn recv_matching(
        &mut self,
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(idx) = self.pending.iter().position(|msg| predicate(msg)) {
                return self.pending.remove(idx);
            }

            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let remaining = deadline - now;
            match self.rx.recv_timeout(remaining) {
                Ok(msg) => self.pending.push_back(msg),
                Err(_) => return None,
            }
        }
    }

    fn recv_response_with_id(&mut self, id: i64, timeout: Duration) -> Option<Value> {
        self.recv_matching(timeout, |msg| {
            msg.get("method").is_none() && msg.get("id").and_then(|v| v.as_i64()) == Some(id)
        })
    }

    fn recv_notification_with_method(&mut self, method: &str, timeout: Duration) -> Option<Value> {
        self.recv_matching(timeout, |msg| {
            msg.get("method").and_then(|m| m.as_str()) == Some(method)
        })
    }
}

#[test]
fn cancel_request_triggers_salsa_cancellation() {
    let _guard = stdio_server_lock();

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let stdin: ChildStdin = child.stdin.take().expect("stdin");
    let stdout: ChildStdout = child.stdout.take().expect("stdout");
    let stdin = Arc::new(Mutex::new(stdin));

    let (rx, reader_handle) = spawn_message_reader(stdout);
    let mut messages = MessagePump::new(rx);

    // Initialize + initialized.
    {
        let mut stdin = stdin.lock().expect("stdin mutex poisoned");
        write_jsonrpc_message(&mut *stdin, &initialize_request_empty(1));
    }
    let _initialize_resp = messages
        .recv_response_with_id(1, Duration::from_secs(5))
        .expect("initialize response");
    {
        let mut stdin = stdin.lock().expect("stdin mutex poisoned");
        write_jsonrpc_message(&mut *stdin, &initialized_notification());
    }

    // Run a long-running Salsa query that only checks Salsa cancellation (not the per-request token).
    {
        let mut stdin = stdin.lock().expect("stdin mutex poisoned");
        write_jsonrpc_message(
            &mut *stdin,
            &jsonrpc_request(
                Value::Object({
                    let mut params = serde_json::Map::new();
                    params.insert(
                        "steps".to_string(),
                        Value::Number(serde_json::Number::from(1_000_000_000u64)),
                    );
                    params
                }),
                2,
                INTERNAL_INTERRUPTIBLE_WORK_METHOD,
            ),
        );
    }

    // Wait until the server has entered the request handler (after the per-request cancellation
    // token check) before sending `$/cancelRequest`. This makes the test deterministic and ensures
    // we're validating mid-query Salsa cancellation (not the request-token guard in `handle_request_json`).
    messages
        .recv_notification_with_method(
            INTERNAL_INTERRUPTIBLE_WORK_STARTED_NOTIFICATION,
            Duration::from_secs(5),
        )
        .expect("interruptibleWorkStarted notification");

    let cancel_done = Arc::new(AtomicBool::new(false));
    let cancel_done_thread = cancel_done.clone();
    let cancel_stdin = stdin.clone();
    let cancel_message = jsonrpc_notification(
        CancelParams {
            id: NumberOrString::Number(2),
        },
        "$/cancelRequest",
    );
    let cancel_thread = thread::spawn(move || {
        for _ in 0..400 {
            if cancel_done_thread.load(Ordering::SeqCst) {
                break;
            }
            if let Ok(mut stdin) = cancel_stdin.lock() {
                if try_write_jsonrpc_message(&mut *stdin, &cancel_message).is_err() {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    });

    let start = Instant::now();
    let resp = match messages.recv_response_with_id(2, Duration::from_secs(5)) {
        Some(resp) => resp,
        None => {
            cancel_done.store(true, Ordering::SeqCst);
            let _ = cancel_thread.join();
            let _ = child.kill();
            panic!("timed out waiting for interruptibleWork response (Salsa cancellation likely not triggered)");
        }
    };
    cancel_done.store(true, Ordering::SeqCst);
    cancel_thread.join().expect("cancel thread");

    // Cancellation should take effect quickly; if we hit the full timeout above the test would have
    // already failed.
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "interruptibleWork cancellation was too slow"
    );

    let code = resp
        .get("error")
        .and_then(|err| err.get("code"))
        .and_then(|v| v.as_i64());
    assert_eq!(
        code,
        Some(-32800),
        "expected cancelled request to return -32800, got: {resp:?}"
    );

    // Shutdown + exit.
    {
        let mut stdin = stdin.lock().expect("stdin mutex poisoned");
        write_jsonrpc_message(&mut *stdin, &shutdown_request(3));
    }
    let _shutdown_resp = messages
        .recv_response_with_id(3, Duration::from_secs(5))
        .expect("shutdown response");
    {
        let mut stdin = stdin.lock().expect("stdin mutex poisoned");
        write_jsonrpc_message(&mut *stdin, &exit_notification());
    }
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());

    let _ = reader_handle.join();
}

use std::{
    collections::VecDeque,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::time::{timeout, Instant};

use nova_dap::dap_tokio::{DapReader, DapWriter};
use nova_dap::wire_server;

const FIXTURE_MAIN_CLASS: &str = "Main";
const FIXTURE_BREAKPOINT_LINE: i32 = 5;

fn tool_available(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn compile_fixture(classes_dir: &Path) -> anyhow::Result<PathBuf> {
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("java")
        .join("Main.java");

    anyhow::ensure!(
        fixture_path.exists(),
        "java fixture missing at {}",
        fixture_path.display()
    );

    let output = Command::new("javac")
        .arg("-g")
        .arg("-encoding")
        .arg("UTF-8")
        .arg("-d")
        .arg(classes_dir)
        .arg(&fixture_path)
        .output()?;

    anyhow::ensure!(
        output.status.success(),
        "javac failed (exit={}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(fixture_path)
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn spawn(port: u16, classes_dir: &Path) -> anyhow::Result<Self> {
        let jdwp = format!(
            "-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address={port}"
        );
        let child = Command::new("java")
            .arg(jdwp)
            .arg("-cp")
            .arg(classes_dir)
            .arg(FIXTURE_MAIN_CLASS)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;

        Ok(Self { child: Some(child) })
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn pick_free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral port")
        .local_addr()
        .expect("query local addr")
        .port()
}

struct DapHarness<R, W> {
    reader: DapReader<R>,
    writer: DapWriter<W>,
    buffered: VecDeque<Value>,
}

impl<R, W> DapHarness<R, W>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    fn new(reader: R, writer: W) -> Self {
        Self {
            reader: DapReader::new(reader),
            writer: DapWriter::new(writer),
            buffered: VecDeque::new(),
        }
    }

    async fn send_request(&mut self, seq: i64, command: &str, arguments: Value) {
        let msg = json!({
            "seq": seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        });
        self.writer.write_value(&msg).await.unwrap();
    }

    async fn read_from_wire(&mut self, deadline: Instant) -> Value {
        let now = Instant::now();
        assert!(now < deadline, "timed out waiting for DAP message");
        let dur = deadline - now;
        match timeout(dur, self.reader.read_value()).await {
            Ok(Ok(Some(msg))) => msg,
            Ok(Ok(None)) => panic!("DAP stream closed"),
            Ok(Err(err)) => panic!("DAP read error: {err}"),
            Err(_) => panic!("timed out waiting for DAP message"),
        }
    }

    fn take_buffered(&mut self, predicate: impl Fn(&Value) -> bool) -> Option<Value> {
        let idx = self.buffered.iter().position(|msg| predicate(msg))?;
        self.buffered.remove(idx)
    }

    async fn wait_for_response(&mut self, request_seq: i64, deadline: Instant) -> Value {
        if let Some(msg) = self.take_buffered(|msg| is_response_for(msg, request_seq)) {
            return msg;
        }

        loop {
            let msg = self.read_from_wire(deadline).await;
            if is_response_for(&msg, request_seq) {
                return msg;
            }
            self.buffered.push_back(msg);
        }
    }

    async fn wait_for_event(&mut self, event: &str, deadline: Instant) -> Value {
        if let Some(msg) = self.take_buffered(|msg| is_event(msg, event)) {
            return msg;
        }

        loop {
            let msg = self.read_from_wire(deadline).await;
            if is_event(&msg, event) {
                return msg;
            }
            self.buffered.push_back(msg);
        }
    }
}

fn is_response_for(msg: &Value, request_seq: i64) -> bool {
    msg.get("type").and_then(|v| v.as_str()) == Some("response")
        && msg.get("request_seq").and_then(|v| v.as_i64()) == Some(request_seq)
}

fn is_event(msg: &Value, event: &str) -> bool {
    msg.get("type").and_then(|v| v.as_str()) == Some("event")
        && msg.get("event").and_then(|v| v.as_str()) == Some(event)
}

#[tokio::test]
#[cfg_attr(
    not(feature = "real-jvm-tests"),
    ignore = "enable with `cargo test -p nova-dap --features real-jvm-tests`"
)]
async fn dap_can_attach_to_real_jvm_set_breakpoint_and_stop() {
    if !tool_available("java") || !tool_available("javac") {
        // There isn't a built-in "skip" in Rust's test harness; treat this as a no-op
        // so CI environments without a JDK stay green.
        eprintln!("skipping real JVM test: java and/or javac not found in PATH");
        return;
    }

    let classes_dir = TempDir::new().unwrap();
    let fixture_source = compile_fixture(classes_dir.path()).unwrap();

    let port = pick_free_port();
    let mut jvm = ChildGuard::spawn(port, classes_dir.path()).unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task = tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut dap = DapHarness::new(client_read, client_write);

    dap.send_request(1, "initialize", json!({})).await;
    let init_resp = dap
        .wait_for_response(1, Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(init_resp.get("success").and_then(|v| v.as_bool()), Some(true));
    let initialized = dap
        .wait_for_event("initialized", Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(initialized.get("event").and_then(|v| v.as_str()), Some("initialized"));

    let attach_deadline = Instant::now() + Duration::from_secs(30);
    let mut seq = 2i64;
    loop {
        dap.send_request(
            seq,
            "attach",
            json!({
                "host": "127.0.0.1",
                "port": port,
            }),
        )
        .await;
        let resp = dap.wait_for_response(seq, attach_deadline).await;
        if resp.get("success").and_then(|v| v.as_bool()) == Some(true) {
            break;
        }

        if let Some(status) = jvm.try_wait().unwrap() {
            panic!("JVM exited before attach succeeded (status={status})");
        }

        if Instant::now() >= attach_deadline {
            panic!("timed out waiting to attach to JDWP port {port}: {resp}");
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
        seq += 1;
    }

    let set_bps_seq = seq + 1;
    dap.send_request(
        set_bps_seq,
        "setBreakpoints",
        json!({
            "source": { "path": fixture_source.to_string_lossy() },
            "breakpoints": [ { "line": FIXTURE_BREAKPOINT_LINE } ]
        }),
    )
    .await;
    let bp_resp = dap
        .wait_for_response(set_bps_seq, Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(bp_resp.get("success").and_then(|v| v.as_bool()), Some(true));
    let breakpoints = bp_resp
        .pointer("/body/breakpoints")
        .and_then(|v| v.as_array())
        .expect("setBreakpoints.body.breakpoints missing");
    assert_eq!(breakpoints.len(), 1);
    assert_eq!(
        breakpoints[0].get("line").and_then(|v| v.as_i64()),
        Some(FIXTURE_BREAKPOINT_LINE as i64)
    );

    let continue_seq = set_bps_seq + 1;
    dap.send_request(continue_seq, "continue", json!({})).await;
    let continue_resp = dap
        .wait_for_response(continue_seq, Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(
        continue_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let stopped = dap
        .wait_for_event("stopped", Instant::now() + Duration::from_secs(30))
        .await;
    assert_eq!(
        stopped.pointer("/body/reason").and_then(|v| v.as_str()),
        Some("breakpoint")
    );
    let thread_id = stopped.pointer("/body/threadId").and_then(|v| v.as_i64()).unwrap();

    let stack_seq = continue_seq + 1;
    dap.send_request(stack_seq, "stackTrace", json!({ "threadId": thread_id }))
        .await;
    let stack_resp = dap
        .wait_for_response(stack_seq, Instant::now() + Duration::from_secs(15))
        .await;
    if stack_resp.get("success").and_then(|v| v.as_bool()) != Some(true) {
        panic!("stackTrace request failed: {stack_resp}");
    }

    let frames = stack_resp
        .pointer("/body/stackFrames")
        .and_then(|v| v.as_array())
        .expect("stackTrace.body.stackFrames missing");

    let fixture_source = fixture_source.canonicalize().unwrap_or(fixture_source);
    let fixture_source = fixture_source.to_string_lossy().to_string();
    assert!(Path::new(&fixture_source).exists());
    let has_fixture_frame = frames.iter().any(|frame| {
        frame
            .pointer("/source/path")
            .and_then(|v| v.as_str())
            .is_some_and(|path| path == fixture_source.as_str())
    });
    assert!(
        has_fixture_frame,
        "expected at least one frame to reference the fixture source path {fixture_source}\nstackTrace response: {stack_resp}"
    );

    let disconnect_seq = stack_seq + 1;
    dap.send_request(disconnect_seq, "disconnect", json!({})).await;
    let _ = dap
        .wait_for_response(disconnect_seq, Instant::now() + Duration::from_secs(5))
        .await;

    server_task.await.unwrap().unwrap();
}

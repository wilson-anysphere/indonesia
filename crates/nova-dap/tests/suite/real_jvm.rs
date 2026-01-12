#![cfg(feature = "real-jvm-tests")]

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
const SMART_STEP_BREAKPOINT_LINE: i32 = 10;
const STREAM_DEBUG_BREAKPOINT_LINE: i32 = 6;

fn tool_available(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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
        // Keep JVM memory usage low so this test can run under the `cargo_agent` RLIMIT_AS cap.
        .arg("-J-Xms16m")
        .arg("-J-Xmx256m")
        .arg("-J-XX:CompressedClassSpaceSize=64m")
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

fn compile_inline_java_source(source_path: &Path, classes_dir: &Path) -> anyhow::Result<()> {
    let output = Command::new("javac")
        // Keep JVM memory usage low so this test can run under the `cargo_agent` RLIMIT_AS cap.
        .arg("-J-Xms16m")
        .arg("-J-Xmx256m")
        .arg("-J-XX:CompressedClassSpaceSize=64m")
        .arg("-g")
        .arg("-encoding")
        .arg("UTF-8")
        .arg("-d")
        .arg(classes_dir)
        .arg(source_path)
        .output()?;

    anyhow::ensure!(
        output.status.success(),
        "javac failed (exit={}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    Ok(())
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn spawn(port: u16, classes_dir: &Path) -> anyhow::Result<Self> {
        Self::spawn_with_args(port, classes_dir, &[])
    }

    fn spawn_with_args(
        port: u16,
        classes_dir: &Path,
        extra_vm_args: &[&str],
    ) -> anyhow::Result<Self> {
        let jdwp = format!("-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address={port}");
        let mut cmd = Command::new("java");
        cmd
            // Keep memory usage low so the debuggee can start under the test RLIMIT.
            .arg("-Xms16m")
            .arg("-Xmx256m")
            .arg("-XX:CompressedClassSpaceSize=64m");
        cmd.args(extra_vm_args);
        let child = cmd
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
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut dap = DapHarness::new(client_read, client_write);

    dap.send_request(1, "initialize", json!({})).await;
    let init_resp = dap
        .wait_for_response(1, Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(
        init_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let initialized = dap
        .wait_for_event("initialized", Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(
        initialized.get("event").and_then(|v| v.as_str()),
        Some("initialized")
    );

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
    let thread_id = stopped
        .pointer("/body/threadId")
        .and_then(|v| v.as_i64())
        .unwrap();

    // Step over the assignment so `answer` is definitely in-scope and initialized before
    // attempting to read locals.
    let next_seq = continue_seq + 1;
    dap.send_request(next_seq, "next", json!({ "threadId": thread_id }))
        .await;
    let next_resp = dap
        .wait_for_response(next_seq, Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(
        next_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let stopped_step = dap
        .wait_for_event("stopped", Instant::now() + Duration::from_secs(30))
        .await;
    assert_eq!(
        stopped_step
            .pointer("/body/reason")
            .and_then(|v| v.as_str()),
        Some("step"),
        "expected a step stop after next: {stopped_step}"
    );

    let stack_seq = next_seq + 1;
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

    let fixture_frame_id = frames
        .iter()
        .find(|frame| {
            frame
                .pointer("/source/path")
                .and_then(|v| v.as_str())
                .is_some_and(|path| path == fixture_source.as_str())
        })
        .and_then(|frame| frame.get("id").and_then(|v| v.as_i64()))
        .expect("fixture frame should include an id");

    let scopes_seq = stack_seq + 1;
    dap.send_request(scopes_seq, "scopes", json!({ "frameId": fixture_frame_id }))
        .await;
    let scopes_resp = dap
        .wait_for_response(scopes_seq, Instant::now() + Duration::from_secs(10))
        .await;
    if scopes_resp.get("success").and_then(|v| v.as_bool()) != Some(true) {
        panic!("scopes request failed: {scopes_resp}");
    }

    let locals_ref = scopes_resp
        .pointer("/body/scopes/0/variablesReference")
        .and_then(|v| v.as_i64())
        .expect("scopes[0].variablesReference missing");

    let vars_seq = scopes_seq + 1;
    dap.send_request(
        vars_seq,
        "variables",
        json!({ "variablesReference": locals_ref }),
    )
    .await;
    let vars_resp = dap
        .wait_for_response(vars_seq, Instant::now() + Duration::from_secs(10))
        .await;
    if vars_resp.get("success").and_then(|v| v.as_bool()) != Some(true) {
        panic!("variables request failed: {vars_resp}");
    }

    let variables = vars_resp
        .pointer("/body/variables")
        .and_then(|v| v.as_array())
        .expect("variables.body.variables missing");
    let answer_value = variables
        .iter()
        .find(|v| v.get("name").and_then(|v| v.as_str()) == Some("answer"))
        .and_then(|v| v.get("value").and_then(|v| v.as_str()))
        .expect("expected locals to include `answer`");
    assert_eq!(answer_value, "42");

    let eval_seq = vars_seq + 1;
    dap.send_request(
        eval_seq,
        "evaluate",
        json!({ "expression": "answer", "frameId": fixture_frame_id }),
    )
    .await;
    let eval_resp = dap
        .wait_for_response(eval_seq, Instant::now() + Duration::from_secs(10))
        .await;
    if eval_resp.get("success").and_then(|v| v.as_bool()) != Some(true) {
        panic!("evaluate request failed: {eval_resp}");
    }
    assert_eq!(
        eval_resp.pointer("/body/result").and_then(|v| v.as_str()),
        Some("42")
    );

    let disconnect_seq = eval_seq + 1;
    dap.send_request(disconnect_seq, "disconnect", json!({}))
        .await;
    let _ = dap
        .wait_for_response(disconnect_seq, Instant::now() + Duration::from_secs(5))
        .await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_stream_debug_request_works_on_real_jvm() {
    if !tool_available("java") || !tool_available("javac") {
        // There isn't a built-in "skip" in Rust's test harness; treat this as a no-op
        // so CI environments without a JDK stay green.
        eprintln!("skipping real JVM stream debug test: java and/or javac not found in PATH");
        return;
    }

    let sources_dir = TempDir::new().unwrap();
    let classes_dir = TempDir::new().unwrap();
    let source_path = sources_dir.path().join("Main.java");

    std::fs::write(
        &source_path,
        "import java.util.*;\npublic class Main {\n  public static void main(String[] args) throws Exception {\n    Thread.sleep(500);\n    List<Integer> nums = Arrays.asList(1,2,3);\n    long count = nums.stream().filter(x -> x > 1).map(x -> x * 2).count(); // BREAKPOINT\n    System.out.println(count);\n  }\n}\n",
    )
    .unwrap();

    compile_inline_java_source(&source_path, classes_dir.path()).unwrap();

    let port = pick_free_port();
    let mut jvm = ChildGuard::spawn(port, classes_dir.path()).unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut dap = DapHarness::new(client_read, client_write);

    dap.send_request(1, "initialize", json!({})).await;
    let init_resp = dap
        .wait_for_response(1, Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(
        init_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let _initialized = dap
        .wait_for_event("initialized", Instant::now() + Duration::from_secs(5))
        .await;

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
            "source": { "path": source_path.to_string_lossy() },
            "breakpoints": [ { "line": STREAM_DEBUG_BREAKPOINT_LINE } ]
        }),
    )
    .await;
    let bp_resp = dap
        .wait_for_response(set_bps_seq, Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(bp_resp.get("success").and_then(|v| v.as_bool()), Some(true));

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
        Some("breakpoint"),
        "expected breakpoint stop: {stopped}"
    );
    let thread_id = stopped
        .pointer("/body/threadId")
        .and_then(|v| v.as_i64())
        .unwrap();

    let stack_seq = continue_seq + 1;
    dap.send_request(stack_seq, "stackTrace", json!({ "threadId": thread_id }))
        .await;
    let stack_resp = dap
        .wait_for_response(stack_seq, Instant::now() + Duration::from_secs(15))
        .await;
    assert_eq!(
        stack_resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "stackTrace request failed: {stack_resp}",
    );

    let frames = stack_resp
        .pointer("/body/stackFrames")
        .and_then(|v| v.as_array())
        .expect("stackTrace.body.stackFrames missing");

    let source_path = source_path.canonicalize().unwrap_or(source_path);
    let source_path = source_path.to_string_lossy().to_string();
    assert!(Path::new(&source_path).exists());
    let frame_id = frames
        .iter()
        .find(|frame| {
            frame
                .pointer("/source/path")
                .and_then(|v| v.as_str())
                .is_some_and(|path| path == source_path.as_str())
        })
        .and_then(|frame| frame.get("id").and_then(|v| v.as_i64()))
        .expect("expected a frame id for Main.java");

    let expr = "nums.stream().filter(x -> x > 1).map(x -> x * 2).count()";
    let stream_seq = stack_seq + 1;
    dap.send_request(
        stream_seq,
        "nova/streamDebug",
        json!({
            "expression": expr,
            "frameId": frame_id,
            "maxSampleSize": 3,
            "allowTerminalOps": true,
        }),
    )
    .await;
    let stream_resp = dap
        .wait_for_response(stream_seq, Instant::now() + Duration::from_secs(60))
        .await;
    assert_eq!(
        stream_resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "nova/streamDebug request failed: {stream_resp}"
    );

    let intermediates = stream_resp
        .pointer("/body/analysis/intermediates")
        .and_then(|v| v.as_array())
        .expect("nova/streamDebug.body.analysis.intermediates missing");
    let intermediate_names: Vec<_> = intermediates
        .iter()
        .filter_map(|op| op.get("name").and_then(|v| v.as_str()))
        .collect();
    assert!(
        intermediate_names.iter().any(|&name| name == "filter"),
        "expected analysis intermediates to include filter: {stream_resp}"
    );
    assert!(
        intermediate_names.iter().any(|&name| name == "map"),
        "expected analysis intermediates to include map: {stream_resp}"
    );

    let source_elements: Vec<_> = stream_resp
        .pointer("/body/runtime/sourceSample/elements")
        .and_then(|v| v.as_array())
        .expect("nova/streamDebug.body.runtime.sourceSample.elements missing")
        .iter()
        .map(|v| v.as_str().expect("sourceSample element should be a string"))
        .collect();
    assert_eq!(source_elements, vec!["1", "2", "3"]);

    let steps = stream_resp
        .pointer("/body/runtime/steps")
        .and_then(|v| v.as_array())
        .expect("nova/streamDebug.body.runtime.steps missing");

    let filter_step = steps
        .iter()
        .find(|step| step.get("operation").and_then(|v| v.as_str()) == Some("filter"))
        .expect("expected a filter step");
    let filter_output: Vec<_> = filter_step
        .pointer("/output/elements")
        .and_then(|v| v.as_array())
        .expect("filter step output.elements missing")
        .iter()
        .map(|v| {
            v.as_str()
                .expect("filter output element should be a string")
        })
        .collect();
    assert_eq!(filter_output, vec!["2", "3"]);

    let map_step = steps
        .iter()
        .find(|step| step.get("operation").and_then(|v| v.as_str()) == Some("map"))
        .expect("expected a map step");
    let map_output: Vec<_> = map_step
        .pointer("/output/elements")
        .and_then(|v| v.as_array())
        .expect("map step output.elements missing")
        .iter()
        .map(|v| v.as_str().expect("map output element should be a string"))
        .collect();
    assert_eq!(map_output, vec!["4", "6"]);

    assert_eq!(
        stream_resp
            .pointer("/body/runtime/terminal/value")
            .and_then(|v| v.as_str()),
        Some("2")
    );

    let disconnect_seq = stream_seq + 1;
    dap.send_request(disconnect_seq, "disconnect", json!({}))
        .await;
    let _ = dap
        .wait_for_response(disconnect_seq, Instant::now() + Duration::from_secs(5))
        .await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_stream_debug_supports_static_field_sources_on_real_jvm() {
    if !tool_available("java") || !tool_available("javac") {
        // There isn't a built-in "skip" in Rust's test harness; treat this as a no-op
        // so CI environments without a JDK stay green.
        eprintln!(
            "skipping real JVM stream debug static-field test: java and/or javac not found in PATH"
        );
        return;
    }

    let sources_dir = TempDir::new().unwrap();
    let classes_dir = TempDir::new().unwrap();
    let source_path = sources_dir.path().join("Main.java");

    std::fs::write(
        &source_path,
        "import java.util.*;\npublic class Main {\n  static List<Integer> nums = Arrays.asList(1,2,3);\n  public static void main(String[] args) throws Exception {\n    Thread.sleep(500);\n    long c = nums.stream().filter(x -> x > 1).map(x -> x * 2).count(); // BREAKPOINT\n    System.out.println(c);\n  }\n}\n",
    )
    .unwrap();

    compile_inline_java_source(&source_path, classes_dir.path()).unwrap();

    let port = pick_free_port();
    let mut jvm = ChildGuard::spawn(port, classes_dir.path()).unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut dap = DapHarness::new(client_read, client_write);

    dap.send_request(1, "initialize", json!({})).await;
    let init_resp = dap
        .wait_for_response(1, Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(
        init_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let _initialized = dap
        .wait_for_event("initialized", Instant::now() + Duration::from_secs(5))
        .await;

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
            "source": { "path": source_path.to_string_lossy() },
            "breakpoints": [ { "line": STREAM_DEBUG_BREAKPOINT_LINE } ]
        }),
    )
    .await;
    let bp_resp = dap
        .wait_for_response(set_bps_seq, Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(bp_resp.get("success").and_then(|v| v.as_bool()), Some(true));

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
        Some("breakpoint"),
        "expected breakpoint stop: {stopped}"
    );
    let thread_id = stopped
        .pointer("/body/threadId")
        .and_then(|v| v.as_i64())
        .unwrap();

    let stack_seq = continue_seq + 1;
    dap.send_request(stack_seq, "stackTrace", json!({ "threadId": thread_id }))
        .await;
    let stack_resp = dap
        .wait_for_response(stack_seq, Instant::now() + Duration::from_secs(15))
        .await;
    assert_eq!(
        stack_resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "stackTrace request failed: {stack_resp}",
    );

    let frames = stack_resp
        .pointer("/body/stackFrames")
        .and_then(|v| v.as_array())
        .expect("stackTrace.body.stackFrames missing");

    let source_path = source_path.canonicalize().unwrap_or(source_path);
    let source_path = source_path.to_string_lossy().to_string();
    assert!(Path::new(&source_path).exists());
    let frame_id = frames
        .iter()
        .find(|frame| {
            frame
                .pointer("/source/path")
                .and_then(|v| v.as_str())
                .is_some_and(|path| path == source_path.as_str())
        })
        .and_then(|frame| frame.get("id").and_then(|v| v.as_i64()))
        .expect("expected a frame id for Main.java");

    let expr = "nums.stream().filter(x -> x > 1).map(x -> x * 2).count()";
    let stream_seq = stack_seq + 1;
    dap.send_request(
        stream_seq,
        "nova/streamDebug",
        json!({
            "expression": expr,
            "frameId": frame_id,
            "maxSampleSize": 3,
            "allowTerminalOps": true,
        }),
    )
    .await;
    let stream_resp = dap
        .wait_for_response(stream_seq, Instant::now() + Duration::from_secs(60))
        .await;
    if stream_resp.get("success").and_then(|v| v.as_bool()) != Some(true) {
        panic!("nova/streamDebug request failed: {stream_resp}");
    }

    let source_elements: Vec<_> = stream_resp
        .pointer("/body/runtime/sourceSample/elements")
        .and_then(|v| v.as_array())
        .expect("nova/streamDebug.body.runtime.sourceSample.elements missing")
        .iter()
        .map(|v| v.as_str().expect("sourceSample element should be a string"))
        .collect();
    assert_eq!(source_elements, vec!["1", "2", "3"]);

    let steps = stream_resp
        .pointer("/body/runtime/steps")
        .and_then(|v| v.as_array())
        .expect("nova/streamDebug.body.runtime.steps missing");

    let filter_step = steps
        .iter()
        .find(|step| step.get("operation").and_then(|v| v.as_str()) == Some("filter"))
        .expect("expected a filter step");
    let filter_output: Vec<_> = filter_step
        .pointer("/output/elements")
        .and_then(|v| v.as_array())
        .expect("filter step output.elements missing")
        .iter()
        .map(|v| {
            v.as_str()
                .expect("filter output element should be a string")
        })
        .collect();
    assert_eq!(filter_output, vec!["2", "3"]);

    let map_step = steps
        .iter()
        .find(|step| step.get("operation").and_then(|v| v.as_str()) == Some("map"))
        .expect("expected a map step");
    let map_output: Vec<_> = map_step
        .pointer("/output/elements")
        .and_then(|v| v.as_array())
        .expect("map step output.elements missing")
        .iter()
        .map(|v| v.as_str().expect("map output element should be a string"))
        .collect();
    assert_eq!(map_output, vec!["4", "6"]);

    assert_eq!(
        stream_resp
            .pointer("/body/runtime/terminal/value")
            .and_then(|v| v.as_str()),
        Some("2")
    );

    let disconnect_seq = stream_seq + 1;
    dap.send_request(disconnect_seq, "disconnect", json!({}))
        .await;
    let _ = dap
        .wait_for_response(disconnect_seq, Instant::now() + Duration::from_secs(5))
        .await;

    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dap_smart_step_into_target_works_on_real_jvm() {
    if !tool_available("java") || !tool_available("javac") {
        // There isn't a built-in "skip" in Rust's test harness; treat this as a no-op
        // so CI environments without a JDK stay green.
        eprintln!("skipping real JVM smart-step test: java and/or javac not found in PATH");
        return;
    }

    let sources_dir = TempDir::new().unwrap();
    let classes_dir = TempDir::new().unwrap();
    let source_path = sources_dir.path().join("Main.java");

    std::fs::write(
        &source_path,
        "public class Main {\n  static int bar() { return 1; }\n  static int qux() { return 2; }\n  static int baz(int v) { return v; }\n  static int corge() { return 3; }\n  static int foo(int v) { return v; }\n\n  public static void main(String[] args) throws Exception {\n    Thread.sleep(500);\n    int result = foo(bar() + baz(qux()) + corge());\n    System.out.println(result);\n  }\n}\n",
    )
    .unwrap();

    compile_inline_java_source(&source_path, classes_dir.path()).unwrap();

    let port = pick_free_port();
    // Use the interpreter for predictable stepping behavior.
    let mut jvm = ChildGuard::spawn_with_args(port, classes_dir.path(), &["-Xint"]).unwrap();

    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task =
        tokio::spawn(async move { wire_server::run(server_read, server_write).await });

    let (client_read, client_write) = tokio::io::split(client);
    let mut dap = DapHarness::new(client_read, client_write);

    dap.send_request(1, "initialize", json!({})).await;
    let init_resp = dap
        .wait_for_response(1, Instant::now() + Duration::from_secs(5))
        .await;
    assert_eq!(
        init_resp.get("success").and_then(|v| v.as_bool()),
        Some(true)
    );

    let _initialized = dap
        .wait_for_event("initialized", Instant::now() + Duration::from_secs(5))
        .await;

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
            "source": { "path": source_path.to_string_lossy() },
            "breakpoints": [ { "line": SMART_STEP_BREAKPOINT_LINE } ]
        }),
    )
    .await;
    let bp_resp = dap
        .wait_for_response(set_bps_seq, Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(bp_resp.get("success").and_then(|v| v.as_bool()), Some(true));

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
    let thread_id = stopped
        .pointer("/body/threadId")
        .and_then(|v| v.as_i64())
        .unwrap();

    let stack_seq = continue_seq + 1;
    dap.send_request(stack_seq, "stackTrace", json!({ "threadId": thread_id }))
        .await;
    let stack_resp = dap
        .wait_for_response(stack_seq, Instant::now() + Duration::from_secs(15))
        .await;
    assert_eq!(
        stack_resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "stackTrace request failed: {stack_resp}",
    );

    let frames = stack_resp
        .pointer("/body/stackFrames")
        .and_then(|v| v.as_array())
        .expect("stackTrace.body.stackFrames missing");

    let source_path = source_path.canonicalize().unwrap_or(source_path);
    let source_path = source_path.to_string_lossy().to_string();
    let frame_id = frames
        .iter()
        .find(|frame| {
            frame
                .pointer("/source/path")
                .and_then(|v| v.as_str())
                .is_some_and(|path| path == source_path.as_str())
        })
        .and_then(|frame| frame.get("id").and_then(|v| v.as_i64()))
        .expect("expected a frame id for Main.java");

    let targets_seq = stack_seq + 1;
    dap.send_request(targets_seq, "stepInTargets", json!({ "frameId": frame_id }))
        .await;
    let targets_resp = dap
        .wait_for_response(targets_seq, Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(
        targets_resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "stepInTargets request failed: {targets_resp}"
    );

    let targets = targets_resp
        .pointer("/body/targets")
        .and_then(|v| v.as_array())
        .expect("stepInTargets.body.targets missing");
    let baz_target_id = targets
        .iter()
        .find(|t| t.get("label").and_then(|v| v.as_str()) == Some("baz()"))
        .and_then(|t| t.get("id").and_then(|v| v.as_i64()))
        .expect("expected baz() target");

    let step_seq = targets_seq + 1;
    dap.send_request(
        step_seq,
        "stepIn",
        json!({ "threadId": thread_id, "targetId": baz_target_id }),
    )
    .await;
    let step_resp = dap
        .wait_for_response(step_seq, Instant::now() + Duration::from_secs(10))
        .await;
    assert_eq!(
        step_resp.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "stepIn request failed: {step_resp}"
    );

    let stopped_step = dap
        .wait_for_event("stopped", Instant::now() + Duration::from_secs(30))
        .await;
    assert_eq!(
        stopped_step
            .pointer("/body/reason")
            .and_then(|v| v.as_str()),
        Some("step"),
        "expected a step stop after smart step: {stopped_step}"
    );

    let stack_after_seq = step_seq + 1;
    dap.send_request(
        stack_after_seq,
        "stackTrace",
        json!({ "threadId": thread_id }),
    )
    .await;
    let stack_after = dap
        .wait_for_response(stack_after_seq, Instant::now() + Duration::from_secs(15))
        .await;
    assert_eq!(
        stack_after.get("success").and_then(|v| v.as_bool()),
        Some(true),
        "stackTrace after stepIn failed: {stack_after}"
    );

    let top_method = stack_after
        .pointer("/body/stackFrames/0/name")
        .and_then(|v| v.as_str());
    assert_eq!(top_method, Some("baz"));

    let disconnect_seq = stack_after_seq + 1;
    dap.send_request(disconnect_seq, "disconnect", json!({}))
        .await;
    let _ = dap
        .wait_for_response(disconnect_seq, Instant::now() + Duration::from_secs(5))
        .await;

    server_task.await.unwrap().unwrap();
}

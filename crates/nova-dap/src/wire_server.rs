use std::{
    collections::{BTreeMap, HashMap},
    net::IpAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use base64::{engine::general_purpose, Engine as _};
use nova_build::{BuildManager, DefaultCommandRunner};
use nova_jdwp::wire::{JdwpClient, JdwpError, JdwpValue, ObjectId};
use nova_scheduler::CancellationToken;
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpListener,
    process::{Child, Command},
    sync::{broadcast, mpsc, watch, Mutex},
    task::JoinSet,
};

use nova_bugreport::{global_crash_store, BugReportBuilder, BugReportOptions, PerfStats};
use nova_config::NovaConfig;

use crate::{
    dap_tokio::{make_event, make_response, DapError, DapReader, DapWriter, Request},
    eval_context::EvalOptions,
    hot_swap::{
        BuildSystemMulti, CompileError, CompileOutputMulti, CompiledClass, HotSwapClassResult,
        HotSwapEngine, HotSwapStatus,
    },
    wire_debugger::{
        AttachArgs, BreakpointDisposition, BreakpointSpec, Debugger, DebuggerError,
        FunctionBreakpointSpec, StepDepth, VmStoppedValue,
    },
    EvaluateResult,
};

#[derive(Debug, Error)]
pub enum WireServerError {
    #[error(transparent)]
    Dap(#[from] DapError),

    #[error(transparent)]
    Debugger(#[from] DebuggerError),
}

type Result<T> = std::result::Result<T, WireServerError>;

static HOT_SWAP_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EvaluateArguments {
    expression: String,
    #[serde(default)]
    frame_id: Option<i64>,
    #[serde(default)]
    context: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetVariableArguments {
    variables_reference: i64,
    name: String,
    value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionKind {
    Attach,
    Launch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleState {
    Uninitialized,
    Initialized,
    LaunchedOrAttached,
    Configured,
    Running,
}

#[derive(Debug)]
struct SessionLifecycle {
    lifecycle: LifecycleState,
    kind: Option<SessionKind>,
    awaiting_configuration_done_resume: bool,
    configuration_done_received: bool,
    project_root: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct PendingConfiguration {
    breakpoints: HashMap<String, Vec<BreakpointSpec>>,
    exception_breakpoints: Option<(bool, bool)>,
    function_breakpoints: Option<Vec<FunctionBreakpointSpec>>,
}

impl Default for SessionLifecycle {
    fn default() -> Self {
        Self {
            lifecycle: LifecycleState::Uninitialized,
            kind: None,
            awaiting_configuration_done_resume: false,
            configuration_done_received: false,
            project_root: None,
        }
    }
}

/// Run the experimental JDWP-backed DAP adapter over stdio.
pub async fn run_stdio() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    run(stdin, stdout).await.map_err(anyhow::Error::from)
}

pub async fn run<R, W>(reader: R, writer: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let seq = Arc::new(AtomicI64::new(1));
    let terminated_sent = Arc::new(AtomicBool::new(false));
    let debugger: Arc<Mutex<Option<Debugger>>> = Arc::new(Mutex::new(None));
    let launched_process: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let session: Arc<Mutex<SessionLifecycle>> = Arc::new(Mutex::new(SessionLifecycle::default()));
    let pending_config: Arc<Mutex<PendingConfiguration>> =
        Arc::new(Mutex::new(PendingConfiguration::default()));
    let in_flight: Arc<Mutex<HashMap<i64, CancellationToken>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let server_shutdown = CancellationToken::new();
    let (initialized_tx, initialized_rx) = watch::channel(false);

    let writer_task = tokio::spawn(async move {
        let mut writer = DapWriter::new(writer);
        while let Some(msg) = out_rx.recv().await {
            let _ = writer.write_value(&msg).await;
        }
    });

    let mut reader = DapReader::new(reader);
    let mut tasks = JoinSet::new();
    let mut shutdown_request_seq: Option<i64> = None;

    loop {
        let has_tasks = !tasks.is_empty();
        tokio::select! {
            _ = server_shutdown.cancelled() => break,
            Some(res) = tasks.join_next(), if has_tasks => {
                let _ = res;
            }
            res = reader.read_request() => {
                let Some(request) = res? else {
                    break;
                };
                if request.message_type != "request" {
                    continue;
                }

                let request_token = CancellationToken::new();
                {
                    let mut in_flight = in_flight.lock().await;
                    in_flight.insert(request.seq, request_token.clone());
                }

                let is_shutdown_request =
                    matches!(request.command.as_str(), "disconnect" | "terminate");
                if is_shutdown_request {
                    shutdown_request_seq = Some(request.seq);
                    server_shutdown.cancel();
                }

                tasks.spawn(handle_request(
                    request,
                    request_token,
                    out_tx.clone(),
                    seq.clone(),
                    debugger.clone(),
                    launched_process.clone(),
                    session.clone(),
                    pending_config.clone(),
                    in_flight.clone(),
                    initialized_tx.clone(),
                    initialized_rx.clone(),
                    server_shutdown.clone(),
                    terminated_sent.clone(),
                ));

                if is_shutdown_request {
                    break;
                }
            }
        }
    }

    // Ensure any background tasks (including event forwarding) observe shutdown.
    server_shutdown.cancel();

    // Cancel in-flight requests so long JDWP operations unwind quickly.
    {
        let in_flight_guard = in_flight.lock().await;
        for (seq, token) in in_flight_guard.iter() {
            if shutdown_request_seq.map(|s| s == *seq).unwrap_or(false) {
                continue;
            }
            token.cancel();
        }
    }

    while let Some(res) = tasks.join_next().await {
        let _ = res;
    }

    // Best-effort: ensure the JDWP connection is torn down even if the DAP client
    // disconnects without sending the explicit request.
    {
        let mut guard = debugger.lock().await;
        if let Some(mut dbg) = guard.take() {
            dbg.disconnect().await;
        }
    }

    // Best-effort cleanup for launched debuggees if the client disconnects unexpectedly.
    terminate_existing_process(&launched_process).await;

    drop(out_tx);
    let _ = writer_task.await;
    Ok(())
}

async fn handle_request(
    request: Request,
    cancel: CancellationToken,
    out_tx: mpsc::UnboundedSender<Value>,
    seq: Arc<AtomicI64>,
    debugger: Arc<Mutex<Option<Debugger>>>,
    launched_process: Arc<Mutex<Option<Child>>>,
    session: Arc<Mutex<SessionLifecycle>>,
    pending_config: Arc<Mutex<PendingConfiguration>>,
    in_flight: Arc<Mutex<HashMap<i64, CancellationToken>>>,
    initialized_tx: watch::Sender<bool>,
    initialized_rx: watch::Receiver<bool>,
    server_shutdown: CancellationToken,
    terminated_sent: Arc<AtomicBool>,
) {
    let _request_metrics =
        RequestMetricsGuard::new(&request.command, nova_metrics::MetricsRegistry::global());
    let request_seq = request.seq;

    handle_request_inner(
        &request,
        &cancel,
        &out_tx,
        &seq,
        &debugger,
        &launched_process,
        &session,
        &pending_config,
        &in_flight,
        &initialized_tx,
        initialized_rx,
        &server_shutdown,
        &terminated_sent,
    )
    .await;

    let mut guard = in_flight.lock().await;
    guard.remove(&request_seq);
}

async fn handle_request_inner(
    request: &Request,
    cancel: &CancellationToken,
    out_tx: &mpsc::UnboundedSender<Value>,
    seq: &Arc<AtomicI64>,
    debugger: &Arc<Mutex<Option<Debugger>>>,
    launched_process: &Arc<Mutex<Option<Child>>>,
    session: &Arc<Mutex<SessionLifecycle>>,
    pending_config: &Arc<Mutex<PendingConfiguration>>,
    in_flight: &Arc<Mutex<HashMap<i64, CancellationToken>>>,
    initialized_tx: &watch::Sender<bool>,
    initialized_rx: watch::Receiver<bool>,
    server_shutdown: &CancellationToken,
    terminated_sent: &Arc<AtomicBool>,
) {
    if requires_initialized(request.command.as_str()) {
        if !wait_initialized(cancel, initialized_rx.clone()).await {
            send_response(
                out_tx,
                seq,
                request,
                false,
                None,
                Some("cancelled".to_string()),
            );
            return;
        }
    }

    match request.command.as_str() {
        "initialize" => {
            {
                let mut sess = session.lock().await;
                sess.lifecycle = LifecycleState::Initialized;
                sess.kind = None;
                sess.awaiting_configuration_done_resume = false;
                sess.configuration_done_received = false;
                sess.project_root = None;
            }

            {
                let mut pending = pending_config.lock().await;
                *pending = PendingConfiguration::default();
            }

            let body = json!({
                "supportsConfigurationDoneRequest": true,
                "supportsEvaluateForHovers": true,
                "supportsPauseRequest": true,
                "supportsCancelRequest": true,
                "supportsTerminateRequest": true,
                "supportsRestartRequest": false,
                "supportsSetVariable": true,
                "supportsStepInTargetsRequest": true,
                "supportsStepBack": false,
                "supportsFunctionBreakpoints": true,
                "supportsVariablePaging": true,
                "supportsExceptionBreakpoints": true,
                "supportsExceptionInfoRequest": true,
                "exceptionBreakpointFilters": [
                    { "filter": "caught", "label": "Caught Exceptions", "default": false },
                    { "filter": "uncaught", "label": "Uncaught Exceptions", "default": false },
                    { "filter": "all", "label": "All Exceptions", "default": false },
                ],
                "supportsConditionalBreakpoints": true,
                "supportsHitConditionalBreakpoints": true,
                "supportsLogPoints": true,
            });
            send_response(out_tx, seq, request, true, Some(body), None);

            if !*initialized_rx.borrow() {
                send_event(out_tx, seq, "initialized", None);
                let _ = initialized_tx.send(true);
            }
        }
        "nova/metrics" => {
            match serde_json::to_value(nova_metrics::MetricsRegistry::global().snapshot()) {
                Ok(snapshot) => send_response(out_tx, seq, request, true, Some(snapshot), None),
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "nova/bugReport" => {
            if cancel.is_cancelled() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                );
                return;
            }

            let max_log_lines = request
                .arguments
                .get("maxLogLines")
                .and_then(|v| v.as_u64())
                .and_then(|v| usize::try_from(v).ok());
            let reproduction = request
                .arguments
                .get("reproduction")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let cfg = NovaConfig::default();
            let log_buffer = nova_config::init_tracing_with_config(&cfg);
            let crash_store = global_crash_store();
            let perf = PerfStats::default();
            let options = BugReportOptions {
                max_log_lines: max_log_lines.unwrap_or(500),
                reproduction,
            };

            match BugReportBuilder::new(&cfg, log_buffer.as_ref(), crash_store.as_ref(), &perf)
                .options(options)
                .extra_attachments(|dir| {
                    if let Ok(metrics_json) = serde_json::to_string_pretty(
                        &nova_metrics::MetricsRegistry::global().snapshot(),
                    ) {
                        let _ = std::fs::write(dir.join("metrics.json"), metrics_json);
                    }
                    Ok(())
                })
                .build()
            {
                Ok(bundle) => {
                    let archive_path = bundle.archive_path().map(|p| p.display().to_string());
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!({
                            "path": bundle.path().display().to_string(),
                            "archivePath": archive_path,
                        })),
                        None,
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            };
        }
        "cancel" => {
            let Some(request_id) = request.arguments.get("requestId").and_then(|v| v.as_i64())
            else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancel.requestId is required".to_string()),
                );
                return;
            };

            let token = {
                let in_flight = in_flight.lock().await;
                in_flight.get(&request_id).cloned()
            };
            if let Some(token) = token {
                token.cancel();
            }
            // Best-effort: DAP `cancel` doesn't guarantee the target is still running.
            send_response(out_tx, seq, request, true, None, None);
        }
        "configurationDone" => {
            // When `supportsConfigurationDoneRequest` is true, VS Code sends this request
            // after breakpoints have been configured.
            let should_resume = {
                let mut sess = session.lock().await;
                sess.lifecycle = LifecycleState::Configured;
                sess.configuration_done_received = true;
                if sess.awaiting_configuration_done_resume {
                    sess.awaiting_configuration_done_resume = false;
                    true
                } else {
                    false
                }
            };

            if should_resume {
                let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                    Some(guard) => guard,
                    None => {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("cancelled".to_string()),
                        );
                        return;
                    }
                };
                let Some(dbg) = guard.as_mut() else {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("not attached".to_string()),
                    );
                    return;
                };

                match dbg.continue_(cancel, None).await {
                    Ok(()) => {
                        send_response(out_tx, seq, request, true, None, None);
                        send_event(
                            out_tx,
                            seq,
                            "continued",
                            Some(json!({ "allThreadsContinued": true })),
                        );
                        let mut sess = session.lock().await;
                        sess.lifecycle = LifecycleState::Running;
                    }
                    Err(err) if is_cancelled_error(&err) => {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("cancelled".to_string()),
                        );
                    }
                    Err(err) => {
                        send_response(out_tx, seq, request, false, None, Some(err.to_string()))
                    }
                }
            } else {
                send_response(out_tx, seq, request, true, None, None);
            }
        }
        "launch" => {
            let args: LaunchArguments = match serde_json::from_value(request.arguments.clone()) {
                Ok(args) => args,
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(format!("launch arguments are invalid: {err}")),
                    );
                    return;
                }
            };

            {
                let sess = session.lock().await;
                if sess.lifecycle == LifecycleState::Uninitialized {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("launch is only valid after initialize".to_string()),
                    );
                    return;
                }
                if sess.kind.is_some() {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("debug session already started".to_string()),
                    );
                    return;
                }
            }

            // Ensure we don't leak previous launched processes if the client retries.
            terminate_existing_process(launched_process).await;

            // `launch` must not run concurrently with an existing debugger connection because
            // event forwarding tasks cannot be restarted safely mid-session.
            {
                let guard = debugger.lock().await;
                if guard.is_some() {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("already attached".to_string()),
                    );
                    return;
                }
            }

            let source_roots =
                match resolve_source_roots(request.command.as_str(), &request.arguments) {
                    Ok(roots) => roots,
                    Err(err) => {
                        send_response(out_tx, seq, request, false, None, Some(err.to_string()));
                        return;
                    }
                };
            let project_root = parse_project_root(&request.arguments);

            let attach_timeout = Duration::from_millis(args.attach_timeout_ms.unwrap_or(30_000));

            // Determine which launch mode we are in.
            let mode = if args.command.is_some() {
                LaunchMode::Command
            } else if args.main_class.is_some() {
                LaunchMode::Java
            } else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some(
                        "launch must specify either {command,cwd} or {mainClass,classpath}"
                            .to_string(),
                    ),
                );
                return;
            };

            let attach = match mode {
                LaunchMode::Command => {
                    let Some(cwd) = args.cwd.as_deref() else {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("launch.cwd is required".to_string()),
                        );
                        return;
                    };
                    let Some(command) = args.command.as_deref() else {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("launch.command is required".to_string()),
                        );
                        return;
                    };

                    let host = args.host.as_deref().unwrap_or("127.0.0.1");
                    let host: IpAddr = match host.parse() {
                        Ok(host) => host,
                        Err(err) => {
                            send_response(
                                out_tx,
                                seq,
                                request,
                                false,
                                None,
                                Some(format!("invalid host {host:?}: {err}")),
                            );
                            return;
                        }
                    };
                    let port = args.port.unwrap_or(5005);

                    let mut cmd = Command::new(command);
                    cmd.args(&args.args);
                    cmd.current_dir(cwd);
                    cmd.stdin(Stdio::null());
                    cmd.stdout(Stdio::piped());
                    cmd.stderr(Stdio::piped());
                    for (k, v) in &args.env {
                        cmd.env(k, v);
                    }

                    let mut child = match cmd.spawn() {
                        Ok(child) => child,
                        Err(err) => {
                            send_response(
                                out_tx,
                                seq,
                                request,
                                false,
                                None,
                                Some(format!("failed to spawn {command:?}: {err}")),
                            );
                            return;
                        }
                    };

                    if let Some(stdout) = child.stdout.take() {
                        spawn_output_task(
                            stdout,
                            out_tx.clone(),
                            seq.clone(),
                            "stdout",
                            server_shutdown.clone(),
                        );
                    }
                    if let Some(stderr) = child.stderr.take() {
                        spawn_output_task(
                            stderr,
                            out_tx.clone(),
                            seq.clone(),
                            "stderr",
                            server_shutdown.clone(),
                        );
                    }

                    {
                        let mut guard = launched_process.lock().await;
                        *guard = Some(child);
                    }

                    AttachArgs {
                        host,
                        port,
                        source_roots: source_roots.clone(),
                    }
                }
                LaunchMode::Java => {
                    let main_class = args.main_class.as_deref().unwrap_or_default();
                    let Some(classpath) = args.classpath.clone() else {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("launch.classpath is required for Java launch".to_string()),
                        );
                        return;
                    };

                    let port = match args.port {
                        Some(port) => port,
                        None => match pick_free_port().await {
                            Ok(port) => port,
                            Err(err) => {
                                send_response(
                                    out_tx,
                                    seq,
                                    request,
                                    false,
                                    None,
                                    Some(format!("failed to select debug port: {err}")),
                                );
                                return;
                            }
                        },
                    };
                    let host: IpAddr = "127.0.0.1".parse().unwrap();

                    let java = args.java.clone().unwrap_or_else(|| "java".to_string());

                    let cp_joined = match join_classpath(&classpath) {
                        Ok(cp) => cp,
                        Err(err) => {
                            send_response(out_tx, seq, request, false, None, Some(err));
                            return;
                        }
                    };

                    let suspend = if args.stop_on_entry { "y" } else { "n" };
                    let debug_arg = format!(
                        "-agentlib:jdwp=transport=dt_socket,server=y,suspend={suspend},address={port}"
                    );

                    let mut cmd = Command::new(java);
                    cmd.stdin(Stdio::null());
                    cmd.stdout(Stdio::piped());
                    cmd.stderr(Stdio::piped());
                    if let Some(cwd) = args.cwd.as_deref() {
                        cmd.current_dir(cwd);
                    }
                    for (k, v) in &args.env {
                        cmd.env(k, v);
                    }
                    cmd.args(&args.vm_args);
                    cmd.arg(debug_arg);
                    if let Some(module_name) = args.module_name.as_deref() {
                        cmd.arg("--module-path");
                        cmd.arg(cp_joined.clone());
                        cmd.arg("-m");
                        cmd.arg(format!("{module_name}/{main_class}"));
                    } else {
                        cmd.arg("-classpath");
                        cmd.arg(cp_joined);
                        cmd.arg(main_class);
                    }
                    cmd.args(&args.args);

                    let mut child = match cmd.spawn() {
                        Ok(child) => child,
                        Err(err) => {
                            send_response(
                                out_tx,
                                seq,
                                request,
                                false,
                                None,
                                Some(format!("failed to spawn java: {err}")),
                            );
                            return;
                        }
                    };

                    if let Some(stdout) = child.stdout.take() {
                        spawn_output_task(
                            stdout,
                            out_tx.clone(),
                            seq.clone(),
                            "stdout",
                            server_shutdown.clone(),
                        );
                    }
                    if let Some(stderr) = child.stderr.take() {
                        spawn_output_task(
                            stderr,
                            out_tx.clone(),
                            seq.clone(),
                            "stderr",
                            server_shutdown.clone(),
                        );
                    }

                    {
                        let mut guard = launched_process.lock().await;
                        *guard = Some(child);
                    }

                    AttachArgs {
                        host,
                        port,
                        source_roots: source_roots.clone(),
                    }
                }
            };

            let attach_fut = Debugger::attach_with_retry(attach, attach_timeout);
            let dbg = tokio::select! {
                _ = cancel.cancelled() => {
                    terminate_existing_process(launched_process).await;
                    send_response(out_tx, seq, request, false, None, Some("cancelled".to_string()));
                    return;
                }
                res = attach_fut => match res {
                    Ok(dbg) => dbg,
                    Err(err) => {
                        terminate_existing_process(launched_process).await;
                        send_response(out_tx, seq, request, false, None, Some(err.to_string()));
                        return;
                    }
                }
            };

            {
                let mut guard = debugger.lock().await;
                *guard = Some(dbg);
            }

            spawn_event_task(
                debugger.clone(),
                out_tx.clone(),
                seq.clone(),
                terminated_sent.clone(),
                server_shutdown.clone(),
            );

            let resume_after_launch = {
                let mut sess = session.lock().await;
                sess.lifecycle = LifecycleState::LaunchedOrAttached;
                sess.kind = Some(SessionKind::Launch);
                // DAP clients typically send `configurationDone` after breakpoint configuration.
                // When `stopOnEntry` is enabled (or defaulted), keep the debuggee suspended until
                // configuration is complete, then resume via JDWP `VirtualMachine.Resume`.
                let needs_config_done_resume = args.stop_on_entry;
                let resume_after_launch =
                    needs_config_done_resume && sess.configuration_done_received;
                sess.awaiting_configuration_done_resume =
                    needs_config_done_resume && !sess.configuration_done_received;
                sess.project_root = project_root;
                resume_after_launch
            };

            apply_pending_configuration(cancel, debugger, pending_config).await;

            if resume_after_launch {
                let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                    Some(guard) => guard,
                    None => {
                        terminate_existing_process(launched_process).await;
                        disconnect_debugger(debugger).await;
                        {
                            let mut sess = session.lock().await;
                            sess.kind = None;
                            sess.awaiting_configuration_done_resume = false;
                        }
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("cancelled".to_string()),
                        );
                        return;
                    }
                };
                let Some(dbg) = guard.as_mut() else {
                    terminate_existing_process(launched_process).await;
                    disconnect_debugger(debugger).await;
                    {
                        let mut sess = session.lock().await;
                        sess.kind = None;
                        sess.awaiting_configuration_done_resume = false;
                    }
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("not attached".to_string()),
                    );
                    return;
                };

                match dbg.continue_(cancel, None).await {
                    Ok(()) => {
                        let mut sess = session.lock().await;
                        sess.lifecycle = LifecycleState::Running;
                    }
                    Err(err) if is_cancelled_error(&err) => {
                        drop(guard);
                        terminate_existing_process(launched_process).await;
                        disconnect_debugger(debugger).await;
                        {
                            let mut sess = session.lock().await;
                            sess.kind = None;
                            sess.awaiting_configuration_done_resume = false;
                        }
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("cancelled".to_string()),
                        );
                        return;
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        drop(guard);
                        terminate_existing_process(launched_process).await;
                        disconnect_debugger(debugger).await;
                        {
                            let mut sess = session.lock().await;
                            sess.kind = None;
                            sess.awaiting_configuration_done_resume = false;
                        }
                        send_response(out_tx, seq, request, false, None, Some(msg));
                        return;
                    }
                }

                send_response(out_tx, seq, request, true, None, None);
                send_event(
                    out_tx,
                    seq,
                    "continued",
                    Some(json!({ "allThreadsContinued": true })),
                );
            } else {
                send_response(out_tx, seq, request, true, None, None);
            }
        }
        "attach" => {
            {
                let sess = session.lock().await;
                if sess.lifecycle == LifecycleState::Uninitialized {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("attach is only valid after initialize".to_string()),
                    );
                    return;
                }
                if sess.kind.is_some() {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("debug session already started".to_string()),
                    );
                    return;
                }
            }

            let host = request
                .arguments
                .get("host")
                .and_then(|v| v.as_str())
                .unwrap_or("127.0.0.1");
            let Some(port) = request.arguments.get("port").and_then(|v| v.as_u64()) else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some(format!("{}.port is required", request.command)),
                );
                return;
            };
            let port = match u16::try_from(port) {
                Ok(port) => port,
                Err(_) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(format!("{}.port must be between 0-65535", request.command)),
                    );
                    return;
                }
            };
            let host: IpAddr = match host.parse() {
                Ok(host) => host,
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(format!("invalid host {host:?}: {err}")),
                    );
                    return;
                }
            };

            {
                let guard = debugger.lock().await;
                if guard.is_some() {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("already attached".to_string()),
                    );
                    return;
                }
            }

            let source_roots =
                match resolve_source_roots(request.command.as_str(), &request.arguments) {
                    Ok(roots) => roots,
                    Err(err) => {
                        send_response(out_tx, seq, request, false, None, Some(err.to_string()));
                        return;
                    }
                };
            let project_root = parse_project_root(&request.arguments);

            let dbg = match Debugger::attach(AttachArgs {
                host,
                port,
                source_roots,
            })
            .await
            {
                Ok(dbg) => dbg,
                Err(err) => {
                    send_response(out_tx, seq, request, false, None, Some(err.to_string()));
                    return;
                }
            };

            {
                let mut guard = debugger.lock().await;
                *guard = Some(dbg);
            }

            spawn_event_task(
                debugger.clone(),
                out_tx.clone(),
                seq.clone(),
                terminated_sent.clone(),
                server_shutdown.clone(),
            );

            {
                let mut sess = session.lock().await;
                sess.lifecycle = LifecycleState::LaunchedOrAttached;
                sess.kind = Some(SessionKind::Attach);
                sess.awaiting_configuration_done_resume = false;
                sess.project_root = project_root;
            }

            apply_pending_configuration(cancel, debugger, pending_config).await;

            send_response(out_tx, seq, request, true, None, None);
        }
        "setFunctionBreakpoints" => {
            if cancel.is_cancelled() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                );
                return;
            }

            let breakpoints: Vec<FunctionBreakpointSpec> = request
                .arguments
                .get("breakpoints")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|bp| {
                            let name = bp.get("name").and_then(|v| v.as_str())?.to_string();
                            let condition = bp
                                .get("condition")
                                .and_then(|c| c.as_str())
                                .map(|s| s.to_string());
                            let hit_condition = bp
                                .get("hitCondition")
                                .and_then(|c| c.as_str())
                                .map(|s| s.to_string());
                            let log_message = bp
                                .get("logMessage")
                                .and_then(|c| c.as_str())
                                .map(|s| s.to_string());

                            Some(FunctionBreakpointSpec {
                                name,
                                condition,
                                hit_condition,
                                log_message,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            {
                let mut pending = pending_config.lock().await;
                pending.function_breakpoints = Some(breakpoints.clone());
            }

            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };

            let Some(dbg) = guard.as_mut() else {
                // Allow function breakpoint configuration before attach/launch by caching it and
                // returning an "unverified" response. The cached configuration will be applied
                // automatically once the debugger is attached.
                let pending_bps: Vec<Value> = breakpoints
                    .iter()
                    .map(|_| {
                        json!({
                            "verified": false,
                            "message": "pending attach/launch",
                        })
                    })
                    .collect();
                send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "breakpoints": pending_bps })),
                    None,
                );
                return;
            };

            match dbg.set_function_breakpoints(cancel, breakpoints).await {
                Ok(bps) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Ok(bps) => send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "breakpoints": bps })),
                    None,
                ),
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "setBreakpoints" => {
            if cancel.is_cancelled() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                );
                return;
            }

            let source_path = request
                .arguments
                .get("source")
                .and_then(|s| s.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let breakpoints: Vec<BreakpointSpec> = request
                .arguments
                .get("breakpoints")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|bp| {
                            let line = bp.get("line").and_then(|l| l.as_i64())? as i32;
                            let condition = bp
                                .get("condition")
                                .and_then(|c| c.as_str())
                                .map(|s| s.to_string());
                            let hit_condition = bp
                                .get("hitCondition")
                                .and_then(|c| c.as_str())
                                .map(|s| s.to_string());
                            let log_message = bp
                                .get("logMessage")
                                .and_then(|c| c.as_str())
                                .map(|s| s.to_string());

                            Some(BreakpointSpec {
                                line,
                                condition,
                                hit_condition,
                                log_message,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();

            {
                let mut pending = pending_config.lock().await;
                if breakpoints.is_empty() {
                    pending.breakpoints.remove(source_path);
                } else {
                    pending
                        .breakpoints
                        .insert(source_path.to_string(), breakpoints.clone());
                }
            }

            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };

            let Some(dbg) = guard.as_mut() else {
                let pending_bps: Vec<Value> = breakpoints
                    .iter()
                    .map(|bp| {
                        json!({
                            "verified": false,
                            "line": bp.line,
                            "message": "pending attach/launch",
                        })
                    })
                    .collect();
                send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "breakpoints": pending_bps })),
                    None,
                );
                return;
            };

            match dbg.set_breakpoints(cancel, source_path, breakpoints).await {
                Ok(bps) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Ok(bps) => send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "breakpoints": bps })),
                    None,
                ),
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "setExceptionBreakpoints" => {
            let filters: Vec<String> = request
                .arguments
                .get("filters")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            let mut caught = false;
            let mut uncaught = false;
            for filter in &filters {
                match filter.as_str() {
                    "all" => {
                        caught = true;
                        uncaught = true;
                    }
                    "caught" => caught = true,
                    "uncaught" => uncaught = true,
                    _ => {}
                }
            }

            if let Some(options) = request
                .arguments
                .get("exceptionOptions")
                .and_then(|v| v.as_array())
            {
                for opt in options {
                    match opt.get("breakMode").and_then(|v| v.as_str()) {
                        Some("always") => {
                            caught = true;
                            uncaught = true;
                        }
                        Some("unhandled" | "userUnhandled") => uncaught = true,
                        _ => {}
                    }
                }
            }

            {
                let mut pending = pending_config.lock().await;
                pending.exception_breakpoints = Some((caught, uncaught));
            }

            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                // Cache the configuration and apply it once the debugger is attached.
                send_response(out_tx, seq, request, true, None, None);
                return;
            };

            match dbg.set_exception_breakpoints(caught, uncaught).await {
                Ok(()) => send_response(out_tx, seq, request, true, None, None),
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "threads" => {
            let guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_ref() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "threads": [] })),
                    None,
                );
                return;
            };

            match dbg.threads(cancel).await {
                Ok(threads) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Ok(threads) => {
                    let threads: Vec<Value> = threads
                        .into_iter()
                        .map(|(id, name)| json!({ "id": id, "name": name }))
                        .collect();
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!({ "threads": threads })),
                        None,
                    );
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "stackTrace" => {
            let Some(thread_id) = request.arguments.get("threadId").and_then(|v| v.as_i64()) else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("stackTrace.threadId is required".to_string()),
                );
                return;
            };

            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("not attached".to_string()),
                );
                return;
            };

            match dbg.stack_trace(cancel, thread_id).await {
                Ok(frames) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Ok(frames) => send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "stackFrames": frames, "totalFrames": frames.len() })),
                    None,
                ),
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "scopes" => {
            let Some(frame_id) = request.arguments.get("frameId").and_then(|v| v.as_i64()) else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("scopes.frameId is required".to_string()),
                );
                return;
            };

            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "scopes": [] })),
                    None,
                );
                return;
            };

            match dbg.scopes(frame_id) {
                Ok(scopes) => send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "scopes": scopes })),
                    None,
                ),
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "stepInTargets" => {
            let Some(frame_id) = request.arguments.get("frameId").and_then(|v| v.as_i64()) else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("stepInTargets.frameId is required".to_string()),
                );
                return;
            };

            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "targets": [] })),
                    None,
                );
                return;
            };

            match dbg.step_in_targets(cancel, frame_id).await {
                Ok(targets) if cancel.is_cancelled() => send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                ),
                Ok(targets) => send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "targets": targets })),
                    None,
                ),
                Err(err) if is_cancelled_error(&err) => send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                ),
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "variables" => {
            let Some(variables_reference) = request
                .arguments
                .get("variablesReference")
                .and_then(|v| v.as_i64())
            else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("variables.variablesReference is required".to_string()),
                );
                return;
            };
            let start = request.arguments.get("start").and_then(|v| v.as_i64());
            let count = request.arguments.get("count").and_then(|v| v.as_i64());

            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "variables": [] })),
                    None,
                );
                return;
            };

            match dbg
                .variables(cancel, variables_reference, start, count)
                .await
            {
                Ok(vars) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Ok(vars) => send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "variables": vars })),
                    None,
                ),
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "setVariable" => {
            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("not attached".to_string()),
                );
                return;
            };

            let args: SetVariableArguments = match serde_json::from_value(request.arguments.clone())
            {
                Ok(args) => args,
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(format!("invalid setVariable arguments: {err}")),
                    );
                    return;
                }
            };

            match dbg
                .set_variable(cancel, args.variables_reference, &args.name, &args.value)
                .await
            {
                Ok(body) if cancel.is_cancelled() => send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                ),
                Ok(body) => send_response(out_tx, seq, request, true, body, None),
                Err(err) if is_cancelled_error(&err) => send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                ),
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "exceptionInfo" => {
            let Some(thread_id) = request.arguments.get("threadId").and_then(|v| v.as_i64()) else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("exceptionInfo.threadId is required".to_string()),
                );
                return;
            };

            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("not attached".to_string()),
                );
                return;
            };

            match dbg.exception_info(cancel, thread_id).await {
                Ok(Some(info)) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Ok(Some(info)) => {
                    let full_type_name = info.exception_id;
                    let type_name = full_type_name
                        .rsplit(['.', '$'])
                        .next()
                        .unwrap_or(full_type_name.as_str())
                        .to_string();
                    let description = info.description;
                    let break_mode = info.break_mode;

                    let mut body = serde_json::Map::new();
                    body.insert("exceptionId".to_string(), json!(full_type_name.clone()));
                    if let Some(description) = description.clone() {
                        body.insert("description".to_string(), json!(description));
                    }

                    let mut details = serde_json::Map::new();
                    details.insert("fullTypeName".to_string(), json!(full_type_name));
                    details.insert("typeName".to_string(), json!(type_name));
                    if let Some(message) = description {
                        details.insert("message".to_string(), json!(message));
                    }
                    body.insert("details".to_string(), Value::Object(details));

                    body.insert("breakMode".to_string(), json!(break_mode));
                    send_response(out_tx, seq, request, true, Some(Value::Object(body)), None);
                }
                Ok(None) => send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some(format!("no exception context for threadId {thread_id}")),
                ),
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "continue" => {
            let thread_id = request.arguments.get("threadId").and_then(|v| v.as_i64());

            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("not attached".to_string()),
                );
                return;
            };

            let all_threads_continued = thread_id.is_none();
            match dbg.continue_(cancel, thread_id).await {
                Ok(()) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!({ "allThreadsContinued": all_threads_continued })),
                        None,
                    );

                    let mut body = serde_json::Map::new();
                    body.insert(
                        "allThreadsContinued".to_string(),
                        json!(all_threads_continued),
                    );
                    if let Some(thread_id) = thread_id {
                        body.insert("threadId".to_string(), json!(thread_id));
                    }
                    send_event(out_tx, seq, "continued", Some(Value::Object(body)));
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "pause" => {
            let thread_id = request.arguments.get("threadId").and_then(|v| v.as_i64());

            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("not attached".to_string()),
                );
                return;
            };

            let all_threads_stopped = thread_id.is_none();
            match dbg.pause(cancel, thread_id).await {
                Ok(()) => {
                    send_response(out_tx, seq, request, true, None, None);
                    let mut body = serde_json::Map::new();
                    body.insert("reason".to_string(), json!("pause"));
                    body.insert("allThreadsStopped".to_string(), json!(all_threads_stopped));
                    if let Some(thread_id) = thread_id {
                        body.insert("threadId".to_string(), json!(thread_id));
                    }
                    send_event(out_tx, seq, "stopped", Some(Value::Object(body)));
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "next" | "stepIn" | "stepOut" => {
            let Some(thread_id) = request.arguments.get("threadId").and_then(|v| v.as_i64()) else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some(format!("{}.threadId is required", request.command)),
                );
                return;
            };
            let depth = match request.command.as_str() {
                "next" => StepDepth::Over,
                "stepIn" => StepDepth::Into,
                _ => StepDepth::Out,
            };
            let target_id = (request.command == "stepIn")
                .then(|| request.arguments.get("targetId").and_then(|v| v.as_i64()))
                .flatten();

            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("not attached".to_string()),
                );
                return;
            };

            let step_result = match target_id {
                Some(target_id) => dbg.step_in_target(cancel, thread_id, target_id).await,
                None => dbg.step(cancel, thread_id, depth).await,
            };

            match step_result {
                Ok(()) => send_response(out_tx, seq, request, true, None, None),
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "evaluate" => {
            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("not attached".to_string()),
                );
                return;
            };

            let args: EvaluateArguments = match serde_json::from_value(request.arguments.clone()) {
                Ok(args) => args,
                Err(err) => {
                    let body = EvaluateResult {
                        result: format!("invalid evaluate arguments: {err}"),
                        type_: None,
                        variables_reference: 0,
                        evaluate_name: None,
                        presentation_hint: None,
                    };
                    send_response(out_tx, seq, request, true, Some(json!(body)), None);
                    return;
                }
            };

            let frame_id = args.frame_id.filter(|frame_id| *frame_id > 0);
            let Some(frame_id) = frame_id else {
                let body = EvaluateResult {
                    result: "This evaluation requires a stack frame. Retry while stopped or pass frameId.".to_string(),
                    type_: None,
                    variables_reference: 0,
                    evaluate_name: None,
                    presentation_hint: None,
                };
                send_response(out_tx, seq, request, true, Some(json!(body)), None);
                return;
            };

            let options = EvalOptions::from_dap_context(args.context.as_deref());

            match dbg
                .evaluate(cancel, frame_id, &args.expression, options)
                .await
            {
                Ok(body) if cancel.is_cancelled() => send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                ),
                Ok(body) => send_response(out_tx, seq, request, true, body, None),
                Err(err) if is_cancelled_error(&err) => send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                ),
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        "nova/pinObject" => {
            let Some(variables_reference) = request
                .arguments
                .get("variablesReference")
                .and_then(|v| v.as_i64())
            else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("pinObject.variablesReference is required".to_string()),
                );
                return;
            };
            let pinned = request
                .arguments
                .get("pinned")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("not attached".to_string()),
                );
                return;
            };

            match dbg
                .set_object_pinned(cancel, variables_reference, pinned)
                .await
            {
                Ok(pinned) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Ok(pinned) => send_response(
                    out_tx,
                    seq,
                    request,
                    true,
                    Some(json!({ "pinned": pinned })),
                    None,
                ),
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
        }
        // Data breakpoints / watchpoints (requires JDWP canWatchField* capabilities).
        "dataBreakpointInfo" | "setDataBreakpoints" => {
            if cancel.is_cancelled() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                );
                return;
            }

            let guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };

            let Some(dbg) = guard.as_ref() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("not attached".to_string()),
                );
                return;
            };

            let caps = dbg.capabilities().await;
            if !caps.supports_watchpoints() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some(format!(
                        "watchpoints are not supported by the target VM (JDWP canWatchFieldModification={}, canWatchFieldAccess={})",
                        caps.can_watch_field_modification, caps.can_watch_field_access
                    )),
                );
                return;
            }

            // The wire adapter doesn't implement watchpoint event requests yet, but we can still
            // provide a capability-accurate error message for better UX.
            send_response(
                out_tx,
                seq,
                request,
                false,
                None,
                Some("watchpoints are not implemented in the wire adapter yet".to_string()),
            );
        }
        // Hot swap support (class redefinition).
        "redefineClasses" | "hotCodeReplace" | "nova/hotSwap" => {
            #[derive(Debug, Default, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct HotSwapArgs {
                #[serde(default)]
                changed_files: Vec<PathBuf>,
                #[serde(default)]
                classes: Vec<HotSwapClassArg>,
                #[serde(default)]
                project_root: Option<PathBuf>,
            }

            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct HotSwapClassArg {
                class_name: String,
                bytecode_base64: String,
                #[serde(default)]
                file: Option<PathBuf>,
            }

            #[derive(Debug)]
            struct PrecompiledBuildMulti {
                outputs: HashMap<PathBuf, CompileOutputMulti>,
            }

            impl BuildSystemMulti for PrecompiledBuildMulti {
                fn compile_files_multi(&mut self, files: &[PathBuf]) -> Vec<CompileOutputMulti> {
                    files
                        .iter()
                        .map(|file| {
                            self.outputs
                                .get(file)
                                .cloned()
                                .unwrap_or_else(|| CompileOutputMulti {
                                    file: file.clone(),
                                    result: Err(CompileError::new("no bytecode provided")),
                                })
                        })
                        .collect()
                }
            }

            fn derive_source_path(class_name: &str) -> PathBuf {
                let outer = class_name.split('$').next().unwrap_or(class_name);
                PathBuf::from(format!("{}.java", outer.replace('.', "/")))
            }

            fn resolve_file_path(file: PathBuf, project_root: Option<&PathBuf>) -> PathBuf {
                if file.is_absolute() {
                    return file;
                }

                match project_root {
                    Some(root) => root.join(file),
                    None => file,
                }
            }

            fn summarize_file_class_results(
                classes: &[HotSwapClassResult],
            ) -> (HotSwapStatus, Option<String>) {
                // Deterministic precedence order:
                // 1) RedefinitionError: any non-schema redefine error.
                // 2) SchemaChange: redefine rejected due to unsupported change (restart required).
                // 3) CompileError: failed to decode bytecode, compile, etc.
                // 4) Success: all classes successfully redefined.
                if classes
                    .iter()
                    .any(|class| class.status == HotSwapStatus::RedefinitionError)
                {
                    let message = classes
                        .iter()
                        .find(|class| class.status == HotSwapStatus::RedefinitionError)
                        .and_then(|class| class.message.clone());
                    return (HotSwapStatus::RedefinitionError, message);
                }

                if classes
                    .iter()
                    .any(|class| class.status == HotSwapStatus::SchemaChange)
                {
                    let message = classes
                        .iter()
                        .find(|class| class.status == HotSwapStatus::SchemaChange)
                        .and_then(|class| class.message.clone());
                    return (HotSwapStatus::SchemaChange, message);
                }

                if classes
                    .iter()
                    .any(|class| class.status == HotSwapStatus::CompileError)
                {
                    let message = classes
                        .iter()
                        .find(|class| class.status == HotSwapStatus::CompileError)
                        .and_then(|class| class.message.clone());
                    return (HotSwapStatus::CompileError, message);
                }

                (HotSwapStatus::Success, None)
            }

            if cancel.is_cancelled() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                );
                return;
            }

            let jdwp = {
                let guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                    Some(guard) => guard,
                    None => {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("cancelled".to_string()),
                        );
                        return;
                    }
                };

                let Some(dbg) = guard.as_ref() else {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("not attached".to_string()),
                    );
                    return;
                };

                dbg.jdwp_client()
            };

            let caps = jdwp.capabilities().await;
            if !caps.supports_redefine_classes() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some(format!(
                        "hot swap is not supported by the target VM (JDWP canRedefineClasses={}, canUnrestrictedlyRedefineClasses={})",
                        caps.can_redefine_classes, caps.can_unrestrictedly_redefine_classes
                    )),
                );
                return;
            }

            let args: HotSwapArgs = match serde_json::from_value(request.arguments.clone()) {
                Ok(v) => v,
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(format!("invalid arguments: {err}")),
                    );
                    return;
                }
            };

            let project_root = {
                let sess_root = session.lock().await.project_root.clone();
                args.project_root
                    .clone()
                    .or(sess_root)
                    .map(|root| std::fs::canonicalize(&root).unwrap_or(root))
            };
            let base_dir = project_root
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));

            let mut changed_files = Vec::new();
            let mut outputs = HashMap::<PathBuf, CompileOutputMulti>::new();
            let mut class_errors = HashMap::<PathBuf, Vec<HotSwapClassResult>>::new();

            if !args.classes.is_empty() {
                let use_changed_files = !args.changed_files.is_empty()
                    && args.changed_files.len() == args.classes.len();
                for (idx, class) in args.classes.into_iter().enumerate() {
                    let file = match class.file {
                        Some(file) => resolve_file_path(file, project_root.as_ref()),
                        None if use_changed_files => args.changed_files[idx].clone(),
                        None => derive_source_path(&class.class_name),
                    };
                    if !changed_files.iter().any(|existing| existing == &file) {
                        changed_files.push(file.clone());
                    }

                    let HotSwapClassArg {
                        class_name,
                        bytecode_base64,
                        file: _,
                    } = class;

                    match general_purpose::STANDARD.decode(bytecode_base64) {
                        Ok(bytecode) => {
                            let compiled = CompiledClass { class_name, bytecode };
                            outputs
                                .entry(file.clone())
                                .and_modify(|existing| {
                                    if let Ok(classes) = &mut existing.result {
                                        classes.push(compiled.clone());
                                    }
                                })
                                .or_insert_with(|| CompileOutputMulti {
                                    file,
                                    result: Ok(vec![compiled]),
                                });
                        }
                        Err(err) => {
                            class_errors.entry(file.clone()).or_default().push(
                                HotSwapClassResult {
                                    class_name,
                                    status: HotSwapStatus::CompileError,
                                    message: Some(format!("invalid bytecodeBase64: {err}")),
                                },
                            );

                            outputs.entry(file.clone()).or_insert_with(|| CompileOutputMulti {
                                file,
                                result: Ok(Vec::new()),
                            });
                        }
                    }
                }
            } else if !args.changed_files.is_empty() {
                let javac =
                    resolve_hot_swap_javac_config(&cancel, &jdwp, project_root.as_deref()).await;

                for file in args.changed_files {
                    if cancel.is_cancelled() {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("cancelled".to_string()),
                        );
                        return;
                    }

                    let resolved = if file.is_absolute() {
                        file.clone()
                    } else {
                        base_dir.join(&file)
                    };

                    if !changed_files.iter().any(|existing| existing == &file) {
                        changed_files.push(file.clone());
                    }

                    let result = compile_java_for_hot_swap(&cancel, &javac, &resolved).await;
                    if cancel.is_cancelled() {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("cancelled".to_string()),
                        );
                        return;
                    }
                    outputs.insert(file.clone(), CompileOutputMulti { file, result });
                }
            } else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("expected either `classes` or `changedFiles`".to_string()),
                );
                return;
            }

            let build = PrecompiledBuildMulti { outputs };
            let mut engine = HotSwapEngine::new(build, jdwp);
            let mut result = tokio::select! {
                _ = cancel.cancelled() => {
                    send_response(out_tx, seq, request, false, None, Some("cancelled".to_string()));
                    return;
                }
                result = engine.hot_swap_multi_async(&changed_files) => result,
            };

            if !class_errors.is_empty() {
                for file_result in result.results.iter_mut() {
                    let Some(errors) = class_errors.get(&file_result.file) else {
                        continue;
                    };
                    file_result.classes.extend(errors.iter().cloned());
                    let (status, message) = summarize_file_class_results(&file_result.classes);
                    file_result.status = status;
                    file_result.message = message;
                }
            }

            send_response(
                out_tx,
                seq,
                request,
                true,
                Some(serde_json::to_value(result).unwrap_or_else(|_| json!({}))),
                None,
            );
        }
        // Method return values (e.g. step-out with return value).
        "nova/enableMethodReturnValues" => {
            if cancel.is_cancelled() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                );
                return;
            }

            let guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                    return;
                }
            };

            let Some(dbg) = guard.as_ref() else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("not attached".to_string()),
                );
                return;
            };

            let caps = dbg.capabilities().await;
            if !caps.supports_method_return_values() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some(format!(
                        "method return values are not supported by the target VM (JDWP canGetMethodReturnValues={})",
                        caps.can_get_method_return_values
                    )),
                );
                return;
            }

            send_response(
                out_tx,
                seq,
                request,
                true,
                Some(json!({ "enabled": true })),
                None,
            );
        }
        "terminate" => {
            terminate_existing_process(launched_process).await;
            disconnect_debugger(debugger).await;
            send_response(out_tx, seq, request, true, None, None);
            send_terminated_once(out_tx, seq, terminated_sent);
            server_shutdown.cancel();
        }
        "disconnect" => {
            let terminate_debuggee = match request
                .arguments
                .get("terminateDebuggee")
                .and_then(|v| v.as_bool())
            {
                Some(value) => value,
                None => launched_process.lock().await.is_some(),
            };

            if terminate_debuggee {
                terminate_existing_process(launched_process).await;
            } else {
                // Drop the process handle without killing. The process will continue running.
                let _ = launched_process.lock().await.take();
            }

            disconnect_debugger(debugger).await;
            send_response(out_tx, seq, request, true, None, None);
            send_terminated_once(out_tx, seq, terminated_sent);
            server_shutdown.cancel();
        }
        _ => {
            send_response(
                out_tx,
                seq,
                request,
                false,
                None,
                Some(format!("unhandled request {}", request.command)),
            );
        }
    }
}

fn is_cancelled_error(err: &DebuggerError) -> bool {
    matches!(err, DebuggerError::Jdwp(JdwpError::Cancelled))
}

fn resolve_source_roots(
    command: &str,
    arguments: &Value,
) -> std::result::Result<Vec<PathBuf>, DebuggerError> {
    let project_root = arguments
        .get("projectRoot")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .map(|root| std::fs::canonicalize(&root).unwrap_or(root));

    let mut roots = Vec::new();

    if let Some(source_roots) = arguments.get("sourceRoots").and_then(|v| v.as_array()) {
        for entry in source_roots {
            let Some(root) = entry.as_str() else {
                return Err(DebuggerError::InvalidRequest(format!(
                    "{command}.sourceRoots must be an array of strings"
                )));
            };
            roots.push(PathBuf::from(root));
        }
    }

    if let Some(root) = project_root.as_deref() {
        let config = nova_project::load_project(root).map_err(|err| {
            DebuggerError::InvalidRequest(format!(
                "{command}.projectRoot could not be loaded: {err}"
            ))
        })?;
        for source_root in config.source_roots {
            roots.push(source_root.path);
        }
    }

    let base_dir = project_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let mut out = Vec::new();
    for root in roots {
        let root = if root.is_absolute() {
            root
        } else {
            base_dir.join(root)
        };
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        if !out.iter().any(|existing| existing == &root) {
            out.push(root);
        }
    }

    Ok(out)
}

fn parse_project_root(arguments: &Value) -> Option<PathBuf> {
    arguments
        .get("projectRoot")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .map(|root| std::fs::canonicalize(&root).unwrap_or(root))
}

#[derive(Debug, Clone, Default)]
struct HotSwapJavacConfig {
    javac: String,
    classpath: Option<std::ffi::OsString>,
    module_path: Option<std::ffi::OsString>,
    release: Option<String>,
    source: Option<String>,
    target: Option<String>,
    enable_preview: bool,
}

async fn resolve_hot_swap_javac_config(
    cancel: &CancellationToken,
    jdwp: &JdwpClient,
    project_root: Option<&Path>,
) -> HotSwapJavacConfig {
    let mut config = HotSwapJavacConfig {
        javac: "javac".to_string(),
        ..HotSwapJavacConfig::default()
    };

    if let Some(project_root) = project_root {
        if let Some(build_cfg) = resolve_build_java_compile_config(cancel, project_root).await {
            config.release = build_cfg.release.clone();
            config.source = build_cfg.source.clone();
            config.target = build_cfg.target.clone();
            config.enable_preview = build_cfg.enable_preview;

            if !build_cfg.compile_classpath.is_empty() {
                config.classpath = std::env::join_paths(build_cfg.compile_classpath.iter()).ok();
            }
            if !build_cfg.module_path.is_empty() {
                config.module_path = std::env::join_paths(build_cfg.module_path.iter()).ok();
            }
        }
    }

    if config.classpath.is_none() {
        config.classpath = resolve_vm_classpath(cancel, jdwp).await;
    }

    config
}

async fn resolve_vm_classpath(
    cancel: &CancellationToken,
    jdwp: &JdwpClient,
) -> Option<std::ffi::OsString> {
    let paths = tokio::select! {
        _ = cancel.cancelled() => return None,
        res = tokio::time::timeout(Duration::from_secs(2), jdwp.virtual_machine_class_paths()) => match res {
            Ok(Ok(paths)) => paths,
            _ => return None,
        }
    };

    let base_dir = PathBuf::from(paths.base_dir);
    let entries: Vec<PathBuf> = paths
        .classpaths
        .into_iter()
        .map(PathBuf::from)
        .map(|entry| {
            if entry.is_absolute() {
                entry
            } else {
                base_dir.join(entry)
            }
        })
        .collect();
    std::env::join_paths(entries.iter()).ok()
}

async fn resolve_build_java_compile_config(
    cancel: &CancellationToken,
    project_root: &Path,
) -> Option<nova_build::JavaCompileConfig> {
    let project_root = project_root.to_path_buf();
    if !project_root.join("pom.xml").is_file()
        && !project_root.join("build.gradle").is_file()
        && !project_root.join("build.gradle.kts").is_file()
    {
        return None;
    }

    let cache_dir = std::env::temp_dir().join("nova-build-cache");
    let build_cancel = CancellationToken::new();
    let build_cancel_runner = build_cancel.clone();

    let mut handle = tokio::task::spawn_blocking(move || {
        let runner = Arc::new(DefaultCommandRunner {
            timeout: Some(Duration::from_secs(10)),
            cancellation: Some(build_cancel_runner),
        });
        let manager = BuildManager::with_runner(cache_dir, runner);
        if project_root.join("pom.xml").is_file() {
            manager.java_compile_config_maven(&project_root, None)
        } else {
            manager.java_compile_config_gradle(&project_root, None)
        }
    });

    let cfg = tokio::select! {
        _ = cancel.cancelled() => {
            build_cancel.cancel();
            handle.abort();
            return None;
        }
        _ = tokio::time::sleep(Duration::from_secs(10)) => {
            build_cancel.cancel();
            handle.abort();
            return None;
        }
        res = &mut handle => match res {
            Ok(Ok(cfg)) => cfg,
            _ => return None,
        }
    };

    Some(cfg)
}

async fn compile_java_for_hot_swap(
    cancel: &CancellationToken,
    javac: &HotSwapJavacConfig,
    source_file: &Path,
) -> std::result::Result<Vec<CompiledClass>, CompileError> {
    if !source_file.is_file() {
        return Err(CompileError::new(format!(
            "file does not exist: {}",
            source_file.display()
        )));
    }

    let output_dir = match hot_swap_temp_dir() {
        Ok(dir) => dir,
        Err(err) => {
            return Err(CompileError::new(format!(
                "failed to create hot swap output directory: {err}"
            )))
        }
    };

    let compile_result = compile_java_to_dir(cancel, javac, source_file, &output_dir).await;
    let compiled = match compile_result {
        Ok(classes) => Ok(classes),
        Err(err) => Err(err),
    };

    let _ = std::fs::remove_dir_all(&output_dir);
    compiled
}

async fn compile_java_to_dir(
    cancel: &CancellationToken,
    javac: &HotSwapJavacConfig,
    source_file: &Path,
    output_dir: &Path,
) -> std::result::Result<Vec<CompiledClass>, CompileError> {
    let mut cmd = Command::new(&javac.javac);
    // Keep memory usage low so hot-swap compilation remains reliable under the
    // `cargo_agent` RLIMIT_AS cap used in CI.
    cmd.arg("-J-Xms16m");
    cmd.arg("-J-Xmx256m");
    cmd.arg("-J-XX:CompressedClassSpaceSize=64m");
    cmd.arg("-g");
    cmd.arg("-encoding");
    cmd.arg("UTF-8");
    cmd.arg("-d");
    cmd.arg(output_dir);
    if let Some(classpath) = javac.classpath.as_ref() {
        cmd.arg("-classpath");
        cmd.arg(classpath);
    }
    if let Some(module_path) = javac.module_path.as_ref() {
        cmd.arg("--module-path");
        cmd.arg(module_path);
    }
    if let Some(release) = javac.release.as_deref() {
        cmd.arg("--release");
        cmd.arg(release);
    } else {
        if let Some(source) = javac.source.as_deref() {
            cmd.arg("--source");
            cmd.arg(source);
        }
        if let Some(target) = javac.target.as_deref() {
            cmd.arg("--target");
            cmd.arg(target);
        }
    }
    if javac.enable_preview {
        cmd.arg("--enable-preview");
    }
    cmd.arg(source_file);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|err| {
        CompileError::new(format!("failed to spawn {}: {err}", javac.javac.as_str()))
    })?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| CompileError::new("javac stdout unavailable"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| CompileError::new("javac stderr unavailable"))?;

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut buf).await;
        buf
    });

    let timeout = Duration::from_secs(30);
    let status = tokio::select! {
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(CompileError::new("cancelled"));
        }
        res = tokio::time::timeout(timeout, child.wait()) => match res {
            Ok(Ok(status)) => status,
            Ok(Err(err)) => {
                stdout_task.abort();
                stderr_task.abort();
                return Err(CompileError::new(format!("javac failed: {err}")));
            }
            Err(_elapsed) => {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                stdout_task.abort();
                stderr_task.abort();
                return Err(CompileError::new(format!("javac timed out after {timeout:?}")));
            }
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    if !status.success() {
        let output = format_javac_failure(&stdout, &stderr);
        return Err(CompileError::new(output));
    }

    let class_files = match collect_class_files(output_dir) {
        Ok(files) => files,
        Err(err) => {
            return Err(CompileError::new(format!(
                "failed to read compiled classes: {err}"
            )))
        }
    };

    let mut classes = Vec::new();
    for class_file in class_files {
        let Some(class_name) = class_name_from_class_file(output_dir, &class_file) else {
            continue;
        };
        match std::fs::read(&class_file) {
            Ok(bytecode) => classes.push(CompiledClass {
                class_name,
                bytecode,
            }),
            Err(err) => {
                return Err(CompileError::new(format!(
                    "failed to read class file {}: {err}",
                    class_file.display()
                )))
            }
        }
    }

    if classes.is_empty() {
        return Err(CompileError::new("javac produced no class files"));
    }

    Ok(classes)
}

fn hot_swap_temp_dir() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir().join("nova-dap-hot-swap");
    std::fs::create_dir_all(&base)?;
    let id = HOT_SWAP_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("compile-{id}-{}", std::process::id()));
    std::fs::create_dir(&dir)?;
    Ok(dir)
}

fn collect_class_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_class_files_inner(dir, &mut out)?;
    Ok(out)
}

fn collect_class_files_inner(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_class_files_inner(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("class") {
            out.push(path);
        }
    }
    Ok(())
}

fn class_name_from_class_file(output_dir: &Path, class_file: &Path) -> Option<String> {
    let rel = class_file.strip_prefix(output_dir).ok()?;
    let mut components: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(os) => Some(os.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    let last = components.pop()?;
    let last = last.strip_suffix(".class").unwrap_or(&last).to_string();
    components.push(last);
    Some(components.join("."))
}

fn format_javac_failure(stdout: &[u8], stderr: &[u8]) -> String {
    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str(&String::from_utf8_lossy(stdout));
    }
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(stderr));
    }
    let combined = combined.trim().to_string();

    let diagnostics = nova_build::parse_javac_diagnostics(&combined, "javac");
    let message = if diagnostics.is_empty() {
        combined
    } else {
        diagnostics
            .into_iter()
            .map(|diag| {
                let line = diag.range.start.line + 1;
                let col = diag.range.start.character + 1;
                format!("{}:{line}:{col}: {}", diag.file.display(), diag.message)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    truncate_message(message, 8 * 1024)
}

fn truncate_message(mut message: String, max_len: usize) -> String {
    if message.len() <= max_len {
        return message;
    }

    message.truncate(max_len);
    message.push_str("\n<output truncated>");
    message
}

fn requires_initialized(command: &str) -> bool {
    !matches!(
        command,
        "initialize" | "cancel" | "disconnect" | "terminate" | "nova/bugReport" | "nova/metrics"
    )
}

async fn apply_pending_configuration(
    cancel: &CancellationToken,
    debugger: &Arc<Mutex<Option<Debugger>>>,
    pending_config: &Arc<Mutex<PendingConfiguration>>,
) {
    let (breakpoints, exception_breakpoints, function_breakpoints) = {
        let pending = pending_config.lock().await;
        (
            pending.breakpoints.clone(),
            pending.exception_breakpoints,
            pending.function_breakpoints.clone(),
        )
    };

    if breakpoints.is_empty() && exception_breakpoints.is_none() && function_breakpoints.is_none() {
        return;
    }

    let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
        Some(guard) => guard,
        None => return,
    };
    let Some(dbg) = guard.as_mut() else {
        return;
    };

    for (source_path, bps) in breakpoints {
        if cancel.is_cancelled() {
            return;
        }
        let _ = dbg.set_breakpoints(cancel, &source_path, bps).await;
    }

    if let Some((caught, uncaught)) = exception_breakpoints {
        if cancel.is_cancelled() {
            return;
        }
        let _ = dbg.set_exception_breakpoints(caught, uncaught).await;
    }

    if let Some(function_breakpoints) = function_breakpoints {
        if cancel.is_cancelled() {
            return;
        }
        let _ = dbg
            .set_function_breakpoints(cancel, function_breakpoints)
            .await;
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LaunchArguments {
    // Command-based launch (build tools / tests).
    cwd: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    host: Option<String>,
    #[serde(alias = "debugPort")]
    port: Option<u16>,
    attach_timeout_ms: Option<u64>,

    // Direct Java launch.
    #[serde(rename = "javaPath", alias = "java")]
    java: Option<String>,
    classpath: Option<Classpath>,
    main_class: Option<String>,
    module_name: Option<String>,
    #[serde(default)]
    vm_args: Vec<String>,
    #[serde(default = "default_stop_on_entry")]
    stop_on_entry: bool,
}

fn default_stop_on_entry() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Classpath {
    One(String),
    Many(Vec<String>),
}

impl Classpath {
    fn entries(&self) -> Vec<&str> {
        match self {
            Classpath::One(cp) => vec![cp.as_str()],
            Classpath::Many(cps) => cps.iter().map(|s| s.as_str()).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum LaunchMode {
    Command,
    Java,
}

fn join_classpath(classpath: &Classpath) -> std::result::Result<std::ffi::OsString, String> {
    let parts: Vec<std::ffi::OsString> = classpath
        .entries()
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect();
    std::env::join_paths(parts.iter()).map_err(|err| format!("launch.classpath is invalid: {err}"))
}

async fn pick_free_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    Ok(port)
}

fn spawn_output_task<R>(
    reader: R,
    tx: mpsc::UnboundedSender<Value>,
    seq: Arc<AtomicI64>,
    category: &'static str,
    server_shutdown: CancellationToken,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();

        loop {
            buf.clear();
            let read = tokio::select! {
                _ = server_shutdown.cancelled() => return,
                res = reader.read_until(b'\n', &mut buf) => match res {
                    Ok(n) => n,
                    Err(_) => return,
                }
            };

            if read == 0 {
                return;
            }

            let output = String::from_utf8_lossy(&buf).to_string();
            send_event(
                &tx,
                &seq,
                "output",
                Some(json!({ "category": category, "output": output })),
            );
        }
    });
}

async fn terminate_existing_process(launched_process: &Arc<Mutex<Option<Child>>>) {
    let mut child = {
        let mut guard = launched_process.lock().await;
        guard.take()
    };

    if let Some(child) = child.as_mut() {
        let _ = terminate_child(child).await;
    }
}

async fn terminate_child(child: &mut Child) -> std::io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }

    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // Best effort: send SIGTERM first so shutdown hooks can run.
            let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    return Err(err);
                }
            }
        }

        let deadline = Instant::now() + Duration::from_millis(750);
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    child.start_kill()?;

    // Reap the process, but don't hang shutdown if it refuses to die.
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    Ok(())
}

async fn disconnect_debugger(debugger: &Arc<Mutex<Option<Debugger>>>) {
    let mut dbg = {
        let mut guard = debugger.lock().await;
        guard.take()
    };
    if let Some(dbg) = dbg.as_mut() {
        dbg.disconnect().await;
    }
}

async fn wait_initialized(
    cancel: &CancellationToken,
    mut initialized: watch::Receiver<bool>,
) -> bool {
    loop {
        if *initialized.borrow() {
            return true;
        }

        tokio::select! {
            _ = cancel.cancelled() => return false,
            changed = initialized.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
        }
    }
}

async fn lock_or_cancel<'a, T>(
    cancel: &'a CancellationToken,
    mutex: &'a Mutex<T>,
) -> Option<tokio::sync::MutexGuard<'a, T>> {
    tokio::select! {
        _ = cancel.cancelled() => None,
        guard = mutex.lock() => Some(guard),
    }
}

fn send_event(
    tx: &mpsc::UnboundedSender<Value>,
    seq: &Arc<AtomicI64>,
    event: impl Into<String>,
    body: Option<Value>,
) {
    let s = seq.fetch_add(1, Ordering::Relaxed);
    let evt = make_event(s, event, body);
    let _ = tx.send(serde_json::to_value(evt).unwrap_or_else(|_| json!({})));
}

fn send_response(
    tx: &mpsc::UnboundedSender<Value>,
    seq: &Arc<AtomicI64>,
    request: &Request,
    success: bool,
    body: Option<Value>,
    message: Option<String>,
) {
    if !success {
        nova_metrics::MetricsRegistry::global().record_error(&request.command);
    }
    let s = seq.fetch_add(1, Ordering::Relaxed);
    let resp = make_response(s, request, success, body, message);
    let _ = tx.send(serde_json::to_value(resp).unwrap_or_else(|_| json!({})));
}

struct RequestMetricsGuard<'a> {
    command: &'a str,
    start: Instant,
    metrics: &'static nova_metrics::MetricsRegistry,
}

impl<'a> RequestMetricsGuard<'a> {
    fn new(command: &'a str, metrics: &'static nova_metrics::MetricsRegistry) -> Self {
        Self {
            command,
            start: Instant::now(),
            metrics,
        }
    }
}

impl Drop for RequestMetricsGuard<'_> {
    fn drop(&mut self) {
        self.metrics
            .record_request(self.command, self.start.elapsed());
        if std::thread::panicking() {
            self.metrics.record_panic(self.command);
            self.metrics.record_error(self.command);
        }
    }
}

fn send_terminated_once(
    tx: &mpsc::UnboundedSender<Value>,
    seq: &Arc<AtomicI64>,
    terminated_sent: &Arc<AtomicBool>,
) {
    if terminated_sent
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        send_event(tx, seq, "terminated", None);
    }
}

fn spawn_event_task(
    debugger: Arc<Mutex<Option<Debugger>>>,
    tx: mpsc::UnboundedSender<Value>,
    seq: Arc<AtomicI64>,
    terminated_sent: Arc<AtomicBool>,
    server_shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut events: Option<broadcast::Receiver<nova_jdwp::wire::JdwpEvent>> = None;
        let mut jdwp_shutdown: Option<CancellationToken> = None;
        let mut throwable_detail_message_field_cache: Option<Option<u64>> = None;

        {
            let guard = debugger.lock().await;
            if let Some(dbg) = guard.as_ref() {
                events = Some(dbg.subscribe_events());
                jdwp_shutdown = Some(dbg.jdwp_shutdown_token());
            }
        }

        let Some(mut events) = events else {
            return;
        };
        let Some(jdwp_shutdown) = jdwp_shutdown else {
            return;
        };

        let format_value = |value: &nova_jdwp::wire::JdwpValue| -> String {
            match value {
                nova_jdwp::wire::JdwpValue::Object { tag, id } => {
                    if *id == 0 {
                        "null".to_string()
                    } else if *tag == b'[' {
                        format!("array@0x{id:x}")
                    } else {
                        format!("object@0x{id:x}")
                    }
                }
                other => other.to_string(),
            }
        };

        loop {
            let event = tokio::select! {
                _ = server_shutdown.cancelled() => return,
                _ = jdwp_shutdown.cancelled() => {
                    send_terminated_once(&tx, &seq, &terminated_sent);
                    server_shutdown.cancel();
                    return;
                }
                event = events.recv() => match event {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Closed) => {
                        send_terminated_once(&tx, &seq, &terminated_sent);
                        server_shutdown.cancel();
                        return;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                },
            };

            // Some events require consulting debugger state (conditional/log breakpoints).
            let mut breakpoint_disposition: Option<BreakpointDisposition> = None;
            // Best-effort exception label for DAP's stopped `text` field.
            let mut exception_context: Option<(JdwpClient, ObjectId)> = None;
            let mut step_value: Option<VmStoppedValue> = None;
            let mut suppress_stopped_event = false;

            {
                let mut guard = debugger.lock().await;
                if let Some(dbg) = guard.as_mut() {
                    dbg.handle_vm_event(&event).await;

                    if let nova_jdwp::wire::JdwpEvent::Breakpoint {
                        request_id,
                        thread,
                        location,
                    } = &event
                    {
                        match dbg
                            .handle_breakpoint_event(*request_id, *thread, *location)
                            .await
                        {
                            Ok(disposition) => breakpoint_disposition = Some(disposition),
                            Err(_) => breakpoint_disposition = Some(BreakpointDisposition::Stop),
                        }

                        let is_logpoint = dbg.breakpoint_is_logpoint(*request_id);

                        // If the breakpoint should not stop execution, resume immediately.
                        //
                        // Logpoints are configured with `SuspendPolicy::NONE`, so there is no
                        // suspension to resume (and resuming could accidentally unpause another
                        // thread that is stopped in the debugger).
                        match breakpoint_disposition.as_ref() {
                            Some(BreakpointDisposition::Continue) => {
                                if !is_logpoint {
                                    let _ =
                                        dbg.continue_(&server_shutdown, Some(*thread as i64)).await;
                                }
                            }
                            Some(BreakpointDisposition::Log { .. }) => {}
                            Some(BreakpointDisposition::Stop) => {
                                step_value =
                                    dbg.take_step_output_value(&server_shutdown, *thread).await;
                            }
                            None => {}
                        }
                    } else if let nova_jdwp::wire::JdwpEvent::SingleStep { thread, .. } = &event {
                        step_value = dbg.take_step_output_value(&server_shutdown, *thread).await;
                    } else if let nova_jdwp::wire::JdwpEvent::Exception { thread, .. } = &event {
                        step_value = dbg.take_step_output_value(&server_shutdown, *thread).await;
                    }

                    if dbg
                        .maybe_continue_smart_step(&server_shutdown, &event)
                        .await
                    {
                        suppress_stopped_event = true;
                        step_value = None;
                    }

                    if let nova_jdwp::wire::JdwpEvent::Exception { exception, .. } = &event {
                        exception_context = Some((dbg.jdwp_client(), *exception));
                    }
                }
            }

            if suppress_stopped_event {
                continue;
            }

            let exception_text = if let Some((jdwp, exception)) = exception_context {
                // Avoid delaying the stopped event for too long; exception details are also
                // available via the dedicated `exceptionInfo` request.
                match tokio::time::timeout(
                    Duration::from_millis(200),
                    exception_stopped_text(
                        &jdwp,
                        exception,
                        &mut throwable_detail_message_field_cache,
                    ),
                )
                .await
                {
                    Ok(text) => text,
                    Err(_elapsed) => None,
                }
            } else {
                None
            };

            match event {
                nova_jdwp::wire::JdwpEvent::Breakpoint { thread, .. } => {
                    match breakpoint_disposition.unwrap_or(BreakpointDisposition::Stop) {
                        BreakpointDisposition::Stop => {
                            if let Some(value) = step_value {
                                let (label, value) = match value {
                                    VmStoppedValue::Return(v) => ("Return value", v),
                                    VmStoppedValue::Expression(v) => ("Expression value", v),
                                };
                                let output = format!("{label}: {}\n", format_value(&value));
                                send_event(
                                    &tx,
                                    &seq,
                                    "output",
                                    Some(json!({"category": "console", "output": output})),
                                );
                            }

                            send_event(
                                &tx,
                                &seq,
                                "stopped",
                                Some(
                                    json!({"reason": "breakpoint", "threadId": thread as i64, "allThreadsStopped": false}),
                                ),
                            );
                        }
                        BreakpointDisposition::Continue => {}
                        BreakpointDisposition::Log { message } => {
                            send_event(
                                &tx,
                                &seq,
                                "output",
                                Some(json!({
                                    "category": "console",
                                    "output": format!("{message}\n")
                                })),
                            );
                        }
                    }
                }
                nova_jdwp::wire::JdwpEvent::SingleStep { thread, .. } => {
                    if let Some(value) = step_value {
                        let (label, value) = match value {
                            VmStoppedValue::Return(v) => ("Return value", v),
                            VmStoppedValue::Expression(v) => ("Expression value", v),
                        };
                        let output = format!("{label}: {}\n", format_value(&value));
                        send_event(
                            &tx,
                            &seq,
                            "output",
                            Some(json!({"category": "console", "output": output})),
                        );
                    }
                    send_event(
                        &tx,
                        &seq,
                        "stopped",
                        Some(
                            json!({"reason": "step", "threadId": thread as i64, "allThreadsStopped": false}),
                        ),
                    );
                }
                nova_jdwp::wire::JdwpEvent::Exception { thread, .. } => {
                    if let Some(value) = step_value {
                        let (label, value) = match value {
                            VmStoppedValue::Return(v) => ("Return value", v),
                            VmStoppedValue::Expression(v) => ("Expression value", v),
                        };
                        let output = format!("{label}: {}\n", format_value(&value));
                        send_event(
                            &tx,
                            &seq,
                            "output",
                            Some(json!({"category": "console", "output": output})),
                        );
                    }
                    let mut body = serde_json::Map::new();
                    body.insert("reason".to_string(), json!("exception"));
                    body.insert("threadId".to_string(), json!(thread as i64));
                    body.insert("allThreadsStopped".to_string(), json!(false));
                    if let Some(text) = exception_text {
                        body.insert("text".to_string(), json!(text));
                    }
                    send_event(&tx, &seq, "stopped", Some(Value::Object(body)));
                }
                nova_jdwp::wire::JdwpEvent::ThreadStart { thread, .. } => {
                    send_event(
                        &tx,
                        &seq,
                        "thread",
                        Some(json!({"reason": "started", "threadId": thread as i64})),
                    );
                }
                nova_jdwp::wire::JdwpEvent::ThreadDeath { thread, .. } => {
                    send_event(
                        &tx,
                        &seq,
                        "thread",
                        Some(json!({"reason": "exited", "threadId": thread as i64})),
                    );
                }
                nova_jdwp::wire::JdwpEvent::VmDeath => {
                    send_terminated_once(&tx, &seq, &terminated_sent);
                    server_shutdown.cancel();
                    return;
                }
                _ => {}
            }
        }
    });
}

async fn exception_stopped_text(
    jdwp: &JdwpClient,
    exception: ObjectId,
    throwable_detail_message_field_cache: &mut Option<Option<u64>>,
) -> Option<String> {
    if exception == 0 {
        return None;
    }

    let (_ref_type_tag, class_id) = jdwp.object_reference_reference_type(exception).await.ok()?;
    let sig = jdwp.reference_type_signature(class_id).await.ok()?;
    let full_type_name = signature_to_object_type_name(&sig)?;
    let type_name = full_type_name
        .rsplit(['.', '$'])
        .next()
        .unwrap_or(full_type_name.as_str());

    let message = exception_message(jdwp, exception, throwable_detail_message_field_cache).await;
    match message.as_deref() {
        Some(message) if !message.is_empty() => Some(format!("{type_name}: {message}")),
        _ => Some(type_name.to_string()),
    }
}

async fn exception_message(
    jdwp: &JdwpClient,
    exception: ObjectId,
    throwable_detail_message_field_cache: &mut Option<Option<u64>>,
) -> Option<String> {
    let field_id =
        throwable_detail_message_field_id(jdwp, throwable_detail_message_field_cache).await?;

    let values = jdwp
        .object_reference_get_values(exception, &[field_id])
        .await
        .ok()?;
    let value = values.into_iter().next()?;
    let JdwpValue::Object { id, .. } = value else {
        return None;
    };
    if id == 0 {
        return None;
    }
    let message = jdwp.string_reference_value(id).await.ok()?;
    if message.is_empty() {
        None
    } else {
        Some(message)
    }
}

async fn throwable_detail_message_field_id(
    jdwp: &JdwpClient,
    cache: &mut Option<Option<u64>>,
) -> Option<u64> {
    if let Some(cached) = *cache {
        return cached;
    }

    let classes = jdwp
        .classes_by_signature("Ljava/lang/Throwable;")
        .await
        .ok()?;
    let throwable = classes.first()?.type_id;
    let fields = jdwp.reference_type_fields(throwable).await.ok()?;
    let field_id = fields
        .iter()
        .find(|field| field.name == "detailMessage")
        .map(|field| field.field_id);
    *cache = Some(field_id);
    field_id
}

fn signature_to_object_type_name(sig: &str) -> Option<String> {
    let mut sig = sig.trim();
    let mut dims = 0;
    while let Some(rest) = sig.strip_prefix('[') {
        dims += 1;
        sig = rest;
    }

    let base = sig
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))?
        .replace('/', ".");

    let mut out = base;
    for _ in 0..dims {
        out.push_str("[]");
    }
    Some(out)
}

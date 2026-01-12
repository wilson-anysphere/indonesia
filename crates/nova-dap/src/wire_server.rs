use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::IpAddr,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use base64::{engine::general_purpose, Engine as _};
use nova_jdwp::wire::{JdwpClient, JdwpError, JdwpValue, ObjectId};
use nova_scheduler::CancellationToken;
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::{lookup_host, TcpListener},
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
    javac::{compile_java_for_hot_swap, resolve_hot_swap_javac_config},
    stream_debug::{StreamDebugArguments, STREAM_DEBUG_COMMAND},
    wire_debugger::{
        is_retryable_attach_error, AttachArgs, BreakpointDisposition, BreakpointSpec, Debugger,
        DebuggerError, FunctionBreakpointSpec, StepDepth, VmStoppedValue,
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

const OUTGOING_DAP_QUEUE_CAPACITY: usize = 1024;

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

struct LaunchedProcess {
    /// Cancellation token used to "detach" from the launched process (drop the child handle)
    /// without treating it as a debuggee exit.
    detach: CancellationToken,
    /// Request that the launched process be terminated. We still emit `exited` when it actually
    /// exits (best-effort).
    kill: watch::Sender<bool>,
    /// Whether the launch completed successfully.
    ///
    /// When set to `Some(true)`, the monitor task treats process exit as the end of the debug
    /// session and emits `exited`/`terminated`.
    ///
    /// When set to `Some(false)`, the monitor task suppresses those events. This is used by the
    /// DAP `restart` request to terminate the old debuggee without tearing down the DAP session.
    outcome: watch::Sender<Option<bool>>,
    /// Background task that waits for the child to exit and emits DAP events.
    monitor: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
struct SessionLifecycle {
    lifecycle: LifecycleState,
    kind: Option<SessionKind>,
    awaiting_configuration_done_resume: bool,
    configuration_done_received: bool,
    project_root: Option<PathBuf>,
    last_launch: Option<StoredLaunchConfig>,
    debugger_id: Option<u64>,
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
            last_launch: None,
            debugger_id: None,
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
    // In stdio DAP mode we forward debuggee output via pipes. If the user disconnects with
    // `terminateDebuggee=false`, the adapter exits and closes those pipes. Without SIGPIPE ignored,
    // debuggees may be terminated by the kernel when they continue writing to stdout/stderr.
    //
    // We set SIGPIPE to `SIG_IGN` in the adapter process so launched debuggees inherit the
    // behavior without requiring unsafe `pre_exec` hooks (which can force `fork` under
    // `posix_spawn`-capable platforms).
    #[cfg(unix)]
    ignore_sigpipe();

    let (out_tx, mut out_rx) = mpsc::channel::<Value>(OUTGOING_DAP_QUEUE_CAPACITY);
    let seq = Arc::new(AtomicI64::new(1));
    let terminated_sent = Arc::new(AtomicBool::new(false));
    let exited_sent = Arc::new(AtomicBool::new(false));
    let next_debugger_id = Arc::new(AtomicU64::new(1));
    let suppress_termination_debugger_id = Arc::new(AtomicU64::new(0));
    let debugger: Arc<Mutex<Option<Debugger>>> = Arc::new(Mutex::new(None));
    let launched_process: Arc<Mutex<Option<LaunchedProcess>>> = Arc::new(Mutex::new(None));
    let session: Arc<Mutex<SessionLifecycle>> = Arc::new(Mutex::new(SessionLifecycle::default()));
    let pending_config: Arc<Mutex<PendingConfiguration>> =
        Arc::new(Mutex::new(PendingConfiguration::default()));
    let in_flight: Arc<Mutex<HashMap<i64, CancellationToken>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let server_shutdown = CancellationToken::new();
    let (initialized_tx, initialized_rx) = watch::channel(false);

    let mut writer_task = tokio::spawn(async move {
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
                    next_debugger_id.clone(),
                    suppress_termination_debugger_id.clone(),
                    debugger.clone(),
                    launched_process.clone(),
                    session.clone(),
                    pending_config.clone(),
                    in_flight.clone(),
                    initialized_tx.clone(),
                    initialized_rx.clone(),
                    server_shutdown.clone(),
                    terminated_sent.clone(),
                    exited_sent.clone(),
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
    match tokio::time::timeout(Duration::from_secs(2), &mut writer_task).await {
        Ok(res) => {
            let _ = res;
        }
        Err(_elapsed) => {
            writer_task.abort();
            let _ = writer_task.await;
        }
    }
    Ok(())
}

async fn handle_request(
    request: Request,
    cancel: CancellationToken,
    out_tx: mpsc::Sender<Value>,
    seq: Arc<AtomicI64>,
    next_debugger_id: Arc<AtomicU64>,
    suppress_termination_debugger_id: Arc<AtomicU64>,
    debugger: Arc<Mutex<Option<Debugger>>>,
    launched_process: Arc<Mutex<Option<LaunchedProcess>>>,
    session: Arc<Mutex<SessionLifecycle>>,
    pending_config: Arc<Mutex<PendingConfiguration>>,
    in_flight: Arc<Mutex<HashMap<i64, CancellationToken>>>,
    initialized_tx: watch::Sender<bool>,
    initialized_rx: watch::Receiver<bool>,
    server_shutdown: CancellationToken,
    terminated_sent: Arc<AtomicBool>,
    exited_sent: Arc<AtomicBool>,
) {
    // The request handler is the "root" for bookkeeping. If `handle_request_inner` panics we still
    // need to (1) send an error response, and (2) remove the request from `in_flight` so follow-up
    // `cancel` requests and shutdown bookkeeping don't leak entries.
    //
    // We keep the top-level guard here to capture end-to-end request latency.
    let command_for_metrics = request.command.clone();
    let _request_metrics = RequestMetricsGuard::new(
        &command_for_metrics,
        nova_metrics::MetricsRegistry::global(),
    );
    let request_seq = request.seq;
    let mut in_flight_cleanup = InFlightCleanupGuard::new(in_flight.clone(), request_seq);

    // Run the real handler in a child task so panics become a `JoinError` instead of unwinding
    // past our cleanup/response logic.
    let handler_request = request.clone();
    let handler_out_tx = out_tx.clone();
    let handler_seq = seq.clone();
    let handler_next_debugger_id = next_debugger_id.clone();
    let handler_suppress_termination_debugger_id = suppress_termination_debugger_id.clone();
    let handler_debugger = debugger.clone();
    let handler_launched_process = launched_process.clone();
    let handler_session = session.clone();
    let handler_pending_config = pending_config.clone();
    let handler_in_flight = in_flight.clone();
    let handler_initialized_tx = initialized_tx.clone();
    let handler_server_shutdown = server_shutdown.clone();
    let handler_terminated_sent = terminated_sent.clone();
    let handler_exited_sent = exited_sent.clone();
    let handler_cancel = cancel;
    let handler = tokio::spawn(async move {
        handle_request_inner(
            &handler_request,
            &handler_cancel,
            &handler_out_tx,
            &handler_seq,
            &handler_next_debugger_id,
            &handler_suppress_termination_debugger_id,
            &handler_debugger,
            &handler_launched_process,
            &handler_session,
            &handler_pending_config,
            &handler_in_flight,
            &handler_initialized_tx,
            initialized_rx,
            &handler_server_shutdown,
            &handler_terminated_sent,
            &handler_exited_sent,
        )
        .await;
    });

    match handler.await {
        Ok(()) => {}
        Err(err) => {
            if err.is_panic() {
                // Best-effort metrics for panic visibility (the metrics guard isn't on the
                // panicking task, so it won't see `std::thread::panicking()`).
                nova_metrics::MetricsRegistry::global().record_panic(&command_for_metrics);

                let mut message = "internal error (panic)".to_string();
                // In release builds, try to capture a bug report bundle to aid debugging.
                #[cfg(all(not(test), not(debug_assertions)))]
                {
                    if let Ok(Some(path)) =
                        std::panic::catch_unwind(|| build_panic_bug_report_bundle())
                    {
                        message.push_str(&format!(" bug report: {path}"));
                    }
                }

                send_response(
                    &out_tx,
                    &seq,
                    &request,
                    false,
                    None,
                    Some(message),
                    &server_shutdown,
                )
                .await;
            } else {
                // Aborted tasks are rare (generally shutdown), but still respond best-effort so
                // clients don't hang.
                send_response(
                    &out_tx,
                    &seq,
                    &request,
                    false,
                    None,
                    Some("internal error".to_string()),
                    &server_shutdown,
                )
                .await;
            }
        }
    }

    let mut guard = in_flight.lock().await;
    guard.remove(&request_seq);
    in_flight_cleanup.disarm();
}

async fn handle_request_inner(
    request: &Request,
    cancel: &CancellationToken,
    out_tx: &mpsc::Sender<Value>,
    seq: &Arc<AtomicI64>,
    next_debugger_id: &Arc<AtomicU64>,
    suppress_termination_debugger_id: &Arc<AtomicU64>,
    debugger: &Arc<Mutex<Option<Debugger>>>,
    launched_process: &Arc<Mutex<Option<LaunchedProcess>>>,
    session: &Arc<Mutex<SessionLifecycle>>,
    pending_config: &Arc<Mutex<PendingConfiguration>>,
    in_flight: &Arc<Mutex<HashMap<i64, CancellationToken>>>,
    initialized_tx: &watch::Sender<bool>,
    initialized_rx: watch::Receiver<bool>,
    server_shutdown: &CancellationToken,
    terminated_sent: &Arc<AtomicBool>,
    exited_sent: &Arc<AtomicBool>,
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
                server_shutdown,
            )
            .await;
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
                sess.last_launch = None;
                sess.debugger_id = None;
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
                "supportsRestartRequest": true,
                "supportsSetVariable": true,
                "supportsStepInTargetsRequest": true,
                "supportsDelayedStackTraceLoading": true,
                "supportsStepBack": false,
                "supportsFunctionBreakpoints": true,
                "supportsVariablePaging": true,
                "supportsExceptionBreakpoints": true,
                "supportsExceptionInfoRequest": true,
                "supportsBreakpointLocationsRequest": true,
                "supportsDataBreakpoints": true,
                "exceptionBreakpointFilters": [
                    { "filter": "caught", "label": "Caught Exceptions", "default": false },
                    { "filter": "uncaught", "label": "Uncaught Exceptions", "default": false },
                    { "filter": "all", "label": "All Exceptions", "default": false },
                ],
                "supportsConditionalBreakpoints": true,
                "supportsHitConditionalBreakpoints": true,
                "supportsLogPoints": true,
            });
            send_response(out_tx, seq, request, true, Some(body), None, server_shutdown).await;

            if !*initialized_rx.borrow() {
                send_event(out_tx, seq, "initialized", None, server_shutdown).await;
                let _ = initialized_tx.send(true);
            }
        }
        "nova/metrics" => {
            match serde_json::to_value(nova_metrics::MetricsRegistry::global().snapshot()) {
                Ok(snapshot) => {
                    send_response(out_tx, seq, request, true, Some(snapshot), None, server_shutdown)
                        .await
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
            }
        }
        // Test-only escape hatch for validating panic isolation. Note that `nova-dap` is compiled
        // *without* `cfg(test)` for `tests/` integration tests, so we enable this handler in debug
        // builds too.
        #[cfg(test)]
        "nova/testPanic" => {
            panic!("intentional panic from nova/testPanic");
        }
        #[cfg(all(not(test), debug_assertions))]
        "nova/testPanic" => {
            panic!("intentional panic from nova/testPanic");
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                    server_shutdown,
                )
                .await;
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
            send_response(out_tx, seq, request, true, None, None, server_shutdown).await;
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
                            server_shutdown,
                        )
                        .await;
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
                        server_shutdown,
                    )
                    .await;
                    return;
                };

                match dbg.continue_(cancel, None).await {
                    Ok(()) => {
                        send_response(out_tx, seq, request, true, None, None, server_shutdown).await;
                        send_event(
                            out_tx,
                            seq,
                            "continued",
                            Some(json!({ "allThreadsContinued": true })),
                            server_shutdown,
                        )
                        .await;
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
                            server_shutdown,
                        )
                        .await;
                    }
                    Err(err) => {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some(err.to_string()),
                            server_shutdown,
                        )
                        .await
                    }
                }
            } else {
                send_response(out_tx, seq, request, true, None, None, server_shutdown).await;
            }
        }
        "launch" => {
            let mut args: LaunchArguments = match serde_json::from_value(request.arguments.clone())
            {
                Ok(args) => args,
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(format!("launch arguments are invalid: {err}")),
                        server_shutdown,
                    )
                    .await;
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
                        server_shutdown,
                    )
                    .await;
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
                        server_shutdown,
                    )
                    .await;
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
                        server_shutdown,
                    )
                    .await;
                    return;
                }
            }

            let source_roots =
                match resolve_source_roots(request.command.as_str(), &request.arguments) {
                    Ok(roots) => roots,
                    Err(err) => {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some(err.to_string()),
                            server_shutdown,
                        )
                        .await;
                        return;
                    }
                };
            let project_root = parse_project_root(&request.arguments);

            let attach_timeout_ms = args.attach_timeout_ms.unwrap_or(30_000);
            args.attach_timeout_ms = Some(attach_timeout_ms);
            let attach_timeout = Duration::from_millis(attach_timeout_ms);

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
                    server_shutdown,
                )
                .await;
                return;
            };

            let process_name = match mode {
                LaunchMode::Command => args.command.clone().unwrap_or_default(),
                LaunchMode::Java => args.main_class.clone().unwrap_or_default(),
            };

            // Apply launch defaults so they can be reused by `restart`.
            match mode {
                LaunchMode::Command => {
                    if args.host.is_none() {
                        args.host = Some("127.0.0.1".to_string());
                    }
                    if args.port.is_none() {
                        args.port = Some(5005);
                    }
                }
                LaunchMode::Java => {
                    if args.java.is_none() {
                        args.java = Some("java".to_string());
                    }
                }
            }

            let mut launch_outcome_tx: Option<watch::Sender<Option<bool>>>;
            let (attach_hosts, attach_port, attach_target_label, launched_pid) = match mode {
                LaunchMode::Command => {
                    let Some(cwd) = args.cwd.as_deref() else {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("launch.cwd is required".to_string()),
                            server_shutdown,
                        )
                        .await;
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
                            server_shutdown,
                        )
                        .await;
                        return;
                    };

                    let port = args.port.unwrap_or(5005);
                    let host = args.host.as_deref().unwrap_or("127.0.0.1");
                    let host_label = host.to_string();
                    let resolved_hosts = match resolve_host_candidates(host, port).await {
                        Ok(hosts) if !hosts.is_empty() => hosts,
                        Ok(_) => {
                            send_response(
                                out_tx,
                                seq,
                                request,
                                false,
                                None,
                                Some(format!(
                                    "failed to resolve host {host_label:?}: no addresses found"
                                )),
                                server_shutdown,
                            )
                            .await;
                            return;
                        }
                        Err(err) => {
                            send_response(
                                out_tx,
                                seq,
                                request,
                                false,
                                None,
                                Some(format!("invalid host {host_label:?}: {err}")),
                                server_shutdown,
                            )
                            .await;
                            return;
                        }
                    };
                    let attach_target_label = format!("{host_label}:{port}");

                    let mut cmd = Command::new(command);
                    cmd.args(&args.args);
                    cmd.current_dir(cwd);
                    cmd.stdin(Stdio::null());
                    cmd.stdout(Stdio::piped());
                    cmd.stderr(Stdio::piped());
                    // Ensure `disconnect` with `terminateDebuggee=false` can safely detach without
                    // killing the launched process.
                    cmd.kill_on_drop(false);
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
                                server_shutdown,
                            )
                            .await;
                            return;
                        }
                    };
                    let Some(pid) = child.id() else {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("failed to determine launched process pid".to_string()),
                            server_shutdown,
                        )
                        .await;
                        return;
                    };

                    let launched_pid = Some(pid);
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
                        let (proc, outcome_tx) = spawn_launched_process_exit_task(
                            child,
                            out_tx.clone(),
                            seq.clone(),
                            Arc::clone(exited_sent),
                            Arc::clone(terminated_sent),
                            server_shutdown.clone(),
                        );
                        launch_outcome_tx = Some(outcome_tx);
                        *guard = Some(proc);
                    }

                    (resolved_hosts, port, attach_target_label, launched_pid)
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
                            server_shutdown,
                        )
                        .await;
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
                                    server_shutdown,
                                )
                                .await;
                                return;
                            }
                        },
                    };
                    // Persist the resolved port for restart so we can re-use it.
                    args.port = Some(port);
                    let host: IpAddr = "127.0.0.1".parse().unwrap();
                    let attach_target_label = format!("{host}:{port}");

                    let java = args.java.clone().unwrap_or_else(|| "java".to_string());

                    let cp_joined = match join_classpath(&classpath) {
                        Ok(cp) => cp,
                        Err(err) => {
                            send_response(out_tx, seq, request, false, None, Some(err), server_shutdown)
                                .await;
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
                    // Ensure `disconnect` with `terminateDebuggee=false` can safely detach without
                    // killing the launched JVM.
                    cmd.kill_on_drop(false);
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
                                server_shutdown,
                            )
                            .await;
                            return;
                        }
                    };
                    let Some(pid) = child.id() else {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("failed to determine launched process pid".to_string()),
                            server_shutdown,
                        )
                        .await;
                        return;
                    };

                    let launched_pid = Some(pid);
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
                        let (proc, outcome_tx) = spawn_launched_process_exit_task(
                            child,
                            out_tx.clone(),
                            seq.clone(),
                            Arc::clone(exited_sent),
                            Arc::clone(terminated_sent),
                            server_shutdown.clone(),
                        );
                        launch_outcome_tx = Some(outcome_tx);
                        *guard = Some(proc);
                    }

                    (vec![host], port, attach_target_label, launched_pid)
                }
            };

            let process_event_body =
                make_process_event_body(&process_name, launched_pid, true, "launch");

            {
                let mut sess = session.lock().await;
                sess.last_launch = Some(StoredLaunchConfig {
                    mode,
                    args: args.clone(),
                    source_roots: source_roots.clone(),
                    project_root: project_root.clone(),
                });
            }

            let attach_fut = attach_debugger_with_retry_hosts(
                attach_hosts,
                attach_port,
                source_roots,
                attach_timeout,
            );
            let dbg = tokio::select! {
                _ = cancel.cancelled() => {
                    if let Some(tx) = launch_outcome_tx.take() {
                        let _ = tx.send(Some(false));
                    }
                    terminate_existing_process(launched_process).await;
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                    return;
                }
                res = attach_fut => match res {
                    Ok(dbg) => dbg,
                    Err(err) => {
                        if let Some(tx) = launch_outcome_tx.take() {
                            let _ = tx.send(Some(false));
                        }
                        terminate_existing_process(launched_process).await;
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some(format!("failed to attach to {attach_target_label}: {err}")),
                            server_shutdown,
                        )
                        .await;
                        return;
                    }
                }
            };

            {
                let mut guard = debugger.lock().await;
                *guard = Some(dbg);
            }

            let debugger_id = next_debugger_id.fetch_add(1, Ordering::Relaxed);
            spawn_event_task(
                debugger.clone(),
                out_tx.clone(),
                seq.clone(),
                terminated_sent.clone(),
                server_shutdown.clone(),
                debugger_id,
                suppress_termination_debugger_id.clone(),
            );

            let resume_after_launch = {
                let mut sess = session.lock().await;
                sess.lifecycle = LifecycleState::LaunchedOrAttached;
                sess.kind = Some(SessionKind::Launch);
                sess.debugger_id = Some(debugger_id);
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
                        if let Some(tx) = launch_outcome_tx.take() {
                            let _ = tx.send(Some(false));
                        }
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
                            server_shutdown,
                        )
                        .await;
                        return;
                    }
                };
                let Some(dbg) = guard.as_mut() else {
                    if let Some(tx) = launch_outcome_tx.take() {
                        let _ = tx.send(Some(false));
                    }
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
                        server_shutdown,
                    )
                    .await;
                    return;
                };

                match dbg.continue_(cancel, None).await {
                    Ok(()) => {
                        let mut sess = session.lock().await;
                        sess.lifecycle = LifecycleState::Running;
                    }
                    Err(err) if is_cancelled_error(&err) => {
                        if let Some(tx) = launch_outcome_tx.take() {
                            let _ = tx.send(Some(false));
                        }
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
                            server_shutdown,
                        )
                        .await;
                        return;
                    }
                    Err(err) => {
                        if let Some(tx) = launch_outcome_tx.take() {
                            let _ = tx.send(Some(false));
                        }
                        let msg = err.to_string();
                        drop(guard);
                        terminate_existing_process(launched_process).await;
                        disconnect_debugger(debugger).await;
                        {
                            let mut sess = session.lock().await;
                            sess.kind = None;
                            sess.awaiting_configuration_done_resume = false;
                        }
                        send_response(out_tx, seq, request, false, None, Some(msg), server_shutdown)
                            .await;
                        return;
                    }
                }

                send_response(out_tx, seq, request, true, None, None, server_shutdown).await;
                send_event(
                    out_tx,
                    seq,
                    "process",
                    Some(process_event_body),
                    server_shutdown,
                )
                .await;
                send_event(
                    out_tx,
                    seq,
                    "continued",
                    Some(json!({ "allThreadsContinued": true })),
                    server_shutdown,
                )
                .await;
            } else {
                send_response(out_tx, seq, request, true, None, None, server_shutdown).await;
                send_event(
                    out_tx,
                    seq,
                    "process",
                    Some(process_event_body),
                    server_shutdown,
                )
                .await;
            }

            if let Some(tx) = launch_outcome_tx.take() {
                let _ = tx.send(Some(true));
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
                        server_shutdown,
                    )
                    .await;
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
                        server_shutdown,
                    )
                    .await;
                    return;
                }
            }

            let host = request
                .arguments
                .get("host")
                .and_then(|v| v.as_str())
                .unwrap_or("127.0.0.1");
            let host_label = host.to_string();
            let Some(port) = request.arguments.get("port").and_then(|v| v.as_u64()) else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some(format!("{}.port is required", request.command)),
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
                    return;
                }
            };
            let resolved_hosts = match resolve_host_candidates(host, port).await {
                Ok(hosts) if !hosts.is_empty() => hosts,
                Ok(_) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(format!("failed to resolve host {host_label:?}: no addresses found")),
                        server_shutdown,
                    )
                    .await;
                    return;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(format!("invalid host {host_label:?}: {err}")),
                        server_shutdown,
                    )
                    .await;
                    return;
                }
            };
            let is_local_process = resolved_hosts.iter().any(|addr| addr.is_loopback());

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
                        server_shutdown,
                    )
                    .await;
                    return;
                }
            }

            let source_roots =
                match resolve_source_roots(request.command.as_str(), &request.arguments) {
                    Ok(roots) => roots,
                    Err(err) => {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some(err.to_string()),
                            server_shutdown,
                        )
                        .await;
                        return;
                    }
                };
            let project_root = parse_project_root(&request.arguments);

            let mut last_err: Option<DebuggerError> = None;
            let mut dbg: Option<Debugger> = None;
            for host in resolved_hosts {
                match Debugger::attach(AttachArgs {
                    host,
                    port,
                    source_roots: source_roots.clone(),
                })
                .await
                {
                    Ok(attached) => {
                        dbg = Some(attached);
                        break;
                    }
                    Err(err) => {
                        last_err = Some(err);
                    }
                }
            }

            let Some(dbg) = dbg else {
                let msg = last_err
                    .map(|err| format!("failed to attach to {host_label}:{port}: {err}"))
                    .unwrap_or_else(|| {
                        format!("failed to attach to {host_label}:{port}: no addresses resolved")
                    });
                send_response(out_tx, seq, request, false, None, Some(msg), server_shutdown).await;
                return;
            };

            {
                let mut guard = debugger.lock().await;
                *guard = Some(dbg);
            }

            let debugger_id = next_debugger_id.fetch_add(1, Ordering::Relaxed);
            spawn_event_task(
                debugger.clone(),
                out_tx.clone(),
                seq.clone(),
                terminated_sent.clone(),
                server_shutdown.clone(),
                debugger_id,
                suppress_termination_debugger_id.clone(),
            );

            {
                let mut sess = session.lock().await;
                sess.lifecycle = LifecycleState::LaunchedOrAttached;
                sess.kind = Some(SessionKind::Attach);
                sess.debugger_id = Some(debugger_id);
                sess.awaiting_configuration_done_resume = false;
                sess.project_root = project_root;
            }

            apply_pending_configuration(cancel, debugger, pending_config).await;

            send_response(out_tx, seq, request, true, None, None, server_shutdown).await;
            // For attach sessions we don't generally know the target process name or PID.
            // Use the attach target itself as a stable label.
            let process_name = format!("{host_label}:{port}");
            let process_event_body =
                make_process_event_body(&process_name, None, is_local_process, "attach");
            send_event(
                out_tx,
                seq,
                "process",
                Some(process_event_body),
                server_shutdown,
            )
            .await;
        }
        "restart" => {
            let (launch_cfg, previous_debugger_id) = {
                let sess = session.lock().await;
                if sess.kind != Some(SessionKind::Launch) {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("restart is only supported for launch sessions".to_string()),
                        server_shutdown,
                    )
                    .await;
                    return;
                }

                let Some(launch_cfg) = sess.last_launch.clone() else {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("restart requires a previous successful launch".to_string()),
                        server_shutdown,
                    )
                    .await;
                    return;
                };

                (launch_cfg, sess.debugger_id)
            };

            // Prevent the old JDWP event-forwarder task from treating this intentional disconnect
            // as a full debug session termination.
            if let Some(debugger_id) = previous_debugger_id {
                suppress_termination_debugger_id.store(debugger_id, Ordering::Relaxed);
            }

            // Suppress `exited`/`terminated` events from the launched-process monitor for this
            // adapter-initiated restart.
            {
                let guard = launched_process.lock().await;
                if let Some(proc) = guard.as_ref() {
                    let _ = proc.outcome.send(Some(false));
                }
            }

            // `restart` semantics: terminate the current debuggee and disconnect the debugger.
            terminate_existing_process(launched_process).await;
            disconnect_debugger(debugger).await;
            {
                let mut sess = session.lock().await;
                sess.debugger_id = None;
            }

            // Re-launch using stored launch configuration.
            let mut args = launch_cfg.args.clone();
            let mode = launch_cfg.mode;
            let source_roots = launch_cfg.source_roots.clone();
            let project_root = launch_cfg.project_root.clone();

            // `restart` must not run concurrently with an existing debugger connection.
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
                        server_shutdown,
                    )
                    .await;
                    return;
                }
            }

            let attach_timeout_ms = args.attach_timeout_ms.unwrap_or(30_000);
            args.attach_timeout_ms = Some(attach_timeout_ms);
            let attach_timeout = Duration::from_millis(attach_timeout_ms);

            // Apply defaults again defensively (the stored launch config should already have these).
            match mode {
                LaunchMode::Command => {
                    if args.host.is_none() {
                        args.host = Some("127.0.0.1".to_string());
                    }
                    if args.port.is_none() {
                        args.port = Some(5005);
                    }
                }
                LaunchMode::Java => {
                    if args.java.is_none() {
                        args.java = Some("java".to_string());
                    }
                }
            }

            let mut launch_outcome_tx: Option<watch::Sender<Option<bool>>>;
            let (attach_hosts, attach_port, attach_target_label, process_name, process_pid) =
                match mode {
                    LaunchMode::Command => {
                        let Some(cwd) = args.cwd.as_deref() else {
                            send_response(
                                out_tx,
                                seq,
                                request,
                                false,
                                None,
                                Some("launch.cwd is required".to_string()),
                                server_shutdown,
                            )
                            .await;
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
                                server_shutdown,
                            )
                            .await;
                            return;
                        };

                        let port = args.port.unwrap_or(5005);
                        let host = args.host.as_deref().unwrap_or("127.0.0.1");
                        let host_label = host.to_string();
                        let resolved_hosts = match resolve_host_candidates(host, port).await {
                            Ok(hosts) if !hosts.is_empty() => hosts,
                            Ok(_) => {
                                send_response(
                                    out_tx,
                                    seq,
                                    request,
                                    false,
                                    None,
                                    Some(format!(
                                        "failed to resolve host {host_label:?}: no addresses found"
                                    )),
                                    server_shutdown,
                                )
                                .await;
                                return;
                            }
                            Err(err) => {
                                send_response(
                                    out_tx,
                                    seq,
                                    request,
                                    false,
                                    None,
                                    Some(format!("invalid host {host_label:?}: {err}")),
                                    server_shutdown,
                                )
                                .await;
                                return;
                            }
                        };
                        let attach_target_label = format!("{host_label}:{port}");

                        let mut cmd = Command::new(command);
                        cmd.args(&args.args);
                        cmd.current_dir(cwd);
                        cmd.stdin(Stdio::null());
                        cmd.stdout(Stdio::piped());
                        cmd.stderr(Stdio::piped());
                        // Ensure `disconnect` with `terminateDebuggee=false` can safely detach without
                        // killing the launched process.
                        cmd.kill_on_drop(false);
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
                                    server_shutdown,
                                )
                                .await;
                                return;
                            }
                        };
                        let Some(pid) = child.id() else {
                            send_response(
                                out_tx,
                                seq,
                                request,
                                false,
                                None,
                                Some("failed to determine launched process pid".to_string()),
                                server_shutdown,
                            )
                            .await;
                            return;
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
                            let (proc, outcome_tx) = spawn_launched_process_exit_task(
                                child,
                                out_tx.clone(),
                                seq.clone(),
                                Arc::clone(exited_sent),
                                Arc::clone(terminated_sent),
                                server_shutdown.clone(),
                            );
                            launch_outcome_tx = Some(outcome_tx);
                            *guard = Some(proc);
                        }

                        (resolved_hosts, port, attach_target_label, command.to_string(), pid)
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
                                server_shutdown,
                            )
                            .await;
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
                                        server_shutdown,
                                    )
                                    .await;
                                    return;
                                }
                            },
                        };
                        // Persist the resolved port for restart so we can re-use it.
                        args.port = Some(port);
                        let host: IpAddr = "127.0.0.1".parse().unwrap();
                        let attach_target_label = format!("{host}:{port}");

                        let java = args.java.clone().unwrap_or_else(|| "java".to_string());

                        let cp_joined = match join_classpath(&classpath) {
                            Ok(cp) => cp,
                            Err(err) => {
                                send_response(out_tx, seq, request, false, None, Some(err), server_shutdown)
                                    .await;
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
                        // Ensure `disconnect` with `terminateDebuggee=false` can safely detach without
                        // killing the launched JVM.
                        cmd.kill_on_drop(false);
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
                                    server_shutdown,
                                )
                                .await;
                                return;
                            }
                        };
                        let Some(pid) = child.id() else {
                            send_response(
                                out_tx,
                                seq,
                                request,
                                false,
                                None,
                                Some("failed to determine launched process pid".to_string()),
                                server_shutdown,
                            )
                            .await;
                            return;
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
                            let (proc, outcome_tx) = spawn_launched_process_exit_task(
                                child,
                                out_tx.clone(),
                                seq.clone(),
                                Arc::clone(exited_sent),
                                Arc::clone(terminated_sent),
                                server_shutdown.clone(),
                            );
                            launch_outcome_tx = Some(outcome_tx);
                            *guard = Some(proc);
                        }

                        (
                            vec![host],
                            port,
                            attach_target_label,
                            main_class.to_string(),
                            pid,
                        )
                    }
                };

            let process_event_body =
                make_process_event_body(&process_name, Some(process_pid), true, "launch");

            // Update stored config to reflect any defaults we re-applied during restart.
            {
                let mut sess = session.lock().await;
                sess.last_launch = Some(StoredLaunchConfig {
                    mode,
                    args: args.clone(),
                    source_roots: source_roots.clone(),
                    project_root: project_root.clone(),
                });
            }

            let attach_fut = attach_debugger_with_retry_hosts(
                attach_hosts,
                attach_port,
                source_roots,
                attach_timeout,
            );
            let dbg = tokio::select! {
                _ = cancel.cancelled() => {
                    if let Some(tx) = launch_outcome_tx.take() {
                        let _ = tx.send(Some(false));
                    }
                    terminate_existing_process(launched_process).await;
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                    return;
                }
                res = attach_fut => match res {
                    Ok(dbg) => dbg,
                    Err(err) => {
                        if let Some(tx) = launch_outcome_tx.take() {
                            let _ = tx.send(Some(false));
                        }
                        terminate_existing_process(launched_process).await;
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some(format!("failed to attach to {attach_target_label}: {err}")),
                            server_shutdown,
                        )
                        .await;
                        return;
                    }
                }
            };

            {
                let mut guard = debugger.lock().await;
                *guard = Some(dbg);
            }

            let debugger_id = next_debugger_id.fetch_add(1, Ordering::Relaxed);
            spawn_event_task(
                debugger.clone(),
                out_tx.clone(),
                seq.clone(),
                terminated_sent.clone(),
                server_shutdown.clone(),
                debugger_id,
                suppress_termination_debugger_id.clone(),
            );

            let resume_after_restart = {
                let mut sess = session.lock().await;
                sess.lifecycle = LifecycleState::LaunchedOrAttached;
                sess.kind = Some(SessionKind::Launch);
                sess.debugger_id = Some(debugger_id);
                let needs_config_done_resume = args.stop_on_entry;
                let resume_after_launch =
                    needs_config_done_resume && sess.configuration_done_received;
                sess.awaiting_configuration_done_resume =
                    needs_config_done_resume && !sess.configuration_done_received;
                sess.project_root = project_root;
                resume_after_launch
            };

            apply_pending_configuration(cancel, debugger, pending_config).await;

            if resume_after_restart {
                let mut guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                    Some(guard) => guard,
                    None => {
                        if let Some(tx) = launch_outcome_tx.take() {
                            let _ = tx.send(Some(false));
                        }
                        terminate_existing_process(launched_process).await;
                        disconnect_debugger(debugger).await;
                        {
                            let mut sess = session.lock().await;
                            sess.kind = None;
                            sess.debugger_id = None;
                            sess.awaiting_configuration_done_resume = false;
                        }
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("cancelled".to_string()),
                            server_shutdown,
                        )
                        .await;
                        return;
                    }
                };
                let Some(dbg) = guard.as_mut() else {
                    if let Some(tx) = launch_outcome_tx.take() {
                        let _ = tx.send(Some(false));
                    }
                    terminate_existing_process(launched_process).await;
                    disconnect_debugger(debugger).await;
                    {
                        let mut sess = session.lock().await;
                        sess.kind = None;
                        sess.debugger_id = None;
                        sess.awaiting_configuration_done_resume = false;
                    }
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("not attached".to_string()),
                        server_shutdown,
                    )
                    .await;
                    return;
                };

                match dbg.continue_(cancel, None).await {
                    Ok(()) => {
                        let mut sess = session.lock().await;
                        sess.lifecycle = LifecycleState::Running;
                    }
                    Err(err) if is_cancelled_error(&err) => {
                        if let Some(tx) = launch_outcome_tx.take() {
                            let _ = tx.send(Some(false));
                        }
                        drop(guard);
                        terminate_existing_process(launched_process).await;
                        disconnect_debugger(debugger).await;
                        {
                            let mut sess = session.lock().await;
                            sess.kind = None;
                            sess.debugger_id = None;
                            sess.awaiting_configuration_done_resume = false;
                        }
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("cancelled".to_string()),
                            server_shutdown,
                        )
                        .await;
                        return;
                    }
                    Err(err) => {
                        if let Some(tx) = launch_outcome_tx.take() {
                            let _ = tx.send(Some(false));
                        }
                        let msg = err.to_string();
                        drop(guard);
                        terminate_existing_process(launched_process).await;
                        disconnect_debugger(debugger).await;
                        {
                            let mut sess = session.lock().await;
                            sess.kind = None;
                            sess.debugger_id = None;
                            sess.awaiting_configuration_done_resume = false;
                        }
                        send_response(out_tx, seq, request, false, None, Some(msg), server_shutdown)
                            .await;
                        return;
                    }
                }

                send_response(out_tx, seq, request, true, None, None, server_shutdown).await;
                send_event(
                    out_tx,
                    seq,
                    "process",
                    Some(process_event_body),
                    server_shutdown,
                )
                .await;
                send_event(
                    out_tx,
                    seq,
                    "continued",
                    Some(json!({ "allThreadsContinued": true })),
                    server_shutdown,
                )
                .await;
            } else {
                send_response(out_tx, seq, request, true, None, None, server_shutdown).await;
                send_event(
                    out_tx,
                    seq,
                    "process",
                    Some(process_event_body),
                    server_shutdown,
                )
                .await;
                if !args.stop_on_entry {
                    // The debuggee is expected to be running immediately after attach.
                    send_event(
                        out_tx,
                        seq,
                        "continued",
                        Some(json!({ "allThreadsContinued": true })),
                        server_shutdown,
                    )
                    .await;
                }
            }

            if let Some(tx) = launch_outcome_tx.take() {
                let _ = tx.send(Some(true));
            }
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
                }
                Ok(bps) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!({ "breakpoints": bps })),
                        None,
                        server_shutdown,
                    )
                    .await
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
                }
                Ok(bps) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!({ "breakpoints": bps })),
                        None,
                        server_shutdown,
                    )
                    .await
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
            }
        }
        "breakpointLocations" => {
            if cancel.is_cancelled() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                    server_shutdown,
                )
                .await;
                return;
            }

            let source_path = request
                .arguments
                .get("source")
                .and_then(|s| s.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            let Some(line) = request.arguments.get("line").and_then(|v| v.as_i64()) else {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("breakpointLocations.line is required".to_string()),
                    server_shutdown,
                )
                .await;
                return;
            };

            let end_line = request.arguments.get("endLine").and_then(|v| v.as_i64());
            let breakpoints = Debugger::breakpoint_locations(source_path, line, end_line);
            send_response(
                out_tx,
                seq,
                request,
                true,
                Some(json!({ "breakpoints": breakpoints })),
                None,
                server_shutdown,
            )
            .await;
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
                        server_shutdown,
                    )
                    .await;
                    return;
                }
            };
            let Some(dbg) = guard.as_mut() else {
                // Cache the configuration and apply it once the debugger is attached.
                send_response(out_tx, seq, request, true, None, None, server_shutdown).await;
                return;
            };

            match dbg.set_exception_breakpoints(caught, uncaught).await {
                Ok(()) => send_response(out_tx, seq, request, true, None, None, server_shutdown).await,
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
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
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                    server_shutdown,
                )
                .await;
                return;
            };

            let start_frame = request.arguments.get("startFrame").and_then(|v| v.as_i64());
            if start_frame.is_some_and(|start| start < 0) {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("stackTrace.startFrame must be >= 0".to_string()),
                    server_shutdown,
                )
                .await;
                return;
            }

            let levels = request.arguments.get("levels").and_then(|v| v.as_i64());
            if levels.is_some_and(|levels| levels < 0) {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("stackTrace.levels must be >= 0".to_string()),
                    server_shutdown,
                )
                .await;
                return;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
                return;
            };

            match dbg
                .stack_trace(cancel, thread_id, start_frame, levels)
                .await
            {
                Ok((frames, total_frames)) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Ok((frames, total_frames)) => {
                    let mut body = serde_json::Map::new();
                    body.insert("stackFrames".to_string(), json!(frames));
                    if let Some(total_frames) = total_frames {
                        body.insert("totalFrames".to_string(), json!(total_frames));
                    }
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(Value::Object(body)),
                        None,
                        server_shutdown,
                    )
                    .await
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
                return;
            };

            match dbg.scopes(frame_id) {
                Ok(scopes) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!({ "scopes": scopes })),
                        None,
                        server_shutdown,
                    )
                    .await
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
                return;
            };

            match dbg.step_in_targets(cancel, frame_id).await {
                Ok(targets) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await
                }
                Ok(targets) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!({ "targets": targets })),
                        None,
                        server_shutdown,
                    )
                    .await
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
                }
                Ok(vars) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!({ "variables": vars })),
                        None,
                        server_shutdown,
                    )
                    .await
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
                    return;
                }
            };

            match dbg
                .set_variable(cancel, args.variables_reference, &args.name, &args.value)
                .await
            {
                Ok(body) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await
                }
                Ok(body) => send_response(out_tx, seq, request, true, body, None, server_shutdown).await,
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
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
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(Value::Object(body)),
                        None,
                        server_shutdown,
                    )
                    .await;
                }
                Ok(None) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(format!("no exception context for threadId {thread_id}")),
                        server_shutdown,
                    )
                    .await
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;

                    let mut body = serde_json::Map::new();
                    body.insert(
                        "allThreadsContinued".to_string(),
                        json!(all_threads_continued),
                    );
                    if let Some(thread_id) = thread_id {
                        body.insert("threadId".to_string(), json!(thread_id));
                    }
                    send_event(out_tx, seq, "continued", Some(Value::Object(body)), server_shutdown)
                        .await;
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
                return;
            };

            let all_threads_stopped = thread_id.is_none();
            match dbg.pause(cancel, thread_id).await {
                Ok(()) => {
                    send_response(out_tx, seq, request, true, None, None, server_shutdown).await;
                    let mut body = serde_json::Map::new();
                    body.insert("reason".to_string(), json!("pause"));
                    body.insert("allThreadsStopped".to_string(), json!(all_threads_stopped));
                    if let Some(thread_id) = thread_id {
                        body.insert("threadId".to_string(), json!(thread_id));
                    }
                    send_event(out_tx, seq, "stopped", Some(Value::Object(body)), server_shutdown)
                        .await;
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
                return;
            };

            let step_result = match target_id {
                Some(target_id) => dbg.step_in_target(cancel, thread_id, target_id).await,
                None => dbg.step(cancel, thread_id, depth).await,
            };

            match step_result {
                Ok(()) => send_response(out_tx, seq, request, true, None, None, server_shutdown).await,
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                    send_response(out_tx, seq, request, true, Some(json!(body)), None, server_shutdown)
                        .await;
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
                send_response(out_tx, seq, request, true, Some(json!(body)), None, server_shutdown)
                    .await;
                return;
            };

            let options = EvalOptions::from_dap_context(args.context.as_deref());

            match dbg
                .evaluate(cancel, frame_id, &args.expression, options)
                .await
            {
                Ok(body) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await
                }
                Ok(body) => send_response(out_tx, seq, request, true, body, None, server_shutdown).await,
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
            }
        }
        STREAM_DEBUG_COMMAND => {
            if cancel.is_cancelled() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                    server_shutdown,
                )
                .await;
                return;
            }

            let args: StreamDebugArguments = match serde_json::from_value(request.arguments.clone())
            {
                Ok(args) => args,
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(format!("invalid streamDebug arguments: {err}")),
                        server_shutdown,
                    )
                    .await;
                    return;
                }
            };

            // IMPORTANT: Do not hold the debugger mutex while stream-debugging. Stream debug may
            // execute JDWP operations that interleave with asynchronous events; the event
            // forwarding task must be able to lock the debugger concurrently.
            //
            // While stream debug is in-flight, mark the evaluation thread as being in internal
            // evaluation mode so the JDWP event task can auto-resume any breakpoint hits without
            // emitting DAP stop/output events or mutating hit-count breakpoint state.
            let (fut, _eval_guard) = {
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
                            server_shutdown,
                        )
                        .await;
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
                        server_shutdown,
                    )
                    .await;
                    return;
                };

                let frame_id = args.frame_id.filter(|frame_id| *frame_id > 0);
                let Some(frame_id) = frame_id else {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(
                            "This request requires a stack frame. Retry while stopped or pass frameId."
                                .to_string(),
                        ),
                        server_shutdown,
                    )
                    .await;
                    return;
                };

                let Some((thread_id, _jdwp_frame_id)) = dbg.jdwp_frame(frame_id) else {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(format!("unknown frameId {frame_id}")),
                        server_shutdown,
                    )
                    .await;
                    return;
                };
                let eval_guard = dbg.begin_internal_evaluation(thread_id);

                let config = args.into_config();

                match dbg.stream_debug(cancel.clone(), frame_id, args.expression.clone(), config) {
                    Ok(fut) => (fut, eval_guard),
                    Err(err) if is_cancelled_error(&err) => {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some("cancelled".to_string()),
                            server_shutdown,
                        )
                        .await;
                        return;
                    }
                    Err(err) => {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some(err.to_string()),
                            server_shutdown,
                        )
                        .await;
                        return;
                    }
                }
            };

            match fut.await {
                Ok(_body) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await
                }
                Ok(body) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!(body)),
                        None,
                        server_shutdown,
                    )
                    .await
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await
                }
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
                }
                Ok(pinned) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!({ "pinned": pinned })),
                        None,
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await;
                }
            }
        }
        // Data breakpoints / watchpoints (requires JDWP canWatchField* capabilities).
        "dataBreakpointInfo" => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct DataBreakpointInfoArguments {
                variables_reference: i64,
                name: String,
                #[serde(default)]
                frame_id: Option<i64>,
            }

            if cancel.is_cancelled() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                    server_shutdown,
                )
                .await;
                return;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                    server_shutdown,
                )
                .await;
                return;
            }

            let args: DataBreakpointInfoArguments = match serde_json::from_value(request.arguments.clone())
            {
                Ok(args) => args,
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(format!("invalid dataBreakpointInfo arguments: {err}")),
                        server_shutdown,
                    )
                    .await;
                    return;
                }
            };

            // `frameId` is optional in the DAP spec; the debugger can resolve the field based on
            // the variables reference alone.
            let _ = args.frame_id;

            match dbg
                .data_breakpoint_info(cancel, args.variables_reference, &args.name)
                .await
            {
                Ok(_body) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Ok(body) => {
                    send_response(out_tx, seq, request, true, Some(body), None, server_shutdown).await;
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await;
                }
            }
        }
        "setDataBreakpoints" => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct SetDataBreakpointsArguments {
                #[serde(default)]
                breakpoints: Vec<crate::wire_debugger::DataBreakpointSpec>,
            }

            if cancel.is_cancelled() {
                send_response(
                    out_tx,
                    seq,
                    request,
                    false,
                    None,
                    Some("cancelled".to_string()),
                    server_shutdown,
                )
                .await;
                return;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                    server_shutdown,
                )
                .await;
                return;
            }

            let args: SetDataBreakpointsArguments =
                match serde_json::from_value(request.arguments.clone()) {
                    Ok(args) => args,
                    Err(err) => {
                        send_response(
                            out_tx,
                            seq,
                            request,
                            false,
                            None,
                            Some(format!("invalid setDataBreakpoints arguments: {err}")),
                            server_shutdown,
                        )
                        .await;
                        return;
                    }
                };

            match dbg.set_data_breakpoints(cancel, args.breakpoints).await {
                Ok(_bps) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Ok(bps) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!({ "breakpoints": bps })),
                        None,
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) if is_cancelled_error(&err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
                }
                Err(err) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some(err.to_string()),
                        server_shutdown,
                    )
                    .await;
                }
            }
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
                    server_shutdown,
                )
                .await;
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
                            server_shutdown,
                        )
                        .await;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
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
                            let compiled = CompiledClass {
                                class_name,
                                bytecode,
                            };
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

                            outputs
                                .entry(file.clone())
                                .or_insert_with(|| CompileOutputMulti {
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
                            server_shutdown,
                        )
                        .await;
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
                            server_shutdown,
                        )
                        .await;
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
                    server_shutdown,
                )
                .await;
                return;
            }

            let build = PrecompiledBuildMulti { outputs };
            let mut engine = HotSwapEngine::new(build, jdwp);
            let mut result = tokio::select! {
                _ = cancel.cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                        server_shutdown,
                    )
                    .await;
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
                server_shutdown,
            )
            .await;
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
                    server_shutdown,
                )
                .await;
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
                        server_shutdown,
                    )
                    .await;
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
                    server_shutdown,
                )
                .await;
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
                    server_shutdown,
                )
                .await;
                return;
            }

            send_response(
                out_tx,
                seq,
                request,
                true,
                Some(json!({ "enabled": true })),
                None,
                server_shutdown,
            )
            .await;
        }
        "terminate" => {
            let has_launched_process = launched_process.lock().await.is_some();

            if has_launched_process {
                terminate_existing_process(launched_process).await;
                disconnect_debugger(debugger).await;
            } else {
                // Attach session: request that the target VM exits via JDWP.
                let mut dbg = {
                    let mut guard = debugger.lock().await;
                    guard.take()
                };
                if let Some(dbg) = dbg.as_mut() {
                    let _ = dbg.terminate_vm(cancel, 0).await;
                }
            }

            send_response(out_tx, seq, request, true, None, None, server_shutdown).await;
            send_terminated_once(out_tx, seq, terminated_sent, server_shutdown).await;
            server_shutdown.cancel();
        }
        "disconnect" => {
            let has_launched_process = launched_process.lock().await.is_some();
            let terminate_debuggee = match request
                .arguments
                .get("terminateDebuggee")
                .and_then(|v| v.as_bool())
            {
                Some(value) => value,
                None => has_launched_process,
            };

            if terminate_debuggee {
                if has_launched_process {
                    terminate_existing_process(launched_process).await;
                    disconnect_debugger(debugger).await;
                } else {
                    // Attach session: request that the target VM exits via JDWP.
                    let mut dbg = {
                        let mut guard = debugger.lock().await;
                        guard.take()
                    };
                    if let Some(dbg) = dbg.as_mut() {
                        let _ = dbg.terminate_vm(cancel, 0).await;
                    }
                }
            } else {
                detach_existing_process(launched_process).await;
                // Detach from the target VM (best-effort).
                let mut dbg = {
                    let mut guard = debugger.lock().await;
                    guard.take()
                };
                if let Some(dbg) = dbg.as_mut() {
                    let _ = dbg.detach(cancel).await;
                }
            }

            send_response(out_tx, seq, request, true, None, None, server_shutdown).await;
            send_terminated_once(out_tx, seq, terminated_sent, server_shutdown).await;
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
                server_shutdown,
            )
            .await;
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

#[derive(Debug, Clone)]
struct StoredLaunchConfig {
    mode: LaunchMode,
    args: LaunchArguments,
    source_roots: Vec<PathBuf>,
    project_root: Option<PathBuf>,
}

fn join_classpath(classpath: &Classpath) -> std::result::Result<std::ffi::OsString, String> {
    let parts: Vec<std::ffi::OsString> = classpath
        .entries()
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect();
    std::env::join_paths(parts.iter()).map_err(|err| format!("launch.classpath is invalid: {err}"))
}

async fn resolve_host_candidates(host: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
    if let Ok(host) = host.parse::<IpAddr>() {
        return Ok(vec![host]);
    }

    let addrs = lookup_host((host, port)).await?;
    let mut unique = BTreeSet::new();
    for addr in addrs {
        unique.insert(addr.ip());
    }

    // `IpAddr` sorts IPv4 before IPv6, which ensures `localhost` prefers `127.0.0.1` over `::1`.
    Ok(unique.into_iter().collect())
}

async fn attach_debugger_with_retry_hosts(
    hosts: Vec<IpAddr>,
    port: u16,
    source_roots: Vec<PathBuf>,
    timeout: Duration,
) -> std::result::Result<Debugger, DebuggerError> {
    if hosts.is_empty() {
        return Err(DebuggerError::InvalidRequest(
            "no host addresses resolved".to_string(),
        ));
    }

    if hosts.len() == 1 {
        return Debugger::attach_with_retry(
            AttachArgs {
                host: hosts[0],
                port,
                source_roots,
            },
            timeout,
        )
        .await;
    }

    // For hostnames that resolve to multiple addresses (e.g. IPv4 + IPv6), try each candidate
    // per retry tick to avoid getting stuck on an address family the debuggee isn't listening on.
    //
    // `hosts` is ordered to prefer IPv4 before IPv6 (see `resolve_host_candidates`).
    let start = Instant::now();
    let mut backoff = Duration::from_millis(50);
    let max_backoff = Duration::from_secs(1);

    loop {
        let mut last_err: Option<DebuggerError> = None;
        let mut any_retryable = false;
        for &host in &hosts {
            match Debugger::attach(AttachArgs {
                host,
                port,
                source_roots: source_roots.clone(),
            })
            .await
            {
                Ok(dbg) => return Ok(dbg),
                Err(err) => {
                    any_retryable |= is_retryable_attach_error(&err);
                    last_err = Some(err);
                }
            }
        }

        let err = last_err.unwrap_or_else(|| {
            DebuggerError::InvalidRequest("no host addresses resolved".to_string())
        });

        // If `timeout` is disabled, behave like an `attach` attempt and return the last error.
        if timeout == Duration::ZERO {
            return Err(err);
        }

        let elapsed = start.elapsed();
        if elapsed >= timeout || !any_retryable {
            return Err(err);
        }

        let remaining = timeout.saturating_sub(elapsed);
        tokio::time::sleep(backoff.min(remaining)).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

async fn pick_free_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    Ok(port)
}

const MAX_DEBUGGEE_OUTPUT_LINE_BYTES: usize = 64 * 1024;
const DEBUGGEE_OUTPUT_TRUNCATION_MARKER: &str = "<output truncated>";
const DEBUGGEE_OUTPUT_TRUNCATION_SUFFIX: &str = "<output truncated>\n";

#[cfg(unix)]
fn ignore_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn spawn_output_task<R>(
    reader: R,
    tx: mpsc::Sender<Value>,
    seq: Arc<AtomicI64>,
    category: &'static str,
    server_shutdown: CancellationToken,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        // Output from debuggees can be adversarially large (e.g., a single "line" with no newline
        // or an extremely long line). Reading with `read_until('\n')` would grow an unbounded
        // buffer and can OOM the adapter or retain a huge capacity forever. Bound buffering and
        // truncate long lines instead.
        let mut reader = BufReader::new(reader);

        // Buffer for the current line (capped to MAX_DEBUGGEE_OUTPUT_LINE_BYTES).
        let mut buf = Vec::new();
        // Whether we've hit the cap for the current line and are currently discarding bytes until
        // we see the next newline.
        let mut discarding_until_newline = false;

        loop {
            let available = tokio::select! {
                _ = server_shutdown.cancelled() => return,
                res = reader.fill_buf() => match res {
                    Ok(buf) => buf,
                    Err(_) => return,
                }
            };

            if available.is_empty() {
                // EOF. Preserve the old behavior of emitting a final (possibly unterminated) line.
                if !buf.is_empty() || discarding_until_newline {
                    let mut output = String::from_utf8_lossy(&buf).into_owned();
                    if discarding_until_newline {
                        output.push_str(DEBUGGEE_OUTPUT_TRUNCATION_MARKER);
                    }
                    send_event(
                        &tx,
                        &seq,
                        "output",
                        Some(json!({ "category": category, "output": output })),
                        &server_shutdown,
                    )
                    .await;
                }
                return;
            }

            let mut consumed = 0;
            while consumed < available.len() {
                if discarding_until_newline {
                    // Discard until we find a newline, then emit the truncated line once.
                    if let Some(pos) = available[consumed..].iter().position(|&b| b == b'\n') {
                        consumed += pos + 1;

                        let mut output = String::from_utf8_lossy(&buf).into_owned();
                        output.push_str(DEBUGGEE_OUTPUT_TRUNCATION_SUFFIX);
                        send_event(
                            &tx,
                            &seq,
                            "output",
                            Some(json!({ "category": category, "output": output })),
                            &server_shutdown,
                        )
                        .await;
                        buf.clear();
                        discarding_until_newline = false;
                    } else {
                        // No newline in this chunk; discard it all.
                        consumed = available.len();
                    }
                    continue;
                }

                // Normal line capture mode.
                let newline_pos = available[consumed..].iter().position(|&b| b == b'\n');
                let take = newline_pos
                    .map(|pos| pos + 1)
                    .unwrap_or(available.len() - consumed);

                let remaining = MAX_DEBUGGEE_OUTPUT_LINE_BYTES.saturating_sub(buf.len());
                if take <= remaining {
                    buf.extend_from_slice(&available[consumed..consumed + take]);
                    consumed += take;

                    if newline_pos.is_some() {
                        let output = String::from_utf8_lossy(&buf).into_owned();
                        send_event(
                            &tx,
                            &seq,
                            "output",
                            Some(json!({ "category": category, "output": output })),
                            &server_shutdown,
                        )
                        .await;
                        buf.clear();
                    }
                    continue;
                }

                // This line is longer than MAX_DEBUGGEE_OUTPUT_LINE_BYTES.
                if remaining > 0 {
                    buf.extend_from_slice(&available[consumed..consumed + remaining]);
                }
                consumed += take;

                if newline_pos.is_some() {
                    // Newline is within `take`, so we can emit the truncated line now.
                    let mut output = String::from_utf8_lossy(&buf).into_owned();
                    output.push_str(DEBUGGEE_OUTPUT_TRUNCATION_SUFFIX);
                    send_event(
                        &tx,
                        &seq,
                        "output",
                        Some(json!({ "category": category, "output": output })),
                        &server_shutdown,
                    )
                    .await;
                    buf.clear();
                } else {
                    // No newline yet; keep discarding until we encounter one.
                    discarding_until_newline = true;
                }
            }

            reader.consume(consumed);
        }
    });
}

fn spawn_launched_process_exit_task(
    mut child: Child,
    tx: mpsc::Sender<Value>,
    seq: Arc<AtomicI64>,
    exited_sent: Arc<AtomicBool>,
    terminated_sent: Arc<AtomicBool>,
    server_shutdown: CancellationToken,
) -> (LaunchedProcess, watch::Sender<Option<bool>>) {
    let detach = CancellationToken::new();
    let detach_task = detach.clone();

    let (kill_tx, mut kill_rx) = watch::channel(false);

    let (outcome_tx, mut outcome_rx) = watch::channel(None::<bool>);

    let monitor = tokio::spawn(async move {
        let mut kill_listener = true;
        let mut kill_requested = false;

        let status = loop {
            if !kill_requested && *kill_rx.borrow() {
                kill_requested = true;
                let _ = child.start_kill();
            }

            tokio::select! {
                _ = detach_task.cancelled() => return,
                changed = kill_rx.changed(), if kill_listener && !kill_requested => {
                    if changed.is_err() {
                        kill_listener = false;
                    }
                    continue;
                }
                status = child.wait() => break status,
            };
        };

        let exit_code = match status {
            Ok(status) => status.code().map(|code| code as i64).unwrap_or(-1),
            Err(_) => return,
        };

        loop {
            match *outcome_rx.borrow() {
                Some(true) => break,
                Some(false) => return,
                None => {}
            }

            tokio::select! {
                _ = detach_task.cancelled() => return,
                changed = outcome_rx.changed() => {
                    if changed.is_err() {
                        // Sender dropped without ever marking the launch as successful.
                        return;
                    }
                }
            }
        }

        send_exited_once(&tx, &seq, &exited_sent, exit_code, &server_shutdown).await;
        send_terminated_once(&tx, &seq, &terminated_sent, &server_shutdown).await;
        server_shutdown.cancel();
    });

    (
        LaunchedProcess {
            detach,
            kill: kill_tx,
            outcome: outcome_tx.clone(),
            monitor,
        },
        outcome_tx,
    )
}

async fn detach_existing_process(launched_process: &Arc<Mutex<Option<LaunchedProcess>>>) {
    let proc = {
        let mut guard = launched_process.lock().await;
        guard.take()
    };

    let Some(proc) = proc else { return };

    proc.detach.cancel();
    let mut monitor = proc.monitor;
    if tokio::time::timeout(Duration::from_millis(250), &mut monitor)
        .await
        .is_err()
    {
        monitor.abort();
        let _ = tokio::time::timeout(Duration::from_millis(250), monitor).await;
    }
}

async fn terminate_existing_process(launched_process: &Arc<Mutex<Option<LaunchedProcess>>>) {
    let proc = {
        let mut guard = launched_process.lock().await;
        guard.take()
    };

    let Some(proc) = proc else { return };

    let _ = proc.kill.send(true);

    // Reap the process via the monitor task, but don't hang shutdown if it refuses to die.
    let mut monitor = proc.monitor;
    if tokio::time::timeout(Duration::from_secs(2), &mut monitor)
        .await
        .is_err()
    {
        monitor.abort();
        let _ = tokio::time::timeout(Duration::from_millis(250), monitor).await;
    }
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

async fn send_event(
    tx: &mpsc::Sender<Value>,
    seq: &Arc<AtomicI64>,
    event: impl Into<String>,
    body: Option<Value>,
    server_shutdown: &CancellationToken,
) {
    let s = seq.fetch_add(1, Ordering::Relaxed);
    let evt = make_event(s, event, body);
    let msg = serde_json::to_value(evt).unwrap_or_else(|_| json!({}));
    tokio::select! {
        biased;
        res = tx.send(msg) => {
            let _ = res;
        }
        _ = server_shutdown.cancelled() => {}
    }
}

fn make_process_event_body(
    name: &str,
    system_process_id: Option<u32>,
    is_local_process: bool,
    start_method: &str,
) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("name".to_string(), json!(name));
    if let Some(pid) = system_process_id {
        body.insert("systemProcessId".to_string(), json!(pid));
    }
    body.insert("isLocalProcess".to_string(), json!(is_local_process));
    body.insert("startMethod".to_string(), json!(start_method));
    Value::Object(body)
}

async fn send_response(
    tx: &mpsc::Sender<Value>,
    seq: &Arc<AtomicI64>,
    request: &Request,
    success: bool,
    body: Option<Value>,
    message: Option<String>,
    server_shutdown: &CancellationToken,
) {
    if !success {
        nova_metrics::MetricsRegistry::global().record_error(&request.command);
    }
    let s = seq.fetch_add(1, Ordering::Relaxed);
    let resp = make_response(s, request, success, body, message);
    let msg = serde_json::to_value(resp).unwrap_or_else(|_| json!({}));
    tokio::select! {
        biased;
        res = tx.send(msg) => {
            let _ = res;
        }
        _ = server_shutdown.cancelled() => {}
    }
}

#[cfg(all(not(test), not(debug_assertions)))]
fn build_panic_bug_report_bundle() -> Option<String> {
    let cfg = NovaConfig::default();
    let log_buffer = nova_config::init_tracing_with_config(&cfg);
    let crash_store = global_crash_store();
    let perf = PerfStats::default();
    let options = BugReportOptions {
        max_log_lines: 500,
        reproduction: Some("panic in wire DAP request handler".to_string()),
    };

    let bundle = BugReportBuilder::new(&cfg, log_buffer.as_ref(), crash_store.as_ref(), &perf)
        .options(options)
        .extra_attachments(|dir| {
            if let Ok(metrics_json) =
                serde_json::to_string_pretty(&nova_metrics::MetricsRegistry::global().snapshot())
            {
                let _ = std::fs::write(dir.join("metrics.json"), metrics_json);
            }
            Ok(())
        })
        .build()
        .ok()?;

    Some(bundle.path().display().to_string())
}

struct InFlightCleanupGuard {
    in_flight: Arc<Mutex<HashMap<i64, CancellationToken>>>,
    request_seq: i64,
    armed: bool,
}

impl InFlightCleanupGuard {
    fn new(in_flight: Arc<Mutex<HashMap<i64, CancellationToken>>>, request_seq: i64) -> Self {
        Self {
            in_flight,
            request_seq,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InFlightCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        let in_flight = self.in_flight.clone();
        let request_seq = self.request_seq;
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };

        // Best-effort: ensure we don't leak `in_flight` entries even if the request task itself
        // panics. This runs in a detached task because `Drop` can't be async.
        handle.spawn(async move {
            let mut guard = in_flight.lock().await;
            guard.remove(&request_seq);
        });
    }
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

async fn send_exited_once(
    tx: &mpsc::Sender<Value>,
    seq: &Arc<AtomicI64>,
    exited_sent: &Arc<AtomicBool>,
    exit_code: i64,
    server_shutdown: &CancellationToken,
) {
    if exited_sent
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        send_event(
            tx,
            seq,
            "exited",
            Some(json!({ "exitCode": exit_code })),
            server_shutdown,
        )
        .await;
    }
}

async fn send_terminated_once(
    tx: &mpsc::Sender<Value>,
    seq: &Arc<AtomicI64>,
    terminated_sent: &Arc<AtomicBool>,
    server_shutdown: &CancellationToken,
) {
    if terminated_sent
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        send_event(tx, seq, "terminated", None, server_shutdown).await;
    }
}

fn spawn_event_task(
    debugger: Arc<Mutex<Option<Debugger>>>,
    tx: mpsc::Sender<Value>,
    seq: Arc<AtomicI64>,
    terminated_sent: Arc<AtomicBool>,
    server_shutdown: CancellationToken,
    debugger_id: u64,
    suppress_termination_debugger_id: Arc<AtomicU64>,
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
                    if suppress_termination_debugger_id
                        .compare_exchange(debugger_id, 0, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        return;
                    }

                    send_terminated_once(&tx, &seq, &terminated_sent, &server_shutdown).await;
                    server_shutdown.cancel();
                    return;
                }
                event = events.recv() => match event {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Closed) => {
                        if suppress_termination_debugger_id
                            .compare_exchange(debugger_id, 0, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                        {
                            return;
                        }

                        send_terminated_once(&tx, &seq, &terminated_sent, &server_shutdown).await;
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
            let mut internal_eval_stop_event = false;
            let mut breakpoint_updates: Vec<Value> = Vec::new();

            {
                let mut guard = debugger.lock().await;
                if let Some(dbg) = guard.as_mut() {
                    // Stream debug uses JDWP `InvokeMethod`, which resumes the stopped thread
                    // to execute user code. That code can hit breakpoints (or exceptions),
                    // which would normally be treated as a real stop by the adapter.
                    //
                    // While internal evaluation is in progress for a given thread, suppress
                    // these stop events, avoid mutating breakpoint state, and immediately
                    // resume the thread so the invoke can complete.
                    match &event {
                        nova_jdwp::wire::JdwpEvent::Breakpoint {
                            thread, request_id, ..
                        } => {
                            if dbg.is_internal_evaluation_thread(*thread) {
                                internal_eval_stop_event = true;
                                // Logpoints use `SuspendPolicy::NONE`, so there is no thread
                                // suspension to resume.
                                if !dbg.breakpoint_is_logpoint(*request_id) {
                                    let _ = dbg.jdwp_client().thread_resume(*thread).await;
                                }
                            }
                        }
                        nova_jdwp::wire::JdwpEvent::SingleStep { thread, .. }
                        | nova_jdwp::wire::JdwpEvent::Exception { thread, .. } => {
                            if dbg.is_internal_evaluation_thread(*thread) {
                                internal_eval_stop_event = true;
                                let _ = dbg.jdwp_client().thread_resume(*thread).await;
                            }
                        }
                        _ => {}
                    }

                    if internal_eval_stop_event {
                        // Skip all other stop-event processing. In particular, do *not* call
                        // `handle_breakpoint_event` (hit counts/conditions/logpoints) and do not
                        // update smart-step state.
                        continue;
                    }

                    dbg.handle_vm_event(&event).await;
                    breakpoint_updates = dbg.take_breakpoint_updates();

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

            for breakpoint in breakpoint_updates {
                send_event(
                    &tx,
                    &seq,
                    "breakpoint",
                    Some(json!({ "reason": "changed", "breakpoint": breakpoint })),
                    &server_shutdown,
                )
                .await;
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
                                    &server_shutdown,
                                )
                                .await;
                            }

                            send_event(
                                &tx,
                                &seq,
                                "stopped",
                                Some(
                                    json!({"reason": "breakpoint", "threadId": thread as i64, "allThreadsStopped": false}),
                                ),
                                &server_shutdown,
                            )
                            .await;
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
                                &server_shutdown,
                            )
                            .await;
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
                            &server_shutdown,
                        )
                        .await;
                    }
                    send_event(
                        &tx,
                        &seq,
                        "stopped",
                        Some(
                            json!({"reason": "step", "threadId": thread as i64, "allThreadsStopped": false}),
                        ),
                        &server_shutdown,
                    )
                    .await;
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
                            &server_shutdown,
                        )
                        .await;
                    }
                    let mut body = serde_json::Map::new();
                    body.insert("reason".to_string(), json!("exception"));
                    body.insert("threadId".to_string(), json!(thread as i64));
                    body.insert("allThreadsStopped".to_string(), json!(false));
                    if let Some(text) = exception_text {
                        body.insert("text".to_string(), json!(text));
                    }
                    send_event(&tx, &seq, "stopped", Some(Value::Object(body)), &server_shutdown).await;
                }
                nova_jdwp::wire::JdwpEvent::FieldAccess {
                    thread,
                    field_id,
                    object,
                    value,
                    ..
                } => {
                    let mut body = serde_json::Map::new();
                    body.insert("reason".to_string(), json!("data breakpoint"));
                    body.insert("threadId".to_string(), json!(thread as i64));
                    body.insert("allThreadsStopped".to_string(), json!(false));
                    body.insert(
                        "text".to_string(),
                        json!(format!(
                            "Read field 0x{field_id:x} on object@0x{object:x}: {}",
                            format_value(&value)
                        )),
                    );
                    send_event(&tx, &seq, "stopped", Some(Value::Object(body)), &server_shutdown).await;
                }
                nova_jdwp::wire::JdwpEvent::FieldModification {
                    thread,
                    field_id,
                    object,
                    value_to_be,
                    ..
                } => {
                    let mut body = serde_json::Map::new();
                    body.insert("reason".to_string(), json!("data breakpoint"));
                    body.insert("threadId".to_string(), json!(thread as i64));
                    body.insert("allThreadsStopped".to_string(), json!(false));
                    body.insert(
                        "text".to_string(),
                        json!(format!(
                            "Wrote field 0x{field_id:x} on object@0x{object:x}: {}",
                            format_value(&value_to_be)
                        )),
                    );
                    send_event(&tx, &seq, "stopped", Some(Value::Object(body)), &server_shutdown).await;
                }
                nova_jdwp::wire::JdwpEvent::ThreadStart { thread, .. } => {
                    send_event(
                        &tx,
                        &seq,
                        "thread",
                        Some(json!({"reason": "started", "threadId": thread as i64})),
                        &server_shutdown,
                    )
                    .await;
                }
                nova_jdwp::wire::JdwpEvent::ThreadDeath { thread, .. } => {
                    send_event(
                        &tx,
                        &seq,
                        "thread",
                        Some(json!({"reason": "exited", "threadId": thread as i64})),
                        &server_shutdown,
                    )
                    .await;
                }
                nova_jdwp::wire::JdwpEvent::VmDeath | nova_jdwp::wire::JdwpEvent::VmDisconnect => {
                    if suppress_termination_debugger_id
                        .compare_exchange(debugger_id, 0, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                    {
                        return;
                    }
                    send_terminated_once(&tx, &seq, &terminated_sent, &server_shutdown).await;
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

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    };

    use serde_json::json;
    use tokio::io::AsyncWriteExt;
    use tokio::time::{timeout, Duration};

    use super::*;

    async fn read_message<R: tokio::io::AsyncRead + Unpin>(reader: &mut DapReader<R>) -> Value {
        timeout(Duration::from_secs(2), reader.read_value())
            .await
            .expect("timed out waiting for DAP message")
            .expect("failed to read DAP message")
            .expect("unexpected EOF from server")
    }

    async fn read_response<R: tokio::io::AsyncRead + Unpin>(
        reader: &mut DapReader<R>,
        request_seq: i64,
    ) -> Value {
        loop {
            let msg = read_message(reader).await;
            let is_response = msg.get("type").and_then(|v| v.as_str()) == Some("response");
            let matches_seq = msg.get("request_seq").and_then(|v| v.as_i64()) == Some(request_seq);
            if is_response && matches_seq {
                return msg;
            }
        }
    }

    #[tokio::test]
    async fn request_handler_panics_are_isolated_and_do_not_wedge_server() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);

        let server_task = tokio::spawn(async move { run(server_read, server_write).await });

        let (client_read, client_write) = tokio::io::split(client);
        let mut writer = DapWriter::new(client_write);
        let mut reader = DapReader::new(client_read);

        // Initialize the adapter so subsequent requests don't block on the
        // `requires_initialized` gate.
        let init = Request {
            seq: 1,
            message_type: "request".to_string(),
            command: "initialize".to_string(),
            arguments: json!({}),
        };
        writer
            .write_value(&serde_json::to_value(init).unwrap())
            .await
            .unwrap();
        let init_resp = read_response(&mut reader, 1).await;
        assert_eq!(init_resp["success"], true);
        let init_event = read_message(&mut reader).await;
        assert_eq!(init_event["type"], "event");
        assert_eq!(init_event["event"], "initialized");

        // Trigger a deterministic panic inside the request handler.
        let panic_req = Request {
            seq: 2,
            message_type: "request".to_string(),
            command: "nova/testPanic".to_string(),
            arguments: json!({}),
        };
        writer
            .write_value(&serde_json::to_value(panic_req).unwrap())
            .await
            .unwrap();

        let panic_resp = read_response(&mut reader, 2).await;
        assert_eq!(panic_resp["success"], false);
        let message = panic_resp
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(
            message.contains("internal error (panic)"),
            "unexpected panic response message: {message}"
        );

        // The adapter should continue serving follow-up requests.
        let metrics_req = Request {
            seq: 3,
            message_type: "request".to_string(),
            command: "nova/metrics".to_string(),
            arguments: json!({}),
        };
        writer
            .write_value(&serde_json::to_value(metrics_req).unwrap())
            .await
            .unwrap();

        let metrics_resp = read_response(&mut reader, 3).await;
        assert_eq!(metrics_resp["success"], true);

        // Clean shutdown.
        let disconnect_req = Request {
            seq: 4,
            message_type: "request".to_string(),
            command: "disconnect".to_string(),
            arguments: json!({ "terminateDebuggee": false }),
        };
        writer
            .write_value(&serde_json::to_value(disconnect_req).unwrap())
            .await
            .unwrap();
        let disconnect_resp = read_response(&mut reader, 4).await;
        assert_eq!(disconnect_resp["success"], true);
        let terminated_event = read_message(&mut reader).await;
        assert_eq!(terminated_event["type"], "event");
        assert_eq!(terminated_event["event"], "terminated");

        let server_res = timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server did not shut down in time")
            .expect("server task panicked");
        server_res.expect("server returned error");
    }

    #[tokio::test]
    async fn spawn_output_task_truncates_overlong_lines() {
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel::<Value>(8);
        let seq = Arc::new(AtomicI64::new(1));
        let shutdown = CancellationToken::new();

        spawn_output_task(reader, tx, Arc::clone(&seq), "stdout", shutdown.clone());

        let oversized = vec![b'a'; 200 * 1024];
        writer.write_all(&oversized).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.shutdown().await.unwrap();

        let msg = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for output event")
            .expect("output channel closed");

        assert_eq!(msg.get("event").and_then(|v| v.as_str()), Some("output"));
        assert_eq!(
            msg.get("body")
                .and_then(|v| v.get("category"))
                .and_then(|v| v.as_str()),
            Some("stdout")
        );

        let output = msg
            .get("body")
            .and_then(|v| v.get("output"))
            .and_then(|v| v.as_str())
            .expect("output.body.output should be a string");

        assert!(
            output.contains(DEBUGGEE_OUTPUT_TRUNCATION_MARKER),
            "expected truncation marker in output, got: {output:?}"
        );
        assert!(
            output.len()
                <= MAX_DEBUGGEE_OUTPUT_LINE_BYTES + DEBUGGEE_OUTPUT_TRUNCATION_SUFFIX.len(),
            "expected output length to be bounded (got {}, limit {})",
            output.len(),
            MAX_DEBUGGEE_OUTPUT_LINE_BYTES + DEBUGGEE_OUTPUT_TRUNCATION_SUFFIX.len()
        );

        // Ensure `send_event` used the sequence counter.
        assert_eq!(
            msg.get("seq").and_then(|v| v.as_i64()),
            Some(seq.load(Ordering::Relaxed) - 1)
        );
    }

    #[tokio::test]
    async fn spawn_output_task_truncates_overlong_lines_without_newline() {
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel::<Value>(8);
        let seq = Arc::new(AtomicI64::new(1));
        let shutdown = CancellationToken::new();

        spawn_output_task(reader, tx, Arc::clone(&seq), "stdout", shutdown.clone());

        let oversized = vec![b'a'; 200 * 1024];
        writer.write_all(&oversized).await.unwrap();
        writer.shutdown().await.unwrap();

        let msg = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for output event")
            .expect("output channel closed");

        assert_eq!(msg.get("event").and_then(|v| v.as_str()), Some("output"));

        let output = msg
            .get("body")
            .and_then(|v| v.get("output"))
            .and_then(|v| v.as_str())
            .expect("output.body.output should be a string");

        assert!(
            output.contains(DEBUGGEE_OUTPUT_TRUNCATION_MARKER),
            "expected truncation marker in output, got: {output:?}"
        );
        assert!(
            output.len()
                <= MAX_DEBUGGEE_OUTPUT_LINE_BYTES + DEBUGGEE_OUTPUT_TRUNCATION_MARKER.len(),
            "expected output length to be bounded (got {}, limit {})",
            output.len(),
            MAX_DEBUGGEE_OUTPUT_LINE_BYTES + DEBUGGEE_OUTPUT_TRUNCATION_MARKER.len()
        );
    }

    #[tokio::test]
    async fn spawn_output_task_truncates_overlong_lines_on_stderr() {
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel::<Value>(8);
        let seq = Arc::new(AtomicI64::new(1));
        let shutdown = CancellationToken::new();

        spawn_output_task(reader, tx, Arc::clone(&seq), "stderr", shutdown.clone());

        let oversized = vec![b'a'; 200 * 1024];
        writer.write_all(&oversized).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.shutdown().await.unwrap();

        let msg = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for output event")
            .expect("output channel closed");

        assert_eq!(msg.get("event").and_then(|v| v.as_str()), Some("output"));
        assert_eq!(
            msg.get("body")
                .and_then(|v| v.get("category"))
                .and_then(|v| v.as_str()),
            Some("stderr")
        );

        let output = msg
            .get("body")
            .and_then(|v| v.get("output"))
            .and_then(|v| v.as_str())
            .expect("output.body.output should be a string");

        assert!(
            output.contains(DEBUGGEE_OUTPUT_TRUNCATION_MARKER),
            "expected truncation marker in output, got: {output:?}"
        );
    }
}

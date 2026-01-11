use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
        Arc,
    },
    time::Instant,
};

use nova_jdwp::wire::JdwpError;
use nova_scheduler::CancellationToken;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::{
    sync::{broadcast, mpsc, watch, Mutex},
    task::JoinSet,
};

use nova_bugreport::{create_bug_report_bundle, global_crash_store, BugReportOptions, PerfStats};
use nova_config::NovaConfig;

use crate::{
    dap_tokio::{make_event, make_response, DapError, DapReader, DapWriter, Request},
    wire_debugger::{AttachArgs, Debugger, DebuggerError, StepDepth},
};

#[derive(Debug, Error)]
pub enum WireServerError {
    #[error(transparent)]
    Dap(#[from] DapError),

    #[error(transparent)]
    Debugger(#[from] DebuggerError),
}

type Result<T> = std::result::Result<T, WireServerError>;

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

                let is_disconnect = request.command == "disconnect";
                if is_disconnect {
                    shutdown_request_seq = Some(request.seq);
                    server_shutdown.cancel();
                }

                tasks.spawn(handle_request(
                    request,
                    request_token,
                    out_tx.clone(),
                    seq.clone(),
                    debugger.clone(),
                    in_flight.clone(),
                    initialized_tx.clone(),
                    initialized_rx.clone(),
                    server_shutdown.clone(),
                    terminated_sent.clone(),
                ));

                if is_disconnect {
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
    in_flight: &Arc<Mutex<HashMap<i64, CancellationToken>>>,
    initialized_tx: &watch::Sender<bool>,
    initialized_rx: watch::Receiver<bool>,
    server_shutdown: &CancellationToken,
    terminated_sent: &Arc<AtomicBool>,
) {
    if requires_initialized(request.command.as_str()) {
        if !wait_initialized(cancel, initialized_rx).await {
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
            let body = json!({
                "supportsConfigurationDoneRequest": true,
                "supportsEvaluateForHovers": true,
                "supportsPauseRequest": true,
                "supportsCancelRequest": true,
                "supportsSetVariable": false,
                "supportsStepBack": false,
                "supportsExceptionBreakpoints": true,
                "supportsExceptionInfoRequest": true,
                "exceptionBreakpointFilters": [
                    { "filter": "caught", "label": "Caught Exceptions", "default": false },
                    { "filter": "uncaught", "label": "Uncaught Exceptions", "default": false },
                    { "filter": "all", "label": "All Exceptions", "default": false },
                ],
                "supportsConditionalBreakpoints": false,
            });
            send_response(out_tx, seq, request, true, Some(body), None);
            send_event(out_tx, seq, "initialized", None);
            let _ = initialized_tx.send(true);
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

            match create_bug_report_bundle(
                &cfg,
                log_buffer.as_ref(),
                crash_store.as_ref(),
                &perf,
                options,
            ) {
                Ok(bundle) => {
                    if let Ok(metrics_json) = serde_json::to_string_pretty(
                        &nova_metrics::MetricsRegistry::global().snapshot(),
                    ) {
                        let _ = std::fs::write(bundle.path().join("metrics.json"), metrics_json);
                    }

                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!({ "path": bundle.path().display().to_string() })),
                        None,
                    );
                }
                Err(err) => send_response(out_tx, seq, request, false, None, Some(err.to_string())),
            }
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
            send_response(out_tx, seq, request, true, None, None);
        }
        "attach" => {
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
                    Some("attach.port is required".to_string()),
                );
                return;
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

            let dbg = match Debugger::attach(AttachArgs {
                host,
                port: port as u16,
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
            send_response(out_tx, seq, request, true, None, None);
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
            let lines: Vec<i32> = request
                .arguments
                .get("breakpoints")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|bp| bp.get("line").and_then(|l| l.as_i64()).map(|l| l as i32))
                        .collect()
                })
                .unwrap_or_default();

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

            match dbg.set_breakpoints(cancel, source_path, lines).await {
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

            match dbg.continue_(cancel).await {
                Ok(()) => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        true,
                        Some(json!({ "allThreadsContinued": true })),
                        None,
                    );

                    let mut body = serde_json::Map::new();
                    body.insert("allThreadsContinued".to_string(), json!(true));
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

            match dbg.pause(cancel).await {
                Ok(()) => {
                    send_response(out_tx, seq, request, true, None, None);
                    let mut body = serde_json::Map::new();
                    body.insert("reason".to_string(), json!("pause"));
                    body.insert("allThreadsStopped".to_string(), json!(true));
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

            match dbg.step(cancel, thread_id, depth).await {
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
            let expression = request
                .arguments
                .get("expression")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let frame_id = request
                .arguments
                .get("frameId")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

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

            match dbg.evaluate(cancel, frame_id, expression).await {
                Ok(body) if cancel.is_cancelled() => {
                    send_response(
                        out_tx,
                        seq,
                        request,
                        false,
                        None,
                        Some("cancelled".to_string()),
                    );
                }
                Ok(body) => send_response(out_tx, seq, request, true, body, None),
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
                send_response(out_tx, seq, request, false, None, Some("cancelled".to_string()));
                return;
            }

            let guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(out_tx, seq, request, false, None, Some("cancelled".to_string()));
                    return;
                }
            };

            let Some(dbg) = guard.as_ref() else {
                send_response(out_tx, seq, request, false, None, Some("not attached".to_string()));
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
            if cancel.is_cancelled() {
                send_response(out_tx, seq, request, false, None, Some("cancelled".to_string()));
                return;
            }

            let guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(out_tx, seq, request, false, None, Some("cancelled".to_string()));
                    return;
                }
            };

            let Some(dbg) = guard.as_ref() else {
                send_response(out_tx, seq, request, false, None, Some("not attached".to_string()));
                return;
            };

            let caps = dbg.capabilities().await;
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

            send_response(
                out_tx,
                seq,
                request,
                false,
                None,
                Some("hot swap is not implemented in the wire adapter yet".to_string()),
            );
        }
        // Method return values (e.g. step-out with return value).
        "nova/enableMethodReturnValues" => {
            if cancel.is_cancelled() {
                send_response(out_tx, seq, request, false, None, Some("cancelled".to_string()));
                return;
            }

            let guard = match lock_or_cancel(cancel, debugger.as_ref()).await {
                Some(guard) => guard,
                None => {
                    send_response(out_tx, seq, request, false, None, Some("cancelled".to_string()));
                    return;
                }
            };

            let Some(dbg) = guard.as_ref() else {
                send_response(out_tx, seq, request, false, None, Some("not attached".to_string()));
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
                false,
                None,
                Some("method return values are not implemented in the wire adapter yet".to_string()),
            );
        }
        "disconnect" => {
            let mut guard = debugger.lock().await;
            if let Some(mut dbg) = guard.take() {
                dbg.disconnect().await;
            }
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

fn requires_initialized(command: &str) -> bool {
    !matches!(
        command,
        "initialize" | "cancel" | "disconnect" | "nova/bugReport" | "nova/metrics"
    )
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

            {
                let mut guard = debugger.lock().await;
                if let Some(dbg) = guard.as_mut() {
                    dbg.handle_vm_event(&event).await;
                }
            }

            match event {
                nova_jdwp::wire::JdwpEvent::Breakpoint { thread, .. } => {
                    send_event(
                        &tx,
                        &seq,
                        "stopped",
                        Some(
                            json!({"reason": "breakpoint", "threadId": thread as i64, "allThreadsStopped": false}),
                        ),
                    );
                }
                nova_jdwp::wire::JdwpEvent::SingleStep { thread, .. } => {
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
                    send_event(
                        &tx,
                        &seq,
                        "stopped",
                        Some(
                            json!({"reason": "exception", "threadId": thread as i64, "allThreadsStopped": false}),
                        ),
                    );
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

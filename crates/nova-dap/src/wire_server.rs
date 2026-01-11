use std::{
    net::IpAddr,
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
        Arc,
    },
};

use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, watch, Mutex};

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
    let (terminate_tx, mut terminate_rx) = watch::channel(false);
    let debugger: Arc<Mutex<Option<Debugger>>> = Arc::new(Mutex::new(None));

    let writer_task = tokio::spawn(async move {
        let mut writer = DapWriter::new(writer);
        while let Some(msg) = out_rx.recv().await {
            let _ = writer.write_value(&msg).await;
        }
    });

    let mut reader = DapReader::new(reader);

    loop {
        tokio::select! {
            _ = terminate_rx.changed() => break,
            request = reader.read_request() => {
                let Some(request) = request? else {
                    break;
                };
                if request.message_type != "request" {
                    continue;
                }

                match request.command.as_str() {
                    "initialize" => {
                        let body = json!({
                            "supportsConfigurationDoneRequest": true,
                            "supportsEvaluateForHovers": true,
                            "supportsPauseRequest": true,
                            "supportsSetVariable": false,
                            "supportsStepBack": false,
                            "supportsExceptionInfoRequest": false,
                            "exceptionBreakpointFilters": [
                                { "filter": "caught", "label": "Caught Exceptions", "default": false },
                                { "filter": "uncaught", "label": "Uncaught Exceptions", "default": false },
                                { "filter": "all", "label": "All Exceptions", "default": false },
                            ],
                            "supportsConditionalBreakpoints": false,
                        });
                        send_response(&out_tx, &seq, &request, true, Some(body), None);
                        send_event(&out_tx, &seq, "initialized", None);
                    }
                    "nova/bugReport" => {
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
                            Ok(bundle) => send_response(
                                &out_tx,
                                &seq,
                                &request,
                                true,
                                Some(json!({ "path": bundle.path().display().to_string() })),
                                None,
                            ),
                            Err(err) => send_response(
                                &out_tx,
                                &seq,
                                &request,
                                false,
                                None,
                                Some(err.to_string()),
                            ),
                        }
                    }
                    "configurationDone" => {
                        // When `supportsConfigurationDoneRequest` is true, VS Code sends this request
                        // after breakpoints have been configured.
                        send_response(&out_tx, &seq, &request, true, None, None);
                    }
                    "attach" => {
                        let host = request
                            .arguments
                            .get("host")
                            .and_then(|v| v.as_str())
                            .unwrap_or("127.0.0.1");
                        let port = match request.arguments.get("port").and_then(|v| v.as_u64()) {
                            Some(port) => port,
                            None => {
                                send_response(
                                    &out_tx,
                                    &seq,
                                    &request,
                                    false,
                                    None,
                                    Some("attach.port is required".to_string()),
                                );
                                continue;
                            }
                        };

                        let host: IpAddr = match host.parse() {
                            Ok(host) => host,
                            Err(err) => {
                                send_response(
                                    &out_tx,
                                    &seq,
                                    &request,
                                    false,
                                    None,
                                    Some(format!("invalid host {host:?}: {err}")),
                                );
                                continue;
                            }
                        };

                        match Debugger::attach(AttachArgs { host, port: port as u16 }).await {
                            Ok(dbg) => {
                                let mut guard = debugger.lock().await;
                                *guard = Some(dbg);
                                drop(guard);

                                spawn_event_task(
                                    debugger.clone(),
                                    out_tx.clone(),
                                    seq.clone(),
                                    terminated_sent.clone(),
                                    terminate_tx.clone(),
                                );
                                send_response(&out_tx, &seq, &request, true, None, None);
                            }
                            Err(err) => {
                                send_response(&out_tx, &seq, &request, false, None, Some(err.to_string()));
                            }
                        }
                    }
                    "setBreakpoints" => {
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
                        let mut guard = debugger.lock().await;
                        let Some(dbg) = guard.as_mut() else {
                            send_response(
                                &out_tx,
                                &seq,
                                &request,
                                false,
                                None,
                                Some("not attached".to_string()),
                            );
                            continue;
                        };
                        match dbg.set_breakpoints(source_path, lines).await {
                            Ok(bps) => send_response(
                                &out_tx,
                                &seq,
                                &request,
                                true,
                                Some(json!({ "breakpoints": bps })),
                                None,
                            ),
                            Err(err) => send_response(&out_tx, &seq, &request, false, None, Some(err.to_string())),
                        }
                    }
                    "setExceptionBreakpoints" => {
                        let filters: Vec<String> = request
                            .arguments
                            .get("filters")
                            .and_then(|v| v.as_array())
                            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
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

                        if let Some(options) = request.arguments.get("exceptionOptions").and_then(|v| v.as_array()) {
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

                        let mut guard = debugger.lock().await;
                        let Some(dbg) = guard.as_mut() else {
                            send_response(&out_tx, &seq, &request, false, None, Some("not attached".to_string()));
                            continue;
                        };

                        match dbg.set_exception_breakpoints(caught, uncaught).await {
                            Ok(()) => send_response(&out_tx, &seq, &request, true, None, None),
                            Err(err) => send_response(&out_tx, &seq, &request, false, None, Some(err.to_string())),
                        }
                    }
                    "threads" => {
                        let guard = debugger.lock().await;
                        let Some(dbg) = guard.as_ref() else {
                            send_response(&out_tx, &seq, &request, true, Some(json!({ "threads": [] })), None);
                            continue;
                        };
                        let threads = dbg.threads().await?;
                        let threads: Vec<Value> = threads
                            .into_iter()
                            .map(|(id, name)| json!({ "id": id, "name": name }))
                            .collect();
                        send_response(&out_tx, &seq, &request, true, Some(json!({ "threads": threads })), None);
                    }
                    "stackTrace" => {
                        let thread_id = request
                            .arguments
                            .get("threadId")
                            .and_then(|v| v.as_i64())
                            .ok_or_else(|| DebuggerError::InvalidRequest("stackTrace.threadId is required".to_string()))?;
                        let mut guard = debugger.lock().await;
                        let Some(dbg) = guard.as_mut() else {
                            send_response(&out_tx, &seq, &request, false, None, Some("not attached".to_string()));
                            continue;
                        };
                        match dbg.stack_trace(thread_id).await {
                            Ok(frames) => send_response(
                                &out_tx,
                                &seq,
                                &request,
                                true,
                                Some(json!({ "stackFrames": frames, "totalFrames": frames.len() })),
                                None,
                            ),
                            Err(err) => send_response(&out_tx, &seq, &request, false, None, Some(err.to_string())),
                        }
                    }
                    "scopes" => {
                        let frame_id = request
                            .arguments
                            .get("frameId")
                            .and_then(|v| v.as_i64())
                            .ok_or_else(|| DebuggerError::InvalidRequest("scopes.frameId is required".to_string()))?;
                        let mut guard = debugger.lock().await;
                        let Some(dbg) = guard.as_mut() else {
                            send_response(&out_tx, &seq, &request, true, Some(json!({ "scopes": [] })), None);
                            continue;
                        };
                        match dbg.scopes(frame_id) {
                            Ok(scopes) => send_response(&out_tx, &seq, &request, true, Some(json!({ "scopes": scopes })), None),
                            Err(err) => send_response(&out_tx, &seq, &request, false, None, Some(err.to_string())),
                        }
                    }
                    "variables" => {
                        let variables_reference = request
                            .arguments
                            .get("variablesReference")
                            .and_then(|v| v.as_i64())
                            .ok_or_else(|| DebuggerError::InvalidRequest("variables.variablesReference is required".to_string()))?;
                        let start = request.arguments.get("start").and_then(|v| v.as_i64());
                        let count = request.arguments.get("count").and_then(|v| v.as_i64());
                        let mut guard = debugger.lock().await;
                        let Some(dbg) = guard.as_mut() else {
                            send_response(&out_tx, &seq, &request, true, Some(json!({ "variables": [] })), None);
                            continue;
                        };
                        match dbg.variables(variables_reference, start, count).await {
                            Ok(vars) => send_response(&out_tx, &seq, &request, true, Some(json!({ "variables": vars })), None),
                            Err(err) => send_response(&out_tx, &seq, &request, false, None, Some(err.to_string())),
                        }
                    }
                    "continue" => {
                        let thread_id = request.arguments.get("threadId").and_then(|v| v.as_i64());
                        let guard = debugger.lock().await;
                        let Some(dbg) = guard.as_ref() else {
                            send_response(&out_tx, &seq, &request, false, None, Some("not attached".to_string()));
                            continue;
                        };
                        match dbg.continue_().await {
                            Ok(()) => {
                                send_response(
                                    &out_tx,
                                    &seq,
                                    &request,
                                    true,
                                    Some(json!({ "allThreadsContinued": true })),
                                    None,
                                );

                                let mut body = serde_json::Map::new();
                                body.insert("allThreadsContinued".to_string(), json!(true));
                                if let Some(thread_id) = thread_id {
                                    body.insert("threadId".to_string(), json!(thread_id));
                                }
                                send_event(&out_tx, &seq, "continued", Some(Value::Object(body)));
                            }
                            Err(err) => send_response(&out_tx, &seq, &request, false, None, Some(err.to_string())),
                        }
                    }
                    "pause" => {
                        let thread_id = request.arguments.get("threadId").and_then(|v| v.as_i64());
                        let guard = debugger.lock().await;
                        let Some(dbg) = guard.as_ref() else {
                            send_response(&out_tx, &seq, &request, false, None, Some("not attached".to_string()));
                            continue;
                        };
                        match dbg.pause().await {
                            Ok(()) => {
                                send_response(&out_tx, &seq, &request, true, None, None);
                                let mut body = serde_json::Map::new();
                                body.insert("reason".to_string(), json!("pause"));
                                body.insert("allThreadsStopped".to_string(), json!(true));
                                if let Some(thread_id) = thread_id {
                                    body.insert("threadId".to_string(), json!(thread_id));
                                }
                                send_event(&out_tx, &seq, "stopped", Some(Value::Object(body)));
                            }
                            Err(err) => send_response(&out_tx, &seq, &request, false, None, Some(err.to_string())),
                        }
                    }
                    "next" | "stepIn" | "stepOut" => {
                        let thread_id = request
                            .arguments
                            .get("threadId")
                            .and_then(|v| v.as_i64())
                            .ok_or_else(|| DebuggerError::InvalidRequest(format!("{}.threadId is required", request.command)))?;
                        let depth = match request.command.as_str() {
                            "next" => StepDepth::Over,
                            "stepIn" => StepDepth::Into,
                            _ => StepDepth::Out,
                        };
                        let mut guard = debugger.lock().await;
                        let Some(dbg) = guard.as_mut() else {
                            send_response(&out_tx, &seq, &request, false, None, Some("not attached".to_string()));
                            continue;
                        };
                        match dbg.step(thread_id, depth).await {
                            Ok(()) => send_response(&out_tx, &seq, &request, true, None, None),
                            Err(err) => send_response(&out_tx, &seq, &request, false, None, Some(err.to_string())),
                        }
                    }
                    "evaluate" => {
                        let expression = request
                            .arguments
                            .get("expression")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let frame_id = request.arguments.get("frameId").and_then(|v| v.as_i64()).unwrap_or(0);
                        let mut guard = debugger.lock().await;
                        let Some(dbg) = guard.as_mut() else {
                            send_response(&out_tx, &seq, &request, false, None, Some("not attached".to_string()));
                            continue;
                        };
                        match dbg.evaluate(frame_id, expression).await {
                            Ok(body) => send_response(&out_tx, &seq, &request, true, body, None),
                            Err(err) => send_response(&out_tx, &seq, &request, false, None, Some(err.to_string())),
                        }
                    }
                    "nova/pinObject" => {
                        let variables_reference = request
                            .arguments
                            .get("variablesReference")
                            .and_then(|v| v.as_i64())
                            .ok_or_else(|| DebuggerError::InvalidRequest("pinObject.variablesReference is required".to_string()))?;
                        let pinned = request
                            .arguments
                            .get("pinned")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let mut guard = debugger.lock().await;
                        let Some(dbg) = guard.as_mut() else {
                            send_response(&out_tx, &seq, &request, false, None, Some("not attached".to_string()));
                            continue;
                        };
                        match dbg.set_object_pinned(variables_reference, pinned).await {
                            Ok(pinned) => send_response(
                                &out_tx,
                                &seq,
                                &request,
                                true,
                                Some(json!({ "pinned": pinned })),
                                None,
                            ),
                            Err(err) => send_response(&out_tx, &seq, &request, false, None, Some(err.to_string())),
                        }
                    }
                    "disconnect" => {
                        let mut guard = debugger.lock().await;
                        if let Some(mut dbg) = guard.take() {
                            dbg.disconnect().await;
                        }
                        send_response(&out_tx, &seq, &request, true, None, None);
                        send_terminated_once(&out_tx, &seq, &terminated_sent);
                        let _ = terminate_tx.send(true);
                        break;
                    }
                    _ => {
                        send_response(
                            &out_tx,
                            &seq,
                            &request,
                            false,
                            None,
                            Some(format!("unhandled request {}", request.command)),
                        );
                    }
                }
            }
        }
    }

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
    let s = seq.fetch_add(1, Ordering::Relaxed);
    let resp = make_response(s, request, success, body, message);
    let _ = tx.send(serde_json::to_value(resp).unwrap_or_else(|_| json!({})));
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
    terminate_tx: watch::Sender<bool>,
) {
    tokio::spawn(async move {
        let mut events: Option<broadcast::Receiver<nova_jdwp::wire::JdwpEvent>> = None;
        let mut shutdown = None;

        {
            let guard = debugger.lock().await;
            if let Some(dbg) = guard.as_ref() {
                events = Some(dbg.subscribe_events());
                shutdown = Some(dbg.jdwp_shutdown_token());
            }
        }

        let Some(mut events) = events else {
            return;
        };
        let Some(shutdown) = shutdown else {
            return;
        };

        loop {
            let event = tokio::select! {
                _ = shutdown.cancelled() => {
                    send_terminated_once(&tx, &seq, &terminated_sent);
                    let _ = terminate_tx.send(true);
                    return;
                }
                event = events.recv() => match event {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Closed) => {
                        send_terminated_once(&tx, &seq, &terminated_sent);
                        let _ = terminate_tx.send(true);
                        return;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
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
                nova_jdwp::wire::JdwpEvent::VmDeath => {
                    send_terminated_once(&tx, &seq, &terminated_sent);
                    let _ = terminate_tx.send(true);
                    return;
                }
                _ => {}
            }
        }
    });
}

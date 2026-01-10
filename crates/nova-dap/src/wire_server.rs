use std::{
    net::IpAddr,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};

use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, Mutex};

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
    let debugger: Arc<Mutex<Option<Debugger>>> = Arc::new(Mutex::new(None));

    let writer_task = tokio::spawn(async move {
        let mut writer = DapWriter::new(writer);
        while let Some(msg) = out_rx.recv().await {
            let _ = writer.write_value(&msg).await;
        }
    });

    let mut reader = DapReader::new(reader);

    while let Some(request) = reader.read_request().await? {
        if request.message_type != "request" {
            continue;
        }

        match request.command.as_str() {
            "initialize" => {
                let body = json!({
                    "supportsConfigurationDoneRequest": true,
                    "supportsEvaluateForHovers": true,
                    "supportsSetVariable": false,
                    "supportsStepBack": false,
                    "supportsExceptionInfoRequest": false,
                    "supportsConditionalBreakpoints": false,
                });
                send_response(&out_tx, &seq, &request, true, Some(body), None);
                send_event(&out_tx, &seq, "initialized", None);
            }
            "attach" => {
                let host = request
                    .arguments
                    .get("host")
                    .and_then(|v| v.as_str())
                    .unwrap_or("127.0.0.1");
                let port = request
                    .arguments
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| DebuggerError::InvalidRequest("attach.port is required".to_string()))?;
                let host: IpAddr = host
                    .parse()
                    .map_err(|e| DebuggerError::InvalidRequest(format!("invalid host {host:?}: {e}")))?;

                let dbg = Debugger::attach(AttachArgs { host, port: port as u16 }).await?;
                let mut guard = debugger.lock().await;
                *guard = Some(dbg);
                drop(guard);

                spawn_event_task(debugger.clone(), out_tx.clone(), seq.clone());
                send_response(&out_tx, &seq, &request, true, None, None);
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
                let guard = debugger.lock().await;
                let Some(dbg) = guard.as_ref() else {
                    send_response(&out_tx, &seq, &request, false, None, Some("not attached".to_string()));
                    continue;
                };
                match dbg.continue_().await {
                    Ok(()) => send_response(
                        &out_tx,
                        &seq,
                        &request,
                        true,
                        Some(json!({ "allThreadsContinued": true })),
                        None,
                    ),
                    Err(err) => send_response(&out_tx, &seq, &request, false, None, Some(err.to_string())),
                }
            }
            "pause" => {
                let guard = debugger.lock().await;
                let Some(dbg) = guard.as_ref() else {
                    send_response(&out_tx, &seq, &request, false, None, Some("not attached".to_string()));
                    continue;
                };
                match dbg.pause().await {
                    Ok(()) => send_response(&out_tx, &seq, &request, true, None, None),
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
            "disconnect" => {
                let mut guard = debugger.lock().await;
                if let Some(mut dbg) = guard.take() {
                    dbg.disconnect().await;
                }
                send_response(&out_tx, &seq, &request, true, None, None);
                send_event(&out_tx, &seq, "terminated", None);
                break;
            }
            _ => {
                send_response(
                    &out_tx,
                    &seq,
                    &request,
                    true,
                    None,
                    Some(format!("unhandled request {}", request.command)),
                );
            }
        }
    }

    drop(out_tx);
    let _ = writer_task.await;
    Ok(())
}

fn send_event(tx: &mpsc::UnboundedSender<Value>, seq: &Arc<AtomicI64>, event: impl Into<String>, body: Option<Value>) {
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

fn spawn_event_task(debugger: Arc<Mutex<Option<Debugger>>>, tx: mpsc::UnboundedSender<Value>, seq: Arc<AtomicI64>) {
    tokio::spawn(async move {
        let mut events: Option<broadcast::Receiver<nova_jdwp::wire::JdwpEvent>> = None;

        {
            let guard = debugger.lock().await;
            if let Some(dbg) = guard.as_ref() {
                events = Some(dbg.subscribe_events());
            }
        }

        let Some(mut events) = events else {
            return;
        };

        loop {
            let event = match events.recv().await {
                Ok(e) => e,
                Err(broadcast::error::RecvError::Closed) => {
                    send_event(&tx, &seq, "terminated", None);
                    return;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
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
                        Some(json!({"reason": "breakpoint", "threadId": thread as i64, "allThreadsStopped": true})),
                    );
                }
                nova_jdwp::wire::JdwpEvent::SingleStep { thread, .. } => {
                    send_event(
                        &tx,
                        &seq,
                        "stopped",
                        Some(json!({"reason": "step", "threadId": thread as i64, "allThreadsStopped": true})),
                    );
                }
                nova_jdwp::wire::JdwpEvent::Exception { thread, .. } => {
                    send_event(
                        &tx,
                        &seq,
                        "stopped",
                        Some(json!({"reason": "exception", "threadId": thread as i64, "allThreadsStopped": true})),
                    );
                }
                nova_jdwp::wire::JdwpEvent::VmDeath => {
                    send_event(&tx, &seq, "terminated", None);
                    return;
                }
                _ => {}
            }
        }
    });
}


use crate::breakpoints::map_line_breakpoints;
use crate::dap::codec::{read_json_message, write_json_message};
use crate::dap::messages::{Event, Request, Response};
use crate::object_registry::ObjectHandle;
use crate::session::DebugSession;
use crate::smart_step_into::enumerate_step_in_targets_in_line;
use anyhow::Context;
use nova_db::RootDatabase;
use nova_project::{AttachConfig, LaunchConfig};
use nova_jdwp::{JdwpClient, JdwpEvent, TcpJdwpClient};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::panic::AssertUnwindSafe;

struct Outgoing {
    messages: Vec<serde_json::Value>,
    wait_for_stop: bool,
}

pub struct DapServer<C: JdwpClient> {
    next_seq: u64,
    db: RootDatabase,
    session: DebugSession<C>,
    breakpoints: HashMap<PathBuf, Vec<u32>>,
    thread_ids: HashMap<i64, u64>,
    next_thread_id: i64,
    frame_ids: HashMap<i64, u64>,
    frame_threads: HashMap<i64, u64>,
    next_frame_id: i64,
    frame_locations: HashMap<i64, FrameLocation>,
    should_exit: bool,
}

#[derive(Debug, Clone)]
struct FrameLocation {
    source_name: Option<String>,
    line: u32,
}

impl Default for DapServer<TcpJdwpClient> {
    fn default() -> Self {
        Self::new(TcpJdwpClient::new())
    }
}

impl<C: JdwpClient> DapServer<C> {
    pub fn new(jdwp: C) -> Self {
        Self {
            next_seq: 1,
            db: RootDatabase::new(),
            session: DebugSession::new(jdwp),
            breakpoints: HashMap::new(),
            thread_ids: HashMap::new(),
            next_thread_id: 1,
            frame_ids: HashMap::new(),
            frame_threads: HashMap::new(),
            next_frame_id: 1,
            frame_locations: HashMap::new(),
            should_exit: false,
        }
    }

    pub fn run_stdio(mut self) -> anyhow::Result<()> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut reader = BufReader::new(stdin.lock());
        let mut writer = BufWriter::new(stdout.lock());

        while let Some(request) = read_json_message::<_, Request>(&mut reader)? {
            let outgoing = match std::panic::catch_unwind(AssertUnwindSafe(|| self.handle_request(&request)))
            {
                Ok(Ok(outgoing)) => outgoing,
                Ok(Err(err)) => Outgoing {
                    messages: vec![serde_json::to_value(Response::error(
                        self.alloc_seq(),
                        &request,
                        err.to_string(),
                    ))?],
                    wait_for_stop: false,
                },
                Err(_) => {
                    tracing::error!(
                        target = "nova.dap",
                        "panic in DAP request handler; recovering workspace state"
                    );
                    self.recover_after_panic();
                    Outgoing {
                        messages: vec![serde_json::to_value(Response::error(
                            self.alloc_seq(),
                            &request,
                            "Internal error (panic). The adapter will continue in safe-mode."
                                .to_owned(),
                        ))?],
                        wait_for_stop: false,
                    }
                }
            };

            for msg in outgoing.messages {
                write_json_message(&mut writer, &msg)?;
            }
            writer.flush()?;

            if outgoing.wait_for_stop {
                let Some(event) = self.session.jdwp_mut().wait_for_event().ok().flatten() else {
                    continue;
                };

                if let Some(mut messages) = self.jdwp_event_to_dap_messages(event) {
                    for msg in messages.drain(..) {
                        write_json_message(&mut writer, &msg)?;
                    }
                    writer.flush()?;
                }
            }

            if self.should_exit {
                break;
            }
        }

        Ok(())
    }

    fn recover_after_panic(&mut self) {
        // Best-effort recovery: drop all derived state and restart the semantic DB
        // without taking down the whole adapter process.
        self.db = RootDatabase::new();
        self.breakpoints.clear();
        self.thread_ids.clear();
        self.frame_ids.clear();
        self.frame_locations.clear();
        self.next_thread_id = 1;
        self.next_frame_id = 1;
    }

    fn alloc_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    fn handle_request(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        match request.command.as_str() {
            "initialize" => self.initialize(request),
            "launch" => self.launch(request),
            "attach" => self.attach(request),
            "setBreakpoints" => self.set_breakpoints(request),
            "configurationDone" => self.simple_ok(request, None),
            "threads" => self.threads(request),
            "stackTrace" => self.stack_trace(request),
            "stepInTargets" => self.step_in_targets(request),
            "scopes" => self.scopes(request),
            "variables" => self.variables(request),
            "evaluate" => self.evaluate(request),
            "continue" | "next" | "stepIn" | "stepOut" | "pause" => self.execution_control(request),
            "nova/pinObject" => self.pin_object(request),
            "disconnect" => self.disconnect(request),
            _ => Ok(Outgoing {
                messages: vec![serde_json::to_value(Response::error(
                    self.alloc_seq(),
                    request,
                    format!("Unknown command: {}", request.command),
                ))?],
                wait_for_stop: false,
            }),
        }
    }

    fn initialize(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        let capabilities = json!({
            "supportsConfigurationDoneRequest": true,
            "supportsEvaluateForHovers": true,
            "supportsStepInTargetsRequest": true,
            "supportsStepBack": false,
            "supportsDataBreakpoints": false,
            "supportsTerminateRequest": true,
            "supportsRestartRequest": false,
        });

        let response = Response::success(self.alloc_seq(), request, Some(capabilities));
        let initialized_event = Event::new(self.alloc_seq(), "initialized", None);

        Ok(Outgoing {
            messages: vec![serde_json::to_value(response)?, serde_json::to_value(initialized_event)?],
            wait_for_stop: false,
        })
    }

    fn launch(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        let args: LaunchConfig = request
            .arguments
            .clone()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();

        if let Some(port) = args.port {
            let host = args.host.as_deref().unwrap_or("127.0.0.1");
            let _ = self.session.jdwp_mut().connect(host, port);
        }

        self.simple_ok(request, None)
    }

    fn attach(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        let args: AttachConfig = serde_json::from_value(
            request
                .arguments
                .clone()
                .context("attach requires arguments")?,
        )?;

        let host = args.host.as_deref().unwrap_or("127.0.0.1");
        let _ = self.session.jdwp_mut().connect(host, args.port);
        self.simple_ok(request, None)
    }

    fn set_breakpoints(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Source {
            path: Option<String>,
        }

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct SourceBreakpoint {
            line: u32,
        }

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Args {
            source: Source,
            #[serde(default)]
            breakpoints: Vec<SourceBreakpoint>,
        }

        let args: Args = serde_json::from_value(
            request
                .arguments
                .clone()
                .context("setBreakpoints requires arguments")?,
        )?;

        let Some(path) = args.source.path else {
            return self.simple_ok(request, Some(json!({ "breakpoints": [] })));
        };

        let path_buf = PathBuf::from(&path);

        // Load file text. DAP clients typically provide absolute paths.
        let text = std::fs::read_to_string(&path_buf).unwrap_or_default();
        let file_id = self.db.file_id_for_path(&path_buf);
        self.db.set_file_text(file_id, text);

        let requested: Vec<u32> = args.breakpoints.iter().map(|bp| bp.line).collect();
        let resolved = map_line_breakpoints(&self.db, file_id, &requested);

        let mut dap_breakpoints = Vec::new();
        for bp in resolved {
            let mut verified = bp.verified;
            let mut message: Option<String> = None;
            if verified {
                    if let Some(class) = &bp.enclosing_class {
                        if let Err(err) =
                            self.session
                                .jdwp_mut()
                                .set_line_breakpoint(class, bp.enclosing_method.as_deref(), bp.resolved_line)
                        {
                            verified = false;
                            message = Some(err.to_string());
                        }
                } else {
                    verified = false;
                    message = Some("Unable to determine enclosing class for breakpoint".to_string());
                }
            }
            let mut breakpoint = serde_json::Map::new();
            breakpoint.insert("verified".to_string(), json!(verified));
            breakpoint.insert("line".to_string(), json!(bp.resolved_line));
            if let Some(message) = message {
                breakpoint.insert("message".to_string(), json!(message));
            }
            dap_breakpoints.push(serde_json::Value::Object(breakpoint));
        }

        self.breakpoints.insert(path_buf, requested);

        self.simple_ok(request, Some(json!({ "breakpoints": dap_breakpoints })))
    }

    fn threads(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        let threads = match self.session.jdwp_mut().threads() {
            Ok(threads) => {
                let mut dap_threads = Vec::new();
                for thread in threads {
                    let dap_id = self.alloc_thread_id(thread.id);
                    dap_threads.push(json!({
                        "id": dap_id,
                        "name": thread.name,
                    }));
                }
                json!({ "threads": dap_threads })
            }
            Err(_) => json!({
                "threads": [
                    {"id": 1, "name": "Main Thread"}
                ]
            }),
        };

        self.simple_ok(request, Some(threads))
    }

    fn stack_trace(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Args {
            thread_id: i64,
        }

        let args: Args = serde_json::from_value(
            request
                .arguments
                .clone()
                .context("stackTrace requires arguments")?,
        )?;

        let Some(jdwp_thread_id) = self.thread_ids.get(&args.thread_id).copied() else {
            let body = json!({ "stackFrames": [], "totalFrames": 0 });
            return self.simple_ok(request, Some(body));
        };

        let frames = match self.session.jdwp_mut().stack_frames(jdwp_thread_id) {
            Ok(frames) => frames,
            Err(_) => {
                let body = json!({ "stackFrames": [], "totalFrames": 0 });
                return self.simple_ok(request, Some(body));
            }
        };

        let mut dap_frames = Vec::new();
        self.frame_locations.clear();
        self.frame_threads.clear();
        for frame in frames {
            let dap_frame_id = self.alloc_frame_id(frame.id);
            let source = frame.source_path.as_ref().map(|name| json!({ "name": name }));
            dap_frames.push(json!({
                "id": dap_frame_id,
                "name": frame.name,
                "source": source,
                "line": frame.line as i64,
                "column": 1,
            }));
            self.frame_locations.insert(
                dap_frame_id,
                FrameLocation {
                    source_name: frame.source_path.clone(),
                    line: frame.line,
                },
            );
            self.frame_threads.insert(dap_frame_id, jdwp_thread_id);
        }

        let body = json!({
            "stackFrames": dap_frames,
            "totalFrames": dap_frames.len(),
        });
        self.simple_ok(request, Some(body))
    }

    fn step_in_targets(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Args {
            frame_id: i64,
        }

        let args: Args = serde_json::from_value(
            request
                .arguments
                .clone()
                .context("stepInTargets requires arguments")?,
        )?;

        let Some(frame) = self.frame_locations.get(&args.frame_id).cloned() else {
            return self.simple_ok(request, Some(json!({ "targets": [] })));
        };

        let Some(source_name) = frame.source_name else {
            return self.simple_ok(request, Some(json!({ "targets": [] })));
        };

        let source_path = self
            .breakpoints
            .keys()
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name == source_name)
            })
            .cloned()
            .or_else(|| {
                // Some debug targets embed a full source path in the stack trace.
                let path = PathBuf::from(&source_name);
                (path.is_absolute() && path.exists()).then_some(path)
            });

        let Some(source_path) = source_path else {
            return self.simple_ok(request, Some(json!({ "targets": [] })));
        };

        let text = std::fs::read_to_string(&source_path).unwrap_or_default();
        let line_text = frame
            .line
            .checked_sub(1)
            .and_then(|idx| text.lines().nth(idx as usize))
            .unwrap_or("");

        let mut targets = enumerate_step_in_targets_in_line(line_text);
        for target in &mut targets {
            target.line = Some(frame.line);
            target.end_line = Some(frame.line);
        }

        self.simple_ok(request, Some(json!({ "targets": targets })))
    }

    fn scopes(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        let scopes = self.session.scopes(1);
        let body = serde_json::to_value(json!({ "scopes": scopes }))?;
        self.simple_ok(request, Some(body))
    }

    fn variables(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Args {
            variables_reference: i64,
        }

        let args: Args = serde_json::from_value(
            request
                .arguments
                .clone()
                .context("variables requires arguments")?,
        )?;

        let variables = if args.variables_reference == 1 {
            vec![crate::dap::types::Variable {
                name: "<locals>".to_string(),
                value: "<not implemented>".to_string(),
                type_: None,
                variables_reference: 0,
                evaluate_name: None,
                presentation_hint: Some(crate::dap::types::VariablePresentationHint {
                    kind: Some("virtual".to_string()),
                    attributes: Some(vec!["invalid".to_string()]),
                    visibility: None,
                    lazy: None,
                }),
            }]
        } else {
            self.session.variables(args.variables_reference)?
        };

        self.simple_ok(request, Some(json!({ "variables": variables })))
    }

    fn evaluate(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Args {
            expression: String,
            #[serde(default)]
            frame_id: Option<i64>,
        }

        let args: Args = serde_json::from_value(
            request
                .arguments
                .clone()
                .context("evaluate requires arguments")?,
        )?;

        let Some(dap_frame_id) = args.frame_id else {
            return self.simple_ok(
                request,
                Some(json!({
                    "result": "Evaluation requires a frameId",
                    "variablesReference": 0,
                })),
            );
        };

        let Some(jdwp_frame_id) = self.frame_ids.get(&dap_frame_id).copied() else {
            return self.simple_ok(
                request,
                Some(json!({
                    "result": "Unknown frameId",
                    "variablesReference": 0,
                })),
            );
        };

        match self.session.evaluate(jdwp_frame_id, &args.expression) {
            Ok(eval) => self.simple_ok(request, Some(serde_json::to_value(eval)?)),
            Err(err) => self.simple_ok(
                request,
                Some(json!({
                    "result": err.to_string(),
                    "variablesReference": 0,
                })),
            ),
        }
    }

    fn execution_control(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Args {
            #[serde(default)]
            thread_id: Option<i64>,
        }

        let args: Args = request
            .arguments
            .clone()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or(Args { thread_id: None });

        let jdwp_thread_id = args.thread_id.and_then(|id| self.thread_ids.get(&id).copied());

        if let Some(thread_id) = jdwp_thread_id {
            let _ = match request.command.as_str() {
                "continue" => self.session.jdwp_mut().r#continue(thread_id),
                "pause" => self.session.jdwp_mut().pause(thread_id),
                _ => Ok(()),
            };
        }

        // Best-effort: acknowledge the request so the client can continue
        // interacting with the session.
        let body = match request.command.as_str() {
            "continue" => Some(json!({ "allThreadsContinued": true })),
            _ => None,
        };
        let wait_for_stop = request.command == "continue";

        let mut outgoing = self.simple_ok(request, body)?;
        if request.command == "continue" {
            let mut continued_body = serde_json::Map::new();
            continued_body.insert("allThreadsContinued".to_string(), json!(true));
            if let Some(thread_id) = args.thread_id {
                continued_body.insert("threadId".to_string(), json!(thread_id));
            }

            outgoing.messages.push(serde_json::to_value(Event::new(
                self.alloc_seq(),
                "continued",
                Some(serde_json::Value::Object(continued_body)),
            ))?);
        }

        if request.command == "pause" {
            let mut stopped_body = serde_json::Map::new();
            stopped_body.insert("reason".to_string(), json!("pause"));
            stopped_body.insert("allThreadsStopped".to_string(), json!(true));
            if let Some(thread_id) = args.thread_id {
                stopped_body.insert("threadId".to_string(), json!(thread_id));
            }

            outgoing.messages.push(serde_json::to_value(Event::new(
                self.alloc_seq(),
                "stopped",
                Some(serde_json::Value::Object(stopped_body)),
            ))?);
        }

        if matches!(request.command.as_str(), "next" | "stepIn" | "stepOut") {
            let Some(thread_id) = jdwp_thread_id else {
                outgoing.wait_for_stop = false;
                return Ok(outgoing);
            };

            let step = match request.command.as_str() {
                "next" => self.session.step_over(thread_id)?,
                "stepIn" => self.session.step_in(thread_id)?,
                "stepOut" => self.session.step_out(thread_id)?,
                _ => unreachable!(),
            };

            for output in step.output {
                outgoing.messages.push(serde_json::to_value(Event::new(
                    self.alloc_seq(),
                    "output",
                    Some(serde_json::to_value(output)?),
                ))?);
            }

            if let Some(stopped) = self.stopped_event_to_dap(step.stopped) {
                outgoing.messages.push(stopped);
            }

            outgoing.wait_for_stop = false;
            return Ok(outgoing);
        }

        outgoing.wait_for_stop = wait_for_stop;
        Ok(outgoing)
    }

    fn pin_object(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Args {
            variables_reference: i64,
            #[serde(default)]
            pinned: bool,
        }

        let args: Args = serde_json::from_value(
            request
                .arguments
                .clone()
                .context("pinObject requires arguments")?,
        )?;

        let Some(handle) = ObjectHandle::from_variables_reference(args.variables_reference) else {
            return self.simple_ok(request, Some(json!({ "pinned": false })));
        };

        if args.pinned {
            let _ = self.session.pin_object(handle);
        } else {
            let _ = self.session.unpin_object(handle);
        }

        self.simple_ok(request, Some(json!({ "pinned": args.pinned })))
    }

    fn disconnect(&mut self, request: &Request) -> anyhow::Result<Outgoing> {
        self.should_exit = true;
        let mut outgoing = self.simple_ok(request, None)?;
        outgoing.messages.push(serde_json::to_value(Event::new(
            self.alloc_seq(),
            "terminated",
            None,
        ))?);
        Ok(outgoing)
    }

    fn simple_ok(&mut self, request: &Request, body: Option<serde_json::Value>) -> anyhow::Result<Outgoing> {
        let response = Response::success(self.alloc_seq(), request, body);
        Ok(Outgoing {
            messages: vec![serde_json::to_value(response)?],
            wait_for_stop: false,
        })
    }

    fn alloc_thread_id(&mut self, jdwp_thread_id: u64) -> i64 {
        if let Some(existing) = self
            .thread_ids
            .iter()
            .find_map(|(dap, jdwp)| (*jdwp == jdwp_thread_id).then_some(*dap))
        {
            return existing;
        }

        let id = self.next_thread_id;
        self.next_thread_id = self.next_thread_id.saturating_add(1);
        self.thread_ids.insert(id, jdwp_thread_id);
        id
    }

    fn alloc_frame_id(&mut self, jdwp_frame_id: u64) -> i64 {
        if let Some(existing) = self
            .frame_ids
            .iter()
            .find_map(|(dap, jdwp)| (*jdwp == jdwp_frame_id).then_some(*dap))
        {
            return existing;
        }

        let id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.saturating_add(1);
        self.frame_ids.insert(id, jdwp_frame_id);
        id
    }

    fn jdwp_event_to_dap_messages(&mut self, event: JdwpEvent) -> Option<Vec<serde_json::Value>> {
        match event {
            JdwpEvent::Stopped(stopped) => {
                let mut messages = Vec::new();

                if let Some(return_value) = &stopped.return_value {
                    if let Ok(formatted) = self.session.format_value(return_value) {
                        messages.push(
                            serde_json::to_value(Event::new(
                                self.alloc_seq(),
                                "output",
                                Some(serde_json::to_value(crate::dap::types::OutputEvent {
                                    category: Some("console".to_string()),
                                    output: format!("Return value: {formatted}\n"),
                                })
                                .ok()?),
                            ))
                            .ok()?,
                        );
                    }
                }

                if let Some(expr_value) = &stopped.expression_value {
                    if let Ok(formatted) = self.session.format_value(expr_value) {
                        messages.push(
                            serde_json::to_value(Event::new(
                                self.alloc_seq(),
                                "output",
                                Some(serde_json::to_value(crate::dap::types::OutputEvent {
                                    category: Some("console".to_string()),
                                    output: format!("Expression value: {formatted}\n"),
                                })
                                .ok()?),
                            ))
                            .ok()?,
                        );
                    }
                }

                if let Some(stopped) = self.stopped_event_to_dap(stopped) {
                    messages.push(stopped);
                }

                Some(messages)
            }
        }
    }

    fn stopped_event_to_dap(&mut self, stopped: nova_jdwp::StoppedEvent) -> Option<serde_json::Value> {
        let dap_thread_id = self.alloc_thread_id(stopped.thread_id);
        serde_json::to_value(Event::new(
            self.alloc_seq(),
            "stopped",
            Some(json!({
                "reason": stopped.reason.as_dap_reason(),
                "threadId": dap_thread_id,
                // Breakpoints/steps are configured with `SuspendPolicy.EVENT_THREAD`,
                // so only the event thread is stopped.
                "allThreadsStopped": false,
            })),
        ))
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_jdwp::{JdwpError, StackFrameInfo, ThreadInfo};
    use serde_json::Value;
    use tempfile::TempDir;

    #[derive(Default)]
    struct MockJdwp {
        threads: Vec<ThreadInfo>,
        frames: HashMap<u64, Vec<StackFrameInfo>>,
    }

    impl JdwpClient for MockJdwp {
        fn connect(&mut self, _host: &str, _port: u16) -> Result<(), JdwpError> {
            Ok(())
        }

        fn set_line_breakpoint(
            &mut self,
            _class: &str,
            _method: Option<&str>,
            _line: u32,
        ) -> Result<(), JdwpError> {
            Ok(())
        }

        fn threads(&mut self) -> Result<Vec<ThreadInfo>, JdwpError> {
            Ok(self.threads.clone())
        }

        fn stack_frames(&mut self, thread_id: u64) -> Result<Vec<StackFrameInfo>, JdwpError> {
            Ok(self.frames.get(&thread_id).cloned().unwrap_or_default())
        }

        fn r#continue(&mut self, _thread_id: u64) -> Result<(), JdwpError> {
            Ok(())
        }

        fn next(&mut self, _thread_id: u64) -> Result<(), JdwpError> {
            Ok(())
        }

        fn step_in(&mut self, _thread_id: u64) -> Result<(), JdwpError> {
            Ok(())
        }

        fn step_out(&mut self, _thread_id: u64) -> Result<(), JdwpError> {
            Ok(())
        }

        fn pause(&mut self, _thread_id: u64) -> Result<(), JdwpError> {
            Ok(())
        }
    }

    fn response_body(message: &Value) -> Option<&Value> {
        message.get("body")
    }

    fn event_name(message: &Value) -> Option<&str> {
        message.get("event").and_then(|value| value.as_str())
    }

    #[test]
    fn object_handles_do_not_collide_with_locals_scope_reference() {
        use nova_jdwp::{
            JdwpValue, JdwpVariable, MockJdwpClient, MockObject, ObjectKindPreview, ObjectPreview,
            ObjectRef,
        };

        let mut jdwp = MockJdwpClient::new();
        jdwp.set_threads(vec![ThreadInfo {
            id: 99,
            name: "main".into(),
        }]);
        jdwp.set_stack_frames(
            99,
            vec![StackFrameInfo {
                id: 123,
                name: "m".into(),
                source_path: Some("Main.java".into()),
                line: 1,
            }],
        );

        jdwp.insert_object(
            100,
            MockObject {
                preview: ObjectPreview {
                    runtime_type: "com.example.Foo".to_string(),
                    kind: ObjectKindPreview::Plain,
                },
                children: vec![JdwpVariable {
                    name: "x".to_string(),
                    value: JdwpValue::Int(1),
                    static_type: Some("int".to_string()),
                    evaluate_name: Some("foo.x".to_string()),
                }],
            },
        );
        jdwp.set_evaluation(
            123,
            "foo",
            Ok(JdwpValue::Object(ObjectRef {
                id: 100,
                runtime_type: "com.example.Foo".to_string(),
            })),
        );

        let mut server = DapServer::new(jdwp);

        let threads_req = Request {
            seq: 1,
            type_: "request".into(),
            command: "threads".into(),
            arguments: None,
        };
        let threads_resp = server.handle_request(&threads_req).unwrap();
        let threads = response_body(&threads_resp.messages[0]).unwrap()["threads"]
            .as_array()
            .unwrap();
        let dap_thread_id = threads[0]["id"].as_i64().unwrap();

        let stack_req = Request {
            seq: 2,
            type_: "request".into(),
            command: "stackTrace".into(),
            arguments: Some(json!({ "threadId": dap_thread_id })),
        };
        let stack_resp = server.handle_request(&stack_req).unwrap();
        let frames = response_body(&stack_resp.messages[0]).unwrap()["stackFrames"]
            .as_array()
            .unwrap();
        let dap_frame_id = frames[0]["id"].as_i64().unwrap();

        let eval_req = Request {
            seq: 3,
            type_: "request".into(),
            command: "evaluate".into(),
            arguments: Some(json!({
                "expression": "foo",
                "frameId": dap_frame_id,
            })),
        };
        let eval_resp = server.handle_request(&eval_req).unwrap();
        let vars_ref = response_body(&eval_resp.messages[0]).unwrap()["variablesReference"]
            .as_i64()
            .unwrap();

        // Scope locals uses 1; object handles should never collide with it.
        assert!(vars_ref > crate::object_registry::OBJECT_HANDLE_BASE);

        let vars_req = Request {
            seq: 4,
            type_: "request".into(),
            command: "variables".into(),
            arguments: Some(json!({ "variablesReference": vars_ref })),
        };
        let vars_resp = server.handle_request(&vars_req).unwrap();
        let variables = response_body(&vars_resp.messages[0]).unwrap()["variables"]
            .as_array()
            .unwrap();
        assert_eq!(variables.len(), 1);
        assert_eq!(variables[0]["name"], "x");
        assert_eq!(variables[0]["value"], "1");
        assert_eq!(variables[0]["evaluateName"], "foo.x");
    }

    #[test]
    fn step_in_targets_returns_calls_for_current_frame_line() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let file_path = root.join("Main.java");
        std::fs::write(
            &file_path,
            r#"package com.example;

public class Main {
  void m() {
    foo(bar(), baz(qux()));
  }
}
"#,
        )
        .unwrap();

        let mut jdwp = MockJdwp::default();
        jdwp.threads.push(ThreadInfo {
            id: 99,
            name: "main".into(),
        });
        jdwp.frames.insert(
            99,
            vec![StackFrameInfo {
                id: 123,
                name: "m".into(),
                source_path: Some("Main.java".into()),
                line: 5,
            }],
        );

        let mut server = DapServer::new(jdwp);

        // Seed the source path via setBreakpoints so stepInTargets can resolve the
        // source file name from the stack trace back to an absolute path.
        let set_bps = Request {
            seq: 1,
            type_: "request".into(),
            command: "setBreakpoints".into(),
            arguments: Some(json!({
                "source": { "path": file_path.to_string_lossy() },
                "breakpoints": [],
            })),
        };
        server.handle_request(&set_bps).unwrap();

        let threads_req = Request {
            seq: 2,
            type_: "request".into(),
            command: "threads".into(),
            arguments: None,
        };
        let threads_resp = server.handle_request(&threads_req).unwrap();
        let threads = response_body(&threads_resp.messages[0]).unwrap()["threads"]
            .as_array()
            .unwrap();
        let dap_thread_id = threads[0]["id"].as_i64().unwrap();

        let stack_req = Request {
            seq: 3,
            type_: "request".into(),
            command: "stackTrace".into(),
            arguments: Some(json!({ "threadId": dap_thread_id })),
        };
        let stack_resp = server.handle_request(&stack_req).unwrap();
        let frames = response_body(&stack_resp.messages[0]).unwrap()["stackFrames"]
            .as_array()
            .unwrap();
        let dap_frame_id = frames[0]["id"].as_i64().unwrap();

        let targets_req = Request {
            seq: 4,
            type_: "request".into(),
            command: "stepInTargets".into(),
            arguments: Some(json!({ "frameId": dap_frame_id })),
        };
        let targets_resp = server.handle_request(&targets_req).unwrap();
        let targets = response_body(&targets_resp.messages[0]).unwrap()["targets"]
            .as_array()
            .unwrap();

        let labels: Vec<_> = targets
            .iter()
            .map(|t| t["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, vec!["bar()", "qux()", "baz()", "foo()"]);
    }

    #[test]
    fn disconnect_emits_terminated_event() {
        let server = DapServer::new(MockJdwp::default());
        let mut server = server;

        let disconnect = Request {
            seq: 1,
            type_: "request".into(),
            command: "disconnect".into(),
            arguments: Some(json!({})),
        };

        let outgoing = server.handle_request(&disconnect).unwrap();
        assert!(server.should_exit);
        assert_eq!(outgoing.messages.len(), 2);
        assert_eq!(outgoing.messages[0]["type"], "response");
        assert_eq!(event_name(&outgoing.messages[1]).unwrap(), "terminated");
    }

    #[test]
    fn continue_emits_continued_event() {
        let mut jdwp = MockJdwp::default();
        jdwp.threads.push(ThreadInfo {
            id: 42,
            name: "main".into(),
        });

        let mut server = DapServer::new(jdwp);

        let threads_req = Request {
            seq: 1,
            type_: "request".into(),
            command: "threads".into(),
            arguments: None,
        };
        let threads_resp = server.handle_request(&threads_req).unwrap();
        let threads = response_body(&threads_resp.messages[0]).unwrap()["threads"]
            .as_array()
            .unwrap();
        let dap_thread_id = threads[0]["id"].as_i64().unwrap();

        let continue_req = Request {
            seq: 2,
            type_: "request".into(),
            command: "continue".into(),
            arguments: Some(json!({ "threadId": dap_thread_id })),
        };
        let outgoing = server.handle_request(&continue_req).unwrap();
        assert!(outgoing.wait_for_stop);
        assert_eq!(outgoing.messages.len(), 2);
        assert_eq!(outgoing.messages[0]["type"], "response");
        assert_eq!(event_name(&outgoing.messages[1]).unwrap(), "continued");
    }
}

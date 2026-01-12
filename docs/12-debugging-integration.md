# 12 - Debugging Integration

[← Back to Main Document](../AGENTS.md) | [Previous: Editor Integration](11-editor-integration.md)

## Overview

Debugging is essential for Java development. Nova integrates with the Debug Adapter Protocol (DAP) to provide debugging capabilities that match or exceed IntelliJ's debugger.

**Implementation note:** Protocol stack decisions (including DAP transport framing and cancellation strategy) are tracked in [ADR 0003](adr/0003-protocol-frameworks-lsp-dap.md).

---

## Debug Adapter Protocol

### DAP Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    DAP ARCHITECTURE                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌─────────────┐       ┌─────────────┐       ┌─────────────┐    │
│  │   Editor    │ ◄───► │    Nova     │ ◄───► │    JVM      │    │
│  │  (VS Code)  │  DAP  │  Debug      │  JDWP │  Debuggee   │    │
│  │             │       │  Adapter    │       │             │    │
│  └─────────────┘       └─────────────┘       └─────────────┘    │
│                                                                  │
│  DAP: JSON-based protocol between editor and debug adapter      │
│  JDWP: Java Debug Wire Protocol between adapter and JVM         │
│                                                                  │
│  NOVA'S ROLE:                                                   │
│  • Translate DAP requests to JDWP                               │
│  • Manage debug sessions                                        │
│  • Provide enhanced features using semantic knowledge           │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### `nova-dap` adapter modes and transport

`nova-dap` defaults to the **wire** (JDWP-backed) adapter. The older `--legacy`
flag runs the previous synchronous/skeleton adapter.

The wire adapter serves DAP over **stdio** by default. For tooling/tests, it can
also listen on TCP with `--listen <addr>` (single incoming connection):

- `nova-dap --listen 127.0.0.1:4711` (fixed port)
- `nova-dap --listen 127.0.0.1:0` (port `0` = ask the OS to pick a free port)

Note: `--listen` expects a full `host:port` socket address (for example `127.0.0.1:0`, not just `:0`).

When `--listen` is used, `nova-dap` prints the bound address to stderr (for
example: `listening on 127.0.0.1:4711`), which is useful for tools that bind to
port `0` and need to discover the chosen port.

`--listen` is only supported for the default wire adapter; `--legacy --listen`
is rejected.

### DAP Capabilities

```
┌─────────────────────────────────────────────────────────────────┐
│                    DAP FEATURES                                  │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  LAUNCH/ATTACH                                                  │
│  ✓ Launch new JVM with debugging                                │
│  ✓ Attach to running JVM                                        │
│  ✓ Remote debugging                                             │
│  ✓ Hot code replacement                                         │
│                                                                  │
│  BREAKPOINTS                                                    │
│  ✓ Line breakpoints                                             │
│  ✓ Conditional breakpoints                                      │
│  ✓ Exception breakpoints                                        │
│  ✓ Function breakpoints                                         │
│  ✓ Data breakpoints (watchpoints)                               │
│  ✓ Logpoints                                                    │
│                                                                  │
│  CONTROL                                                        │
│  ✓ Continue / Pause                                             │
│  ✓ Step over / into / out                                       │
│  ✗ Step back                                                    │
│  ✓ Restart                                                      │
│  ✓ Terminate                                                    │
│                                                                  │
│  INSPECTION                                                     │
│  ✓ Stack frames                                                 │
│  ✓ Scopes (local, closure, global)                              │
│  ✓ Variables                                                    │
│  ✓ Watch expressions                                            │
│  ✓ Evaluate expression                                          │
│                                                                  │
│  ENHANCED (NOVA-SPECIFIC)                                       │
│  ✓ Smart step into (choose which method)                        │
│  ✓ Return value display                                         │
│  ✓ Object ID tracking                                           │
│  ✓ Stream debugger                                              │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Implementation

### Debug Adapter

```rust
pub struct NovaDebugAdapter {
    /// Connection to DAP client
    connection: DebugConnection,
    
    /// Active debug sessions
    sessions: HashMap<SessionId, DebugSession>,
    
    /// Reference to main Nova database
    db: Arc<RwLock<Database>>,
}

pub struct DebugSession {
    /// JVM connection
    jvm: JdwpConnection,
    
    /// Breakpoint mapping
    breakpoints: BreakpointManager,
    
    /// Thread states
    threads: HashMap<ThreadId, ThreadState>,
    
    /// Configuration
    config: LaunchConfig,
}

impl NovaDebugAdapter {
    pub async fn handle_request(&mut self, request: Request) -> Result<Response> {
        match request.command.as_str() {
            "initialize" => self.initialize(request).await,
            "launch" => self.launch(request).await,
            "attach" => self.attach(request).await,
            "setBreakpoints" => self.set_breakpoints(request).await,
            "configurationDone" => self.configuration_done(request).await,
            "threads" => self.threads(request).await,
            "stackTrace" => self.stack_trace(request).await,
            "scopes" => self.scopes(request).await,
            "variables" => self.variables(request).await,
            "evaluate" => self.evaluate(request).await,
            "continue" => self.continue_(request).await,
            "next" => self.next(request).await,
            "stepIn" => self.step_in(request).await,
            "stepOut" => self.step_out(request).await,
            "pause" => self.pause(request).await,
            "disconnect" => self.disconnect(request).await,
            _ => Err(DebugError::UnknownCommand),
        }
    }
}
```

### Launch Configuration

```rust
impl NovaDebugAdapter {
    async fn launch(&mut self, request: Request) -> Result<Response> {
        let args: LaunchArguments = serde_json::from_value(request.arguments)?;
        
        // Build command line
        let java_cmd = self.find_java(&args)?;
        let classpath = self.build_classpath(&args)?;
        let debug_port = self.allocate_port();
        
        let mut cmd = Command::new(java_cmd);
        cmd.arg(format!("-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address={}", debug_port))
           .arg("-cp").arg(&classpath)
           .args(&args.vm_args);
        
        if let Some(module) = &args.module_name {
            cmd.arg("-m").arg(format!("{}/{}", module, args.main_class));
        } else {
            cmd.arg(&args.main_class);
        }
        
        cmd.args(&args.args);
        
        // Start process
        let process = cmd.spawn()?;
        
        // Connect debugger
        let jvm = self.connect_jdwp("localhost", debug_port).await?;
        
        // Create session
        let session = DebugSession {
            jvm,
            breakpoints: BreakpointManager::new(),
            threads: HashMap::new(),
            config: LaunchConfig::from_args(args),
        };
        
        let session_id = self.register_session(session);
        
        Ok(Response::ok(request.seq, json!({})))
    }
}
```

### Breakpoint Management

```rust
pub struct BreakpointManager {
    /// Source breakpoints by file
    source_breakpoints: HashMap<PathBuf, Vec<SourceBreakpoint>>,
    
    /// JDWP breakpoint IDs
    jdwp_breakpoints: HashMap<BreakpointId, JdwpBreakpointId>,
    
    /// Exception breakpoints
    exception_breakpoints: Vec<ExceptionBreakpoint>,
}

impl NovaDebugAdapter {
    async fn set_breakpoints(&mut self, request: Request) -> Result<Response> {
        let args: SetBreakpointsArguments = serde_json::from_value(request.arguments)?;
        let session = self.get_session()?;
        
        let source_path = PathBuf::from(&args.source.path.unwrap());
        
        // Clear existing breakpoints for this file
        session.breakpoints.clear_file(&source_path);
        
        let mut results = Vec::new();
        
        for bp in args.breakpoints.unwrap_or_default() {
            // Resolve line to actual location using Nova's semantic info
            let location = self.resolve_breakpoint_location(&source_path, bp.line)?;
            
            // Set breakpoint in JVM
            let jdwp_bp = if let Some(condition) = &bp.condition {
                session.jvm.set_conditional_breakpoint(&location, condition).await?
            } else {
                session.jvm.set_breakpoint(&location).await?
            };
            
            // Track mapping
            let bp_id = session.breakpoints.add(source_path.clone(), bp.clone(), jdwp_bp);
            
            results.push(Breakpoint {
                id: Some(bp_id.0 as i64),
                verified: true,
                line: Some(location.line as i64),
                ..Default::default()
            });
        }
        
        Ok(Response::ok(request.seq, json!({
            "breakpoints": results
        })))
    }
    
    fn resolve_breakpoint_location(
        &self,
        path: &Path,
        line: i64,
    ) -> Result<BreakpointLocation> {
        let db = self.db.read();
        let file = db.file_for_path(path)?;
        
        // Use Nova's semantic knowledge to find valid breakpoint location
        // (e.g., skip empty lines, comments, adjust to statement start)
        let valid_line = db.nearest_breakpoint_location(file, line as u32);
        
        // Get class and method info
        let (class, method) = db.enclosing_method_at(file, valid_line)?;
        let class_name = db.qualified_name(class);
        let method_name = db.method_name(method);
        let method_sig = db.method_signature(method);
        
        Ok(BreakpointLocation {
            class_name,
            method_name,
            method_signature: method_sig,
            line: valid_line,
        })
    }
}
```

### Expression Evaluation

```rust
impl NovaDebugAdapter {
    async fn evaluate(&mut self, request: Request) -> Result<Response> {
        let args: EvaluateArguments = serde_json::from_value(request.arguments)?;
        let session = self.get_session()?;
        
        let frame_id = args.frame_id.ok_or(DebugError::NoFrame)?;
        let expression = &args.expression;
        
        // Parse expression using Nova
        let parsed = self.parse_expression(expression)?;
        
        // Evaluate in JVM context
        let result = match args.context.as_deref() {
            Some("watch") | Some("hover") => {
                // Read-only evaluation
                session.jvm.evaluate_readonly(frame_id, &parsed).await?
            }
            Some("repl") => {
                // Allow side effects
                session.jvm.evaluate(frame_id, &parsed).await?
            }
            _ => {
                session.jvm.evaluate_readonly(frame_id, &parsed).await?
            }
        };
        
        // Format result using Nova's type information
        let formatted = self.format_value(&result)?;
        
        Ok(Response::ok(request.seq, json!({
            "result": formatted.display,
            "type": formatted.type_name,
            "variablesReference": formatted.children_ref,
        })))
    }
    
    fn format_value(&self, value: &JdwpValue) -> FormattedValue {
        match value {
            JdwpValue::Object { id, type_name } => {
                // Use Nova's knowledge for better formatting
                if type_name.starts_with("java.util.") {
                    self.format_collection(value)
                } else if type_name == "java.lang.String" {
                    self.format_string(value)
                } else {
                    self.format_object(value)
                }
            }
            JdwpValue::Primitive { .. } => {
                self.format_primitive(value)
            }
            JdwpValue::Array { .. } => {
                self.format_array(value)
            }
            JdwpValue::Null => FormattedValue {
                display: "null".into(),
                type_name: None,
                children_ref: 0,
            },
        }
    }
}
```

---

## Enhanced Features

### Smart Step Into

```rust
impl NovaDebugAdapter {
    /// Let user choose which method to step into when line has multiple calls
    async fn smart_step_into(&mut self, request: Request) -> Result<Response> {
        let args: SmartStepIntoArguments = serde_json::from_value(request.arguments)?;
        let session = self.get_session()?;
        
        // Get current location
        let frame = session.threads.get(&args.thread_id)
            .and_then(|t| t.stack_frames.first())
            .ok_or(DebugError::NoFrame)?;
        
        // Use Nova to find method calls on current line
        let db = self.db.read();
        let file = db.file_for_class(&frame.class_name)?;
        let calls = db.method_calls_at_line(file, frame.line);
        
        if calls.len() <= 1 {
            // Single call or no call, do regular step into
            return self.step_in(request).await;
        }
        
        // Return choices to user
        let targets: Vec<_> = calls.iter()
            .map(|call| {
                let method = db.resolve_method_call(*call);
                StepIntoTarget {
                    id: call.0 as i64,
                    label: format_method_call(&method),
                }
            })
            .collect();
        
        Ok(Response::ok(request.seq, json!({
            "targets": targets
        })))
    }
}
```

### Stream Debugger

**Implementation note:** The wire adapter’s `nova/streamDebug` execution strategy (current MVP behavior, constraints, and the compile+inject roadmap) is tracked in [ADR 0013](adr/0013-stream-debug-evaluation-strategy.md).

```rust
impl NovaDebugAdapter {
    /// Debug Java Streams step by step
    async fn debug_stream(&mut self, request: Request) -> Result<Response> {
        let args: DebugStreamArguments = serde_json::from_value(request.arguments)?;
        let session = self.get_session()?;
        
        // Parse stream expression
        let db = self.db.read();
        let stream_expr = db.parse_expression(&args.expression)?;
        
        // Identify stream operations
        let operations = db.analyze_stream_chain(&stream_expr)?;
        
        // Execute each operation and collect intermediate results
        let mut results = Vec::new();
        let mut current_value = session.jvm.evaluate(args.frame_id, &operations.source).await?;
        
        for op in &operations.operations {
            results.push(StreamStep {
                operation: op.name.clone(),
                input: self.format_stream_data(&current_value),
            });
            
            // Execute operation
            current_value = session.jvm.evaluate_stream_step(args.frame_id, &current_value, op).await?;
            
            results.last_mut().unwrap().output = self.format_stream_data(&current_value);
        }
        
        Ok(Response::ok(request.seq, json!({
            "steps": results
        })))
    }
}
```

### Hot Code Replacement

Hot code replacement operates at the **JVM class** level. Note that a single `.java` source file can
compile to **multiple** `.class` files (e.g. nested/inner classes like `Outer$Inner.class` and
anonymous classes like `Outer$1.class`). When a source file changes, Nova should attempt to redefine
all compiled classes that are currently loaded in the target VM; classes that are not loaded can be
skipped.

```rust
impl NovaDebugAdapter {
    /// Replace class bytecode while debugging
    async fn hot_code_replace(&mut self, changed_files: Vec<PathBuf>) -> Result<HotSwapResult> {
        let session = self.get_session()?;
        
        let mut results = Vec::new();
        
        for file in changed_files {
            // Compile the changed file
            let compile_result = self.compile_file(&file).await?;
            
            if !compile_result.success {
                results.push(HotSwapFileResult {
                    file: file.clone(),
                    status: HotSwapStatus::CompileError,
                    error: compile_result.error,
                });
                continue;
            }
            
            // Get class bytes
            let class_name = self.class_name_for_file(&file)?;
            let class_bytes = compile_result.class_bytes;
            
            // Check if class can be hot-swapped
            let can_swap = session.jvm.can_redefine_class(&class_name).await?;
            
            if !can_swap {
                results.push(HotSwapFileResult {
                    file: file.clone(),
                    status: HotSwapStatus::SchemaChange,
                    error: Some("Class structure changed. Restart required.".into()),
                });
                continue;
            }
            
            // Perform hot swap
            session.jvm.redefine_class(&class_name, &class_bytes).await?;
            
            results.push(HotSwapFileResult {
                file,
                status: HotSwapStatus::Success,
                error: None,
            });
        }
        
        // Notify client
        self.send_event(Event::HotSwapCompleted { results: results.clone() });
        
        Ok(HotSwapResult { results })
    }
}
```

---

## Debug Configuration Discovery

```rust
impl NovaDebugAdapter {
    /// Discover runnable/debuggable configurations
    pub fn discover_configurations(&self) -> Vec<DebugConfiguration> {
        let db = self.db.read();
        let mut configs = Vec::new();
        
        // Find main classes
        for main_class in db.find_main_classes() {
            configs.push(DebugConfiguration {
                name: format!("Run {}", main_class.simple_name),
                config_type: "java",
                request: "launch",
                main_class: main_class.qualified_name,
                project_name: main_class.project.clone(),
                ..Default::default()
            });
        }
        
        // Find test classes
        for test_class in db.find_test_classes() {
            configs.push(DebugConfiguration {
                name: format!("Debug Tests: {}", test_class.simple_name),
                config_type: "java",
                request: "launch",
                main_class: "org.junit.platform.console.ConsoleLauncher".into(),
                args: vec![
                    "--select-class".into(),
                    test_class.qualified_name,
                ],
                ..Default::default()
            });
        }
        
        // Find Spring Boot applications
        for spring_app in db.find_spring_boot_apps() {
            configs.push(DebugConfiguration {
                name: format!("Spring Boot: {}", spring_app.simple_name),
                config_type: "java",
                request: "launch",
                main_class: spring_app.qualified_name,
                spring_boot: true,
                ..Default::default()
            });
        }
        
        configs
    }
}
```

---

## Integration with Nova Semantic Analysis

```rust
impl NovaDebugAdapter {
    /// Use Nova's semantic info to enhance debugging
    fn enhance_stack_frame(&self, jdwp_frame: JdwpStackFrame) -> StackFrame {
        let db = self.db.read();
        
        // Map to source location using Nova
        let source_location = db.source_location_for_bytecode(
            &jdwp_frame.class_name,
            &jdwp_frame.method_name,
            jdwp_frame.bytecode_index,
        );
        
        // Get variable names from debug info or Nova's analysis
        let local_names = db.local_variable_names_at(
            source_location.file,
            source_location.line,
        );
        
        StackFrame {
            id: jdwp_frame.id,
            name: format_method_name(&jdwp_frame),
            source: Some(Source {
                path: Some(source_location.file.to_string()),
                ..Default::default()
            }),
            line: source_location.line as i64,
            column: source_location.column as i64,
            // Enhanced with Nova's info
            presentation_hint: if jdwp_frame.is_synthetic {
                Some("subtle".into())
            } else {
                None
            },
        }
    }
    
    /// Show inferred types in variables view
    fn enhance_variable(&self, var: JdwpVariable, frame: &StackFrame) -> Variable {
        let db = self.db.read();
        
        // Get Nova's type info (may be more specific than runtime type)
        let static_type = db.variable_type_at(frame.source_path(), frame.line, &var.name);
        
        Variable {
            name: var.name,
            value: self.format_value(&var.value).display,
            type_: static_type.map(|t| format_type(&t)),
            variables_reference: if var.has_children { var.id as i64 } else { 0 },
            // Show additional Nova-derived info
            evaluate_name: Some(var.name.clone()),
        }
    }
}
```

---

## Next Steps

1. → [AI Augmentation](13-ai-augmentation.md): ML-powered features
2. → [Testing Strategy](14-testing-strategy.md): Quality assurance
3. → [Testing Infrastructure](14-testing-infrastructure.md): How to run tests/CI and update fixtures

---

[← Previous: Editor Integration](11-editor-integration.md) | [Next: AI Augmentation →](13-ai-augmentation.md)

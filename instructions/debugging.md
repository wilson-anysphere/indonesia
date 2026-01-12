# Debugging Workstream

> **⚠️ MANDATORY: Read and follow [AGENTS.md](../AGENTS.md) completely before proceeding.**
> **All rules in AGENTS.md apply at all times. This file adds workstream-specific guidance.**

---

## Scope

This workstream owns the Debug Adapter Protocol implementation and JVM debugging:

| Crate | Purpose |
|-------|---------|
| `nova-dap` | Debug Adapter Protocol server |
| `nova-jdwp` | Java Debug Wire Protocol client |
| `nova-stream-debug` | Stream debugging (Java Streams API) |

## `nova-dap` CLI quick reference

- **Default:** wire (JDWP-backed) adapter, serving DAP over **stdio**
- **`--legacy`:** older synchronous/skeleton adapter (stdio only)
- **`--listen <addr>`:** serve DAP over **TCP** instead of stdio (wire adapter only)
   - Example: `--listen 127.0.0.1:4711`
- Use port `0` to auto-pick a free port: `--listen 127.0.0.1:0`
  - Note: `--listen` expects a full `host:port` socket address (for example `127.0.0.1:0`, not just `:0`)
- **`--config <path>` / `NOVA_CONFIG=<path>`:** load a `NovaConfig` TOML file (optional)

---

## Key Documents

**Required reading:**
- [12 - Debugging Integration](../docs/12-debugging-integration.md) - Architecture and features

**ADRs:**
- [ADR-0003: Protocol Frameworks](../docs/adr/0003-protocol-frameworks-lsp-dap.md)

---

## Architecture

### Debug Adapter

```
┌─────────────────────────────────────────────────────────────────┐
│                    Debug Architecture                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Editor ←── DAP (framed JSON messages) ──→ nova-dap             │
│                                │                                 │
│                                │ JDWP                           │
│                                ▼                                 │
│                            JVM (debuggee)                        │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

### JDWP Client

Nova communicates with the JVM via JDWP (Java Debug Wire Protocol):

```rust
pub struct JdwpClient {
    stream: TcpStream,
    // ...
}

impl JdwpClient {
    async fn connect(address: &str) -> Result<Self>;
    async fn set_breakpoint(&mut self, location: Location) -> Result<BreakpointId>;
    async fn resume(&mut self) -> Result<()>;
    async fn get_stack_frames(&mut self, thread: ThreadId) -> Result<Vec<StackFrame>>;
    async fn get_variables(&mut self, frame: FrameId) -> Result<Vec<Variable>>;
}
```

---

## DAP Protocol

### Lifecycle

```
initialize → launch/attach → setBreakpoints → configurationDone
    ↓
[running] ←→ [stopped]
    ↓
disconnect/terminate
```

### Key Requests

| Request | Description |
|---------|-------------|
| `initialize` | Negotiate capabilities |
| `launch` | Start debuggee process |
| `attach` | Attach to running JVM |
| `setBreakpoints` | Set breakpoints for a file |
| `configurationDone` | Signal configuration complete |
| `continue` | Resume execution |
| `next` | Step over |
| `stepIn` | Step into |
| `stepOut` | Step out |
| `stackTrace` | Get stack frames |
| `scopes` | Get variable scopes |
| `variables` | Get variables |
| `evaluate` | Evaluate expression |
| `disconnect` | End debug session |

### Events

| Event | Description |
|-------|-------------|
| `stopped` | Execution stopped (breakpoint, step, exception) |
| `continued` | Execution resumed |
| `exited` | Debuggee exited |
| `terminated` | Debug session ended |
| `output` | Console output |
| `breakpoint` | Breakpoint changed |

---

## Development Guidelines

### Implementing DAP Handlers

```rust
impl DapServer {
    async fn launch(&mut self, args: LaunchRequestArguments) -> Result<()> {
        let program = &args.program;
        let classpath = &args.classpath;
        
        // Start JVM with debug agent
        let process = Command::new("java")
            .args([
                "-agentlib:jdwp=transport=dt_socket,server=y,suspend=y",
                "-cp", classpath,
                program,
            ])
            .spawn()?;
        
        // Connect via JDWP
        self.jdwp = JdwpClient::connect(&address).await?;
        
        Ok(())
    }
    
    async fn set_breakpoints(
        &mut self,
        args: SetBreakpointsArguments,
    ) -> Result<SetBreakpointsResponse> {
        let source = &args.source;
        let breakpoints = args.breakpoints.unwrap_or_default();
        
        let mut results = Vec::new();
        for bp in breakpoints {
            let location = self.resolve_location(source, bp.line)?;
            let id = self.jdwp.set_breakpoint(location).await?;
            results.push(Breakpoint {
                id: Some(id),
                verified: true,
                line: Some(bp.line),
                ..Default::default()
            });
        }
        
        Ok(SetBreakpointsResponse { breakpoints: results })
    }
}
```

### JDWP Implementation

The JDWP protocol uses a binary format:

```rust
// JDWP packet structure
struct JdwpPacket {
    length: u32,
    id: u32,
    flags: u8,
    command_set: u8,  // or error_code for replies
    command: u8,
    data: Vec<u8>,
}

// Example: Set breakpoint command
fn set_breakpoint_command(location: &Location) -> JdwpPacket {
    let mut data = Vec::new();
    data.push(LOCATION_ONLY);  // Event kind
    data.push(SUSPEND_ALL);    // Suspend policy
    data.extend(&1u32.to_be_bytes());  // Modifier count
    data.push(MODIFIER_LOCATION_ONLY);
    data.extend(&location.class_id.to_be_bytes());
    data.extend(&location.method_id.to_be_bytes());
    data.extend(&location.offset.to_be_bytes());
    
    JdwpPacket {
        command_set: EVENT_REQUEST,
        command: SET,
        data,
        ..Default::default()
    }
}
```

### Variable Display

Formatting variables for display:

```rust
fn format_variable(value: &JdwpValue) -> Variable {
    match value {
        JdwpValue::Object(obj) => {
            Variable {
                name: obj.field_name.clone(),
                value: format!("{}@{}", obj.type_name, obj.id),
                type_: Some(obj.type_name.clone()),
                variables_reference: obj.id,  // Expandable
                ..Default::default()
            }
        }
        JdwpValue::Array(arr) => {
            Variable {
                name: arr.name.clone(),
                value: format!("{}[{}]", arr.element_type, arr.length),
                indexed_variables: Some(arr.length),
                ..Default::default()
            }
        }
        JdwpValue::Primitive(prim) => {
            Variable {
                name: prim.name.clone(),
                value: prim.value.to_string(),
                type_: Some(prim.type_name.clone()),
                ..Default::default()
            }
        }
    }
}
```

---

## Advanced Features

### Hot Code Replacement

Replace code while debugging without restarting:

```rust
async fn hot_replace(&mut self, class_id: ClassId, bytecode: &[u8]) -> Result<()> {
    // Use JDWP RedefineClasses command
    self.jdwp.redefine_class(class_id, bytecode).await?;
    
    // Notify editor
    self.client.send_event(Event::Output {
        category: "console",
        output: "Hot code replacement successful",
    }).await?;
    
    Ok(())
}
```

### Smart Step Into

Step into specific method when multiple calls on line:

```rust
async fn step_into_targets(&self, frame: FrameId) -> Result<Vec<StepInTarget>> {
    let line = self.current_line(frame)?;
    let calls = self.workspace.method_calls_on_line(line)?;
    
    calls.iter().map(|call| StepInTarget {
        id: call.id,
        label: format!("{}()", call.method_name),
    }).collect()
}
```

### Stream Debugger

Debug Java Streams by visualizing pipeline:

```java
list.stream()
    .filter(x -> x > 0)      // ← Inspect intermediate state
    .map(x -> x * 2)         // ← See transformations
    .collect(toList());
```

---

## Testing

```bash
# DAP tests
bash scripts/cargo_agent.sh test --locked -p nova-dap --lib

# JDWP tests
bash scripts/cargo_agent.sh test --locked -p nova-jdwp --lib

# Stream debug tests
bash scripts/cargo_agent.sh test --locked -p nova-stream-debug --lib
```

### Integration Testing

```rust
#[tokio::test]
async fn test_breakpoint_hit() {
    let server = TestDapServer::new().await;
    
    // Launch with test program
    server.launch("TestProgram").await;
    
    // Set breakpoint
    server.set_breakpoints("Test.java", vec![5]).await;
    
    // Continue and wait for stop
    server.continue_().await;
    let event = server.wait_for_stopped().await;
    
    assert_eq!(event.reason, "breakpoint");
    assert_eq!(event.line, 5);
}
```

---

## Common Pitfalls

1. **Thread synchronization** - JDWP is async, handle responses correctly
2. **Class loading** - Breakpoints may not resolve until class loads
3. **Suspend policy** - Choose appropriate suspend (all threads vs. one)
4. **Expression evaluation** - Can have side effects, be careful
5. **Stack frame invalidation** - Frames invalid after resume

---

## Dependencies

**Upstream:** `nova-workspace` (for source mapping)
**Downstream:** Editor debug integration

---

*Remember: Always follow [AGENTS.md](../AGENTS.md) rules. Use wrapper scripts. Scope your cargo commands.*

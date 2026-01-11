use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToServer,
    ServerToClient,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::ClientToServer => "->",
            Direction::ServerToClient => "<-",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub direction: Direction,
    pub message: Value,
}

#[derive(Debug, Clone)]
pub struct ExpectedEntry {
    pub direction: Direction,
    pub pattern: Value,
}

/// Matches any JSON value (but requires the field to be present).
///
/// This is useful for fields like `seq`, timestamps, or IDs that are expected to exist but
/// should not be compared for exact equality.
pub fn ignore() -> Value {
    json!({"$ignore": true})
}

pub fn request(command: &str, arguments: Value) -> ExpectedEntry {
    ExpectedEntry {
        direction: Direction::ClientToServer,
        pattern: json!({
            "type": "request",
            "command": command,
            "arguments": arguments,
        }),
    }
}

pub fn response(command: &str, success: bool, body: Option<Value>) -> ExpectedEntry {
    let mut pattern = json!({
        "type": "response",
        "command": command,
        "success": success,
    });

    if let Some(body) = body {
        pattern
            .as_object_mut()
            .expect("json! produces object")
            .insert("body".to_string(), body);
    }

    ExpectedEntry {
        direction: Direction::ServerToClient,
        pattern,
    }
}

pub fn event(event: &str, body: Option<Value>) -> ExpectedEntry {
    let mut pattern = json!({
        "type": "event",
        "event": event,
    });

    if let Some(body) = body {
        pattern
            .as_object_mut()
            .expect("json! produces object")
            .insert("body".to_string(), body);
    }

    ExpectedEntry {
        direction: Direction::ServerToClient,
        pattern,
    }
}

pub fn format_entries(entries: &[Entry]) -> String {
    entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let msg = serde_json::to_string(&entry.message)
                .unwrap_or_else(|_| "<invalid json>".to_string());
            format!("{idx:03} {} {msg}", entry.direction.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn assert_matches(actual: &[Entry], expected: &[ExpectedEntry]) {
    if actual.len() != expected.len() {
        panic!(
            "transcript length mismatch: expected {} entries, got {}\n\nactual transcript:\n{}",
            expected.len(),
            actual.len(),
            format_entries(actual)
        );
    }

    for (idx, (act, exp)) in actual.iter().zip(expected.iter()).enumerate() {
        if act.direction != exp.direction {
            panic!(
                "transcript direction mismatch at index {idx}: expected {:?}, got {:?}\n\nactual transcript:\n{}",
                exp.direction,
                act.direction,
                format_entries(actual)
            );
        }

        if let Err(err) = assert_json_subset(&exp.pattern, &act.message, "$") {
            panic!(
                "transcript mismatch at index {idx}: {err}\n\nexpected pattern:\n{}\n\nactual transcript:\n{}",
                serde_json::to_string_pretty(&exp.pattern).unwrap_or_else(|_| "<invalid json>".to_string()),
                format_entries(actual)
            );
        }
    }
}

fn is_ignore(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    obj.len() == 1 && obj.get("$ignore") == Some(&Value::Bool(true))
}

fn assert_json_subset(expected: &Value, actual: &Value, path: &str) -> Result<(), String> {
    if is_ignore(expected) {
        return Ok(());
    }

    match expected {
        Value::Object(exp) => {
            let Some(act) = actual.as_object() else {
                return Err(format!("expected object at {path}, got {actual:?}"));
            };
            for (key, exp_value) in exp {
                let Some(act_value) = act.get(key) else {
                    return Err(format!("missing key {path}/{key} in actual message"));
                };
                assert_json_subset(exp_value, act_value, &format!("{path}/{key}"))?;
            }
            Ok(())
        }
        Value::Array(exp) => {
            let Some(act) = actual.as_array() else {
                return Err(format!("expected array at {path}, got {actual:?}"));
            };
            if exp.len() != act.len() {
                return Err(format!(
                    "array length mismatch at {path}: expected {}, got {}",
                    exp.len(),
                    act.len()
                ));
            }
            for (idx, (exp_value, act_value)) in exp.iter().zip(act.iter()).enumerate() {
                assert_json_subset(exp_value, act_value, &format!("{path}/{idx}"))?;
            }
            Ok(())
        }
        _ => {
            if expected != actual {
                Err(format!(
                    "value mismatch at {path}: expected {}, got {}",
                    serde_json::to_string(expected).unwrap_or_else(|_| expected.to_string()),
                    serde_json::to_string(actual).unwrap_or_else(|_| actual.to_string())
                ))
            } else {
                Ok(())
            }
        }
    }
}

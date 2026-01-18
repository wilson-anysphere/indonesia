use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// A DAP-style step-in target.
///
/// DAP's canonical shape is `{ id, label }` with optional source positions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StepInTarget {
    pub id: i64,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
}

#[derive(Debug, Clone)]
struct CallSpan {
    name: String,
    name_start: usize,
    close_paren: usize,
}

#[derive(Debug, Clone, Copy)]
struct ParenFrame {
    call_index: Option<usize>,
}

/// Enumerate all method/constructor call step targets that appear on `line`.
///
/// This is designed as a "pure" function to keep it easy to test. A production
/// implementation would use Nova's parsed syntax tree / HIR to avoid heuristic
/// parsing.
pub fn enumerate_step_in_targets_in_line(line: &str) -> Vec<StepInTarget> {
    let bytes = line.as_bytes();
    let mut calls: Vec<CallSpan> = Vec::new();
    let mut paren_stack: Vec<ParenFrame> = Vec::new();

    let mut i = 0;
    let mut in_string: Option<u8> = None;
    let mut escaped = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let b = bytes[i];

        if let Some(quote) = in_string {
            if escaped {
                escaped = false;
                i += 1;
                continue;
            }
            if b == b'\\' {
                escaped = true;
                i += 1;
                continue;
            }
            if b == quote {
                in_string = None;
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        // Comment starts.
        if b == b'/' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'/' {
                // Line comment => ignore remainder.
                break;
            }
            if next == b'*' {
                in_block_comment = true;
                i += 2;
                continue;
            }
        }

        // String / char literal starts.
        if b == b'"' || b == b'\'' {
            in_string = Some(b);
            i += 1;
            continue;
        }

        if b == b'(' {
            if let Some((name, name_start, name_end)) = identifier_before_paren(bytes, i) {
                if !is_java_paren_keyword(&name) {
                    let call_index = calls.len();
                    calls.push(CallSpan {
                        name,
                        name_start,
                        close_paren: usize::MAX,
                    });
                    paren_stack.push(ParenFrame {
                        call_index: Some(call_index),
                    });
                } else {
                    paren_stack.push(ParenFrame { call_index: None });
                }

                // The identifier lookup already consumed the trailing identifier.
                // Continue scanning from `i + 1`; the main loop will move forward.
                let _ = name_end;
            } else {
                paren_stack.push(ParenFrame { call_index: None });
            }
        } else if b == b')' {
            if let Some(frame) = paren_stack.pop() {
                if let Some(call_index) = frame.call_index {
                    if let Some(call) = calls.get_mut(call_index) {
                        call.close_paren = i;
                    }
                }
            }
        }

        i += 1;
    }

    // Filter out spans with no matching close paren.
    calls.retain(|c| c.close_paren != usize::MAX);

    // Evaluation order for calls on a line is equivalent to the order of their
    // closing parenthesis (the call "happens" once arguments are evaluated).
    calls.sort_by(|a, b| {
        a.close_paren
            .cmp(&b.close_paren)
            .then_with(|| a.name_start.cmp(&b.name_start))
    });

    calls
        .into_iter()
        .enumerate()
        .map(|(idx, call)| StepInTarget {
            id: idx as i64,
            label: format!("{}()", call.name),
            line: None,
            column: Some((call.name_start.saturating_add(1)) as u32),
            end_line: None,
            end_column: Some((call.close_paren.saturating_add(2)) as u32),
        })
        .collect()
}

fn identifier_before_paren(bytes: &[u8], open_paren: usize) -> Option<(String, usize, usize)> {
    static IDENTIFIER_UTF8_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

    if open_paren == 0 {
        return None;
    }

    let mut i = open_paren;
    while i > 0 {
        let prev = bytes[i - 1];
        if prev.is_ascii_whitespace() {
            i -= 1;
            continue;
        }
        break;
    }

    if i == 0 {
        return None;
    }

    let end = i - 1;
    if !is_java_ident_char(bytes[end]) {
        return None;
    }

    let mut start = end;
    while start > 0 && is_java_ident_char(bytes[start - 1]) {
        start -= 1;
    }

    let name = match std::str::from_utf8(&bytes[start..=end]) {
        Ok(name) => name.to_string(),
        Err(err) => {
            if IDENTIFIER_UTF8_ERROR_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.dap",
                    start,
                    end,
                    error = ?err,
                    "failed to parse identifier slice as UTF-8 for step-in targets"
                );
            }
            return None;
        }
    };
    Some((name, start, end))
}

fn is_java_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn is_java_paren_keyword(name: &str) -> bool {
    matches!(
        name,
        "if" | "for" | "while" | "switch" | "catch" | "synchronized" | "try" | "do"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_nested_calls_in_evaluation_order() {
        let line = r#"foo(bar(), baz(qux()), corge()).trim();"#;
        let targets = enumerate_step_in_targets_in_line(line);
        let labels: Vec<_> = targets.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["bar()", "qux()", "baz()", "corge()", "foo()", "trim()"]
        );
    }

    #[test]
    fn ignores_calls_in_comments_and_strings() {
        let line = r#"foo(); // bar(baz())"#;
        let targets = enumerate_step_in_targets_in_line(line);
        let labels: Vec<_> = targets.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, vec!["foo()"]);

        let line = r#"foo("bar(baz())");"#;
        let targets = enumerate_step_in_targets_in_line(line);
        let labels: Vec<_> = targets.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, vec!["foo()"]);
    }
}

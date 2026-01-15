use nova_core::{BuildDiagnostic, BuildDiagnosticSeverity, Position, Range};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavacDiagnosticFormat {
    Maven,
    Standard,
}

fn make_range(line_1_based: u32, col_1_based: u32) -> Range {
    let line = line_1_based.saturating_sub(1);
    let col = col_1_based.saturating_sub(1);
    Range::point(Position::new(line, col))
}

/// Parse javac-style diagnostics from Maven / Gradle output.
///
/// The parser is intentionally tolerant; build tools frequently interleave
/// additional logging around compiler messages.
pub fn parse_javac_diagnostics(output: &str, source: &str) -> Vec<BuildDiagnostic> {
    let mut diags: Vec<BuildDiagnostic> = Vec::new();
    let mut current: Option<BuildDiagnostic> = None;

    let mut lines = output.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some((sev, file, line_no, col_no, msg)) = parse_maven_diag_header(line) {
            if let Some(prev) = current.take() {
                diags.push(prev);
            }
            current = Some(BuildDiagnostic::new(
                file,
                make_range(line_no, col_no),
                sev,
                msg,
                Some(source.to_string()),
            ));
            continue;
        }

        if let Some((sev, file, line_no, msg, consumed)) =
            parse_standard_javac_header(line, &mut lines)
        {
            if let Some(prev) = current.take() {
                diags.push(prev);
            }

            let range = make_range(line_no, consumed.col_1_based.unwrap_or(1));
            current = Some(BuildDiagnostic::new(
                file,
                range,
                sev,
                msg,
                Some(source.to_string()),
            ));
            continue;
        }

        // Continuation lines.
        if let Some(ref mut diag) = current {
            if let Some(extra) = maven_continuation_text(line) {
                if !extra.is_empty() {
                    diag.message.push('\n');
                    diag.message.push_str(&extra);
                }
            } else if is_standard_continuation(line) {
                diag.message.push('\n');
                diag.message.push_str(line.trim_end());
            }
        }
    }

    if let Some(diag) = current.take() {
        diags.push(diag);
    }

    diags
}

fn parse_maven_diag_header(
    line: &str,
) -> Option<(BuildDiagnosticSeverity, PathBuf, u32, u32, String)> {
    // Example:
    // [ERROR] /path/Foo.java:[10,5] cannot find symbol
    let level = line.strip_prefix('[')?.split_once(']')?.0;
    let severity = match level {
        "ERROR" => BuildDiagnosticSeverity::Error,
        "WARNING" => BuildDiagnosticSeverity::Warning,
        _ => return None,
    };

    let rest = line.split_once("] ")?.1;
    let (path_part, loc_and_msg) = rest.rsplit_once(":[")?;
    let (loc_part, msg) = loc_and_msg.split_once("] ")?;
    let (line_s, col_s) = loc_part.split_once(',')?;
    let line_no = line_s.parse::<u32>().ok()?;
    let col_no = col_s.parse::<u32>().ok()?;

    Some((
        severity,
        PathBuf::from(path_part),
        line_no,
        col_no,
        msg.to_string(),
    ))
}

fn maven_continuation_text(line: &str) -> Option<String> {
    // Maven prefixes with [ERROR] / [WARNING] but continuation lines typically
    // don't contain `:[line,col]`.
    let stripped = line.strip_prefix('[')?;
    let (level, rest) = stripped.split_once("] ")?;
    if level != "ERROR" && level != "WARNING" {
        return None;
    }
    Some(rest.trim_end().to_string())
}

#[derive(Debug)]
struct StandardHeaderConsumed {
    col_1_based: Option<u32>,
}

fn parse_standard_javac_header<'a>(
    line: &str,
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
) -> Option<(
    BuildDiagnosticSeverity,
    PathBuf,
    u32,
    String,
    StandardHeaderConsumed,
)> {
    // Example:
    // /path/Foo.java:10: error: cannot find symbol
    //     foo.bar();
    //         ^

    let (sev, sev_marker) = if let Some(pos) = line.rfind(": error:") {
        (BuildDiagnosticSeverity::Error, (pos, ": error:".len()))
    } else if let Some(pos) = line.rfind(": warning:") {
        (BuildDiagnosticSeverity::Warning, (pos, ": warning:".len()))
    } else {
        return None;
    };

    let (left, msg_part) = line.split_at(sev_marker.0);
    let msg = msg_part[sev_marker.1..].trim_start();

    let (path_s, line_s) = left.rsplit_once(':')?;
    let line_no = line_s.trim().parse::<u32>().ok()?;

    // Best-effort column extraction using the caret line (if present).
    let mut col_1_based: Option<u32> = None;
    // Peek the next two lines without consuming unrelated content.
    let code_line = lines.peek().copied();
    if code_line.is_some() {
        // Consume code line.
        let _ = lines.next();
        let caret_line = lines.peek().copied();
        if let Some(caret) = caret_line {
            if let Some(idx) = caret.find('^') {
                // Consume caret line.
                let _ = lines.next();
                col_1_based = Some((idx as u32) + 1);
            }
        }
    }

    Some((
        sev,
        PathBuf::from(path_s),
        line_no,
        msg.to_string(),
        StandardHeaderConsumed { col_1_based },
    ))
}

fn is_standard_continuation(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Heuristic: javac continuation lines often start with `symbol:` or
    // `location:` or similar indentation.
    trimmed.starts_with("symbol:") || trimmed.starts_with("location:") || line.starts_with(' ')
}

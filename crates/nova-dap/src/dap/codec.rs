use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{self, BufRead, Write};

// Re-export these constants so existing users of the historical
// `nova_dap::dap::codec::MAX_*` paths continue to compile.
pub use super::{MAX_DAP_HEADER_LINE_BYTES, MAX_DAP_MESSAGE_BYTES};

fn sanitize_json_error_message(message: &str) -> String {
    // `serde_json::Error` display strings can include user-provided scalar values (for example:
    // `invalid type: string "..."` or `unknown field `...``). Avoid echoing those values in error
    // messages because DAP payloads can include secrets (launch args/env, evaluated expressions,
    // etc).
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find('"') {
        // Include the opening quote.
        out.push_str(&rest[..start + 1]);
        rest = &rest[start + 1..];

        let Some(end) = rest.find('"') else {
            // Unterminated quote: append the remainder and stop.
            out.push_str(rest);
            return out;
        };
        out.push_str("<redacted>\"");
        rest = &rest[end + 1..];
    }
    out.push_str(rest);

    // `serde` wraps unknown fields/variants in backticks:
    // `unknown field `secret`, expected ...`
    //
    // Redact only the first backticked segment so we keep the expected value list actionable.
    if let Some(start) = out.find('`') {
        if let Some(end_rel) = out[start.saturating_add(1)..].find('`') {
            let end = start.saturating_add(1).saturating_add(end_rel);
            if start + 1 <= end && end <= out.len() {
                out.replace_range(start + 1..end, "<redacted>");
            }
        }
    }

    out
}

fn read_line_limited<R: BufRead>(reader: &mut R, max_len: usize) -> io::Result<Option<String>> {
    let mut buf = Vec::<u8>::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }

        let newline_pos = available.iter().position(|&b| b == b'\n');
        let take = newline_pos.map(|pos| pos + 1).unwrap_or(available.len());
        if buf.len() + take > max_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("DAP header line exceeds maximum size ({max_len} bytes)"),
            ));
        }

        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline_pos.is_some() {
            break;
        }
    }

    let line = String::from_utf8(buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "DAP header line is not UTF-8"))?;
    Ok(Some(line))
}

/// Read a single DAP-framed JSON message from `reader`.
///
/// DAP messages are framed using an HTTP-like header section:
///
/// ```text
/// Content-Length: 123\r\n
/// \r\n
/// { ...json... }
/// ```
pub fn read_json_message<R: BufRead, T: DeserializeOwned>(reader: &mut R) -> io::Result<Option<T>> {
    let bytes = match read_raw_message(reader)? {
        Some(bytes) => bytes,
        None => return Ok(None),
    };

    let parsed = serde_json::from_slice(&bytes).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            sanitize_json_error_message(&err.to_string()),
        )
    })?;
    Ok(Some(parsed))
}

/// Write a single DAP-framed JSON message to `writer`.
pub fn write_json_message<W: Write, T: Serialize>(writer: &mut W, message: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(message).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            sanitize_json_error_message(&err.to_string()),
        )
    })?;
    write_raw_message(writer, &bytes)?;
    Ok(())
}

pub fn read_raw_message<R: BufRead>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut content_length: Option<usize> = None;
    let mut saw_header_line = false;

    // Read header lines until the blank separator line.
    loop {
        let Some(line) = read_line_limited(reader, MAX_DAP_HEADER_LINE_BYTES)? else {
            // EOF without a message.
            if !saw_header_line {
                return Ok(None);
            }

            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF while reading DAP headers",
            ));
        };
        saw_header_line = true;

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                let value = value.trim();
                content_length = Some(value.parse::<usize>().map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid Content-Length {value:?}: {err}"),
                    )
                })?);
            }
        }
    }

    let Some(content_length) = content_length else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "DAP message missing Content-Length header",
        ));
    };

    if content_length > MAX_DAP_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "DAP message Content-Length {} exceeds maximum allowed size {}",
                content_length, MAX_DAP_MESSAGE_BYTES
            ),
        ));
    }

    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf)?;
    Ok(Some(buf))
}

pub fn write_raw_message<W: Write>(writer: &mut W, json_bytes: &[u8]) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", json_bytes.len())?;
    writer.write_all(json_bytes)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dap::MAX_DAP_MESSAGE_BYTES;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn roundtrips_json_message_with_correct_content_length() {
        let msg = json!({
            "seq": 1,
            "type": "request",
            "command": "initialize",
            "arguments": {"adapterID": "nova"}
        });

        let mut buf = Vec::new();
        write_json_message(&mut buf, &msg).unwrap();

        let payload = serde_json::to_vec(&msg).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", payload.len());
        assert!(buf.starts_with(header.as_bytes()));

        let mut cursor = Cursor::new(buf);
        let decoded: serde_json::Value = read_json_message(&mut cursor).unwrap().unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn accepts_additional_headers() {
        let payload = br#"{"seq":1,"type":"request","command":"threads"}"#;
        let framed = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}",
            payload.len(),
            std::str::from_utf8(payload).unwrap()
        );
        let mut cursor = Cursor::new(framed.into_bytes());
        let decoded: serde_json::Value = read_json_message(&mut cursor).unwrap().unwrap();
        assert_eq!(decoded["command"], "threads");
    }

    #[test]
    fn rejects_oversized_content_length_without_allocating_message_body() {
        let framed = format!("Content-Length: {}\r\n\r\n", MAX_DAP_MESSAGE_BYTES + 1);
        let mut cursor = Cursor::new(framed.into_bytes());
        let err = read_raw_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds maximum allowed size"));
    }

    #[test]
    fn rejects_overlong_header_lines() {
        // Ensure a malicious client can't force us to allocate an unbounded header line buffer.
        let long = "A".repeat(MAX_DAP_HEADER_LINE_BYTES + 1);
        let framed = format!("{long}\n\n");
        let mut cursor = Cursor::new(framed.into_bytes());
        let err = read_raw_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("header line exceeds maximum size"));
    }

    #[test]
    fn rejects_pathologically_large_content_length_without_attempting_allocation() {
        // `usize::MAX` is intentionally far beyond the maximum. This guards against
        // regressions where we might try to allocate the body buffer before checking
        // the limit (which would likely panic or OOM).
        let framed = format!("Content-Length: {}\r\n\r\n", usize::MAX);
        let mut cursor = Cursor::new(framed.into_bytes());
        let err = read_raw_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("Content-Length"));
    }

    #[test]
    fn eof_mid_headers_returns_unexpected_eof() {
        // The message starts, but the stream ends before the blank header separator line.
        // This should not be treated as a clean EOF.
        let framed = "Content-Length: 2\r\n";
        let mut cursor = Cursor::new(framed.as_bytes().to_vec());
        let err = read_raw_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains("EOF while reading DAP headers"));
    }

    #[test]
    fn dap_codec_json_errors_do_not_echo_string_values() {
        #[derive(Debug, serde::Deserialize)]
        struct Dummy {
            seq: i64,
        }

        let secret = "dap-codec-super-secret-token";
        let payload = format!(r#"{{"seq":"{secret}"}}"#);
        let framed = format!("Content-Length: {}\r\n\r\n{}", payload.len(), payload);

        let mut cursor = Cursor::new(framed.into_bytes());
        let err = read_json_message::<_, Dummy>(&mut cursor).unwrap_err();
        let message = err.to_string();
        assert!(
            !message.contains(secret),
            "expected DAP codec JSON error message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected DAP codec JSON error message to include redaction marker: {message}"
        );
    }

    #[test]
    fn dap_codec_json_errors_do_not_echo_backticked_values() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            foo: u32,
        }

        let secret = "dap-codec-backticked-secret";
        let payload = format!(r#"{{"{secret}": 1}}"#);
        let framed = format!("Content-Length: {}\r\n\r\n{}", payload.len(), payload);

        let mut cursor = Cursor::new(framed.into_bytes());
        let err = read_json_message::<_, OnlyFoo>(&mut cursor).unwrap_err();
        let message = err.to_string();
        assert!(
            !message.contains(secret),
            "expected DAP codec JSON error message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected DAP codec JSON error message to include redaction marker: {message}"
        );
    }
}

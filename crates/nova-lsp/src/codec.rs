use serde_json::Value;
use std::io::{self, BufRead, Write};

/// Maximum allowed LSP/JSON-RPC message payload size (in bytes).
///
/// This caps the value of the incoming `Content-Length` header. Without an upper bound, a
/// malformed/hostile client can send an enormous `Content-Length` and force the server to
/// allocate huge buffers (potentially triggering OOM / RLIMIT_AS kills) before we even attempt to
/// read the message body.
pub const MAX_LSP_MESSAGE_BYTES: usize = 16 * 1024 * 1024; // 16 MiB

/// Maximum allowed size of a single LSP header line (in bytes).
pub const MAX_LSP_HEADER_LINE_BYTES: usize = 8 * 1024; // 8 KiB

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
                format!("LSP header line exceeds maximum size ({max_len} bytes)"),
            ));
        }

        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline_pos.is_some() {
            break;
        }
    }

    let line = String::from_utf8(buf).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "LSP header line is not UTF-8")
    })?;
    Ok(Some(line))
}

/// Write a JSON-RPC message framed with LSP-style `Content-Length` headers.
pub fn write_json_message(writer: &mut impl Write, message: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(message)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

/// Read a JSON-RPC message framed with LSP-style `Content-Length` headers.
pub fn read_json_message(reader: &mut impl BufRead) -> io::Result<Value> {
    let mut content_length: Option<usize> = None;

    loop {
        let Some(line) = read_line_limited(reader, MAX_LSP_HEADER_LINE_BYTES)? else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF while reading headers",
            ));
        };

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

    let len = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;

    if len > MAX_LSP_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "LSP message Content-Length {} exceeds maximum allowed size {}",
                len, MAX_LSP_MESSAGE_BYTES
            ),
        ));
    }

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    serde_json::from_slice(&buf)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn roundtrips_json_message_with_correct_content_length() {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"capabilities": {}}
        });

        let mut buf = Vec::new();
        write_json_message(&mut buf, &msg).unwrap();

        let payload = serde_json::to_vec(&msg).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", payload.len());
        assert!(buf.starts_with(header.as_bytes()));

        let mut cursor = Cursor::new(buf);
        let decoded = read_json_message(&mut cursor).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn accepts_additional_headers() {
        let payload = br#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
        let framed = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}",
            payload.len(),
            std::str::from_utf8(payload).unwrap()
        );
        let mut cursor = Cursor::new(framed.into_bytes());
        let decoded = read_json_message(&mut cursor).unwrap();
        assert_eq!(decoded["method"], "initialized");
    }

    #[test]
    fn rejects_oversized_content_length_without_attempting_allocation() {
        let framed = format!("Content-Length: {}\r\n\r\n", MAX_LSP_MESSAGE_BYTES + 1);
        let mut cursor = Cursor::new(framed.into_bytes());
        let err = read_json_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds maximum allowed size"));
    }

    #[test]
    fn rejects_overlong_header_lines() {
        let long = "A".repeat(MAX_LSP_HEADER_LINE_BYTES + 1);
        let framed = format!("{long}\n\n");
        let mut cursor = Cursor::new(framed.into_bytes());
        let err = read_json_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("header line exceeds maximum size"));
    }

    #[test]
    fn rejects_pathologically_large_content_length_without_attempting_allocation() {
        let framed = format!("Content-Length: {}\r\n\r\n", usize::MAX);
        let mut cursor = Cursor::new(framed.into_bytes());
        let err = read_json_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("Content-Length"));
    }
}

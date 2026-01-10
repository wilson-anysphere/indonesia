use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{self, BufRead, Write};

/// Read a single JSON-RPC message framed with `Content-Length` headers.
///
/// Both DAP and LSP use an HTTP-like framing:
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
    let parsed = serde_json::from_slice(&bytes)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(Some(parsed))
}

pub fn write_json_message<W: Write, T: Serialize>(writer: &mut W, message: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(message)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    write_raw_message(writer, &bytes)?;
    Ok(())
}

pub fn read_raw_message<R: BufRead>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut content_length: Option<usize> = None;

    // Read headers until the blank separator line.
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            return Ok(None);
        }

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }

    let Some(content_length) = content_length else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "JSON-RPC message missing Content-Length header",
        ));
    };

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

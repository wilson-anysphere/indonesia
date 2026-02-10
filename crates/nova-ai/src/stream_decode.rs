use crate::AiError;

/// Maximum number of bytes we will buffer for a single streaming frame/line before
/// seeing a delimiter (newline / SSE event boundary).
///
/// This guards against misbehaving servers that never send delimiters or that
/// send extremely large frames, which would otherwise grow our buffers without
/// bound and risk OOM.
pub(crate) const MAX_STREAM_FRAME_BYTES: usize = 1024 * 1024; // 1 MiB

pub(crate) fn stream_frame_too_large_error(attempted_len: usize, max_len: usize) -> AiError {
    AiError::UnexpectedResponse(format!(
        "stream frame too large: {attempted_len} bytes (max {max_len} bytes)"
    ))
}

/// Ensures that buffering `chunk` (which may contain newlines) on top of an existing partial frame
/// of length `pending_len` cannot exceed `max_len` bytes for any single frame.
///
/// This is intended to be called **before** copying `chunk` into a buffer to ensure we fail fast
/// (and avoid further allocations) when a server streams an unbounded frame.
pub(crate) fn ensure_max_stream_frame_size(
    pending_len: usize,
    chunk: &[u8],
    max_len: usize,
) -> Result<(), AiError> {
    if pending_len > max_len {
        return Err(stream_frame_too_large_error(pending_len, max_len));
    }

    let mut current = pending_len;
    for &b in chunk {
        if b == b'\n' {
            current = 0;
            continue;
        }

        current = current.saturating_add(1);
        if current > max_len {
            return Err(stream_frame_too_large_error(current, max_len));
        }
    }

    Ok(())
}

/// Trims ASCII whitespace from both ends of `bytes`.
pub(crate) fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while let Some(first) = bytes.first() {
        if first.is_ascii_whitespace() {
            bytes = &bytes[1..];
        } else {
            break;
        }
    }
    while let Some(last) = bytes.last() {
        if last.is_ascii_whitespace() {
            bytes = &bytes[..bytes.len() - 1];
        } else {
            break;
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_max_stream_frame_size_errors_when_line_exceeds_limit_without_newline() {
        let max = 8;

        ensure_max_stream_frame_size(0, b"1234", max).expect("ok");
        ensure_max_stream_frame_size(4, b"5678", max).expect("ok");

        let err = ensure_max_stream_frame_size(8, b"9", max)
            .expect_err("expected frame-too-large error");
        match err {
            AiError::UnexpectedResponse(msg) => {
                assert!(
                    msg.contains("stream frame too large"),
                    "expected actionable error message, got: {msg}"
                );
            }
            other => panic!("expected UnexpectedResponse, got {other:?}"),
        }
    }

    #[test]
    fn ensure_max_stream_frame_size_allows_newline_boundary_to_reset() {
        let max = 8;
        ensure_max_stream_frame_size(8, b"\n9", max).expect("ok");
    }
}

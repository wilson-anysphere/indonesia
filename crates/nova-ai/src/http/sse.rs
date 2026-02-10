use crate::stream_decode::{
    ensure_max_stream_frame_size, stream_frame_too_large_error, MAX_STREAM_FRAME_BYTES,
};
use crate::AiError;
use futures::{Stream, StreamExt};
use std::marker::PhantomData;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A decoded Server-Sent Events (SSE) event.
///
/// This is intentionally minimal and provider-friendly: most streaming providers only
/// care about the concatenated `data` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

#[derive(Default, Debug)]
struct EventBuilder {
    event: Option<String>,
    data: String,
    saw_data: bool,
}

impl EventBuilder {
    fn is_empty(&self) -> bool {
        self.event.is_none() && !self.saw_data
    }

    fn push_line(&mut self, line: &str) -> Result<(), AiError> {
        // Comments start with ':' and are ignored.
        if line.starts_with(':') {
            return Ok(());
        }

        let (field, value) = match line.split_once(':') {
            Some((field, value)) => {
                let value = value.strip_prefix(' ').unwrap_or(value);
                (field, value)
            }
            None => (line, ""),
        };

        match field {
            "event" => {
                let value = value.trim();
                if value.is_empty() {
                    self.event = None;
                } else {
                    self.event = Some(value.to_string());
                }
            }
            "data" => {
                // Spec: keep the value as-is (except for the optional leading space after ':').
                let additional = value.len().saturating_add(if self.saw_data { 1 } else { 0 });
                let attempted_len = self.data.len().saturating_add(additional);
                if attempted_len > MAX_STREAM_FRAME_BYTES {
                    return Err(stream_frame_too_large_error(
                        attempted_len,
                        MAX_STREAM_FRAME_BYTES,
                    ));
                }

                if self.saw_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.saw_data = true;
            }
            _ => {
                // Ignore unsupported fields (id, retry, etc).
            }
        }

        Ok(())
    }

    fn take_event(&mut self) -> Option<SseEvent> {
        if self.is_empty() {
            return None;
        }

        let out = SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
        };
        self.saw_data = false;
        Some(out)
    }
}

/// Decode a `reqwest::Response::bytes_stream()` into SSE events.
///
/// This implementation is UTF-8 safe across chunk boundaries by buffering bytes until
/// complete lines/events are available.
pub(crate) struct SseDecoder<S, B> {
    bytes: S,
    buf: Vec<u8>,
    cursor: usize,
    current: EventBuilder,
    eof: bool,
    _marker: PhantomData<B>,
}

impl<S, B> SseDecoder<S, B>
where
    S: Stream<Item = Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
{
    pub(crate) fn new(bytes: S) -> Self {
        Self {
            bytes,
            buf: Vec::new(),
            cursor: 0,
            current: EventBuilder::default(),
            eof: false,
            _marker: PhantomData,
        }
    }

    pub(crate) async fn next_event(
        &mut self,
        cancel: &CancellationToken,
        idle_timeout: Duration,
    ) -> Result<Option<SseEvent>, AiError> {
        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }

        loop {
            // Try to consume as many complete lines as we have buffered, stopping once we
            // can yield an event.
            while let Some((start, end)) = self.try_next_line_range() {
                if start == end {
                    if let Some(event) = self.current.take_event() {
                        self.maybe_compact_buffer();
                        return Ok(Some(event));
                    }
                    continue;
                }

                let line = std::str::from_utf8(&self.buf[start..end]).map_err(|err| {
                    AiError::UnexpectedResponse(format!("invalid SSE UTF-8: {err}"))
                })?;
                self.current.push_line(line)?;
            }

            self.maybe_compact_buffer();

            if self.eof {
                // No more bytes will arrive; emit any pending event (even if the stream ended
                // without a trailing blank line).
                if let Some(event) = self.current.take_event() {
                    return Ok(Some(event));
                }
                return Ok(None);
            }

            // Need more bytes to form a full line/event.
            let next_chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(AiError::Cancelled),
                chunk = read_next_chunk(&mut self.bytes, idle_timeout) => chunk?,
            };

            match next_chunk {
                Some(chunk) => {
                    // At this point, we've consumed all buffered newlines; the remaining buffer is
                    // a single partial line. Validate the next chunk before buffering to avoid
                    // unbounded growth if the server never sends delimiters.
                    let pending_len = self.buf.len().saturating_sub(self.cursor);
                    ensure_max_stream_frame_size(
                        pending_len,
                        chunk.as_ref(),
                        MAX_STREAM_FRAME_BYTES,
                    )?;
                    self.buf.extend_from_slice(chunk.as_ref());
                }
                None => {
                    // Stream ended; treat remaining bytes as a final line (without requiring a
                    // newline terminator).
                    self.eof = true;
                    self.consume_trailing_line_at_eof()?;
                }
            }
        }
    }

    fn try_next_line_range(&mut self) -> Option<(usize, usize)> {
        let search = self.buf.get(self.cursor..)?;
        let nl_offset = search.iter().position(|&b| b == b'\n')?;
        let nl_pos = self.cursor + nl_offset;

        // Exclude the '\n' and trim a single '\r' (CRLF support).
        let start = self.cursor;
        let mut end = nl_pos;
        if end > self.cursor && self.buf[end - 1] == b'\r' {
            end -= 1;
        }

        self.cursor = nl_pos + 1;
        Some((start, end))
    }

    fn consume_trailing_line_at_eof(&mut self) -> Result<(), AiError> {
        if self.cursor >= self.buf.len() {
            return Ok(());
        }

        // Consume the final (non-terminated) line.
        let mut end = self.buf.len();
        if end > self.cursor && self.buf[end - 1] == b'\r' {
            end -= 1;
        }

        let line_bytes = &self.buf[self.cursor..end];
        self.cursor = self.buf.len();

        if line_bytes.is_empty() {
            return Ok(());
        }

        let line = std::str::from_utf8(line_bytes)
            .map_err(|err| AiError::UnexpectedResponse(format!("invalid SSE UTF-8: {err}")))?;
        self.current.push_line(line)?;
        Ok(())
    }

    fn maybe_compact_buffer(&mut self) {
        // Avoid O(n) shifting on every consumed line by compacting only when the consumed prefix is
        // reasonably large.
        const COMPACT_THRESHOLD: usize = 8 * 1024;

        if self.cursor == 0 {
            return;
        }

        if self.cursor >= self.buf.len() {
            self.buf.clear();
            self.cursor = 0;
            return;
        }

        if self.cursor < COMPACT_THRESHOLD {
            return;
        }

        self.buf.drain(..self.cursor);
        self.cursor = 0;
    }
}

async fn read_next_chunk<S, B>(bytes: &mut S, idle_timeout: Duration) -> Result<Option<B>, AiError>
where
    S: Stream<Item = Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
{
    let next = if idle_timeout.is_zero() {
        bytes.next().await
    } else {
        tokio::time::timeout(idle_timeout, bytes.next())
            .await
            .map_err(|_| AiError::Timeout)?
    };

    match next {
        Some(Ok(chunk)) => Ok(Some(chunk)),
        Some(Err(err)) => Err(super::map_reqwest_error(err)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn event_framing_across_chunk_splits() {
        let chunks = vec![
            Ok::<_, reqwest::Error>(b"data: o".to_vec()),
            Ok::<_, reqwest::Error>(b"ne\n".to_vec()),
            Ok::<_, reqwest::Error>(b"\n".to_vec()),
        ];
        let mut decoder = SseDecoder::new(stream::iter(chunks));

        let event = decoder
            .next_event(&cancel(), Duration::from_secs(1))
            .await
            .expect("decode ok")
            .expect("event");

        assert_eq!(event.event, None);
        assert_eq!(event.data, "one");
        assert_eq!(
            decoder
                .next_event(&cancel(), Duration::from_secs(1))
                .await
                .expect("decode ok"),
            None
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn multiple_data_lines_concatenate_per_spec() {
        let chunks = vec![Ok::<_, reqwest::Error>(
            b"data: first\ndata: second\n\n".to_vec(),
        )];
        let mut decoder = SseDecoder::new(stream::iter(chunks));

        let event = decoder
            .next_event(&cancel(), Duration::from_secs(1))
            .await
            .expect("decode ok")
            .expect("event");

        assert_eq!(event.event, None);
        assert_eq!(event.data, "first\nsecond");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn crlf_handling() {
        let chunks = vec![Ok::<_, reqwest::Error>(
            b"event: update\r\ndata: hello\r\n\r\n".to_vec(),
        )];
        let mut decoder = SseDecoder::new(stream::iter(chunks));

        let event = decoder
            .next_event(&cancel(), Duration::from_secs(1))
            .await
            .expect("decode ok")
            .expect("event");

        assert_eq!(event.event.as_deref(), Some("update"));
        assert_eq!(event.data, "hello");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn utf8_multibyte_split_across_chunks() {
        let raw = "data: café\n\n".as_bytes().to_vec();

        // Split between the two bytes of 'é' (0xC3 0xA9).
        let split_at = raw
            .windows(2)
            .position(|w| w == [0xC3, 0xA9])
            .map(|idx| idx + 1)
            .expect("find é bytes");

        let chunks = vec![
            Ok::<_, reqwest::Error>(raw[..split_at].to_vec()),
            Ok::<_, reqwest::Error>(raw[split_at..].to_vec()),
        ];
        let mut decoder = SseDecoder::new(stream::iter(chunks));

        let event = decoder
            .next_event(&cancel(), Duration::from_secs(1))
            .await
            .expect("decode ok")
            .expect("event");

        assert_eq!(event.data, "café");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn errors_on_oversized_line_without_newline() {
        let chunk = vec![b'a'; crate::stream_decode::MAX_STREAM_FRAME_BYTES + 1];
        let chunks = vec![Ok::<_, reqwest::Error>(chunk)];
        let mut decoder = SseDecoder::new(stream::iter(chunks));

        let err = decoder
            .next_event(&cancel(), Duration::from_secs(1))
            .await
            .expect_err("expected frame-too-large error");
        match err {
            AiError::UnexpectedResponse(msg) => assert!(
                msg.contains("stream frame too large"),
                "unexpected error message: {msg}"
            ),
            other => panic!("expected UnexpectedResponse, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn errors_on_oversized_event_data_across_multiple_data_fields() {
        let max = crate::stream_decode::MAX_STREAM_FRAME_BYTES;
        let half = max / 2;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"data: ");
        bytes.extend(std::iter::repeat(b'a').take(half));
        bytes.extend_from_slice(b"\n");
        bytes.extend_from_slice(b"data: ");
        // This pushes the total event payload over `max` once we include the inserted `\n`.
        bytes.extend(std::iter::repeat(b'b').take(half + 16));
        bytes.extend_from_slice(b"\n");

        let chunks = vec![Ok::<_, reqwest::Error>(bytes)];
        let mut decoder = SseDecoder::new(stream::iter(chunks));

        let err = decoder
            .next_event(&cancel(), Duration::from_secs(1))
            .await
            .expect_err("expected frame-too-large error");
        match err {
            AiError::UnexpectedResponse(msg) => assert!(
                msg.contains("stream frame too large"),
                "unexpected error message: {msg}"
            ),
            other => panic!("expected UnexpectedResponse, got {other:?}"),
        }
    }
}

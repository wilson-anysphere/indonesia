use crate::AiError;

const COMPACT_START_THRESHOLD: usize = 4 * 1024;

pub(crate) struct Utf8Pending {
    buf: Vec<u8>,
    start: usize,
}

impl Utf8Pending {
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            start: 0,
        }
    }

    pub(crate) fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub(crate) fn flush(
        &mut self,
        stream: &tokio::sync::mpsc::UnboundedSender<Result<String, AiError>>,
    ) {
        if self.start >= self.buf.len() {
            self.buf.clear();
            self.start = 0;
            return;
        }

        loop {
            match std::str::from_utf8(&self.buf[self.start..]) {
                Ok(text) => {
                    if !text.is_empty() {
                        let _ = stream.send(Ok(text.to_string()));
                    }
                    self.buf.clear();
                    self.start = 0;
                    return;
                }
                Err(err) => {
                    let valid = err.valid_up_to();
                    if valid == 0 {
                        // If this is a hard UTF-8 error, drop a byte to avoid spinning forever.
                        if err.error_len().is_some() {
                            self.start = self.start.saturating_add(1);
                            if self.start >= self.buf.len() {
                                self.buf.clear();
                                self.start = 0;
                            } else if self.start > COMPACT_START_THRESHOLD {
                                self.buf.drain(..self.start);
                                self.start = 0;
                            }
                        }
                        return;
                    }

                    let end = self.start + valid;
                    // SAFETY: `valid_up_to` guarantees this prefix is valid UTF-8.
                    let prefix =
                        unsafe { std::str::from_utf8_unchecked(&self.buf[self.start..end]) };
                    if !prefix.is_empty() {
                        let _ = stream.send(Ok(prefix.to_string()));
                    }
                    self.start = end;
                    if self.start == self.buf.len() {
                        self.buf.clear();
                        self.start = 0;
                        return;
                    }
                    if self.start > COMPACT_START_THRESHOLD {
                        self.buf.drain(..self.start);
                        self.start = 0;
                    }
                }
            }
        }
    }

    pub(crate) fn flush_lossy(
        &mut self,
        stream: &tokio::sync::mpsc::UnboundedSender<Result<String, AiError>>,
    ) {
        if self.start >= self.buf.len() {
            self.buf.clear();
            self.start = 0;
            return;
        }

        let out = String::from_utf8_lossy(&self.buf[self.start..]).to_string();
        if !out.is_empty() {
            let _ = stream.send(Ok(out));
        }
        self.buf.clear();
        self.start = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::Utf8Pending;
    use super::COMPACT_START_THRESHOLD;
    use crate::AiError;

    #[test]
    fn flushes_valid_utf8_and_clears() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, AiError>>();
        let mut pending = Utf8Pending::new();

        pending.extend(b"hello");
        pending.flush(&tx);

        assert_eq!(rx.try_recv().unwrap().unwrap(), "hello");
        assert!(rx.try_recv().is_err());

        pending.extend(b"world");
        pending.flush(&tx);

        assert_eq!(rx.try_recv().unwrap().unwrap(), "world");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn buffers_partial_multibyte_until_complete() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, AiError>>();
        let mut pending = Utf8Pending::new();

        // "é" is a 2-byte UTF-8 sequence.
        pending.extend(&[0xC3]);
        pending.flush(&tx);
        assert!(rx.try_recv().is_err());

        pending.extend(&[0xA9]);
        pending.flush(&tx);
        assert_eq!(rx.try_recv().unwrap().unwrap(), "é");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn drops_invalid_leading_bytes_and_makes_progress() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, AiError>>();
        let mut pending = Utf8Pending::new();

        pending.extend(&[0xFF, b'a', b'b']);
        pending.flush(&tx);
        assert!(rx.try_recv().is_err());

        pending.flush(&tx);
        assert_eq!(rx.try_recv().unwrap().unwrap(), "ab");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn compacts_after_many_invalid_bytes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, AiError>>();
        let mut pending = Utf8Pending::new();

        let initial_len = COMPACT_START_THRESHOLD + 500;
        pending.extend(&vec![0xFF; initial_len]);
        assert_eq!(pending.buf.len(), initial_len);
        assert_eq!(pending.start, 0);

        let flushes = COMPACT_START_THRESHOLD + 1;
        for _ in 0..flushes {
            pending.flush(&tx);
        }
        assert!(rx.try_recv().is_err());

        assert_eq!(pending.start, 0);
        assert_eq!(pending.buf.len(), initial_len - flushes);
    }

    #[test]
    fn flush_lossy_emits_replacement_chars() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, AiError>>();
        let mut pending = Utf8Pending::new();

        // Incomplete multi-byte sequence should flush lossy at end.
        pending.extend(&[0xC3]);
        pending.flush_lossy(&tx);

        assert_eq!(rx.try_recv().unwrap().unwrap(), "�");
        assert!(rx.try_recv().is_err());
    }
}

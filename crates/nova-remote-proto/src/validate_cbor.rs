use anyhow::{bail, ensure, Context};

use crate::{MAX_FILE_TEXT_BYTES, MAX_MESSAGE_BYTES, MAX_SYMBOLS_PER_SHARD_INDEX};

/// Conservative validation of a CBOR buffer to avoid allocation bombs during `serde_cbor` decode.
///
/// `serde_cbor::from_slice` may allocate `Vec`/`String` with the capacity derived from attacker-
/// controlled length prefixes *before* it has verified that the input contains enough bytes.
/// A small buffer can therefore trigger very large allocations.
///
/// This validator walks the CBOR structure without allocating and rejects any length prefixes
/// that are:
/// - larger than the remaining bytes in the buffer (can't be valid), or
/// - larger than hard limits suitable for Nova's RPC payloads.
pub(crate) fn validate_cbor(bytes: &[u8]) -> anyhow::Result<()> {
    ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "CBOR payload too large: {} bytes (max {})",
        bytes.len(),
        MAX_MESSAGE_BYTES
    );

    let mut r = Reader::new(bytes);
    validate_item(&mut r, 0).context("validate CBOR root item")?;
    ensure!(
        r.is_empty(),
        "trailing {} bytes after CBOR value",
        r.remaining()
    );
    Ok(())
}

const MAX_CBOR_NESTING: usize = 64;
const MAX_CBOR_MAP_LEN: usize = 1024;

// In practice the largest arrays we carry over the wire are symbols in a shard index.
const MAX_CBOR_ARRAY_LEN: usize = MAX_SYMBOLS_PER_SHARD_INDEX;

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn is_empty(&self) -> bool {
        self.offset >= self.bytes.len()
    }

    fn peek_u8(&self) -> anyhow::Result<u8> {
        ensure!(self.remaining() >= 1, "unexpected EOF");
        Ok(self.bytes[self.offset])
    }

    fn read_u8(&mut self) -> anyhow::Result<u8> {
        let b = self.peek_u8()?;
        self.offset += 1;
        Ok(b)
    }

    fn read_exact(&mut self, len: usize) -> anyhow::Result<&'a [u8]> {
        ensure!(
            self.remaining() >= len,
            "unexpected EOF: need {len} bytes, have {}",
            self.remaining()
        );
        let start = self.offset;
        self.offset += len;
        Ok(&self.bytes[start..start + len])
    }

    fn skip(&mut self, len: usize) -> anyhow::Result<()> {
        let _ = self.read_exact(len)?;
        Ok(())
    }

    fn read_be_u16(&mut self) -> anyhow::Result<u16> {
        let bytes = self.read_exact(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn read_be_u32(&mut self) -> anyhow::Result<u32> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_be_u64(&mut self) -> anyhow::Result<u64> {
        let bytes = self.read_exact(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn read_uint(&mut self, ai: u8) -> anyhow::Result<u64> {
        match ai {
            0..=23 => Ok(ai as u64),
            24 => Ok(self.read_u8()? as u64),
            25 => Ok(self.read_be_u16()? as u64),
            26 => Ok(self.read_be_u32()? as u64),
            27 => Ok(self.read_be_u64()?),
            31 => bail!("indefinite length is not valid here"),
            other => bail!("invalid CBOR additional info: {other}"),
        }
    }

    fn read_len(&mut self, ai: u8, field: &'static str) -> anyhow::Result<usize> {
        let len_u64 = self.read_uint(ai).with_context(|| format!("read {field} length"))?;
        let len: usize = len_u64
            .try_into()
            .map_err(|_| anyhow::anyhow!("{field} length does not fit in usize: {len_u64}"))?;
        Ok(len)
    }
}

fn validate_item(r: &mut Reader<'_>, depth: usize) -> anyhow::Result<()> {
    ensure!(depth <= MAX_CBOR_NESTING, "CBOR nesting too deep");
    let head = r.read_u8().context("read CBOR head")?;
    let major = head >> 5;
    let ai = head & 0x1f;

    match major {
        // unsigned int / negative int
        0 | 1 => {
            let _ = r.read_uint(ai)?;
            Ok(())
        }
        // byte string
        2 => match ai {
            31 => validate_indefinite_bytes(r, depth + 1, false),
            _ => {
                let len = r.read_len(ai, "byte string")?;
                ensure!(
                    len <= MAX_MESSAGE_BYTES,
                    "byte string too large: {len} bytes (max {MAX_MESSAGE_BYTES})"
                );
                ensure!(
                    len <= r.remaining(),
                    "byte string length {len} exceeds remaining bytes ({})",
                    r.remaining()
                );
                r.skip(len)?;
                Ok(())
            }
        },
        // text string
        3 => match ai {
            31 => validate_indefinite_bytes(r, depth + 1, true),
            _ => {
                let len = r.read_len(ai, "text string")?;
                ensure!(
                    len <= MAX_FILE_TEXT_BYTES,
                    "text string too large: {len} bytes (max {MAX_FILE_TEXT_BYTES})"
                );
                ensure!(
                    len <= r.remaining(),
                    "text string length {len} exceeds remaining bytes ({})",
                    r.remaining()
                );
                r.skip(len)?;
                Ok(())
            }
        },
        // array
        4 => match ai {
            31 => validate_indefinite_array(r, depth + 1),
            _ => {
                let len = r.read_len(ai, "array")?;
                ensure!(
                    len <= MAX_CBOR_ARRAY_LEN,
                    "array too long: {len} items (max {MAX_CBOR_ARRAY_LEN})"
                );
                // Each child item requires at least one byte, so a longer array can't fit.
                ensure!(
                    len <= r.remaining(),
                    "array length {len} exceeds remaining bytes ({})",
                    r.remaining()
                );
                for _ in 0..len {
                    validate_item(r, depth + 1)?;
                }
                Ok(())
            }
        },
        // map
        5 => match ai {
            31 => validate_indefinite_map(r, depth + 1),
            _ => {
                let len = r.read_len(ai, "map")?;
                ensure!(
                    len <= MAX_CBOR_MAP_LEN,
                    "map too long: {len} pairs (max {MAX_CBOR_MAP_LEN})"
                );
                // Each key/value requires at least one byte.
                ensure!(
                    len.saturating_mul(2) <= r.remaining(),
                    "map length {len} exceeds remaining bytes ({})",
                    r.remaining()
                );
                for _ in 0..len {
                    validate_item(r, depth + 1).context("validate map key")?;
                    validate_item(r, depth + 1).context("validate map value")?;
                }
                Ok(())
            }
        },
        // tag
        6 => {
            let _ = r.read_uint(ai)?;
            validate_item(r, depth + 1).context("validate tagged item")
        }
        // simple / float
        7 => match ai {
            0..=19 | 20..=23 => Ok(()),
            24 => {
                r.skip(1)?;
                Ok(())
            }
            25 => {
                r.skip(2)?;
                Ok(())
            }
            26 => {
                r.skip(4)?;
                Ok(())
            }
            27 => {
                r.skip(8)?;
                Ok(())
            }
            31 => bail!("unexpected CBOR break"),
            other => bail!("invalid CBOR additional info for major type 7: {other}"),
        },
        other => bail!("unknown CBOR major type: {other}"),
    }
}

fn validate_indefinite_array(r: &mut Reader<'_>, depth: usize) -> anyhow::Result<()> {
    let mut items = 0usize;
    loop {
        let next = r.peek_u8().context("peek indefinite array item")?;
        if next == 0xff {
            r.read_u8()?;
            return Ok(());
        }
        ensure!(
            items < MAX_CBOR_ARRAY_LEN,
            "indefinite array too long (max {MAX_CBOR_ARRAY_LEN})"
        );
        validate_item(r, depth)?;
        items += 1;
    }
}

fn validate_indefinite_map(r: &mut Reader<'_>, depth: usize) -> anyhow::Result<()> {
    let mut pairs = 0usize;
    loop {
        let next = r.peek_u8().context("peek indefinite map item")?;
        if next == 0xff {
            r.read_u8()?;
            return Ok(());
        }
        ensure!(
            pairs < MAX_CBOR_MAP_LEN,
            "indefinite map too long (max {MAX_CBOR_MAP_LEN})"
        );
        validate_item(r, depth).context("validate map key")?;
        validate_item(r, depth).context("validate map value")?;
        pairs += 1;
    }
}

fn validate_indefinite_bytes(r: &mut Reader<'_>, depth: usize, is_text: bool) -> anyhow::Result<()> {
    // Indefinite byte/text strings are a sequence of definite-length chunks terminated by `break`.
    // We validate each chunk individually and also cap the total length.
    let max_total = if is_text {
        MAX_FILE_TEXT_BYTES
    } else {
        MAX_MESSAGE_BYTES
    };

    let mut total = 0usize;
    loop {
        let next = r.peek_u8().context("peek indefinite string chunk")?;
        if next == 0xff {
            r.read_u8()?;
            return Ok(());
        }

        ensure!(depth <= MAX_CBOR_NESTING, "CBOR nesting too deep");

        let head = r.read_u8().context("read chunk head")?;
        let major = head >> 5;
        let ai = head & 0x1f;
        let expected_major = if is_text { 3 } else { 2 };
        ensure!(
            major == expected_major,
            "indefinite {} string chunk has wrong major type: {major}",
            if is_text { "text" } else { "byte" }
        );
        ensure!(ai != 31, "nested indefinite strings are not supported");

        let len = r.read_len(ai, "chunk")?;
        ensure!(
            len <= max_total,
            "indefinite string chunk too large: {len} bytes (max {max_total})"
        );
        ensure!(
            len <= r.remaining(),
            "chunk length {len} exceeds remaining bytes ({})",
            r.remaining()
        );
        total = total.saturating_add(len);
        ensure!(
            total <= max_total,
            "indefinite string total length exceeds limit: {total} bytes (max {max_total})"
        );
        r.skip(len)?;
    }
}


use anyhow::{bail, ensure, Context};

use crate::{
    MAX_DIAGNOSTICS_PER_MESSAGE, MAX_FILES_PER_MESSAGE, MAX_FILE_TEXT_BYTES, MAX_MESSAGE_BYTES,
    MAX_SEARCH_RESULTS_PER_MESSAGE, MAX_SMALL_STRING_BYTES, MAX_SYMBOLS_PER_SHARD_INDEX,
};

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
    validate_item(&mut r, 0, Limits::DEFAULT).context("validate CBOR root item")?;
    ensure!(
        r.is_empty(),
        "trailing {} bytes after CBOR value",
        r.remaining()
    );
    Ok(())
}

const MAX_CBOR_NESTING: usize = 64;
const MAX_CBOR_MAP_LEN: usize = 1024;
const MAX_CBOR_KEY_BYTES: usize = 64;

// For arrays that are expected to contain maps/structs (files, symbols, etc) we require a minimum
// number of bytes per item. This prevents allocation bombs where a small payload declares a very
// large array length of tiny items and triggers a huge `Vec<T>::with_capacity(len)` allocation.
const MIN_BYTES_PER_COMPLEX_ARRAY_ITEM: usize = 8;

#[derive(Clone, Copy)]
struct Limits {
    max_text_len: usize,
    max_bytes_len: usize,
    max_array_len: usize,
    min_array_item_bytes: usize,
}

impl Limits {
    const DEFAULT: Limits = Limits {
        max_text_len: MAX_FILE_TEXT_BYTES,
        max_bytes_len: MAX_MESSAGE_BYTES,
        max_array_len: MAX_SYMBOLS_PER_SHARD_INDEX,
        min_array_item_bytes: 1,
    };
}

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
        let len_u64 = self
            .read_uint(ai)
            .with_context(|| format!("read {field} length"))?;
        let len: usize = len_u64
            .try_into()
            .map_err(|_| anyhow::anyhow!("{field} length does not fit in usize: {len_u64}"))?;
        Ok(len)
    }
}

fn limits_for_map_value(key: &str, base: Limits) -> Limits {
    match key {
        // File payloads.
        "files" => Limits {
            max_array_len: MAX_FILES_PER_MESSAGE,
            min_array_item_bytes: MIN_BYTES_PER_COMPLEX_ARRAY_ITEM,
            ..base
        },

        // Search result payloads.
        "items" => Limits {
            max_array_len: MAX_SEARCH_RESULTS_PER_MESSAGE,
            min_array_item_bytes: MIN_BYTES_PER_COMPLEX_ARRAY_ITEM,
            ..base
        },

        // Diagnostics payloads.
        "diagnostics" => Limits {
            max_array_len: MAX_DIAGNOSTICS_PER_MESSAGE,
            min_array_item_bytes: MIN_BYTES_PER_COMPLEX_ARRAY_ITEM,
            ..base
        },

        // Symbol payloads.
        "symbols" => Limits {
            max_array_len: MAX_SYMBOLS_PER_SHARD_INDEX,
            min_array_item_bytes: MIN_BYTES_PER_COMPLEX_ARRAY_ITEM,
            ..base
        },

        // CBOR byte string, but we also accept an array-of-u8 encoding for compatibility.
        // An array-of-u8 is safe to be 1 byte per item, so do not apply the complex-item ratio.
        "data" => Limits {
            max_bytes_len: MAX_MESSAGE_BYTES,
            max_array_len: MAX_MESSAGE_BYTES,
            min_array_item_bytes: 1,
            ..base
        },

        // Capability lists are expected to be tiny.
        "supported_compression" => Limits {
            max_array_len: 32,
            max_text_len: MAX_SMALL_STRING_BYTES,
            ..base
        },

        // Small strings.
        "path" | "name" | "auth_token" | "message" | "details" | "worker_build" | "query" => Limits {
            max_text_len: MAX_SMALL_STRING_BYTES,
            ..base
        },

        // Enum discriminator strings should always be short.
        "type" | "compression" => Limits {
            max_text_len: 64,
            ..base
        },

        _ => base,
    }
}

fn read_map_key<'a>(r: &mut Reader<'a>) -> anyhow::Result<&'a str> {
    let head = r.read_u8().context("read map key head")?;
    let major = head >> 5;
    let ai = head & 0x1f;
    ensure!(major == 3, "map key must be a text string");
    ensure!(ai != 31, "indefinite-length map keys are not supported");

    let len = r.read_len(ai, "map key")?;
    ensure!(
        len <= MAX_CBOR_KEY_BYTES,
        "map key too long: {len} bytes (max {MAX_CBOR_KEY_BYTES})"
    );
    ensure!(
        len <= r.remaining(),
        "map key length {len} exceeds remaining bytes ({})",
        r.remaining()
    );
    let bytes = r.read_exact(len)?;
    std::str::from_utf8(bytes).context("map key is not valid UTF-8")
}

fn validate_item<'a>(r: &mut Reader<'a>, depth: usize, limits: Limits) -> anyhow::Result<()> {
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
            31 => validate_indefinite_bytes(r, depth + 1, false, limits.max_bytes_len),
            _ => {
                let len = r.read_len(ai, "byte string")?;
                ensure!(
                    len <= limits.max_bytes_len,
                    "byte string too large: {len} bytes (max {})",
                    limits.max_bytes_len
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
            31 => validate_indefinite_bytes(r, depth + 1, true, limits.max_text_len),
            _ => {
                let len = r.read_len(ai, "text string")?;
                ensure!(
                    len <= limits.max_text_len,
                    "text string too large: {len} bytes (max {})",
                    limits.max_text_len
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
            31 => validate_indefinite_array(r, depth + 1, limits),
            _ => {
                let len = r.read_len(ai, "array")?;
                ensure!(
                    len <= limits.max_array_len,
                    "array too long: {len} items (max {})",
                    limits.max_array_len
                );

                let min_item_bytes = limits.min_array_item_bytes.max(1);
                let max_by_remaining = r.remaining().checked_div(min_item_bytes).unwrap_or(0);
                ensure!(
                    len <= max_by_remaining,
                    "array length {len} exceeds remaining bytes ({}) for min item size {min_item_bytes}",
                    r.remaining()
                );
                for _ in 0..len {
                    validate_item(r, depth + 1, limits)?;
                }
                Ok(())
            }
        },
        // map
        5 => match ai {
            31 => validate_indefinite_map(r, depth + 1, limits),
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
                    let key = read_map_key(r).context("validate map key")?;
                    let value_limits = limits_for_map_value(key, limits);
                    validate_item(r, depth + 1, value_limits)
                        .with_context(|| format!("validate map value for key {key:?}"))?;
                }
                Ok(())
            }
        },
        // tag
        6 => {
            let _ = r.read_uint(ai)?;
            validate_item(r, depth + 1, limits).context("validate tagged item")
        }
        // simple / float
        7 => match ai {
            0..=23 => Ok(()),
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

fn validate_indefinite_array<'a>(
    r: &mut Reader<'a>,
    depth: usize,
    limits: Limits,
) -> anyhow::Result<()> {
    let mut items = 0usize;
    loop {
        let next = r.peek_u8().context("peek indefinite array item")?;
        if next == 0xff {
            r.read_u8()?;
            return Ok(());
        }
        ensure!(
            items < limits.max_array_len,
            "indefinite array too long (max {})",
            limits.max_array_len
        );
        validate_item(r, depth, limits)?;
        items += 1;
    }
}

fn validate_indefinite_map<'a>(
    r: &mut Reader<'a>,
    depth: usize,
    limits: Limits,
) -> anyhow::Result<()> {
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
        let key = read_map_key(r).context("validate map key")?;
        let value_limits = limits_for_map_value(key, limits);
        validate_item(r, depth, value_limits)
            .with_context(|| format!("validate map value for key {key:?}"))?;
        pairs += 1;
    }
}

fn validate_indefinite_bytes<'a>(
    r: &mut Reader<'a>,
    depth: usize,
    is_text: bool,
    max_total: usize,
) -> anyhow::Result<()> {
    // Indefinite byte/text strings are a sequence of definite-length chunks terminated by `break`.
    // We validate each chunk individually and also cap the total length.
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

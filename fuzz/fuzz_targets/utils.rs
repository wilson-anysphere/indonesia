use std::str;

pub const MAX_INPUT_SIZE: usize = 256 * 1024;

/// Returns a UTF-8 view of `data` truncated to `MAX_INPUT_SIZE`.
///
/// The fuzzer input is capped to avoid OOM and quadratic behavior on
/// pathological inputs. If the truncated data is not valid UTF-8, we only try
/// trimming up to 3 bytes to recover from cutting a multibyte codepoint.
#[inline]
pub fn truncate_utf8(data: &[u8]) -> Option<&str> {
    let cap = data.len().min(MAX_INPUT_SIZE);
    // If `cap` splits a multibyte codepoint we may need to trim a few bytes.
    for trim in 0..=3 {
        if cap < trim {
            break;
        }
        let slice = &data[..cap - trim];
        if let Ok(text) = str::from_utf8(slice) {
            return Some(text);
        }
    }
    None
}

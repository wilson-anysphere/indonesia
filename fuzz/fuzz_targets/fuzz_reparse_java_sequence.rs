#![no_main]

use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::FuzzRunner;

use nova_syntax::{TextEdit, TextRange};

use nova_fuzz_utils::{truncate_utf8, MAX_INPUT_SIZE};

const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_EDITS: usize = 8;
const MAX_REPLACEMENT_BYTES: usize = 64;
/// Reserve up to this many bytes from the end of the fuzzer input for encoding edit ops.
///
/// Keeping the ops small makes it more likely that `.java` seeds remain mostly intact while still
/// allowing a handful of edits to be described.
const MAX_OP_BYTES: usize = 256;

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    run_case(input);
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| {
        FuzzRunner::new(
            "fuzz_reparse_java_sequence",
            MAX_INPUT_SIZE,
            TIMEOUT,
            init,
            run_one,
        )
    })
}

#[derive(Clone, Copy)]
struct ByteCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn take_u8(&mut self) -> Option<u8> {
        if self.pos >= self.data.len() {
            return None;
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Some(b)
    }

    fn take_u16_le(&mut self) -> Option<u16> {
        let lo = self.take_u8()?;
        let hi = self.take_u8()?;
        Some(u16::from_le_bytes([lo, hi]))
    }

    fn take_bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.remaining() < len {
            return None;
        }
        let start = self.pos;
        self.pos += len;
        Some(&self.data[start..start + len])
    }
}

fn clamp_to_char_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn truncate_str_to_boundary(s: &str, max_len: usize) -> &str {
    let mut end = max_len.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn apply_edit(text: &str, edit: &TextEdit) -> String {
    let start = edit.range.start as usize;
    let end = edit.range.end as usize;

    let mut out =
        String::with_capacity(text.len().saturating_sub(end - start) + edit.replacement.len());
    out.push_str(&text[..start]);
    out.push_str(&edit.replacement);
    out.push_str(&text[end..]);
    out
}

fn decode_edit(cursor: &mut ByteCursor<'_>, text: &str) -> Option<TextEdit> {
    let start_raw = cursor.take_u16_le()? as usize;
    let delete_raw = cursor.take_u16_le()? as usize;
    let replacement_len_raw = cursor.take_u8()? as usize;

    // Bound replacement bytes to keep per-input work small.
    let replacement_len = replacement_len_raw % (MAX_REPLACEMENT_BYTES + 1);
    let replacement_bytes = cursor.take_bytes(replacement_len)?;
    let mut replacement = String::from_utf8_lossy(replacement_bytes).into_owned();

    let text_len = text.len();
    let start = if text_len == 0 {
        0
    } else {
        clamp_to_char_boundary(text, start_raw % (text_len + 1))
    };

    let max_delete = text_len.saturating_sub(start);
    let end = clamp_to_char_boundary(text, start + (delete_raw % (max_delete + 1)));

    // Enforce the global document size cap by truncating the replacement if needed.
    //
    // We truncate the *replacement* (not the whole result) so that the `TextEdit` remains
    // consistent with the produced `new_text`.
    let deleted_len = end.saturating_sub(start);
    let base_len = text_len.saturating_sub(deleted_len);
    let allowed_insert_len = MAX_INPUT_SIZE.saturating_sub(base_len);
    if replacement.len() > allowed_insert_len {
        replacement = truncate_str_to_boundary(&replacement, allowed_insert_len).to_owned();
    }

    Some(TextEdit::new(TextRange::new(start, end), replacement))
}

fn run_case(data: &[u8]) {
    let cap = data.len().min(MAX_INPUT_SIZE);
    let data = &data[..cap];

    if data.is_empty() {
        return;
    }

    // Split the input into an initial UTF-8 buffer and a small "edit op" stream.
    // Keep at least half the bytes for the initial text so short `.java` seeds don't end up with
    // an empty (or near-empty) document.
    let max_op_len = (data.len() / 2).min(MAX_OP_BYTES);
    let op_len = if max_op_len == 0 {
        0
    } else {
        (data[0] as usize % (max_op_len + 1)).min(data.len())
    };
    let split = data.len().saturating_sub(op_len);
    let (text_bytes, op_bytes) = data.split_at(split);

    let Some(text0) = truncate_utf8(text_bytes) else {
        return;
    };

    let mut text = text0.to_owned();
    let mut parse = nova_syntax::parse_java(&text);

    let mut cursor = ByteCursor::new(op_bytes);
    for _ in 0..MAX_EDITS {
        let Some(edit) = decode_edit(&mut cursor, &text) else {
            break;
        };

        let new_text = apply_edit(&text, &edit);
        debug_assert!(new_text.len() <= MAX_INPUT_SIZE);

        let full = nova_syntax::parse_java(&new_text);
        let incr = nova_syntax::reparse_java(&parse, &text, edit, &new_text);
        assert_eq!(incr, full);

        text = new_text;
        parse = incr;
    }
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});

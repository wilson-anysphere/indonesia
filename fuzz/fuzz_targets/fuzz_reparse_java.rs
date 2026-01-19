#![no_main]

use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::{truncate_utf8, FuzzRunner, MAX_INPUT_SIZE};

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OP_BYTES: usize = 256;
const MAX_REPLACEMENT_BYTES: usize = 64;

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    let cap = input.len().min(MAX_INPUT_SIZE);
    let input = &input[..cap];
    if input.is_empty() {
        return;
    }

    // Split the input into an initial UTF-8 buffer and a small "edit op" stream.
    let op_len = (input[0] as usize % (MAX_OP_BYTES + 1)).min(input.len());
    let split = input.len().saturating_sub(op_len);
    let (text_bytes, op_bytes) = input.split_at(split);

    let Some(old_text) = truncate_utf8(text_bytes) else {
        return;
    };

    let mut cursor = ByteCursor::new(op_bytes);
    let edit =
        decode_edit(&mut cursor, old_text).unwrap_or_else(|| nova_syntax::TextEdit::insert(0, ""));
    let new_text = apply_edit(
        old_text,
        edit.range.start as usize,
        edit.range.end as usize,
        &edit.replacement,
    );

    let old_parse = nova_syntax::parse_java(old_text);
    let full_new = nova_syntax::parse_java(&new_text);
    let incr_new = nova_syntax::reparse_java(&old_parse, old_text, edit, &new_text);

    // Reparsing must remain lossless.
    assert_eq!(incr_new.syntax().text().to_string(), new_text);

    // Differential check: incremental reparsing should match a full parse.
    assert_eq!(incr_new, full_new);
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| {
        FuzzRunner::new(
            "fuzz_reparse_java",
            MAX_INPUT_SIZE,
            TIMEOUT,
            init,
            run_one,
        )
    })
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

fn apply_edit(old_text: &str, start: usize, end: usize, replacement: &str) -> String {
    debug_assert!(start <= end);
    debug_assert!(old_text.is_char_boundary(start));
    debug_assert!(old_text.is_char_boundary(end));

    let mut new_text = String::with_capacity(old_text.len() - (end - start) + replacement.len());
    new_text.push_str(&old_text[..start]);
    new_text.push_str(replacement);
    new_text.push_str(&old_text[end..]);
    new_text
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

fn decode_edit(cursor: &mut ByteCursor<'_>, text: &str) -> Option<nova_syntax::TextEdit> {
    let start_raw = cursor.take_u16_le()? as usize;
    let delete_raw = cursor.take_u16_le()? as usize;
    let replacement_len_raw = cursor.take_u8()? as usize;

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

    Some(nova_syntax::TextEdit::new(
        nova_syntax::TextRange::new(start, end),
        replacement,
    ))
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});

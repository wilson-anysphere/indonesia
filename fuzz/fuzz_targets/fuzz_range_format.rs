#![no_main]

use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::{truncate_utf8, FuzzRunner, MAX_INPUT_SIZE};

const TIMEOUT: Duration = Duration::from_secs(2);

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    let Some(text) = truncate_utf8(input) else {
        return;
    };
    let bytes = text.as_bytes();

    let mut config = nova_format::FormatConfig::default();
    if let Some(byte) = bytes.get(16) {
        config.indent_width = 1 + (*byte as usize % 8);
    }
    if let Some(byte) = bytes.get(17) {
        config.indent_style = if byte & 1 == 0 {
            nova_format::IndentStyle::Spaces
        } else {
            nova_format::IndentStyle::Tabs
        };
    }
    if let Some(byte) = bytes.get(18) {
        config.max_line_length = 20 + (*byte as usize % 200);
    }
    if let Some(byte) = bytes.get(19) {
        config.insert_final_newline = match byte % 3 {
            0 => None,
            1 => Some(false),
            _ => Some(true),
        };
    }
    if let Some(byte) = bytes.get(20) {
        config.trim_final_newlines = match byte % 3 {
            0 => None,
            1 => Some(false),
            _ => Some(true),
        };
    }

    let tree = nova_syntax::parse(text);
    let line_index = nova_core::LineIndex::new(text);

    let len_plus_one = text.len().saturating_add(1);
    let start = clamp_to_char_boundary(text, (read_u64_le(bytes, 0) as usize) % len_plus_one);
    let end = clamp_to_char_boundary(text, (read_u64_le(bytes, 8) as usize) % len_plus_one);
    let (start, end) = if start <= end { (start, end) } else { (end, start) };

    let start = nova_core::TextSize::from(start as u32);
    let end = nova_core::TextSize::from(end as u32);
    let byte_range = nova_core::TextRange::new(start, end);
    let range = line_index.range(text, byte_range);

    let res = nova_format::edits_for_range_formatting(&tree, text, range, &config);
    if let Ok(edits) = res {
        let selected = line_index
            .text_range(text, range)
            .expect("range should convert back to a byte range");
        let selected_start = u32::from(selected.start()) as usize;
        let selected_end = u32::from(selected.end()) as usize;

        let formatted = nova_core::apply_text_edits(text, &edits).expect("edits must apply");

        // Range formatting should preserve the text outside the range.
        assert!(formatted.starts_with(&text[..selected_start]));
        assert!(formatted.ends_with(&text[selected_end..]));

        for edit in &edits {
            assert!(
                edit.range.start() >= selected.start() && edit.range.end() <= selected.end(),
                "range formatting produced an out-of-range edit: {edit:?} not within {selected:?}",
            );
        }
    }
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new("fuzz_range_format", MAX_INPUT_SIZE, TIMEOUT, init, run_one))
}

fn read_u64_le(bytes: &[u8], offset: usize) -> u64 {
    let mut buf = [0u8; 8];
    if offset >= bytes.len() {
        return 0;
    }
    let len = (bytes.len() - offset).min(8);
    buf[..len].copy_from_slice(&bytes[offset..offset + len]);
    u64::from_le_bytes(buf)
}

fn clamp_to_char_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});

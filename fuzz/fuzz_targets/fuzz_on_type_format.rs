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

    let tree = nova_syntax::parse(text);
    let line_index = nova_core::LineIndex::new(text);

    let len_plus_one = text.len().saturating_add(1);
    let offset = clamp_to_char_boundary(text, (read_u64_le(bytes, 0) as usize) % len_plus_one);
    let offset = nova_core::TextSize::from(offset as u32);
    let position = line_index.position(text, offset);

    let triggers = ['}', ';', ')', ','];
    let trigger_idx = bytes.get(8).copied().unwrap_or(0) as usize % triggers.len();
    let ch = triggers[trigger_idx];

    if let Ok(edits) = nova_format::edits_for_on_type_formatting(&tree, text, position, ch, &config)
    {
        let line_start = line_index
            .line_start(position.line)
            .expect("position line should be in bounds");
        let line_end = line_index
            .line_end(position.line)
            .expect("position line should be in bounds");
        let line_start_usize = u32::from(line_start) as usize;
        let line_end_usize = u32::from(line_end) as usize;

        let formatted = nova_core::apply_text_edits(text, &edits).expect("edits must apply");

        // On-type formatting should only affect the current line's content.
        assert!(formatted.starts_with(&text[..line_start_usize]));
        assert!(formatted.ends_with(&text[line_end_usize..]));

        for edit in &edits {
            assert!(
                edit.range.start() >= line_start && edit.range.end() <= line_end,
                "on-type formatting produced an out-of-line edit: {edit:?} not within {line_start:?}..{line_end:?}",
            );
        }
    }
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| {
        FuzzRunner::new(
            "fuzz_on_type_format",
            MAX_INPUT_SIZE,
            TIMEOUT,
            init,
            run_one,
        )
    })
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

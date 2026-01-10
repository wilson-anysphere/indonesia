/// Indents each non-empty line in `block` with `indent`.
#[must_use]
pub fn indent_block(block: &str, indent: &str) -> String {
    let mut out = String::with_capacity(block.len() + indent.len() * 4);
    for (idx, line) in block.split_inclusive('\n').enumerate() {
        let line_stripped = line.strip_suffix('\n').unwrap_or(line);
        if !line_stripped.trim().is_empty() {
            out.push_str(indent);
        } else if idx == 0 && line_stripped.is_empty() {
            // Preserve leading empty line without indentation.
        }
        out.push_str(line_stripped);
        if line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Removes common leading indentation from all non-empty lines in `block`.
#[must_use]
pub fn dedent_block(block: &str) -> String {
    let lines: Vec<&str> = block.lines().collect();
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).count())
        .min()
        .unwrap_or(0);

    let mut out = String::with_capacity(block.len());
    for (idx, line) in lines.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if line.trim().is_empty() {
            out.push_str(line);
        } else {
            let mut byte_idx = 0usize;
            let mut removed = 0usize;
            for (i, ch) in line.char_indices() {
                if removed >= min_indent {
                    break;
                }
                if ch.is_whitespace() {
                    byte_idx = i + ch.len_utf8();
                    removed += 1;
                } else {
                    break;
                }
            }
            out.push_str(&line[byte_idx..]);
        }
    }
    out
}

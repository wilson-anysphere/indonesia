#[derive(Debug, Default, Clone)]
pub struct Formatter;

impl Formatter {
    pub fn format_java(&self, source: &str) -> String {
        let normalized = source.replace("\r\n", "\n");
        let mut out = String::new();
        for (idx, line) in normalized.split('\n').enumerate() {
            if idx > 0 {
                out.push('\n');
            }
            out.push_str(line.trim_end());
        }

        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }

        out
    }
}


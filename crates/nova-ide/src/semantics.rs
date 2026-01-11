use nova_core::Line;

/// A valid location for a line breakpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointSite {
    pub line: Line,
    pub enclosing_class: Option<String>,
    pub enclosing_method: Option<String>,
}

/// Collect a conservative set of executable line breakpoint sites from a Java
/// source file.
///
/// This is *not* a full Java parser; it is a lightweight heuristic suitable for
/// unit tests and for exercising the DAP breakpoint mapping path.
pub fn collect_breakpoint_sites(java_source: &str) -> Vec<BreakpointSite> {
    let mut sites = Vec::new();

    let mut in_block_comment = false;
    let mut brace_depth: usize = 0;

    let mut current_package: Option<String> = None;
    let mut current_class: Option<String> = None;
    let mut current_method: Option<String> = None;
    let mut method_body_depth: Option<usize> = None;

    for (idx, raw_line) in java_source.lines().enumerate() {
        let line_no: Line = (idx + 1) as Line;
        let mut line = raw_line.trim();

        // Handle block comments in a minimal way.
        if in_block_comment {
            if let Some(end) = line.find("*/") {
                line = line[end + 2..].trim();
                in_block_comment = false;
            } else {
                continue;
            }
        }

        if line.starts_with("/*") {
            in_block_comment = true;
            if let Some(end) = line.find("*/") {
                line = line[end + 2..].trim();
                in_block_comment = false;
            } else {
                continue;
            }
        }

        if line.starts_with("//") || line.is_empty() {
            // Still need to update brace depth for lines like `// }`.
            brace_depth = update_brace_depth(brace_depth, raw_line);
            // Might have closed a method.
            if let Some(body_depth) = method_body_depth {
                if brace_depth < body_depth {
                    current_method = None;
                    method_body_depth = None;
                }
            }
            continue;
        }

        if current_package.is_none() && line.starts_with("package ") {
            if let Some(pkg) = line
                .strip_prefix("package ")
                .and_then(|rest| rest.strip_suffix(';'))
                .map(str::trim)
            {
                if !pkg.is_empty() {
                    current_package = Some(pkg.to_string());
                }
            }
            brace_depth = update_brace_depth(brace_depth, raw_line);
            continue;
        }

        // Track enclosing class name.
        if current_class.is_none() {
            if let Some(name) = extract_decl_name(line, "class") {
                current_class = Some(name);
            }
        } else if line.contains("class ") {
            // Nested class - update current_class conservatively.
            if let Some(name) = extract_decl_name(line, "class") {
                current_class = Some(name);
            }
        }

        // Detect method declarations when we're not already inside a method.
        if current_method.is_none() && looks_like_method_decl(line) {
            if let Some(name) = extract_method_name(line) {
                current_method = Some(name);
                // We will update `brace_depth` at the end of the loop; however
                // method bodies are considered active starting on the next line
                // after the opening `{`.
                let next_depth = brace_depth
                    .saturating_add(count_char(raw_line, '{'))
                    .saturating_sub(count_char(raw_line, '}'));
                method_body_depth = Some(next_depth);
            }
        } else if current_method.is_none()
            && current_class.is_some()
            && looks_like_constructor_decl(line, current_class.as_deref().unwrap())
        {
            if let Some(name) = extract_method_name(line) {
                current_method = Some(name);
                let next_depth = brace_depth
                    .saturating_add(count_char(raw_line, '{'))
                    .saturating_sub(count_char(raw_line, '}'));
                method_body_depth = Some(next_depth);
            }
        }

        // Consider lines inside a method body as potential breakpoint sites.
        let inside_method = match method_body_depth {
            Some(body_depth) => brace_depth >= body_depth,
            None => false,
        };

        if inside_method && is_executable_line(line) {
            let enclosing_class = current_class.as_ref().map(|class| {
                if let Some(pkg) = &current_package {
                    format!("{pkg}.{class}")
                } else {
                    class.clone()
                }
            });
            sites.push(BreakpointSite {
                line: line_no,
                enclosing_class,
                enclosing_method: current_method.clone(),
            });
        }

        brace_depth = update_brace_depth(brace_depth, raw_line);

        // If we just closed the method, clear the current method.
        if let Some(body_depth) = method_body_depth {
            if brace_depth < body_depth {
                current_method = None;
                method_body_depth = None;
            }
        }
    }

    sites
}

fn update_brace_depth(current: usize, line: &str) -> usize {
    let opens = count_char(line, '{');
    let closes = count_char(line, '}');
    current.saturating_add(opens).saturating_sub(closes)
}

fn count_char(line: &str, c: char) -> usize {
    line.chars().filter(|ch| *ch == c).count()
}

fn extract_decl_name(line: &str, keyword: &str) -> Option<String> {
    let mut iter = line.split_whitespace().peekable();
    while let Some(tok) = iter.next() {
        if tok == keyword {
            return iter
                .peek()
                .map(|s| s.trim_matches('{').trim_matches(';').to_string());
        }
    }
    None
}

fn looks_like_method_decl(line: &str) -> bool {
    if !line.contains('(') || !line.contains(')') || !line.contains('{') {
        return false;
    }

    let trimmed = line.trim_start();
    // Skip common control statements.
    matches!(
        trimmed.split_whitespace().next(),
        Some("if" | "for" | "while" | "switch" | "catch" | "do" | "try" | "synchronized")
    )
    .not()
}

fn looks_like_constructor_decl(line: &str, class_name: &str) -> bool {
    let needle = format!("{class_name}(");
    line.contains(&needle) && line.contains('{')
}

fn extract_method_name(line: &str) -> Option<String> {
    let paren = line.find('(')?;
    let before = &line[..paren];
    let name = before
        .split_whitespace()
        .last()
        .map(|s| s.trim_end_matches('<').trim_end_matches('>'))?;

    // Drop generic / return type artifacts; keep identifier-ish.
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn is_executable_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("//") {
        return false;
    }
    matches!(trimmed, "{" | "}" | "};").not()
}

trait BoolExt {
    fn not(self) -> bool;
}

impl BoolExt for bool {
    fn not(self) -> bool {
        !self
    }
}

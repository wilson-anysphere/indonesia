use crate::edit::WorkspaceEdit;
use crate::java::{find_class, find_method_decl, list_fields, list_methods, ClassBlock};
use crate::move_java::{FileEdit, RefactoringEdit};
use nova_format::{dedent_block, indent_block};
use nova_index::TextRange;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct MoveStaticMemberParams {
    pub from_class: String,
    pub member_name: String,
    pub to_class: String,
}

#[derive(Clone, Debug)]
pub struct MoveMethodParams {
    pub from_class: String,
    pub method_name: String,
    pub to_class: String,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MoveMemberError {
    #[error("class '{0}' not found")]
    ClassNotFound(String),
    #[error("member '{member}' not found in class '{class}'")]
    MemberNotFound { class: String, member: String },
    #[error("member '{member}' already exists in destination class '{class}'")]
    NameCollision { class: String, member: String },
    #[error("method '{method}' accesses private member '{member}' of '{class}'")]
    PrivateMemberAccess {
        class: String,
        method: String,
        member: String,
    },
    #[error("no unique field of type '{ty}' found in class '{class}'")]
    NoUniqueFieldOfType { class: String, ty: String },
    #[error("unsupported call site for '{method}' in file '{file}'")]
    UnsupportedCallSite { method: String, file: PathBuf },
    #[error("unsupported construct in moved method '{method}': {reason}")]
    UnsupportedMethod { method: String, reason: String },
    #[error("internal error: missing file contents for '{0}'")]
    MissingFileContents(PathBuf),
    #[error(transparent)]
    Edit(#[from] crate::edit::EditError),
}

#[derive(Clone, Debug)]
struct LocalEdit {
    range: TextRange,
    replacement: String,
}

fn apply_local_edits(text: &str, mut edits: Vec<LocalEdit>) -> String {
    edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
    let mut out = text.to_string();
    for edit in edits {
        out.replace_range(edit.range.start..edit.range.end, &edit.replacement);
    }
    out
}

fn find_file_containing_class(
    files: &BTreeMap<PathBuf, String>,
    class_name: &str,
) -> Option<(PathBuf, ClassBlock)> {
    for (path, text) in files {
        if let Some(class) = find_class(text, class_name) {
            return Some((path.clone(), class));
        }
    }
    None
}

fn class_indent(text: &str, class: &ClassBlock) -> String {
    let body = &text[class.body_range.start..class.body_range.end];
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let indent_len = line.len() - trimmed.len();
        return line[..indent_len].to_string();
    }
    "    ".to_string()
}

fn insert_before_class_close(text: &str, class: &ClassBlock, insert: &str) -> LocalEdit {
    let insert_pos = class.body_range.end;
    let mut insertion = String::new();
    let prefix = &text[..insert_pos];
    if !prefix.ends_with('\n') {
        insertion.push('\n');
    }
    let body_text = &text[class.body_range.start..class.body_range.end];
    if !body_text.trim().is_empty() && !prefix.ends_with("\n\n") {
        insertion.push('\n');
    }
    insertion.push_str(insert);
    if !insertion.ends_with('\n') {
        insertion.push('\n');
    }
    LocalEdit {
        range: TextRange::new(insert_pos, insert_pos),
        replacement: insertion,
    }
}

fn find_static_field_range(text: &str, class: &ClassBlock, member_name: &str) -> Option<TextRange> {
    let body = &text[class.body_range.start..class.body_range.end];
    let pattern = format!(
        r"(?m)^[^\n]*\bstatic\b[^\n]*\b{}\b[^\n]*;\s*$",
        regex::escape(member_name)
    );
    let re = Regex::new(&pattern).expect("valid regex");
    let m = re.find(body)?;
    Some(TextRange::new(
        class.body_range.start + m.start(),
        class.body_range.start + m.end(),
    ))
}

fn collect_static_reference_edits(
    file: &PathBuf,
    text: &str,
    skip_range: Option<TextRange>,
    from_class: &str,
    to_class: &str,
    member_name: &str,
) -> Vec<LocalEdit> {
    let mut edits = Vec::new();

    // Qualified access: `A.add` (allow whitespace around dot).
    let qualified_pat = format!(
        r"\b{}\s*\.\s*{}\b",
        regex::escape(from_class),
        regex::escape(member_name)
    );
    let qualified_re = Regex::new(&qualified_pat).expect("valid regex");
    for m in qualified_re.find_iter(text) {
        let range = TextRange::new(m.start(), m.end());
        if let Some(skip) = skip_range {
            if range.start < skip.end && skip.start < range.end {
                continue;
            }
        }
        edits.push(LocalEdit {
            range,
            replacement: format!("{to_class}.{member_name}"),
        });
    }

    // Static import: `import static A.add;`
    let import_pat = format!(
        r"(?m)^\s*import\s+static\s+{}\s*\.\s*{}\s*;\s*$",
        regex::escape(from_class),
        regex::escape(member_name)
    );
    let import_re = Regex::new(&import_pat).expect("valid regex");
    for m in import_re.find_iter(text) {
        let range = TextRange::new(m.start(), m.end());
        if let Some(skip) = skip_range {
            if range.start < skip.end && skip.start < range.end {
                continue;
            }
        }
        let old_line = &text[range.start..range.end];
        edits.push(LocalEdit {
            range,
            replacement: old_line.replace(from_class, to_class),
        });
    }

    let _ = file;
    edits
}

/// Move a static member (static method or static field) between two classes.
///
/// Returns Nova's canonical [`WorkspaceEdit`]. Internally this refactoring is still implemented
/// by producing a legacy [`RefactoringEdit`] and converting it to the canonical edit model.
pub fn move_static_member(
    files: &BTreeMap<PathBuf, String>,
    params: MoveStaticMemberParams,
) -> Result<WorkspaceEdit, MoveMemberError> {
    let (from_path, from_class) = find_file_containing_class(files, &params.from_class)
        .ok_or_else(|| MoveMemberError::ClassNotFound(params.from_class.clone()))?;
    let (to_path, to_class) = find_file_containing_class(files, &params.to_class)
        .ok_or_else(|| MoveMemberError::ClassNotFound(params.to_class.clone()))?;

    let from_text = files
        .get(&from_path)
        .expect("from_path returned from file map");
    let to_text = files.get(&to_path).expect("to_path returned from file map");

    // Find the member as either a static method or a static field.
    let method_decl = find_method_decl(from_text, &from_class, &params.member_name);
    let (member_range, is_method) = if let Some(decl) = method_decl {
        let snippet = &from_text[decl.range.start..decl.range.end];
        if !snippet.contains("static") {
            return Err(MoveMemberError::MemberNotFound {
                class: params.from_class.clone(),
                member: params.member_name.clone(),
            });
        }
        (decl.range, true)
    } else {
        let range = find_static_field_range(from_text, &from_class, &params.member_name)
            .ok_or_else(|| MoveMemberError::MemberNotFound {
                class: params.from_class.clone(),
                member: params.member_name.clone(),
            })?;
        (range, false)
    };

    // Collision detection in destination class.
    let to_body = &to_text[to_class.body_range.start..to_class.body_range.end];
    let collision_pat = if is_method {
        format!(r"\b{}\s*\(", regex::escape(&params.member_name))
    } else {
        format!(r"\b{}\b", regex::escape(&params.member_name))
    };
    if Regex::new(&collision_pat)
        .expect("valid regex")
        .is_match(to_body)
    {
        return Err(MoveMemberError::NameCollision {
            class: params.to_class.clone(),
            member: params.member_name.clone(),
        });
    }

    let member_text = from_text[member_range.start..member_range.end].to_string();
    let member_text = dedent_block(member_text.trim_matches('\n'));
    let dest_indent = class_indent(to_text, &to_class);
    let moved_member = indent_block(&member_text, &dest_indent);

    // Build per-file edits (structural + reference rewrites).
    let mut edits_by_file: HashMap<PathBuf, Vec<LocalEdit>> = HashMap::new();

    edits_by_file
        .entry(from_path.clone())
        .or_default()
        .push(LocalEdit {
            range: member_range,
            replacement: String::new(),
        });
    edits_by_file
        .entry(to_path.clone())
        .or_default()
        .push(insert_before_class_close(to_text, &to_class, &moved_member));

    for (path, text) in files {
        let skip = if path == &from_path {
            Some(member_range)
        } else {
            None
        };
        let ref_edits = collect_static_reference_edits(
            path,
            text,
            skip,
            &params.from_class,
            &params.to_class,
            &params.member_name,
        );
        if !ref_edits.is_empty() {
            edits_by_file
                .entry(path.clone())
                .or_default()
                .extend(ref_edits);
        }
    }

    let mut out = RefactoringEdit::default();
    for (path, mut edits) in edits_by_file {
        let Some(original) = files.get(&path).map(String::as_str) else {
            return Err(MoveMemberError::MissingFileContents(path));
        };
        let updated = apply_local_edits(original, {
            edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
            edits
        });
        if updated != original {
            out.file_edits.push(FileEdit {
                path,
                new_contents: updated,
            });
        }
    }

    Ok(out.to_workspace_edit(files)?)
}

/// Move a static member (static method or static field) between two classes, returning Nova's
/// canonical [`WorkspaceEdit`].
pub fn move_static_member_workspace_edit(
    files: &BTreeMap<PathBuf, String>,
    params: MoveStaticMemberParams,
) -> Result<WorkspaceEdit, MoveMemberError> {
    move_static_member(files, params)
}

fn identifier_tokens(text: &str) -> Vec<(usize, usize, String)> {
    let mut tokens = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if ch.is_ascii_alphabetic() || ch == '_' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                let c = bytes[i] as char;
                if c.is_ascii_alphanumeric() || c == '_' {
                    i += 1;
                } else {
                    break;
                }
            }
            tokens.push((start, i, text[start..i].to_string()));
            continue;
        }
        i += 1;
    }
    tokens
}

fn prev_non_ws_char(text: &str, idx: usize) -> Option<char> {
    text[..idx].chars().rev().find(|c| !c.is_whitespace())
}

fn rewrite_method_body(
    body: &str,
    a_param: Option<&str>,
    b_field: &str,
    a_members: &BTreeSet<String>,
) -> String {
    // First collapse `<a_param>.<b_field>` into `this` when possible.
    let mut out = body.to_string();
    if let Some(a_param) = a_param {
        let pat = format!(
            r"\b{}\s*\.\s*{}\b",
            regex::escape(a_param),
            regex::escape(b_field)
        );
        let re = Regex::new(&pat).expect("valid regex");
        out = re.replace_all(&out, "this").to_string();
    }

    let mut rewritten = String::with_capacity(out.len());
    let mut last = 0usize;
    for (start, end, ident) in identifier_tokens(&out) {
        rewritten.push_str(&out[last..start]);

        if ident == "this" {
            if let Some(a_param) = a_param {
                rewritten.push_str(a_param);
            } else {
                rewritten.push_str("this");
            }
        } else if ident == b_field {
            // Unqualified `b` becomes `this` in the moved method.
            if prev_non_ws_char(&out, start) == Some('.') {
                rewritten.push_str(&ident);
            } else {
                rewritten.push_str("this");
            }
        } else if let Some(a_param) = a_param {
            if a_members.contains(&ident) && prev_non_ws_char(&out, start) != Some('.') {
                rewritten.push_str(a_param);
                rewritten.push('.');
                rewritten.push_str(&ident);
            } else {
                rewritten.push_str(&ident);
            }
        } else {
            rewritten.push_str(&ident);
        }

        last = end;
    }
    rewritten.push_str(&out[last..]);
    rewritten
}

fn insert_receiver_param(signature: &str, receiver_ty: &str, receiver_name: &str) -> String {
    let Some(paren) = signature.find('(') else {
        return signature.to_string();
    };
    let close_paren = signature[paren..]
        .find(')')
        .map(|p| paren + p)
        .unwrap_or(signature.len());
    let inside = &signature[paren + 1..close_paren];
    let mut out = String::new();
    out.push_str(&signature[..paren + 1]);
    if inside.trim().is_empty() {
        out.push_str(receiver_ty);
        out.push(' ');
        out.push_str(receiver_name);
    } else {
        out.push_str(receiver_ty);
        out.push(' ');
        out.push_str(receiver_name);
        out.push_str(", ");
    }
    out.push_str(inside);
    out.push_str(&signature[close_paren..]);
    out
}

fn choose_receiver_name(existing_params: &[String]) -> String {
    let base = "a".to_string();
    if !existing_params.contains(&base) {
        return base;
    }
    let mut idx = 0usize;
    loop {
        let candidate = format!("a{idx}");
        if !existing_params.contains(&candidate) {
            return candidate;
        }
        idx += 1;
    }
}

fn parse_param_names(signature: &str) -> Vec<String> {
    let Some(paren) = signature.find('(') else {
        return Vec::new();
    };
    let close = signature[paren..]
        .find(')')
        .map(|p| paren + p)
        .unwrap_or(signature.len());
    let inside = &signature[paren + 1..close];
    let mut names = Vec::new();
    for param in inside.split(',') {
        let param = param.trim();
        if param.is_empty() {
            continue;
        }
        let parts: Vec<_> = param.split_whitespace().collect();
        if let Some(name) = parts.last() {
            names.push((*name).to_string());
        }
    }
    names
}

fn detect_unhandled_dot_calls(text: &str, method_name: &str) -> bool {
    let any_call_re =
        Regex::new(&format!(r"\.\s*{}\s*\(", regex::escape(method_name))).expect("valid regex");
    for m in any_call_re.find_iter(text) {
        let dot = m.start();
        let prev = text[..dot].chars().rev().find(|c| !c.is_whitespace());
        let ok = prev
            .map(|c| c.is_ascii_alphanumeric() || c == '_')
            .unwrap_or(false);
        if !ok {
            return true;
        }
    }
    false
}

/// Move an instance method from one class to another.
///
/// Returns Nova's canonical [`WorkspaceEdit`]. Internally this refactoring is still implemented
/// by producing a legacy [`RefactoringEdit`] and converting it to the canonical edit model.
pub fn move_method(
    files: &BTreeMap<PathBuf, String>,
    params: MoveMethodParams,
) -> Result<WorkspaceEdit, MoveMemberError> {
    let (from_path, from_class) = find_file_containing_class(files, &params.from_class)
        .ok_or_else(|| MoveMemberError::ClassNotFound(params.from_class.clone()))?;
    let (to_path, to_class) = find_file_containing_class(files, &params.to_class)
        .ok_or_else(|| MoveMemberError::ClassNotFound(params.to_class.clone()))?;

    let from_text = files
        .get(&from_path)
        .expect("from_path returned from file map");
    let to_text = files.get(&to_path).expect("to_path returned from file map");

    let method_decl =
        find_method_decl(from_text, &from_class, &params.method_name).ok_or_else(|| {
            MoveMemberError::MemberNotFound {
                class: params.from_class.clone(),
                member: params.method_name.clone(),
            }
        })?;

    let original_method_text = &from_text[method_decl.range.start..method_decl.range.end];
    let original_method_text = dedent_block(original_method_text.trim_matches('\n'));
    if original_method_text.contains("static") {
        return Err(MoveMemberError::UnsupportedMethod {
            method: params.method_name.clone(),
            reason:
                "move_method only supports instance methods (use move_static_member for static)"
                    .into(),
        });
    }
    if original_method_text.contains("super") {
        return Err(MoveMemberError::UnsupportedMethod {
            method: params.method_name.clone(),
            reason: "method references 'super'".into(),
        });
    }

    // Find the field in A that provides the B instance (receiver in the moved call sites).
    let fields = list_fields(from_text, &from_class);
    let candidates: Vec<_> = fields.iter().filter(|f| f.ty == params.to_class).collect();
    if candidates.len() != 1 {
        return Err(MoveMemberError::NoUniqueFieldOfType {
            class: params.from_class.clone(),
            ty: params.to_class.clone(),
        });
    }
    let b_field = candidates[0].name.clone();

    // Name collision in destination.
    if find_method_decl(to_text, &to_class, &params.method_name).is_some() {
        return Err(MoveMemberError::NameCollision {
            class: params.to_class.clone(),
            member: params.method_name.clone(),
        });
    }

    let a_fields = list_fields(from_text, &from_class);
    let a_methods = list_methods(from_text, &from_class);

    let mut a_member_names = BTreeSet::new();
    let mut private_members = BTreeSet::new();

    for f in &a_fields {
        if f.name != b_field {
            a_member_names.insert(f.name.clone());
        }
        if f.is_private {
            private_members.insert(f.name.clone());
        }
    }
    for m in &a_methods {
        if m.name == params.method_name {
            continue;
        }
        if !m.is_static {
            a_member_names.insert(m.name.clone());
        }
        if m.is_private {
            private_members.insert(m.name.clone());
        }
    }

    // Very conservative: if the method mentions a private member name, refuse.
    for priv_name in &private_members {
        let pat = format!(r"\b{}\b", regex::escape(priv_name));
        let re = Regex::new(&pat).expect("valid regex");
        if re.is_match(&original_method_text) {
            return Err(MoveMemberError::PrivateMemberAccess {
                class: params.from_class.clone(),
                method: params.method_name.clone(),
                member: priv_name.clone(),
            });
        }
    }

    // Determine whether we need the original receiver parameter.
    let needs_receiver_param = {
        if Regex::new(r"\bthis\b")
            .unwrap()
            .is_match(&original_method_text)
        {
            let pat = format!(r"\bthis\s*\.\s*{}\b", regex::escape(&b_field));
            let re = Regex::new(&pat).unwrap();
            let mut stripped = re.replace_all(&original_method_text, "").to_string();
            stripped = Regex::new(r"\bthis\b")
                .unwrap()
                .replace_all(&stripped, "")
                .to_string();
            a_member_names.iter().any(|name| {
                Regex::new(&format!(r"\b{}\b", regex::escape(name)))
                    .unwrap()
                    .is_match(&stripped)
            })
        } else {
            a_member_names.iter().any(|name| {
                Regex::new(&format!(r"\b{}\b", regex::escape(name)))
                    .unwrap()
                    .is_match(&original_method_text)
            })
        }
    };

    // Split method into signature and body so we can rewrite body without touching modifiers etc.
    let brace_open = original_method_text.find('{').expect("method has body");
    let brace_close = original_method_text.rfind('}').expect("method has body");
    let signature = &original_method_text[..brace_open];
    let body = &original_method_text[brace_open + 1..brace_close];

    let existing_params = parse_param_names(signature);
    let receiver_name = choose_receiver_name(&existing_params);
    let a_param = needs_receiver_param.then_some(receiver_name.as_str());

    let mut new_signature = signature.to_string();
    if needs_receiver_param {
        new_signature = insert_receiver_param(&new_signature, &params.from_class, &receiver_name);
    }

    let new_body = rewrite_method_body(body, a_param, &b_field, &a_member_names);

    let mut moved_method = String::new();
    moved_method.push_str(new_signature.trim_end());
    moved_method.push_str(" {\n");
    moved_method.push_str(new_body.trim_matches('\n'));
    if !moved_method.ends_with('\n') {
        moved_method.push('\n');
    }
    moved_method.push_str("}\n");

    let dest_indent = class_indent(to_text, &to_class);
    let moved_method = indent_block(moved_method.trim_matches('\n'), &dest_indent);

    let mut edits_by_file: HashMap<PathBuf, Vec<LocalEdit>> = HashMap::new();

    // Structural move (source delete + dest insert).
    edits_by_file
        .entry(from_path.clone())
        .or_default()
        .push(LocalEdit {
            range: method_decl.range,
            replacement: String::new(),
        });
    edits_by_file
        .entry(to_path.clone())
        .or_default()
        .push(insert_before_class_close(to_text, &to_class, &moved_method));

    // Update call sites: `<recv>.method(` -> `<recv>.<b_field>.method(` + maybe add receiver param.
    let call_re = Regex::new(&format!(
        r"(?P<recv>\b[A-Za-z_][A-Za-z0-9_]*\b)\s*\.\s*{}\s*\(",
        regex::escape(&params.method_name)
    ))
    .expect("valid regex");

    for (path, text) in files {
        // We always compute edits against the original text to keep offsets stable.
        if detect_unhandled_dot_calls(text, &params.method_name) {
            // If there's a `.method(` call where the receiver isn't an identifier, refuse.
            // This is conservative but avoids silently breaking code.
            return Err(MoveMemberError::UnsupportedCallSite {
                method: params.method_name.clone(),
                file: path.clone(),
            });
        }

        for caps in call_re.captures_iter(text) {
            let m = caps.get(0).unwrap();
            let recv = caps.name("recv").unwrap().as_str();

            // Skip matches inside the removed method body (otherwise edits can overlap).
            if path == &from_path
                && m.start() >= method_decl.range.start
                && m.end() <= method_decl.range.end
            {
                continue;
            }

            let dot_rel = m.as_str().find('.').unwrap();
            let recv_range = TextRange::new(m.start(), m.start() + dot_rel);
            let new_recv = format!("{recv}.{b_field}");
            edits_by_file
                .entry(path.clone())
                .or_default()
                .push(LocalEdit {
                    range: recv_range,
                    replacement: new_recv,
                });

            if needs_receiver_param {
                let open_paren_abs = m.end() - 1; // match includes '(' as last char
                let mut j = open_paren_abs + 1;
                while j < text.len() {
                    let c = text.as_bytes()[j] as char;
                    if c.is_whitespace() {
                        j += 1;
                        continue;
                    }
                    let insertion = if c == ')' {
                        recv.to_string()
                    } else {
                        format!("{recv}, ")
                    };
                    edits_by_file
                        .entry(path.clone())
                        .or_default()
                        .push(LocalEdit {
                            range: TextRange::new(open_paren_abs + 1, open_paren_abs + 1),
                            replacement: insertion,
                        });
                    break;
                }
            }
        }
    }

    let mut out = RefactoringEdit::default();
    for (path, edits) in edits_by_file {
        let Some(original) = files.get(&path).map(String::as_str) else {
            return Err(MoveMemberError::MissingFileContents(path));
        };
        let updated = apply_local_edits(original, edits);
        if updated != original {
            out.file_edits.push(FileEdit {
                path,
                new_contents: updated,
            });
        }
    }

    Ok(out.to_workspace_edit(files)?)
}

/// Move an instance method from one class to another, returning Nova's canonical [`WorkspaceEdit`].
pub fn move_method_workspace_edit(
    files: &BTreeMap<PathBuf, String>,
    params: MoveMethodParams,
) -> Result<WorkspaceEdit, MoveMemberError> {
    move_method(files, params)
}

use std::collections::HashSet;

use thiserror::Error;

use crate::edit::{apply_text_edits, FileId, TextEdit, TextRange, WorkspaceEdit};
use crate::materialize::{materialize, MaterializeError};
use crate::semantic::{Conflict, RefactorDatabase, SemanticChange};
use crate::java::SymbolId;

#[derive(Debug, Error)]
pub enum RefactorError {
    #[error("refactoring has conflicts: {0:?}")]
    Conflicts(Vec<Conflict>),
    #[error(transparent)]
    Materialize(#[from] MaterializeError),
    #[error("unknown file {0:?}")]
    UnknownFile(FileId),
    #[error("expected a variable with initializer for inline")]
    InlineNotSupported,
    #[error(transparent)]
    Edit(#[from] crate::edit::EditError),
}

pub struct RenameParams {
    pub symbol: SymbolId,
    pub new_name: String,
}

pub fn rename(db: &dyn RefactorDatabase, params: RenameParams) -> Result<WorkspaceEdit, RefactorError> {
    let conflicts = check_rename_conflicts(db, params.symbol, &params.new_name);
    if !conflicts.is_empty() {
        return Err(RefactorError::Conflicts(conflicts));
    }

    let changes = vec![SemanticChange::Rename {
        symbol: params.symbol,
        new_name: params.new_name,
    }];
    Ok(materialize(db, changes)?)
}

fn check_rename_conflicts(db: &dyn RefactorDatabase, symbol: SymbolId, new_name: &str) -> Vec<Conflict> {
    let mut conflicts = Vec::new();

    let Some(def) = db.symbol_definition(symbol) else {
        return conflicts;
    };

    let Some(scope) = db.symbol_scope(symbol) else {
        return conflicts;
    };

    if let Some(existing) = db.resolve_name_in_scope(scope, new_name) {
        if existing != symbol {
            conflicts.push(Conflict::NameCollision {
                file: def.file.clone(),
                name: new_name.to_string(),
                existing_symbol: existing,
            });
        }
    }

    if let Some(shadowed) = db.would_shadow(scope, new_name) {
        if shadowed != symbol {
            conflicts.push(Conflict::Shadowing {
                file: def.file.clone(),
                name: new_name.to_string(),
                shadowed_symbol: shadowed,
            });
        }
    }

    for usage in db.find_references(symbol) {
        if !db.is_visible_from(symbol, &usage.file, new_name) {
            conflicts.push(Conflict::VisibilityLoss {
                file: usage.file.clone(),
                usage_range: usage.range,
                name: new_name.to_string(),
            });
        }
    }

    conflicts
}

pub struct ExtractVariableParams {
    pub file: FileId,
    pub expr_range: TextRange,
    pub name: String,
    pub use_var: bool,
}

pub fn extract_variable(
    db: &dyn RefactorDatabase,
    params: ExtractVariableParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let text = db
        .file_text(&params.file)
        .ok_or_else(|| RefactorError::UnknownFile(params.file.clone()))?;

    if params.expr_range.end > text.len() {
        return Err(RefactorError::Edit(crate::edit::EditError::OutOfBounds {
            file: params.file.clone(),
            range: params.expr_range,
            len: text.len(),
        }));
    }

    let expr_text = text[params.expr_range.start..params.expr_range.end]
        .trim()
        .to_string();
    let insert_pos = line_start(text, params.expr_range.start);
    let indent = current_indent(text, insert_pos);
    let ty = if params.use_var { "var" } else { "var" };

    let name = params.name;
    let decl = format!("{indent}{ty} {} = {};\n", &name, expr_text);

    let mut edit = WorkspaceEdit::new(vec![
        TextEdit::insert(params.file.clone(), insert_pos, decl),
        TextEdit::replace(params.file.clone(), params.expr_range, name),
    ]);
    edit.normalize()?;
    Ok(edit)
}

pub struct InlineVariableParams {
    pub symbol: SymbolId,
    pub inline_all: bool,
}

pub fn inline_variable(
    db: &dyn RefactorDatabase,
    params: InlineVariableParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let def = db
        .symbol_definition(params.symbol)
        .ok_or(RefactorError::InlineNotSupported)?;

    let text = db
        .file_text(&def.file)
        .ok_or_else(|| RefactorError::UnknownFile(def.file.clone()))?;

    let decl_start = line_start(text, def.name_range.start);
    let semi = text[def.name_range.end..].find(';').map(|o| def.name_range.end + o);
    let Some(semi) = semi else {
        return Err(RefactorError::InlineNotSupported);
    };

    let eq = text[def.name_range.end..semi].find('=').map(|o| def.name_range.end + o);
    let Some(eq) = eq else {
        return Err(RefactorError::InlineNotSupported);
    };

    let mut init = text[eq + 1..semi].trim().to_string();
    if init.is_empty() {
        return Err(RefactorError::InlineNotSupported);
    }

    // Preserve parentheses for simple binary expressions.
    if init.contains(' ') && !(init.starts_with('(') && init.ends_with(')')) {
        init = format!("({init})");
    }

    let mut edits = Vec::new();
    for usage in db.find_references(params.symbol) {
        edits.push(TextEdit::replace(usage.file, usage.range, init.clone()));
    }

    if params.inline_all {
        let decl_end = consume_trailing_newline(text, semi + 1);
        edits.push(TextEdit::delete(def.file.clone(), TextRange::new(decl_start, decl_end)));
    }

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

pub struct OrganizeImportsParams {
    pub file: FileId,
}

pub fn organize_imports(
    db: &dyn RefactorDatabase,
    params: OrganizeImportsParams,
) -> Result<WorkspaceEdit, RefactorError> {
    let text = db
        .file_text(&params.file)
        .ok_or_else(|| RefactorError::UnknownFile(params.file.clone()))?;

    let (import_block_range, imports) = parse_import_block(text);
    if imports.is_empty() {
        return Ok(WorkspaceEdit::default());
    }

    let body_start = import_block_range.end;
    let used_idents = collect_identifiers(&text[body_start..]);

    let mut normal = Vec::new();
    let mut static_imports = Vec::new();

    for import in imports {
        let trimmed = import.trim();
        let is_static = trimmed.starts_with("import static ");
        let imported = trimmed
            .trim_start_matches("import ")
            .trim_start_matches("static ")
            .trim()
            .trim_end_matches(';')
            .trim();

        let keep = if imported.ends_with(".*") {
            true
        } else {
            let simple = imported.rsplit('.').next().unwrap_or(imported);
            used_idents.contains(simple)
        };

        if keep {
            if is_static {
                static_imports.push(trimmed.to_string());
            } else {
                normal.push(trimmed.to_string());
            }
        }
    }

    normal.sort();
    static_imports.sort();

    let mut out = String::new();
    for import in &normal {
        out.push_str(import);
        if !import.ends_with('\n') {
            out.push('\n');
        }
    }

    if !normal.is_empty() && !static_imports.is_empty() {
        out.push('\n');
    }

    for import in &static_imports {
        out.push_str(import);
        if !import.ends_with('\n') {
            out.push('\n');
        }
    }

    // Ensure a single blank line after imports if there is any body.
    if !out.ends_with("\n\n") {
        out.push('\n');
    }

    let mut edits = Vec::new();
    edits.push(TextEdit::replace(
        params.file.clone(),
        import_block_range,
        out,
    ));

    let mut edit = WorkspaceEdit::new(edits);
    edit.normalize()?;
    Ok(edit)
}

fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map(|p| p + 1).unwrap_or(0)
}

fn current_indent(text: &str, line_start: usize) -> String {
    let line = &text[line_start..];
    let mut indent = String::new();
    for ch in line.chars() {
        if ch == ' ' || ch == '\t' {
            indent.push(ch);
        } else {
            break;
        }
    }
    indent
}

fn consume_trailing_newline(text: &str, mut offset: usize) -> usize {
    if offset < text.len() && text.as_bytes()[offset] == b'\n' {
        offset += 1;
    }
    offset
}

fn parse_import_block(text: &str) -> (TextRange, Vec<&str>) {
    let mut offset = 0usize;
    let mut imports = Vec::new();
    let mut start_import = None;
    let mut end_import = None;

    for raw_line in text.split_inclusive('\n') {
        let line = raw_line.trim_end_matches('\n');
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim();
        let line_len = raw_line.len();

        if trimmed.starts_with("import ") && trimmed.ends_with(';') {
            if start_import.is_none() {
                start_import = Some(offset);
            }
            end_import = Some(offset + line_len);
            imports.push(&text[offset..offset + line_len]);
            offset += line_len;
            continue;
        }

        if start_import.is_some() {
            break;
        }

        offset += line_len;
    }

    let start = start_import.unwrap_or(0);
    let end = end_import.unwrap_or(start);
    (TextRange::new(start, end), imports)
}

fn collect_identifiers(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '"' {
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i] as char;
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                i += 1;
                if ch == '"' {
                    break;
                }
            }
            continue;
        }

        if c == '/' && i + 1 < bytes.len() {
            let next = bytes[i + 1] as char;
            if next == '/' {
                i += 2;
                while i < bytes.len() && (bytes[i] as char) != '\n' {
                    i += 1;
                }
                continue;
            }
            if next == '*' {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] as char == '*' && bytes[i + 1] as char == '/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
        }

        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            out.insert(text[start..i].to_string());
            continue;
        }

        i += 1;
    }
    out
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident_continue(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

// Keep the public re-exports in lib.rs tidy.
#[allow(dead_code)]
fn _apply_edit_to_file(text: &str, file: FileId, edits: Vec<TextEdit>) -> Result<String, RefactorError> {
    Ok(apply_text_edits(
        text,
        &edits
            .into_iter()
            .map(|mut e| {
                e.file = file.clone();
                e
            })
            .collect::<Vec<_>>(),
    )?)
}

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use thiserror::Error;

use crate::edit::{
    FileId as RefactorFileId, FileOp, TextEdit as WorkspaceTextEdit, TextRange, WorkspaceEdit,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMove {
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub new_contents: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub path: PathBuf,
    pub new_contents: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// Legacy edit type for Java move refactorings.
///
/// This struct is kept temporarily for backwards compatibility. New code should prefer Nova's
/// canonical [`WorkspaceEdit`] via [`move_class_workspace_edit`] / [`move_package_workspace_edit`].
pub struct RefactoringEdit {
    pub file_moves: Vec<FileMove>,
    pub file_edits: Vec<FileEdit>,
}

impl RefactoringEdit {
    /// Applies this edit to an in-memory file map.
    ///
    /// If `apply_file_ops` is `false`, file moves are skipped and only the contents of the
    /// original files are updated.
    pub fn apply_to(&self, files: &mut BTreeMap<PathBuf, String>, apply_file_ops: bool) {
        if apply_file_ops {
            for mv in &self.file_moves {
                let _ = files.remove(&mv.old_path);
                files.insert(mv.new_path.clone(), mv.new_contents.clone());
            }
        } else {
            for mv in &self.file_moves {
                files.insert(mv.old_path.clone(), mv.new_contents.clone());
            }
        }

        for edit in &self.file_edits {
            files.insert(edit.path.clone(), edit.new_contents.clone());
        }
    }

    /// Convert this edit into Nova's canonical [`WorkspaceEdit`] representation.
    ///
    /// The canonical model expresses changes as:
    /// - file operations (`rename`/`create`/`delete`)
    /// - byte-offset text edits (`replace`/`insert`/`delete`)
    ///
    /// This conversion preserves the behavior of `apply_to(..., apply_file_ops=true)`: file moves
    /// become `Rename` file ops, and each touched file is rewritten as a single full-document
    /// replacement edit.
    pub fn to_workspace_edit(
        &self,
        original_files: &BTreeMap<PathBuf, String>,
    ) -> Result<WorkspaceEdit, crate::edit::EditError> {
        let mut out = WorkspaceEdit {
            file_ops: Vec::new(),
            text_edits: Vec::new(),
        };

        for mv in &self.file_moves {
            let from = RefactorFileId::new(mv.old_path.to_string_lossy().into_owned());
            let to = RefactorFileId::new(mv.new_path.to_string_lossy().into_owned());

            let old_contents = original_files
                .get(&mv.old_path)
                .ok_or_else(|| crate::edit::EditError::UnknownFile(from.clone()))?;

            out.file_ops.push(FileOp::Rename {
                from: from.clone(),
                to: to.clone(),
            });

            out.text_edits.push(WorkspaceTextEdit::replace(
                to,
                TextRange::new(0, old_contents.len()),
                mv.new_contents.clone(),
            ));
        }

        for fe in &self.file_edits {
            let file = RefactorFileId::new(fe.path.to_string_lossy().into_owned());
            let old_contents = original_files
                .get(&fe.path)
                .ok_or_else(|| crate::edit::EditError::UnknownFile(file.clone()))?;

            out.text_edits.push(WorkspaceTextEdit::replace(
                file,
                TextRange::new(0, old_contents.len()),
                fe.new_contents.clone(),
            ));
        }

        out.normalize()?;
        Ok(out)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RefactorError {
    #[error("illegal package name `{package}`: {reason}")]
    IllegalPackageName { package: String, reason: String },
    #[error("cannot infer source root for `{path}` and package `{package}`")]
    CannotInferSourceRoot { path: PathBuf, package: String },
    #[error("expected a single public top-level type in `{path}`")]
    MultiplePublicTopLevelTypes { path: PathBuf },
    #[error("`{path}` does not declare a public top-level type named `{expected}`")]
    PublicTypeNotFound { path: PathBuf, expected: String },
    #[error("destination already contains `{path}`")]
    DestinationAlreadyExists { path: PathBuf },
    #[error("destination already defines type `{package}.{name}` in `{path}`")]
    DestinationTypeAlreadyExists {
        package: String,
        name: String,
        path: PathBuf,
    },
    #[error("no files found in package `{package}`")]
    PackageNotFound { package: String },
    #[error("parse error: {0}")]
    Parse(String),
    #[error(transparent)]
    Edit(#[from] crate::edit::EditError),
}

fn parse_err(err: impl std::fmt::Display) -> RefactorError {
    RefactorError::Parse(err.to_string())
}

pub struct MoveClassParams {
    pub source_path: PathBuf,
    pub class_name: String,
    pub target_package: String,
}

pub struct MovePackageParams {
    pub old_package: String,
    pub new_package: String,
}

/// Move a single Java top-level class to a new package.
///
/// Returns Nova's canonical [`WorkspaceEdit`]. Internally this refactoring is still implemented
/// by producing a legacy [`RefactoringEdit`] and converting it to the canonical edit model.
pub fn move_class(
    files: &BTreeMap<PathBuf, String>,
    params: MoveClassParams,
) -> Result<WorkspaceEdit, RefactorError> {
    nova_build_model::validate_package_name(&params.target_package).map_err(|e| {
        RefactorError::IllegalPackageName {
            package: params.target_package.clone(),
            reason: e.to_string(),
        }
    })?;

    let source =
        files
            .get(&params.source_path)
            .ok_or_else(|| RefactorError::PublicTypeNotFound {
                path: params.source_path.clone(),
                expected: params.class_name.clone(),
            })?;

    let pkg = java_text::parse_package_decl(source)
        .map_err(parse_err)?
        .ok_or_else(|| RefactorError::PublicTypeNotFound {
            path: params.source_path.clone(),
            expected: params.class_name.clone(),
        })?;

    let public_types = java_text::find_public_top_level_types(source).map_err(parse_err)?;
    if public_types.len() != 1 {
        return Err(RefactorError::MultiplePublicTopLevelTypes {
            path: params.source_path.clone(),
        });
    }
    if public_types[0].name != params.class_name {
        return Err(RefactorError::PublicTypeNotFound {
            path: params.source_path.clone(),
            expected: params.class_name.clone(),
        });
    }

    // Conflict detection: destination already contains a type with the same name.
    let source_root = nova_build_model::infer_source_root(&params.source_path, &pkg.name)
        .ok_or_else(|| RefactorError::CannotInferSourceRoot {
            path: params.source_path.clone(),
            package: pkg.name.clone(),
        })?;

    let dest_path = source_root
        .join(nova_build_model::package_to_path(&params.target_package))
        .join(nova_build_model::class_to_file_name(&params.class_name));

    if files.contains_key(&dest_path) {
        return Err(RefactorError::DestinationAlreadyExists { path: dest_path });
    }

    let old_fqn = format!("{}.{}", pkg.name, params.class_name);
    let new_fqn = format!("{}.{}", params.target_package, params.class_name);

    let type_index = build_type_index(files, [&params.source_path])?;
    if let Some(path) = type_index.get(&new_fqn) {
        return Err(RefactorError::DestinationTypeAlreadyExists {
            package: params.target_package,
            name: params.class_name,
            path: path.clone(),
        });
    }

    let mut out = RefactoringEdit::default();

    // Moved file: update package declaration and rename file path.
    let moved_contents = update_package_declaration(source, &params.target_package)?;
    let moved_contents = java_text::replace_qualified_name(&moved_contents, &old_fqn, &new_fqn)
        .map_err(parse_err)?;
    let moved_contents = ensure_imports_for_moved_file(
        &moved_contents,
        &pkg.name,
        &params.source_path,
        &params.class_name,
        files,
    )?;
    out.file_moves.push(FileMove {
        old_path: params.source_path.clone(),
        new_path: dest_path,
        new_contents: moved_contents,
    });

    // Update references across the project.
    for (path, content) in files {
        if path == &params.source_path {
            continue;
        }
        let mut new_content =
            java_text::replace_qualified_name(content, &old_fqn, &new_fqn).map_err(parse_err)?;

        let file_pkg = file_package_name(&new_content)?;
        let imports = java_text::parse_import_decls(&new_content).map_err(parse_err)?;
        let uses_old_star_import = imports
            .iter()
            .any(|i| !i.is_static && i.path == format!("{}.*", pkg.name));

        // If this file lived in the original package (or used a wildcard import of the original
        // package), it may have referenced the moved type without an explicit import. After the
        // move it must import the new FQN.
        if file_pkg.as_deref() == Some(pkg.name.as_str()) || uses_old_star_import {
            new_content = ensure_import(&new_content, &new_fqn, &params.class_name)?;
        }

        if new_content != *content {
            out.file_edits.push(FileEdit {
                path: path.clone(),
                new_contents: new_content,
            });
        }
    }

    Ok(out.to_workspace_edit(files)?)
}

/// Move a single Java top-level class to a new package, returning Nova's canonical [`WorkspaceEdit`].
pub fn move_class_workspace_edit(
    files: &BTreeMap<PathBuf, String>,
    params: MoveClassParams,
) -> Result<WorkspaceEdit, RefactorError> {
    move_class(files, params)
}

/// Move/rename a Java package (and its subpackages).
///
/// Returns Nova's canonical [`WorkspaceEdit`]. Internally this refactoring is still implemented
/// by producing a legacy [`RefactoringEdit`] and converting it to the canonical edit model.
pub fn move_package(
    files: &BTreeMap<PathBuf, String>,
    params: MovePackageParams,
) -> Result<WorkspaceEdit, RefactorError> {
    nova_build_model::validate_package_name(&params.old_package).map_err(|e| {
        RefactorError::IllegalPackageName {
            package: params.old_package.clone(),
            reason: e.to_string(),
        }
    })?;

    nova_build_model::validate_package_name(&params.new_package).map_err(|e| {
        RefactorError::IllegalPackageName {
            package: params.new_package.clone(),
            reason: e.to_string(),
        }
    })?;

    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();

    for (path, source) in files {
        let Some(pkg) = file_package_name(source)? else {
            continue;
        };
        if !package_is_prefix(&params.old_package, &pkg) {
            continue;
        }

        let suffix = package_suffix(&params.old_package, &pkg);
        let new_pkg = if suffix.is_empty() {
            params.new_package.clone()
        } else {
            format!("{}.{}", params.new_package, suffix)
        };

        let source_root = nova_build_model::infer_source_root(path, &pkg).ok_or_else(|| {
            RefactorError::CannotInferSourceRoot {
                path: path.clone(),
                package: pkg.clone(),
            }
        })?;

        let file_name = path
            .file_name()
            .ok_or_else(|| RefactorError::Parse(format!("invalid path `{}`", path.display())))?;
        let new_path = source_root
            .join(nova_build_model::package_to_path(&new_pkg))
            .join(file_name);

        moves.push((path.clone(), new_path));
    }

    if moves.is_empty() {
        return Err(RefactorError::PackageNotFound {
            package: params.old_package,
        });
    }

    // Conflict detection: ensure no destinations exist (except the ones being moved).
    let moving_from: HashSet<&PathBuf> = moves.iter().map(|(old, _)| old).collect();
    for (_, new_path) in &moves {
        if files.contains_key(new_path) && !moving_from.contains(new_path) {
            return Err(RefactorError::DestinationAlreadyExists {
                path: new_path.clone(),
            });
        }
    }

    // Also ensure that no two moves map to the same destination.
    let mut seen_dest = HashSet::new();
    for (_, new_path) in &moves {
        if !seen_dest.insert(new_path.clone()) {
            return Err(RefactorError::DestinationAlreadyExists {
                path: new_path.clone(),
            });
        }
    }

    let type_index = build_type_index(files, moving_from.iter().copied())?;
    // Conflict detection: ensure we don't introduce duplicate FQNs in the destination package.
    for (old_path, _) in &moves {
        let old_source = files.get(old_path).expect("file exists");
        let Some(old_pkg) = file_package_name(old_source)? else {
            continue;
        };
        let suffix = package_suffix(&params.old_package, &old_pkg);
        let new_pkg = if suffix.is_empty() {
            params.new_package.clone()
        } else {
            format!("{}.{}", params.new_package, suffix)
        };

        let type_names = java_text::find_top_level_type_names(old_source).map_err(parse_err)?;
        for name in type_names {
            let new_fqn = format!("{new_pkg}.{name}");
            if let Some(path) = type_index.get(&new_fqn) {
                return Err(RefactorError::DestinationTypeAlreadyExists {
                    package: new_pkg,
                    name,
                    path: path.clone(),
                });
            }
        }
    }

    let mut out = RefactoringEdit::default();

    // Update and move all files in the renamed package.
    for (old_path, new_path) in &moves {
        let old_source = files.get(old_path).expect("file exists");
        let updated =
            java_text::replace_qualified_name(old_source, &params.old_package, &params.new_package)
                .map_err(parse_err)?;

        out.file_moves.push(FileMove {
            old_path: old_path.clone(),
            new_path: new_path.clone(),
            new_contents: updated,
        });
    }

    // Update imports and qualified references in all other files by rewriting any occurrences
    // of `old_package` as a qualified name prefix.
    for (path, source) in files {
        if moving_from.contains(path) {
            continue;
        }
        let updated =
            java_text::replace_qualified_name(source, &params.old_package, &params.new_package)
                .map_err(parse_err)?;
        if updated != *source {
            out.file_edits.push(FileEdit {
                path: path.clone(),
                new_contents: updated,
            });
        }
    }

    Ok(out.to_workspace_edit(files)?)
}

/// Move/rename a Java package (and its subpackages), returning Nova's canonical [`WorkspaceEdit`].
pub fn move_package_workspace_edit(
    files: &BTreeMap<PathBuf, String>,
    params: MovePackageParams,
) -> Result<WorkspaceEdit, RefactorError> {
    move_package(files, params)
}

fn package_is_prefix(prefix: &str, package: &str) -> bool {
    package == prefix || package.starts_with(&format!("{}.", prefix))
}

fn package_suffix(prefix: &str, package: &str) -> String {
    if package == prefix {
        return String::new();
    }
    package
        .strip_prefix(&format!("{}.", prefix))
        .unwrap_or(package)
        .to_string()
}

fn file_package_name(source: &str) -> Result<Option<String>, RefactorError> {
    Ok(java_text::parse_package_decl(source)
        .map_err(parse_err)?
        .map(|p| p.name))
}

fn update_package_declaration(source: &str, new_package: &str) -> Result<String, RefactorError> {
    let Some(pkg) = java_text::parse_package_decl(source).map_err(parse_err)? else {
        // Insert at the top (after leading whitespace/comments). For now we assume a package is
        // always present in refactorable files.
        return Ok(format!("package {};\n\n{}", new_package, source));
    };

    let mut out = source.to_string();
    out.replace_range(pkg.name_range, new_package);
    Ok(out)
}

fn ensure_import(source: &str, fqn: &str, simple_name: &str) -> Result<String, RefactorError> {
    // If already imported, no need to add an import.
    let imports = java_text::parse_import_decls(source).map_err(parse_err)?;
    if imports.iter().any(|i| i.path == fqn) {
        return Ok(source.to_string());
    }

    let pkg_stmt_end = java_text::parse_package_decl(source)
        .map_err(parse_err)?
        .map(|p| p.stmt_end);

    // Only add if the file actually references the simple name.
    let header_end = imports
        .iter()
        .map(|i| i.stmt_end)
        .max()
        .or(pkg_stmt_end)
        .unwrap_or(0);

    if !java_text::contains_identifier_after_offset(source, header_end, simple_name) {
        return Ok(source.to_string());
    }

    let insertion_offset = if let Some(last) = imports.iter().max_by_key(|i| i.stmt_end) {
        // After the last import, but before its trailing newline.
        last.stmt_end
    } else if let Some(pkg) = java_text::parse_package_decl(source).map_err(parse_err)? {
        java_text::skip_whitespace(source, pkg.stmt_end)
    } else {
        0
    };

    let needs_leading_newline =
        insertion_offset > 0 && source.as_bytes().get(insertion_offset - 1) == Some(&b';');

    let mut out = source.to_string();
    let import_stmt = if imports.is_empty() {
        if needs_leading_newline {
            format!("\n\nimport {fqn};\n\n")
        } else {
            format!("import {fqn};\n\n")
        }
    } else {
        format!("\nimport {fqn};")
    };
    out.insert_str(insertion_offset, &import_stmt);
    Ok(out)
}

fn ensure_imports_for_moved_file(
    moved_contents: &str,
    old_package: &str,
    source_path: &PathBuf,
    moved_class_name: &str,
    files: &BTreeMap<PathBuf, String>,
) -> Result<String, RefactorError> {
    let mut public_types_in_old_pkg: HashSet<String> = HashSet::new();

    for (path, source) in files {
        if path == source_path {
            continue;
        }
        if file_package_name(source)?.as_deref() != Some(old_package) {
            continue;
        }
        for ty in java_text::find_public_top_level_types(source).map_err(parse_err)? {
            if ty.name != moved_class_name {
                public_types_in_old_pkg.insert(ty.name);
            }
        }
    }

    if public_types_in_old_pkg.is_empty() {
        return Ok(moved_contents.to_string());
    }

    let mut out = moved_contents.to_string();
    let mut sorted: Vec<_> = public_types_in_old_pkg.into_iter().collect();
    sorted.sort();
    for name in sorted {
        let fqn = format!("{old_package}.{name}");
        out = ensure_import(&out, &fqn, &name)?;
    }
    Ok(out)
}

fn build_type_index<'a, I>(
    files: &BTreeMap<PathBuf, String>,
    excluded_paths: I,
) -> Result<HashMap<String, PathBuf>, RefactorError>
where
    I: IntoIterator<Item = &'a PathBuf>,
{
    let excluded: HashSet<&PathBuf> = excluded_paths.into_iter().collect();
    let mut index = HashMap::new();

    for (path, source) in files {
        if excluded.contains(path) {
            continue;
        }
        let Some(pkg) = file_package_name(source)? else {
            continue;
        };
        let type_names = java_text::find_top_level_type_names(source).map_err(parse_err)?;
        for name in type_names {
            index
                .entry(format!("{pkg}.{name}"))
                .or_insert_with(|| path.clone());
        }
    }

    Ok(index)
}

mod java_text {
    use std::ops::Range;

    use thiserror::Error;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PackageDecl {
        pub name: String,
        pub name_range: Range<usize>,
        pub stmt_end: usize,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ImportDecl {
        pub is_static: bool,
        pub path: String,
        pub stmt_end: usize,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PublicTypeDecl {
        pub name: String,
    }

    #[derive(Debug, Error, Clone, PartialEq, Eq)]
    pub enum ParseError {
        #[error("unexpected end of input")]
        UnexpectedEof,
    }

    /// Very small Java lexer: yields identifiers and single-character symbols while skipping
    /// whitespace, comments, and string/char literals.
    #[derive(Debug, Clone)]
    struct Lexer<'a> {
        src: &'a str,
        offset: usize,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TokenKind<'a> {
        Ident(&'a str),
        Symbol(char),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Token<'a> {
        kind: TokenKind<'a>,
        start: usize,
        end: usize,
    }

    impl<'a> Lexer<'a> {
        fn new(src: &'a str) -> Self {
            Self { src, offset: 0 }
        }

        fn peek_byte(&self) -> Option<u8> {
            self.src.as_bytes().get(self.offset).copied()
        }

        fn next_byte(&mut self) -> Option<u8> {
            let b = self.peek_byte()?;
            self.offset += 1;
            Some(b)
        }

        fn starts_with(&self, s: &str) -> bool {
            self.src
                .as_bytes()
                .get(self.offset..)
                .is_some_and(|rest| rest.starts_with(s.as_bytes()))
        }

        fn skip_whitespace(&mut self) {
            while let Some(b) = self.peek_byte() {
                match b {
                    b' ' | b'\t' | b'\r' | b'\n' => {
                        self.offset += 1;
                    }
                    _ => break,
                }
            }
        }

        fn skip_line_comment(&mut self) {
            while let Some(b) = self.peek_byte() {
                self.offset += 1;
                if b == b'\n' {
                    break;
                }
            }
        }

        fn skip_block_comment(&mut self) {
            // Assumes `/*` already matched.
            self.offset += 2;
            while self.offset < self.src.len() {
                if self.starts_with("*/") {
                    self.offset += 2;
                    break;
                }
                self.offset += 1;
            }
        }

        fn skip_string_like(&mut self, quote: u8) {
            // Consumes opening quote.
            self.offset += 1;
            while let Some(b) = self.peek_byte() {
                self.offset += 1;
                if b == b'\\' {
                    // Skip escaped.
                    self.offset += 1;
                    continue;
                }
                if b == quote {
                    break;
                }
            }
        }

        fn next_token(&mut self) -> Option<Token<'a>> {
            loop {
                self.skip_whitespace();
                if self.offset >= self.src.len() {
                    return None;
                }

                if self.starts_with("//") {
                    self.offset += 2;
                    self.skip_line_comment();
                    continue;
                }
                if self.starts_with("/*") {
                    self.skip_block_comment();
                    continue;
                }

                let start = self.offset;
                let b = self.next_byte()?;

                match b {
                    b'"' => {
                        self.offset = start;
                        self.skip_string_like(b'"');
                        continue;
                    }
                    b'\'' => {
                        self.offset = start;
                        self.skip_string_like(b'\'');
                        continue;
                    }
                    b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' => {
                        while let Some(next) = self.peek_byte() {
                            match next {
                                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$' => {
                                    self.offset += 1;
                                }
                                _ => break,
                            }
                        }
                        let end = self.offset;
                        let ident = &self.src[start..end];
                        return Some(Token {
                            kind: TokenKind::Ident(ident),
                            start,
                            end,
                        });
                    }
                    _ => {
                        let ch = b as char;
                        return Some(Token {
                            kind: TokenKind::Symbol(ch),
                            start,
                            end: self.offset,
                        });
                    }
                }
            }
        }
    }

    pub fn parse_package_decl(source: &str) -> Result<Option<PackageDecl>, ParseError> {
        let mut lexer = Lexer::new(source);

        while let Some(token) = lexer.next_token() {
            match token.kind {
                TokenKind::Ident("package") => {
                    let name_start = match lexer.next_token() {
                        Some(Token {
                            kind: TokenKind::Ident(_),
                            start,
                            ..
                        }) => start,
                        _ => return Err(ParseError::UnexpectedEof),
                    };

                    // Rewind to parse name as tokens until ';'.
                    lexer.offset = name_start;
                    let mut last_end = name_start;
                    let mut name = String::new();

                    while let Some(t) = lexer.next_token() {
                        match t.kind {
                            TokenKind::Ident(s) => {
                                name.push_str(s);
                                last_end = t.end;
                            }
                            TokenKind::Symbol('.') => {
                                name.push('.');
                                last_end = t.end;
                            }
                            TokenKind::Symbol(';') => {
                                return Ok(Some(PackageDecl {
                                    name,
                                    name_range: name_start..last_end,
                                    stmt_end: t.end,
                                }))
                            }
                            _ => return Err(ParseError::UnexpectedEof),
                        }
                    }

                    return Err(ParseError::UnexpectedEof);
                }
                _ => {
                    // Continue scanning header.
                }
            }
        }

        Ok(None)
    }

    pub fn parse_import_decls(source: &str) -> Result<Vec<ImportDecl>, ParseError> {
        let mut lexer = Lexer::new(source);
        let mut imports = Vec::new();

        while let Some(token) = lexer.next_token() {
            match token.kind {
                TokenKind::Ident("import") => {
                    let mut is_static = false;
                    let path_start;

                    let token_after_import = lexer.next_token().ok_or(ParseError::UnexpectedEof)?;
                    match token_after_import.kind {
                        TokenKind::Ident("static") => {
                            is_static = true;
                            let next = lexer.next_token().ok_or(ParseError::UnexpectedEof)?;
                            path_start = next.start;
                            lexer.offset = next.start;
                        }
                        TokenKind::Ident(_) => {
                            path_start = token_after_import.start;
                            lexer.offset = token_after_import.start;
                        }
                        _ => return Err(ParseError::UnexpectedEof),
                    }

                    let mut path = String::new();
                    let mut _last_end = path_start;

                    while let Some(t) = lexer.next_token() {
                        match t.kind {
                            TokenKind::Ident(s) => {
                                path.push_str(s);
                                _last_end = t.end;
                            }
                            TokenKind::Symbol('.') => {
                                path.push('.');
                                _last_end = t.end;
                            }
                            TokenKind::Symbol('*') => {
                                path.push('*');
                                _last_end = t.end;
                            }
                            TokenKind::Symbol(';') => {
                                imports.push(ImportDecl {
                                    is_static,
                                    path,
                                    stmt_end: t.end,
                                });
                                break;
                            }
                            _ => return Err(ParseError::UnexpectedEof),
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(imports)
    }

    pub fn find_public_top_level_types(source: &str) -> Result<Vec<PublicTypeDecl>, ParseError> {
        let mut lexer = Lexer::new(source);
        let mut depth = 0usize;
        let mut types = Vec::new();

        while let Some(token) = lexer.next_token() {
            match token.kind {
                TokenKind::Symbol('{') => depth += 1,
                TokenKind::Symbol('}') => depth = depth.saturating_sub(1),
                TokenKind::Ident("public") if depth == 0 => {
                    // Consume modifiers until kind keyword.
                    let mut kind_token = None;
                    while let Some(t) = lexer.next_token() {
                        match t.kind {
                            TokenKind::Ident("class")
                            | TokenKind::Ident("interface")
                            | TokenKind::Ident("enum")
                            | TokenKind::Ident("record") => {
                                kind_token = Some(t);
                                break;
                            }
                            TokenKind::Ident(_) => continue,
                            TokenKind::Symbol('@') => continue,
                            _ => break,
                        }
                    }
                    if kind_token.is_none() {
                        continue;
                    }

                    let name_token = lexer.next_token().ok_or(ParseError::UnexpectedEof)?;
                    if let TokenKind::Ident(name) = name_token.kind {
                        types.push(PublicTypeDecl {
                            name: name.to_string(),
                        });
                    }
                }
                _ => {}
            }
        }

        Ok(types)
    }

    pub fn find_top_level_type_names(source: &str) -> Result<Vec<String>, ParseError> {
        let mut lexer = Lexer::new(source);
        let mut depth = 0usize;
        let mut names = Vec::new();

        while let Some(token) = lexer.next_token() {
            match token.kind {
                TokenKind::Symbol('{') => depth += 1,
                TokenKind::Symbol('}') => depth = depth.saturating_sub(1),
                TokenKind::Ident("class" | "interface" | "enum" | "record") if depth == 0 => {
                    let name_token = lexer.next_token().ok_or(ParseError::UnexpectedEof)?;
                    if let TokenKind::Ident(name) = name_token.kind {
                        names.push(name.to_string());
                    }
                }
                _ => {}
            }
        }

        Ok(names)
    }

    pub fn contains_identifier_after_offset(source: &str, offset: usize, ident: &str) -> bool {
        if offset > source.len() {
            return false;
        }

        // Do not slice `source` at `offset`: callers may provide arbitrary byte offsets that are
        // not UTF-8 boundaries.
        let mut lexer = Lexer::new(source);
        lexer.offset = offset;
        while let Some(token) = lexer.next_token() {
            match token.kind {
                TokenKind::Ident(name) if name == ident => return true,
                _ => {}
            }
        }
        false
    }

    /// Returns the first byte offset at or after `offset` that is not whitespace.
    pub fn skip_whitespace(source: &str, mut offset: usize) -> usize {
        let bytes = source.as_bytes();
        while offset < bytes.len() {
            match bytes[offset] {
                b' ' | b'\t' | b'\r' | b'\n' => offset += 1,
                _ => break,
            }
        }
        offset
    }

    /// Replaces occurrences of a qualified name `old` with `new` in Java code while skipping
    /// comments and string/char literals.
    ///
    /// `old`/`new` should be dot-separated names (`com.foo.Bar`).
    pub fn replace_qualified_name(
        source: &str,
        old: &str,
        new: &str,
    ) -> Result<String, ParseError> {
        let old_parts: Vec<&str> = old.split('.').collect();
        if old_parts.is_empty() {
            return Ok(source.to_string());
        }

        let mut lexer = Lexer::new(source);
        let mut tokens = Vec::new();
        while let Some(t) = lexer.next_token() {
            tokens.push(t);
        }

        let mut edits: Vec<(Range<usize>, &str)> = Vec::new();

        let mut i = 0usize;
        while i < tokens.len() {
            // Attempt match at i
            let start_token = &tokens[i];
            let TokenKind::Ident(first) = start_token.kind else {
                i += 1;
                continue;
            };
            if first != old_parts[0] {
                i += 1;
                continue;
            }

            let mut j = i;
            let mut part_idx = 0usize;
            let mut last_ident_end = start_token.end;
            let start_offset = start_token.start;

            while part_idx < old_parts.len() {
                let token = tokens.get(j).ok_or(ParseError::UnexpectedEof)?;
                match token.kind {
                    TokenKind::Ident(s) if s == old_parts[part_idx] => {
                        last_ident_end = token.end;
                        part_idx += 1;
                        j += 1;
                        if part_idx == old_parts.len() {
                            break;
                        }
                        // Expect '.' between parts.
                        let dot = tokens.get(j).ok_or(ParseError::UnexpectedEof)?;
                        if dot.kind != TokenKind::Symbol('.') {
                            break;
                        }
                        j += 1;
                    }
                    _ => break,
                }
            }

            if part_idx == old_parts.len() {
                edits.push((start_offset..last_ident_end, new));
                i = j;
            } else {
                i += 1;
            }
        }

        if edits.is_empty() {
            return Ok(source.to_string());
        }

        // Apply edits from the back.
        let mut out = source.to_string();
        edits.sort_by_key(|(range, _)| range.start);
        for (range, replacement) in edits.into_iter().rev() {
            out.replace_range(range, replacement);
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::{apply_workspace_edit, FileId};
    use std::path::Path;

    fn files(map: Vec<(&str, &str)>) -> BTreeMap<PathBuf, String> {
        map.into_iter()
            .map(|(p, c)| (PathBuf::from(p), c.to_string()))
            .collect()
    }

    fn apply_edit(
        input: &BTreeMap<PathBuf, String>,
        edit: &WorkspaceEdit,
    ) -> BTreeMap<PathBuf, String> {
        let by_id: BTreeMap<FileId, String> = input
            .iter()
            .map(|(path, text)| {
                (
                    FileId::new(path.to_string_lossy().into_owned()),
                    text.clone(),
                )
            })
            .collect();

        let applied = apply_workspace_edit(&by_id, edit).expect("workspace edit applies cleanly");
        applied
            .into_iter()
            .map(|(file, text)| (PathBuf::from(file.0), text))
            .collect()
    }

    #[test]
    fn move_class_updates_package_imports_and_paths() {
        let input = files(vec![
            (
                "src/main/java/com/foo/A.java",
                r#"package com.foo;

public class A {}
"#,
            ),
            (
                "src/main/java/com/foo/B.java",
                r#"package com.foo;

public class B {
    A a;
    com.foo.A qa;
}
"#,
            ),
            (
                "src/main/java/com/other/C.java",
                r#"package com.other;

import com.foo.A;

public class C { A a; }
"#,
            ),
        ]);

        let edit = move_class(
            &input,
            MoveClassParams {
                source_path: PathBuf::from("src/main/java/com/foo/A.java"),
                class_name: "A".into(),
                target_package: "com.bar".into(),
            },
        )
        .unwrap();

        let applied = apply_edit(&input, &edit);

        assert!(applied.contains_key(Path::new("src/main/java/com/bar/A.java")));
        assert!(!applied.contains_key(Path::new("src/main/java/com/foo/A.java")));

        let moved = &applied[Path::new("src/main/java/com/bar/A.java")];
        assert!(moved.contains("package com.bar;"));

        let b = &applied[Path::new("src/main/java/com/foo/B.java")];
        assert!(b.contains("import com.bar.A;"));
        assert!(b.contains("com.bar.A qa;"));
        assert!(!b.contains("com.foo.A qa;"));

        let c = &applied[Path::new("src/main/java/com/other/C.java")];
        assert!(c.contains("import com.bar.A;"));
        assert!(!c.contains("import com.foo.A;"));
    }

    #[test]
    fn move_class_does_not_panic_on_unicode_in_block_comment() {
        let input = files(vec![(
            "src/main/java/com/foo/A.java",
            "/* 😀 */\npackage com.foo;\n\npublic class A {}\n",
        )]);

        let edit = move_class(
            &input,
            MoveClassParams {
                source_path: PathBuf::from("src/main/java/com/foo/A.java"),
                class_name: "A".into(),
                target_package: "com.bar".into(),
            },
        )
        .unwrap();

        let applied = apply_edit(&input, &edit);

        let moved = &applied[Path::new("src/main/java/com/bar/A.java")];
        assert!(moved.contains("/* 😀 */"));
        assert!(moved.contains("package com.bar;"));
    }

    #[test]
    fn contains_identifier_after_offset_is_utf8_safe() {
        let source = "/* 😀 */\npackage com.foo;\n\npublic class A {}\n";
        let emoji_start = source.find('😀').expect("emoji present");
        // Choose a byte index inside the 4-byte UTF-8 sequence.
        let inside_emoji = emoji_start + 1;

        assert!(java_text::contains_identifier_after_offset(
            source,
            inside_emoji,
            "package"
        ));
    }

    #[test]
    fn move_class_adds_import_when_old_package_star_import_used() {
        let input = files(vec![
            (
                "src/main/java/com/foo/A.java",
                r#"package com.foo;

public class A {}
"#,
            ),
            (
                "src/main/java/com/other/C.java",
                r#"package com.other;

import com.foo.*;

public class C { A a; }
"#,
            ),
        ]);

        let edit = move_class(
            &input,
            MoveClassParams {
                source_path: PathBuf::from("src/main/java/com/foo/A.java"),
                class_name: "A".into(),
                target_package: "com.bar".into(),
            },
        )
        .unwrap();

        let applied = apply_edit(&input, &edit);

        let c = &applied[Path::new("src/main/java/com/other/C.java")];
        assert!(c.contains("import com.foo.*;"));
        assert!(c.contains("import com.bar.A;"));
    }

    #[test]
    fn move_class_detects_conflicting_type_in_destination_package() {
        let input = files(vec![
            (
                "src/main/java/com/foo/A.java",
                r#"package com.foo;

public class A {}
"#,
            ),
            (
                "src/main/java/com/bar/Other.java",
                r#"package com.bar;

class A {}
"#,
            ),
        ]);

        let err = move_class(
            &input,
            MoveClassParams {
                source_path: PathBuf::from("src/main/java/com/foo/A.java"),
                class_name: "A".into(),
                target_package: "com.bar".into(),
            },
        )
        .unwrap_err();

        assert!(matches!(
            err,
            RefactorError::DestinationTypeAlreadyExists { .. }
        ));
    }

    #[test]
    fn move_class_adds_imports_for_old_package_types_used_in_moved_file() {
        let input = files(vec![
            (
                "src/main/java/com/foo/A.java",
                r#"package com.foo;

public class A {
    B b;
}
"#,
            ),
            (
                "src/main/java/com/foo/B.java",
                r#"package com.foo;

public class B {}
"#,
            ),
        ]);

        let edit = move_class(
            &input,
            MoveClassParams {
                source_path: PathBuf::from("src/main/java/com/foo/A.java"),
                class_name: "A".into(),
                target_package: "com.bar".into(),
            },
        )
        .unwrap();

        let applied = apply_edit(&input, &edit);

        let moved = &applied[Path::new("src/main/java/com/bar/A.java")];
        assert!(moved.contains("package com.bar;"));
        assert!(moved.contains("import com.foo.B;"));
        assert!(moved.contains("B b;"));
    }

    #[test]
    fn move_class_updates_self_fully_qualified_references_in_moved_file() {
        let input = files(vec![(
            "src/main/java/com/foo/A.java",
            r#"package com.foo;

public class A {
    com.foo.A self;
}
"#,
        )]);

        let edit = move_class(
            &input,
            MoveClassParams {
                source_path: PathBuf::from("src/main/java/com/foo/A.java"),
                class_name: "A".into(),
                target_package: "com.bar".into(),
            },
        )
        .unwrap();

        let applied = apply_edit(&input, &edit);

        let moved = &applied[Path::new("src/main/java/com/bar/A.java")];
        assert!(moved.contains("package com.bar;"));
        assert!(moved.contains("com.bar.A self;"));
        assert!(!moved.contains("com.foo.A self;"));
    }

    #[test]
    fn move_package_moves_files_and_updates_references() {
        let input = files(vec![
            (
                "src/main/java/com/foo/A.java",
                r#"package com.foo;

public class A {}
"#,
            ),
            (
                "src/main/java/com/foo/sub/B.java",
                r#"package com.foo.sub;

import com.foo.A;

public class B { A a; }
"#,
            ),
            (
                "src/main/java/com/other/C.java",
                r#"package com.other;

import com.foo.sub.B;

public class C {
    B b;
    com.foo.sub.B qb;
}
"#,
            ),
        ]);

        let edit = move_package(
            &input,
            MovePackageParams {
                old_package: "com.foo".into(),
                new_package: "com.bar".into(),
            },
        )
        .unwrap();

        let applied = apply_edit(&input, &edit);

        assert!(applied.contains_key(Path::new("src/main/java/com/bar/A.java")));
        assert!(applied.contains_key(Path::new("src/main/java/com/bar/sub/B.java")));

        let b = &applied[Path::new("src/main/java/com/bar/sub/B.java")];
        assert!(b.contains("package com.bar.sub;"));
        assert!(b.contains("import com.bar.A;"));

        let c = &applied[Path::new("src/main/java/com/other/C.java")];
        assert!(c.contains("import com.bar.sub.B;"));
        assert!(c.contains("com.bar.sub.B qb;"));
    }

    #[test]
    fn move_package_detects_conflicting_type_in_destination_package() {
        let input = files(vec![
            (
                "src/main/java/com/foo/A.java",
                r#"package com.foo;

public class A {}
"#,
            ),
            (
                "src/main/java/com/bar/Other.java",
                r#"package com.bar;

class A {}
"#,
            ),
        ]);

        let err = move_package(
            &input,
            MovePackageParams {
                old_package: "com.foo".into(),
                new_package: "com.bar".into(),
            },
        )
        .unwrap_err();

        assert!(matches!(
            err,
            RefactorError::DestinationTypeAlreadyExists { .. }
        ));
    }
}

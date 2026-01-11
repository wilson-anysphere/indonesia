use crate::comments::CommentStore;
use crate::doc::{self, Doc, PrintConfig};
use crate::{ends_with_line_break, FormatConfig, NewlineStyle};
use nova_syntax::{ast, AstNode, JavaParseResult, SyntaxKind, SyntaxNode};

mod decl;
mod expr;
mod fallback;
mod print;
mod stmt;

pub(crate) struct JavaPrettyFormatter<'a> {
    pub(crate) parse: &'a JavaParseResult,
    pub(crate) source: &'a str,
    pub(crate) config: &'a FormatConfig,
    pub(crate) newline: NewlineStyle,
    #[allow(dead_code)]
    pub(crate) comments: CommentStore,
}

impl<'a> JavaPrettyFormatter<'a> {
    pub(crate) fn new(
        parse: &'a JavaParseResult,
        source: &'a str,
        config: &'a FormatConfig,
        newline: NewlineStyle,
    ) -> Self {
        let comments = CommentStore::new(&parse.syntax(), source);
        Self {
            parse,
            source,
            config,
            newline,
            comments,
        }
    }

    pub(crate) fn build_doc(&mut self) -> Doc<'a> {
        let root = self.parse.syntax();
        match ast::CompilationUnit::cast(root.clone()) {
            Some(unit) => self.print_compilation_unit(unit.syntax()),
            None => fallback::node(self.source, &root),
        }
    }

    pub(crate) fn format(mut self, input_has_final_newline: bool) -> String {
        let doc = self.build_doc();
        let mut out = doc::print(
            doc,
            PrintConfig {
                max_width: self.config.max_line_length,
                indent_width: self.config.indent_width,
                newline: self.newline.as_str(),
            },
        );
        finalize_output(&mut out, self.config, input_has_final_newline, self.newline);
        out
    }

    fn print_compilation_unit(&mut self, node: &SyntaxNode) -> Doc<'a> {
        let mut parts: Vec<Doc<'a>> = Vec::new();
        for el in node.children_with_tokens() {
            if let Some(child) = el.as_node() {
                if let Some(ty) = ast::TypeDeclaration::cast(child.clone()) {
                    parts.push(self.print_type_declaration(ty));
                } else {
                    parts.push(fallback::node(self.source, child));
                }
                continue;
            }

            let Some(tok) = el.as_token() else {
                continue;
            };
            if is_synthetic_missing(tok.kind()) || tok.kind() == SyntaxKind::Eof {
                continue;
            }
            parts.push(fallback::token(self.source, tok));
        }

        Doc::concat(parts)
    }
}

fn is_synthetic_missing(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::MissingSemicolon
            | SyntaxKind::MissingRParen
            | SyntaxKind::MissingRBrace
            | SyntaxKind::MissingRBracket
            | SyntaxKind::MissingGreater
    )
}

pub(crate) fn format_java_pretty(
    parse: &JavaParseResult,
    source: &str,
    config: &FormatConfig,
) -> String {
    let newline = NewlineStyle::detect(source);
    let input_has_final_newline = ends_with_line_break(source);

    JavaPrettyFormatter::new(parse, source, config, newline).format(input_has_final_newline)
}

fn finalize_output(
    out: &mut String,
    config: &FormatConfig,
    input_has_final_newline: bool,
    newline: NewlineStyle,
) {
    let newline = newline.as_str();
    if config.trim_final_newlines == Some(true) {
        while matches!(out.as_bytes().last(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            out.pop();
        }
    }

    match config.insert_final_newline {
        Some(true) => {
            while matches!(out.as_bytes().last(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                out.pop();
            }
            out.push_str(newline);
        }
        Some(false) => {
            while matches!(out.as_bytes().last(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                out.pop();
            }
        }
        None => {
            if input_has_final_newline {
                // Trim trailing indentation/whitespace, but preserve any extra newlines already
                // present at EOF to keep legacy behavior stable.
                while matches!(out.as_bytes().last(), Some(b' ' | b'\t')) {
                    out.pop();
                }
                if !out.is_empty() && !out.ends_with(newline) {
                    if newline == "\r\n" && out.ends_with('\r') {
                        out.push('\n');
                    } else if out.ends_with('\n') && newline == "\r\n" {
                        out.pop();
                        out.push_str("\r\n");
                    } else {
                        out.push_str(newline);
                    }
                }
            } else {
                while matches!(out.as_bytes().last(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                    out.pop();
                }
            }
        }
    }
}

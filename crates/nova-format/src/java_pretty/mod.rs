use crate::doc::{self, Doc, PrintConfig};
use crate::{ends_with_line_break, FormatConfig, JavaComments, NewlineStyle, TokenKey};
use nova_syntax::{ast, AstNode, JavaParseResult, SyntaxKind, SyntaxNode, SyntaxToken};

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
    pub(crate) comments: JavaComments<'a>,
}

impl<'a> JavaPrettyFormatter<'a> {
    pub(crate) fn new(
        parse: &'a JavaParseResult,
        source: &'a str,
        config: &'a FormatConfig,
        newline: NewlineStyle,
    ) -> Self {
        let comments = JavaComments::new(&parse.syntax(), source);
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
            None => {
                self.comments.consume_in_range(root.text_range());
                fallback::node(self.source, &root)
            }
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
                    push_with_separator(&mut parts, self.print_type_declaration(ty));
                } else {
                    // Fallback nodes print verbatim source, including any nested comment tokens.
                    // Consume those comments so they don't trip the drain assertion.
                    push_with_separator(&mut parts, self.print_verbatim_node_with_boundary_comments(child));
                }
                continue;
            }

            let Some(tok) = el.as_token() else {
                continue;
            };
            if is_synthetic_missing(tok.kind()) || tok.kind() == SyntaxKind::Eof {
                continue;
            }
            if tok.kind().is_trivia() {
                // Trivia tokens (whitespace + comments) are printed via `CommentStore` anchors.
                continue;
            }

            let key = TokenKey::from(tok);
            let leading = self.comments.take_leading_doc(key, 0);
            let trailing = self.comments.take_trailing_doc(key, 0);

            push_with_separator(
                &mut parts,
                Doc::concat([leading, fallback::token(self.source, tok), trailing]),
            );
        }

        // Comments at EOF are anchored to the EOF token.
        let eof = self
            .parse
            .syntax()
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| tok.kind() == SyntaxKind::Eof);
        if let Some(eof) = eof {
            push_with_separator(
                &mut parts,
                self.comments.take_leading_doc(TokenKey::from(&eof), 0),
            );
        }

        Doc::concat(parts)
    }

    fn print_verbatim_node_with_boundary_comments(&mut self, node: &SyntaxNode) -> Doc<'a> {
        let Some((first, last)) = boundary_significant_tokens(node) else {
            self.comments.consume_in_range(node.text_range());
            return fallback::node(self.source, node);
        };

        // The verbatim fallback will include any comment tokens *inside* `node`, so consume them to
        // satisfy the drain assertion. Any comments anchored to boundary tokens but living outside
        // the node range (e.g. `import ...; // trailing`) must still be emitted explicitly.
        self.comments.consume_in_range(node.text_range());

        let leading = self.comments.take_leading_doc(TokenKey::from(&first), 0);
        let trailing = self.comments.take_trailing_doc(TokenKey::from(&last), 0);
        Doc::concat([leading, fallback::node(self.source, node), trailing])
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

fn boundary_significant_tokens(node: &SyntaxNode) -> Option<(SyntaxToken, SyntaxToken)> {
    let mut iter = node
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .filter(|tok| tok.kind() != SyntaxKind::Eof && !tok.kind().is_trivia() && !is_synthetic_missing(tok.kind()));

    let first = iter.next()?;
    let mut last = first.clone();
    for tok in iter {
        last = tok;
    }
    Some((first, last))
}

fn push_with_separator<'a>(out: &mut Vec<Doc<'a>>, doc: Doc<'a>) {
    if doc.is_nil() {
        return;
    }
    if !out.is_empty() {
        out.push(Doc::hardline());
    }
    out.push(doc);
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

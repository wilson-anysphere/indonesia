use std::borrow::Cow;

use unicode_ident::{is_xid_continue, is_xid_start};

use crate::syntax_kind::SyntaxKind;
use crate::TextRange;

// NOTE: The JLS specifies that Unicode escape translation (`\\uXXXX`) happens *before*
// lexical analysis. Nova's lexer performs this translation (including the quirky
// case where a translated backslash can begin another unicode escape, e.g.
// `\\u005Cu0041` -> `A`) while keeping token ranges in terms of the original source
// byte offsets via a `TextMap`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub message: String,
    pub range: TextRange,
}

#[derive(Debug, Clone)]
enum TextMap {
    /// Processed text is identical to the source text, so offsets map 1:1.
    Identity,
    /// Maps processed byte offsets to original byte offsets (`len == processed.len() + 1`).
    ///
    /// The mapping is monotonic but not strictly increasing (multi-byte UTF-8 characters and
    /// unicode-escape expansions can cause multiple processed offsets to map to the same
    /// original offset).
    Translated(Vec<u32>),
}

impl TextMap {
    fn original_offset(&self, processed: usize) -> usize {
        match self {
            TextMap::Identity => processed,
            TextMap::Translated(map) => {
                map.get(processed)
                    .copied()
                    .unwrap_or_else(|| map.last().copied().unwrap_or(0)) as usize
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: SyntaxKind,
    pub range: TextRange,
}

impl Token {
    pub fn text<'a>(&self, input: &'a str) -> &'a str {
        &input[self.range.start as usize..self.range.end as usize]
    }

    pub fn is_underscore_identifier(&self, input: &str) -> bool {
        if self.kind != SyntaxKind::Identifier {
            return false;
        }
        let raw = self.text(input);
        if raw == "_" {
            return true;
        }
        // Unicode escape translation happens before lexical analysis in Java, so `\u005F` should
        // behave the same as `_`.
        let (processed, _) = translate_unicode_escapes(raw);
        processed.as_ref() == "_"
    }
}

pub fn lex(input: &str) -> Vec<Token> {
    lex_with_errors(input).0
}

pub fn lex_with_errors(input: &str) -> (Vec<Token>, Vec<LexError>) {
    Lexer::new(input).lex_with_errors()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TemplateDelimiter {
    /// A normal string template: `"..."`.
    Quote,
    /// A text block template: `"""..."""`.
    TextBlock,
}

impl TemplateDelimiter {
    fn is_text_block(self) -> bool {
        matches!(self, TemplateDelimiter::TextBlock)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LexMode {
    Template {
        delimiter: TemplateDelimiter,
        start: usize,
    },
    /// Lexing an embedded Java expression inside a string template interpolation.
    ///
    /// The interpolation is opened by a `\{` sequence lexed as
    /// [`SyntaxKind::StringTemplateExprStart`]. We then lex Java tokens normally until the
    /// matching `}` is reached, tracking `{`/`}` nesting to avoid terminating early on blocks
    /// inside the interpolation expression.
    TemplateInterpolation { brace_depth: u32 },
}

pub struct Lexer<'a> {
    input: Cow<'a, str>,
    text_map: TextMap,
    pos: usize,
    errors: Vec<LexError>,
    mode_stack: Vec<LexMode>,
    last_non_trivia_kind: SyntaxKind,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        let (processed, text_map) = translate_unicode_escapes(input);
        Self {
            input: processed,
            text_map,
            pos: 0,
            errors: Vec::new(),
            mode_stack: Vec::new(),
            last_non_trivia_kind: SyntaxKind::Eof,
        }
    }

    pub fn lex(self) -> Vec<Token> {
        self.lex_with_errors().0
    }

    pub fn lex_with_errors(mut self) -> (Vec<Token>, Vec<LexError>) {
        let mut tokens = Vec::new();
        loop {
            let start = self.pos;
            let kind = self.next_kind();
            let end = self.pos;
            tokens.push(Token {
                kind,
                range: self.range(start, end),
            });
            if !kind.is_trivia() && kind != SyntaxKind::Eof {
                self.last_non_trivia_kind = kind;
            }
            if kind == SyntaxKind::Eof {
                break;
            }
        }
        (tokens, self.errors)
    }

    fn range(&self, start: usize, end: usize) -> TextRange {
        TextRange::new(
            self.text_map.original_offset(start),
            self.text_map.original_offset(end),
        )
    }

    fn push_error(&mut self, message: impl Into<String>, start: usize, end: usize) {
        self.errors.push(LexError {
            message: message.into(),
            range: self.range(start, end),
        });
    }

    fn next_kind(&mut self) -> SyntaxKind {
        if self.is_eof() {
            // If the file ends while we're still in a string template, emit an error token and a
            // diagnostic. We keep the token stream lossless by leaving already-emitted template
            // tokens intact and surfacing a zero-length `Error` sentinel at EOF.
            if !self.mode_stack.is_empty() {
                let mut delimiter = TemplateDelimiter::Quote;
                let mut template_idx = None;

                for (idx, mode) in self.mode_stack.iter().enumerate().rev() {
                    if let LexMode::Template { delimiter: d, .. } = *mode {
                        delimiter = d;
                        template_idx = Some(idx);
                        break;
                    }
                }

                // Report an unterminated interpolation only when the *innermost* unclosed template
                // is currently inside an interpolation. This avoids misleading diagnostics for
                // cases like `STR."outer \{ STR."inner` where the nested template is unterminated
                // but no interpolation has started inside it.
                let in_expr = template_idx.is_some_and(|idx| {
                    self.mode_stack[idx + 1..]
                        .iter()
                        .any(|m| matches!(m, LexMode::TemplateInterpolation { .. }))
                });

                let message = match (delimiter, in_expr) {
                    (TemplateDelimiter::Quote, false) => "unterminated string template",
                    (TemplateDelimiter::Quote, true) => {
                        "unterminated string template interpolation"
                    }
                    (TemplateDelimiter::TextBlock, false) => "unterminated text block template",
                    (TemplateDelimiter::TextBlock, true) => {
                        "unterminated text block template interpolation"
                    }
                };
                // Report the diagnostic at EOF: unterminated templates are missing their closing
                // delimiter, so the error position should be the point where the lexer expected to
                // find it (end-of-file), not the start of the template.
                self.push_error(message, self.pos, self.pos);
                self.mode_stack.clear();
                return SyntaxKind::Error;
            }

            return SyntaxKind::Eof;
        }

        match self.mode_stack.last().copied() {
            Some(LexMode::Template { delimiter, start }) => {
                return self.scan_string_template_token(delimiter, start);
            }
            Some(LexMode::TemplateInterpolation { .. }) => {
                // Lex a normal Java token, but track `{`/`}` so we know when the interpolation
                // closes and we should return to template lexing.
                let kind = self.next_kind_java_token();
                if self.update_template_interpolation_depth(kind) {
                    return SyntaxKind::StringTemplateExprEnd;
                }
                return kind;
            }
            None => {}
        }

        self.next_kind_java_token()
    }

    /// Updates brace depth for the active string-template interpolation.
    ///
    /// Returns `true` if `kind` closed the interpolation (meaning the caller should treat the
    /// closing `}` as a [`SyntaxKind::StringTemplateExprEnd`] token instead of a normal
    /// [`SyntaxKind::RBrace`]).
    fn update_template_interpolation_depth(&mut self, kind: SyntaxKind) -> bool {
        if !matches!(kind, SyntaxKind::LBrace | SyntaxKind::RBrace) {
            return false;
        }

        // The interpolation mode we need to update is the topmost one on the stack. It is
        // normally the last element, but a nested template could have been pushed while
        // lexing the current token (e.g. `\{ STR."x" }`). In that case the interpolation is
        // immediately below the nested template.
        let Some((idx, LexMode::TemplateInterpolation { brace_depth })) = self
            .mode_stack
            .iter_mut()
            .enumerate()
            .rev()
            .find(|(_, m)| matches!(m, LexMode::TemplateInterpolation { .. }))
        else {
            return false;
        };

        match kind {
            SyntaxKind::LBrace => {
                *brace_depth = brace_depth.saturating_add(1);
                false
            }
            SyntaxKind::RBrace => {
                *brace_depth = brace_depth.saturating_sub(1);
                if *brace_depth == 0 {
                    // This `}` closes the interpolation.
                    self.mode_stack.remove(idx);
                    return true;
                }
                false
            }
            _ => unreachable!("guard ensures we only see braces"),
        }
    }

    fn next_kind_java_token(&mut self) -> SyntaxKind {
        let b = self.peek_byte(0).unwrap_or(b'\0');
        match b {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0C => self.scan_whitespace(),
            b'/' => self.scan_slash_or_comment(),
            b'"' => {
                if self.should_start_string_template() {
                    self.start_string_template()
                } else {
                    self.scan_quote()
                }
            }
            b'\'' => self.scan_char_literal(),
            b'0'..=b'9' => self.scan_number(false),
            b'.' => {
                if matches!(self.peek_byte(1), Some(b'0'..=b'9')) {
                    self.scan_number(true)
                } else if self.peek_byte(1) == Some(b'.') && self.peek_byte(2) == Some(b'.') {
                    self.pos += 3;
                    SyntaxKind::Ellipsis
                } else {
                    self.pos += 1;
                    SyntaxKind::Dot
                }
            }
            b'(' => self.single(SyntaxKind::LParen),
            b')' => self.single(SyntaxKind::RParen),
            b'{' => self.single(SyntaxKind::LBrace),
            b'}' => self.single(SyntaxKind::RBrace),
            b'[' => self.single(SyntaxKind::LBracket),
            b']' => self.single(SyntaxKind::RBracket),
            b';' => self.single(SyntaxKind::Semicolon),
            b',' => self.single(SyntaxKind::Comma),
            b'@' => self.single(SyntaxKind::At),
            b'?' => self.single(SyntaxKind::Question),
            b':' => {
                if self.peek_byte(1) == Some(b':') {
                    self.pos += 2;
                    SyntaxKind::DoubleColon
                } else {
                    self.single(SyntaxKind::Colon)
                }
            }
            b'+' => self.scan_plus(),
            b'-' => self.scan_minus(),
            b'*' => self.scan_star(),
            b'%' => self.scan_percent(),
            b'~' => self.single(SyntaxKind::Tilde),
            b'!' => {
                if self.peek_byte(1) == Some(b'=') {
                    self.pos += 2;
                    SyntaxKind::BangEq
                } else {
                    self.single(SyntaxKind::Bang)
                }
            }
            b'=' => {
                if self.peek_byte(1) == Some(b'=') {
                    self.pos += 2;
                    SyntaxKind::EqEq
                } else {
                    self.single(SyntaxKind::Eq)
                }
            }
            b'<' => self.scan_less(),
            b'>' => self.scan_greater(),
            b'&' => self.scan_amp(),
            b'|' => self.scan_pipe(),
            b'^' => {
                if self.peek_byte(1) == Some(b'=') {
                    self.pos += 2;
                    SyntaxKind::CaretEq
                } else {
                    self.single(SyntaxKind::Caret)
                }
            }
            b'$' | b'_' | b'a'..=b'z' | b'A'..=b'Z' => self.scan_identifier_or_keyword(),
            _ => {
                // Non-ascii identifier start or unknown byte.
                let start = self.pos;
                let ch = self.peek_char().unwrap_or('\0');
                if is_ident_start(ch) {
                    self.scan_identifier_or_keyword()
                } else {
                    self.bump_char();
                    self.push_error(
                        format!("unexpected character `{}`", ch.escape_debug()),
                        start,
                        self.pos,
                    );
                    SyntaxKind::Error
                }
            }
        }
    }

    fn should_start_string_template(&self) -> bool {
        self.last_non_trivia_kind == SyntaxKind::Dot
    }

    fn start_string_template(&mut self) -> SyntaxKind {
        let start = self.pos;

        let delimiter = if self.peek_byte(0) == Some(b'"')
            && self.peek_byte(1) == Some(b'"')
            && self.peek_byte(2) == Some(b'"')
        {
            self.pos += 3;

            // Match the text block diagnostic behavior: after `"""`, optional whitespace is
            // permitted but it must be followed by a line terminator.
            let mut i = self.pos;
            let bytes = self.input.as_bytes();
            while matches!(bytes.get(i).copied(), Some(b' ' | b'\t' | 0x0C)) {
                i += 1;
            }
            if !matches!(bytes.get(i).copied(), Some(b'\n' | b'\r')) {
                self.push_error(
                    "text block opening delimiter must be followed by a line terminator",
                    start,
                    i,
                );
            }

            TemplateDelimiter::TextBlock
        } else {
            self.pos += 1;
            TemplateDelimiter::Quote
        };

        self.mode_stack.push(LexMode::Template { delimiter, start });
        SyntaxKind::StringTemplateStart
    }

    fn scan_string_template_token(
        &mut self,
        delimiter: TemplateDelimiter,
        template_start: usize,
    ) -> SyntaxKind {
        // Closing delimiter?
        if delimiter == TemplateDelimiter::Quote {
            if self.peek_byte(0) == Some(b'"') {
                self.pos += 1;
                self.mode_stack.pop(); // Template
                return SyntaxKind::StringTemplateEnd;
            }
        } else {
            // Text block closing delimiter: the last `"""` in a run of quotes.
            if self.peek_byte(0) == Some(b'"')
                && self.peek_byte(1) == Some(b'"')
                && self.peek_byte(2) == Some(b'"')
                && !self.is_escaped_quote()
            {
                // Count the run length.
                let mut run_len = 3usize;
                while self.peek_byte(run_len) == Some(b'"') {
                    run_len += 1;
                }

                if run_len == 3 {
                    self.pos += 3;
                    self.mode_stack.pop(); // Template
                    return SyntaxKind::StringTemplateEnd;
                }

                // Consume all but the final `"""` (those quotes are part of the text block's
                // contents).
                self.pos += run_len - 3;
                return SyntaxKind::StringTemplateText;
            }
        }

        // Interpolation start?
        if self.at_template_expr_start() {
            self.pos += 2;
            self.mode_stack
                .push(LexMode::TemplateInterpolation { brace_depth: 1 });
            return SyntaxKind::StringTemplateExprStart;
        }

        // Consume template text until we reach an interpolation start or the closing delimiter.
        let start = self.pos;
        let mut unterminated = false;
        while !self.is_eof() {
            // Stop before the next interpolation.
            if self.at_template_expr_start() {
                break;
            }

            // Stop before the closing delimiter.
            if delimiter == TemplateDelimiter::Quote {
                if self.peek_byte(0) == Some(b'"') {
                    break;
                }
            } else if self.peek_byte(0) == Some(b'"')
                && self.peek_byte(1) == Some(b'"')
                && self.peek_byte(2) == Some(b'"')
                && !self.is_escaped_quote()
            {
                break;
            }

            match self.peek_char() {
                Some('\\') => {
                    // Preserve escape sequences as part of the text, but do not treat `\{` as an
                    // escape (it begins an interpolation and is handled above, but only when the
                    // backslash itself is not escaped).
                    self.bump_char();

                    match self.peek_char() {
                        Some('\n' | '\r') if delimiter.is_text_block() => {
                            // Text blocks allow backslash + line terminator as a line continuation.
                            // (`""" ... \` + newline.)
                            //
                            // We don't interpret the escape here; we just ensure it doesn't
                            // terminate lexing with a spurious error.
                            let first = self.peek_char().expect("guard ensures Some");
                            self.bump_char();
                            if first == '\r' && self.peek_char() == Some('\n') {
                                self.bump_char();
                            }
                        }
                        None if delimiter.is_text_block() => {
                            // Text blocks can contain a trailing backslash; if the template itself
                            // is unterminated, the EOF handler will report it.
                            break;
                        }
                        Some('\n' | '\r') | None => {
                            // A backslash at end-of-line or end-of-file can't start an escape.
                            self.push_error(
                                "unterminated string template",
                                template_start,
                                self.pos,
                            );
                            self.mode_stack.pop(); // Template
                            unterminated = true;
                            break;
                        }
                        Some(_) => {
                            self.bump_char();
                        }
                    }
                }
                Some('\n' | '\r') if !delimiter.is_text_block() => {
                    // Normal string templates can't contain raw newlines.
                    self.push_error("unterminated string template", template_start, self.pos);
                    self.mode_stack.pop(); // Template
                    unterminated = true;
                    break;
                }
                Some(_) => {
                    self.bump_char();
                }
                None => break,
            }
        }

        if unterminated {
            return SyntaxKind::Error;
        }

        if self.pos == start {
            // We didn't make progress, but we also aren't at a boundary we handled above. Consume
            // one char to avoid an infinite loop and surface an error.
            let err_start = self.pos;
            self.bump_char();
            self.push_error(
                "unexpected character in string template",
                err_start,
                self.pos,
            );
            return SyntaxKind::Error;
        }

        SyntaxKind::StringTemplateText
    }

    fn single(&mut self, kind: SyntaxKind) -> SyntaxKind {
        self.pos += 1;
        kind
    }

    fn scan_whitespace(&mut self) -> SyntaxKind {
        while let Some(ch) = self.peek_char() {
            if ch == '\u{000C}' || ch == '\t' || ch == '\n' || ch == '\r' || ch == ' ' {
                self.bump_char();
            } else if ch.is_whitespace() {
                // Include non-ascii whitespace for losslessness.
                self.bump_char();
            } else {
                break;
            }
        }
        SyntaxKind::Whitespace
    }

    fn scan_slash_or_comment(&mut self) -> SyntaxKind {
        match self.peek_byte(1) {
            Some(b'/') => self.scan_line_comment(),
            Some(b'*') => self.scan_block_comment(),
            Some(b'=') => {
                self.pos += 2;
                SyntaxKind::SlashEq
            }
            _ => self.single(SyntaxKind::Slash),
        }
    }

    fn scan_line_comment(&mut self) -> SyntaxKind {
        self.pos += 2; // //
        while let Some(ch) = self.peek_char() {
            if ch == '\n' || ch == '\r' {
                break;
            }
            self.bump_char();
        }
        SyntaxKind::LineComment
    }

    fn scan_block_comment(&mut self) -> SyntaxKind {
        let start = self.pos;
        let is_doc = self.peek_byte(2) == Some(b'*');
        self.pos += 2; // /*
        while !self.is_eof() {
            if self.peek_byte(0) == Some(b'*') && self.peek_byte(1) == Some(b'/') {
                self.pos += 2;
                return if is_doc {
                    SyntaxKind::DocComment
                } else {
                    SyntaxKind::BlockComment
                };
            }
            self.bump_char();
        }
        self.push_error("unterminated block comment", start, self.pos);
        SyntaxKind::Error
    }

    fn scan_quote(&mut self) -> SyntaxKind {
        // Text block?
        if self.peek_byte(0) == Some(b'"')
            && self.peek_byte(1) == Some(b'"')
            && self.peek_byte(2) == Some(b'"')
        {
            return self.scan_text_block();
        }
        self.scan_string_literal()
    }

    fn scan_string_literal(&mut self) -> SyntaxKind {
        let start = self.pos;
        self.pos += 1; // opening "
        while let Some(ch) = self.peek_char() {
            match ch {
                '"' => {
                    self.bump_char();
                    return SyntaxKind::StringLiteral;
                }
                '\\' => {
                    let escape_start = self.pos;
                    self.bump_char();
                    // Escape sequence: consume next char if present, but do not swallow line
                    // terminators (Java does not support C-style `\\\n` line continuations in
                    // string literals).
                    match self.peek_char() {
                        Some('\n' | '\r') | None => {
                            self.push_error("unterminated string literal", start, self.pos);
                            return SyntaxKind::Error;
                        }
                        Some(next) => {
                            match next {
                                // Single-character escapes.
                                'b' | 't' | 'n' | 'f' | 'r' | '"' | '\'' | '\\' | 's' => {
                                    self.bump_char();
                                }
                                // Octal escape: \0 to \377
                                '0'..='7' => {
                                    let first = next;
                                    self.bump_char();
                                    // Second digit (optional).
                                    if matches!(self.peek_char(), Some('0'..='7')) {
                                        self.bump_char();
                                        // Third digit (optional), only if the first digit was 0..3.
                                        if first <= '3'
                                            && matches!(self.peek_char(), Some('0'..='7'))
                                        {
                                            self.bump_char();
                                        }
                                    }
                                }
                                _ => {
                                    self.bump_char();
                                    self.push_error(
                                        "invalid escape sequence in string literal",
                                        escape_start,
                                        self.pos,
                                    );
                                }
                            }
                        }
                    }
                }
                '\n' | '\r' => {
                    // Unterminated string.
                    self.push_error("unterminated string literal", start, self.pos);
                    return SyntaxKind::Error;
                }
                _ => {
                    self.bump_char();
                }
            }
        }
        self.push_error("unterminated string literal", start, self.pos);
        SyntaxKind::Error
    }

    fn scan_char_literal(&mut self) -> SyntaxKind {
        let start = self.pos;
        self.pos += 1; // opening '
        let mut value_count = 0usize;
        while let Some(ch) = self.peek_char() {
            match ch {
                '\'' => {
                    self.bump_char();
                    if value_count == 1 {
                        return SyntaxKind::CharLiteral;
                    }
                    let message = if value_count == 0 {
                        "empty character literal"
                    } else {
                        "character literal must contain exactly one character"
                    };
                    self.push_error(message, start, self.pos);
                    return SyntaxKind::Error;
                }
                '\\' => {
                    self.bump_char();
                    match self.peek_char() {
                        Some('\n' | '\r') | None => {
                            self.push_error("unterminated character literal", start, self.pos);
                            return SyntaxKind::Error;
                        }
                        Some(next) => {
                            let mut inc = 1usize;
                            match next {
                                // Single-character escapes.
                                'b' | 't' | 'n' | 'f' | 'r' | '"' | '\'' | '\\' => {
                                    self.bump_char();
                                }
                                // Octal escape: \0 to \377
                                '0'..='7' => {
                                    let first = next;
                                    self.bump_char();
                                    // Second digit (optional).
                                    if matches!(self.peek_char(), Some('0'..='7')) {
                                        self.bump_char();
                                        // Third digit (optional), only if the first digit was 0..3.
                                        if first <= '3'
                                            && matches!(self.peek_char(), Some('0'..='7'))
                                        {
                                            self.bump_char();
                                        }
                                    }
                                }
                                _ => {
                                    // Keep the literal token lossless but surface a diagnostic.
                                    self.bump_char();
                                    self.push_error(
                                        "invalid escape sequence in character literal",
                                        start,
                                        self.pos,
                                    );
                                    inc = next.len_utf16();
                                }
                            }
                            value_count += inc;
                        }
                    }
                }
                '\n' | '\r' => {
                    self.push_error("unterminated character literal", start, self.pos);
                    return SyntaxKind::Error;
                }
                _ => {
                    let ch = self
                        .bump_char()
                        .expect("peek_char returned Some for character literal");
                    // Java `char` literals are a single UTF-16 code unit, so a non-BMP scalar
                    // counts as two characters (surrogate pair) and must be rejected.
                    value_count += ch.len_utf16();
                }
            }
        }
        self.push_error("unterminated character literal", start, self.pos);
        SyntaxKind::Error
    }

    fn scan_text_block(&mut self) -> SyntaxKind {
        let start = self.pos;
        // opening """
        self.pos += 3;

        // JLS: after the opening delimiter, optional whitespace may appear but it must be
        // followed by a line terminator. We still lex a single `TextBlock` token for
        // resilience (so braces/semicolons inside don't get tokenized), but we emit a
        // diagnostic for spec-invalid openings.
        while matches!(self.peek_byte(0), Some(b' ' | b'\t' | 0x0C)) {
            self.pos += 1;
        }
        if !matches!(self.peek_byte(0), Some(b'\n' | b'\r')) {
            self.push_error(
                "text block opening delimiter must be followed by a line terminator",
                start,
                self.pos,
            );
        }

        while !self.is_eof() {
            if self.peek_byte(0) == Some(b'"')
                && self.peek_byte(1) == Some(b'"')
                && self.peek_byte(2) == Some(b'"')
                && !self.is_escaped_quote()
            {
                // The closing delimiter is the last `"""` in a run of `"` characters.
                // Any additional quotes before it are part of the text block's contents, so we
                // must consume the entire run.
                let mut run_len = 3usize;
                while self.peek_byte(run_len) == Some(b'"') {
                    run_len += 1;
                }
                self.pos += run_len;
                return SyntaxKind::TextBlock;
            }
            self.bump_char();
        }
        self.push_error("unterminated text block", start, self.pos);
        SyntaxKind::Error
    }

    fn is_escaped_quote(&self) -> bool {
        // Count consecutive backslashes immediately before the quote run.
        // An odd count means the quotes are escaped.
        if self.pos == 0 {
            return false;
        }
        let bytes = self.input.as_bytes();
        let mut idx = self.pos;
        let mut count = 0;
        while idx > 0 && bytes[idx - 1] == b'\\' {
            idx -= 1;
            count += 1;
        }
        count % 2 == 1
    }

    fn at_template_expr_start(&self) -> bool {
        self.peek_byte(0) == Some(b'\\') && self.peek_byte(1) == Some(b'{') && self.is_unescaped()
    }

    fn is_unescaped(&self) -> bool {
        // Count consecutive backslashes immediately before the current position.
        //
        // A `\{` interpolation start is recognized only when the `\` is not itself escaped.
        // That is equivalent to requiring an even number of consecutive `\` characters
        // immediately before the `\` (including zero).
        if self.pos == 0 {
            return true;
        }
        let bytes = self.input.as_bytes();
        let mut idx = self.pos;
        let mut count = 0usize;
        while idx > 0 && bytes[idx - 1] == b'\\' {
            idx -= 1;
            count += 1;
        }
        count.is_multiple_of(2)
    }

    fn scan_identifier_or_keyword(&mut self) -> SyntaxKind {
        let start = self.pos;
        self.bump_char();
        while let Some(ch) = self.peek_char() {
            if is_ident_continue(ch) {
                self.bump_char();
            } else {
                break;
            }
        }

        // Special-case `non-sealed` (a single restricted keyword token).
        if &self.input[start..self.pos] == "non" && self.peek_byte(0) == Some(b'-') {
            let after_dash = self.pos + 1;
            if self
                .input
                .get(after_dash..)
                .is_some_and(|s| s.starts_with("sealed"))
            {
                let sealed_end = after_dash + "sealed".len();
                if self
                    .input
                    .get(sealed_end..)
                    .and_then(|rest| rest.chars().next())
                    .is_none_or(|ch| !is_ident_continue(ch))
                {
                    self.pos = sealed_end;
                    return SyntaxKind::NonSealedKw;
                }
            }
        }

        let text = &self.input[start..self.pos];
        SyntaxKind::from_keyword(text).unwrap_or(SyntaxKind::Identifier)
    }

    fn scan_number(&mut self, started_with_dot: bool) -> SyntaxKind {
        let start = self.pos;
        let result = if started_with_dot {
            self.scan_decimal_float_after_dot()
        } else if self.peek_byte(0) == Some(b'0') {
            match self.peek_byte(1) {
                Some(b'x' | b'X') => self.scan_hex_number(),
                Some(b'b' | b'B') => self.scan_binary_number(),
                _ => self.scan_decimal_or_octal_number(),
            }
        } else {
            self.scan_decimal_or_octal_number()
        };

        match result {
            Ok(kind) => kind,
            Err(message) => {
                self.push_error(message, start, self.pos);
                SyntaxKind::Error
            }
        }
    }

    fn scan_decimal_float_after_dot(&mut self) -> Result<SyntaxKind, String> {
        // Assumes current token begins with `.` and the following character is a digit.
        self.pos += 1; // '.'
        let frac =
            self.scan_digits_with_underscores(|b| b.is_ascii_digit(), |b| b.is_ascii_digit());
        if frac.trailing_underscore {
            return Err("numeric literal cannot end with `_`".to_string());
        }
        if frac.invalid_underscore {
            return Err("`_` must be between digits in numeric literal".to_string());
        }

        if self.try_scan_exponent_part(b'e', b'E')? {
            // already consumed exponent
        }

        if let Some(kind) = self.scan_float_suffix() {
            if matches!(self.peek_byte(0), Some(b'l' | b'L')) {
                self.pos += 1;
                return Err("floating-point literal cannot have `L` suffix".to_string());
            }
            return Ok(kind);
        }
        if matches!(self.peek_byte(0), Some(b'l' | b'L')) {
            self.pos += 1;
            return Err("floating-point literal cannot have `L` suffix".to_string());
        }

        Ok(SyntaxKind::DoubleLiteral)
    }

    fn scan_decimal_or_octal_number(&mut self) -> Result<SyntaxKind, String> {
        let start = self.pos;
        let started_with_zero = self.peek_byte(0) == Some(b'0');
        let whole =
            self.scan_digits_with_underscores(|b| b.is_ascii_digit(), |b| b.is_ascii_digit());
        let whole_end = self.pos;

        // Floating point part.
        let mut is_float = false;

        if self.peek_byte(0) == Some(b'.') && self.peek_byte(1) != Some(b'.') {
            is_float = true;
            self.pos += 1; // '.'

            // Digits after `.` are optional, but if we see `._<digit>` we want to
            // consume it to report the underscore placement error.
            if matches!(self.peek_byte(0), Some(b'0'..=b'9'))
                || (self.peek_byte(0) == Some(b'_')
                    && matches!(self.peek_byte(1), Some(b'0'..=b'9')))
            {
                let frac = self
                    .scan_digits_with_underscores(|b| b.is_ascii_digit(), |b| b.is_ascii_digit());
                if frac.leading_underscore {
                    return Err(
                        "`_` is not allowed directly after `.` in a numeric literal".to_string()
                    );
                }
                if frac.trailing_underscore {
                    return Err("numeric literal cannot end with `_`".to_string());
                }
                if frac.invalid_underscore {
                    return Err("`_` must be between digits in numeric literal".to_string());
                }
            }
        }

        // Exponent part (only for decimal floats).
        if self.try_scan_exponent_part(b'e', b'E')? {
            is_float = true;
        }

        if let Some(kind) = self.scan_float_suffix() {
            if whole.trailing_underscore {
                return Err(
                    "`_` is not allowed at the end of the integer part of a numeric literal"
                        .to_string(),
                );
            }
            if whole.invalid_underscore {
                return Err("`_` must be between digits in numeric literal".to_string());
            }
            if matches!(self.peek_byte(0), Some(b'l' | b'L')) {
                self.pos += 1;
                return Err("floating-point literal cannot have `L` suffix".to_string());
            }
            return Ok(kind);
        }

        if is_float {
            if whole.trailing_underscore {
                return Err(
                    "`_` is not allowed at the end of the integer part of a numeric literal"
                        .to_string(),
                );
            }
            if whole.invalid_underscore {
                return Err("`_` must be between digits in numeric literal".to_string());
            }
            if matches!(self.peek_byte(0), Some(b'l' | b'L')) {
                self.pos += 1;
                return Err("floating-point literal cannot have `L` suffix".to_string());
            }
            return Ok(SyntaxKind::DoubleLiteral);
        }

        // Integer suffix.
        let int_suffix = self.peek_byte(0);
        let has_long_suffix = matches!(int_suffix, Some(b'l' | b'L'));
        if has_long_suffix {
            self.pos += 1;
        }

        // Validate underscores for integer literal.
        if whole.leading_underscore {
            // This can only happen for weird things like `_1` which we don't lex as a number.
            return Err("numeric literal cannot start with `_`".to_string());
        }
        if whole.trailing_underscore {
            return Err("numeric literal cannot end with `_`".to_string());
        }
        if whole.invalid_underscore {
            return Err("`_` must be between digits in numeric literal".to_string());
        }

        // Validate octal digits when the literal starts with `0` and is longer than `0`.
        if started_with_zero && whole.digits > 1 {
            let digits_text = &self.input[start..whole_end];
            if let Some(invalid) = digits_text
                .as_bytes()
                .iter()
                .copied()
                .find(|b| matches!(b, b'8' | b'9'))
            {
                return Err(format!(
                    "invalid digit `{}` in octal literal",
                    invalid as char
                ));
            }
        }

        Ok(if has_long_suffix {
            SyntaxKind::LongLiteral
        } else {
            SyntaxKind::IntLiteral
        })
    }

    fn scan_hex_number(&mut self) -> Result<SyntaxKind, String> {
        // `0x` / `0X`
        self.pos += 2;

        // Digits before optional dot.
        let before = self.scan_digits_with_underscores(is_hex_digit, is_hex_digit);

        let mut has_dot = false;
        let mut after = DigitsScan::empty();

        if self.peek_byte(0) == Some(b'.') && self.peek_byte(1) != Some(b'.') {
            // Treat `.` as part of a hex float candidate when it is followed by:
            // - a hex digit (`0x1.0p0`)
            // - `p`/`P` (`0x1.p0`)
            // - `_<hex digit>` to surface underscore placement errors.
            let next = self.peek_byte(1);
            let next2 = self.peek_byte(2);
            let dot_is_part_of_literal = matches!(next, Some(b'p' | b'P'))
                || next.is_some_and(is_hex_digit)
                || (next == Some(b'_') && next2.is_some_and(is_hex_digit));
            if dot_is_part_of_literal {
                has_dot = true;
                self.pos += 1; // '.'

                let require_after = before.digits == 0;
                if (self.peek_byte(0) == Some(b'_') && self.peek_byte(1).is_some_and(is_hex_digit))
                    || self.peek_byte(0).is_some_and(is_hex_digit)
                {
                    after = self.scan_digits_with_underscores(is_hex_digit, is_hex_digit);
                }

                if require_after && after.digits == 0 {
                    return Err("expected hexadecimal digits after `0x.`".to_string());
                }
                if after.leading_underscore {
                    return Err(
                        "`_` is not allowed directly after `.` in a numeric literal".to_string()
                    );
                }
                if after.trailing_underscore {
                    return Err("numeric literal cannot end with `_`".to_string());
                }
                if after.invalid_underscore {
                    return Err("`_` must be between digits in numeric literal".to_string());
                }
            }
        }

        let has_exponent = self.peek_byte(0).is_some_and(|b| b == b'p' || b == b'P');
        if has_exponent {
            if before.leading_underscore {
                return Err(
                    "`_` is not allowed directly after `0x` in a numeric literal".to_string(),
                );
            }
            if before.trailing_underscore {
                return Err("numeric literal cannot end with `_`".to_string());
            }
            if before.invalid_underscore {
                return Err("`_` must be between digits in numeric literal".to_string());
            }
            if before.digits == 0 && !has_dot {
                return Err("expected hexadecimal digits after `0x`".to_string());
            }
            self.scan_binary_exponent_part()?;
            if let Some(kind) = self.scan_float_suffix() {
                if matches!(self.peek_byte(0), Some(b'l' | b'L')) {
                    self.pos += 1;
                    return Err("floating-point literal cannot have `L` suffix".to_string());
                }
                return Ok(kind);
            }
            if matches!(self.peek_byte(0), Some(b'l' | b'L')) {
                self.pos += 1;
                return Err("floating-point literal cannot have `L` suffix".to_string());
            }
            return Ok(SyntaxKind::DoubleLiteral);
        }

        // If we consumed a dot, we were attempting a hexadecimal floating point literal but
        // didn't find the required `p`/`P` binary exponent.
        if has_dot {
            return Err("hexadecimal floating-point literal requires a `p` exponent".to_string());
        }

        // Integer literal.
        if before.digits == 0 {
            // Attempt to consume `_...` to surface the underscore error.
            if self.peek_byte(0) == Some(b'_') && self.peek_byte(1).is_some_and(is_hex_digit) {
                self.scan_digits_with_underscores(is_hex_digit, is_hex_digit);
                return Err(
                    "`_` is not allowed directly after `0x` in a numeric literal".to_string(),
                );
            }
            return Err("expected hexadecimal digits after `0x`".to_string());
        }
        if before.leading_underscore {
            return Err("`_` is not allowed directly after `0x` in a numeric literal".to_string());
        }
        if before.trailing_underscore {
            return Err("numeric literal cannot end with `_`".to_string());
        }
        if before.invalid_underscore {
            return Err("`_` must be between digits in numeric literal".to_string());
        }

        let suffix = self.peek_byte(0);
        if matches!(suffix, Some(b'l' | b'L')) {
            self.pos += 1;
            Ok(SyntaxKind::LongLiteral)
        } else {
            Ok(SyntaxKind::IntLiteral)
        }
    }

    fn scan_binary_number(&mut self) -> Result<SyntaxKind, String> {
        // `0b` / `0B`
        self.pos += 2;
        let digits =
            self.scan_digits_with_underscores(|b| b.is_ascii_digit(), |b| b == b'0' || b == b'1');

        if digits.digits == 0 {
            return Err("expected binary digits after `0b`".to_string());
        }
        if digits.leading_underscore {
            return Err("`_` is not allowed directly after `0b` in a numeric literal".to_string());
        }
        if digits.trailing_underscore {
            return Err("numeric literal cannot end with `_`".to_string());
        }
        if digits.invalid_underscore {
            return Err("`_` must be between digits in numeric literal".to_string());
        }
        if let Some(b) = digits.invalid_digit {
            return Err(format!("invalid digit `{}` in binary literal", b as char));
        }

        let suffix = self.peek_byte(0);
        if matches!(suffix, Some(b'l' | b'L')) {
            self.pos += 1;
            Ok(SyntaxKind::LongLiteral)
        } else {
            Ok(SyntaxKind::IntLiteral)
        }
    }

    fn scan_binary_exponent_part(&mut self) -> Result<(), String> {
        // Assumes current is `p`/`P`.
        self.pos += 1;
        if matches!(self.peek_byte(0), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        let exp = self.scan_digits_with_underscores(|b| b.is_ascii_digit(), |b| b.is_ascii_digit());
        if exp.digits == 0 {
            return Err("expected exponent digits after `p`".to_string());
        }
        if exp.leading_underscore {
            return Err(
                "`_` is not allowed directly after the exponent sign in a numeric literal"
                    .to_string(),
            );
        }
        if exp.trailing_underscore {
            return Err("numeric literal cannot end with `_`".to_string());
        }
        if exp.invalid_underscore {
            return Err("`_` must be between digits in numeric literal".to_string());
        }
        Ok(())
    }

    fn try_scan_exponent_part(&mut self, lower: u8, upper: u8) -> Result<bool, String> {
        let Some(b) = self.peek_byte(0) else {
            return Ok(false);
        };
        if b != lower && b != upper {
            return Ok(false);
        }

        self.pos += 1; // e/E
        if matches!(self.peek_byte(0), Some(b'+' | b'-')) {
            self.pos += 1;
        }

        let exp = self.scan_digits_with_underscores(|b| b.is_ascii_digit(), |b| b.is_ascii_digit());
        if exp.digits == 0 {
            return Err("expected exponent digits after `e`".to_string());
        }
        if exp.leading_underscore {
            return Err(
                "`_` is not allowed directly after the exponent sign in a numeric literal"
                    .to_string(),
            );
        }
        if exp.trailing_underscore {
            return Err("numeric literal cannot end with `_`".to_string());
        }
        if exp.invalid_underscore {
            return Err("`_` must be between digits in numeric literal".to_string());
        }

        Ok(true)
    }

    fn scan_float_suffix(&mut self) -> Option<SyntaxKind> {
        match self.peek_byte(0) {
            Some(b'f' | b'F') => {
                self.pos += 1;
                Some(SyntaxKind::FloatLiteral)
            }
            Some(b'd' | b'D') => {
                self.pos += 1;
                Some(SyntaxKind::DoubleLiteral)
            }
            _ => None,
        }
    }

    fn scan_digits_with_underscores(
        &mut self,
        allowed_digit: fn(u8) -> bool,
        validate_digit: fn(u8) -> bool,
    ) -> DigitsScan {
        let mut digits = 0usize;
        let mut leading_underscore = false;
        // Computed after the scan loop based on the final token observed.
        let mut prev_underscore = false;
        let mut invalid_underscore = false;
        let mut invalid_digit = None;

        while let Some(b) = self.peek_byte(0) {
            if b == b'_' {
                if digits == 0 {
                    leading_underscore = true;
                }
                let next = self.peek_byte(1);
                // JLS: underscores are only allowed *between* digits.
                if digits == 0
                    || prev_underscore
                    || next.is_none()
                    || !next.is_some_and(allowed_digit)
                {
                    invalid_underscore = true;
                }
                prev_underscore = true;
                self.pos += 1;
                continue;
            }
            if allowed_digit(b) {
                digits += 1;
                if invalid_digit.is_none() && !validate_digit(b) {
                    invalid_digit = Some(b);
                }
                prev_underscore = false;
                self.pos += 1;
                continue;
            }
            break;
        }

        let trailing_underscore = prev_underscore;

        DigitsScan {
            digits,
            leading_underscore,
            trailing_underscore,
            invalid_underscore,
            invalid_digit,
        }
    }

    fn scan_plus(&mut self) -> SyntaxKind {
        match self.peek_byte(1) {
            Some(b'+') => {
                self.pos += 2;
                SyntaxKind::PlusPlus
            }
            Some(b'=') => {
                self.pos += 2;
                SyntaxKind::PlusEq
            }
            _ => self.single(SyntaxKind::Plus),
        }
    }

    fn scan_minus(&mut self) -> SyntaxKind {
        match self.peek_byte(1) {
            Some(b'-') => {
                self.pos += 2;
                SyntaxKind::MinusMinus
            }
            Some(b'=') => {
                self.pos += 2;
                SyntaxKind::MinusEq
            }
            Some(b'>') => {
                self.pos += 2;
                SyntaxKind::Arrow
            }
            _ => self.single(SyntaxKind::Minus),
        }
    }

    fn scan_star(&mut self) -> SyntaxKind {
        if self.peek_byte(1) == Some(b'=') {
            self.pos += 2;
            SyntaxKind::StarEq
        } else {
            self.single(SyntaxKind::Star)
        }
    }

    fn scan_percent(&mut self) -> SyntaxKind {
        if self.peek_byte(1) == Some(b'=') {
            self.pos += 2;
            SyntaxKind::PercentEq
        } else {
            self.single(SyntaxKind::Percent)
        }
    }

    fn scan_less(&mut self) -> SyntaxKind {
        match (self.peek_byte(1), self.peek_byte(2)) {
            (Some(b'<'), Some(b'=')) => {
                self.pos += 3;
                SyntaxKind::LeftShiftEq
            }
            (Some(b'<'), _) => {
                self.pos += 2;
                SyntaxKind::LeftShift
            }
            (Some(b'='), _) => {
                self.pos += 2;
                SyntaxKind::LessEq
            }
            _ => self.single(SyntaxKind::Less),
        }
    }

    fn scan_greater(&mut self) -> SyntaxKind {
        match (self.peek_byte(1), self.peek_byte(2), self.peek_byte(3)) {
            (Some(b'>'), Some(b'>'), Some(b'=')) => {
                self.pos += 4;
                SyntaxKind::UnsignedRightShiftEq
            }
            (Some(b'>'), Some(b'>'), _) => {
                self.pos += 3;
                SyntaxKind::UnsignedRightShift
            }
            (Some(b'>'), Some(b'='), _) => {
                self.pos += 3;
                SyntaxKind::RightShiftEq
            }
            (Some(b'>'), _, _) => {
                self.pos += 2;
                SyntaxKind::RightShift
            }
            (Some(b'='), _, _) => {
                self.pos += 2;
                SyntaxKind::GreaterEq
            }
            _ => self.single(SyntaxKind::Greater),
        }
    }

    fn scan_amp(&mut self) -> SyntaxKind {
        match self.peek_byte(1) {
            Some(b'&') => {
                self.pos += 2;
                SyntaxKind::AmpAmp
            }
            Some(b'=') => {
                self.pos += 2;
                SyntaxKind::AmpEq
            }
            _ => self.single(SyntaxKind::Amp),
        }
    }

    fn scan_pipe(&mut self) -> SyntaxKind {
        match self.peek_byte(1) {
            Some(b'|') => {
                self.pos += 2;
                SyntaxKind::PipePipe
            }
            Some(b'=') => {
                self.pos += 2;
                SyntaxKind::PipeEq
            }
            _ => self.single(SyntaxKind::Pipe),
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek_byte(&self, offset: usize) -> Option<u8> {
        self.input.as_bytes().get(self.pos + offset).copied()
    }

    fn peek_char(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn bump_char(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }
}

fn translate_unicode_escapes(input: &str) -> (Cow<'_, str>, TextMap) {
    // Fast path: no `\u` sequences => no translation needed.
    if !input.as_bytes().windows(2).any(|w| w == b"\\u") {
        return (Cow::Borrowed(input), TextMap::Identity);
    }

    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut map: Vec<u32> = Vec::with_capacity(input.len() + 1);
    map.push(0);

    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if let Some((unit, consumed)) = parse_unicode_escape(bytes, i, true) {
                let span_start = i;
                i += consumed;
                let mut span_end = i;

                let mut ch = decode_code_unit(unit, bytes, &mut i, &mut span_end);

                // A unicode escape can translate to `\`, which can then begin another unicode
                // escape using the following source characters (`\u005Cu0041` -> `A`).
                while ch == '\\' {
                    let Some((unit2, consumed2)) = parse_unicode_escape(bytes, i, false) else {
                        break;
                    };
                    i += consumed2;
                    span_end = i;
                    ch = decode_code_unit(unit2, bytes, &mut i, &mut span_end);
                }

                append_mapped_char(&mut out, &mut map, ch, span_start, span_end);
                continue;
            }
        }

        let ch = input[i..].chars().next().unwrap();
        let span_start = i;
        i += ch.len_utf8();
        let span_end = i;
        append_mapped_char(&mut out, &mut map, ch, span_start, span_end);
    }

    (Cow::Owned(out), TextMap::Translated(map))
}

fn append_mapped_char(
    out: &mut String,
    map: &mut Vec<u32>,
    ch: char,
    span_start: usize,
    span_end: usize,
) {
    debug_assert_eq!(map.len(), out.len() + 1);
    debug_assert_eq!(map.last().copied().unwrap_or(0) as usize, span_start);

    let before = out.len();
    out.push(ch);
    let after = out.len();
    let added = after - before;

    // Intermediate boundaries within a multi-byte UTF-8 codepoint map to the start of the source
    // span; only the final boundary maps to the end.
    for _ in 1..added {
        map.push(span_start as u32);
    }
    map.push(span_end as u32);
}

fn parse_unicode_escape(bytes: &[u8], mut i: usize, has_backslash: bool) -> Option<(u16, usize)> {
    let start = i;
    if has_backslash {
        if bytes.get(i).copied()? != b'\\' {
            return None;
        }
        i += 1;
    }

    // One or more `u` characters.
    let mut saw_u = false;
    while bytes.get(i).copied() == Some(b'u') {
        saw_u = true;
        i += 1;
    }
    if !saw_u {
        return None;
    }

    let mut value: u16 = 0;
    for _ in 0..4 {
        let digit = hex_value(*bytes.get(i)?)?;
        value = value.wrapping_mul(16).wrapping_add(digit);
        i += 1;
    }

    Some((value, i - start))
}

fn hex_value(b: u8) -> Option<u16> {
    Some(match b {
        b'0'..=b'9' => (b - b'0') as u16,
        b'a'..=b'f' => (b - b'a' + 10) as u16,
        b'A'..=b'F' => (b - b'A' + 10) as u16,
        _ => return None,
    })
}

fn decode_code_unit(unit: u16, bytes: &[u8], i: &mut usize, span_end: &mut usize) -> char {
    // If the escape produced a UTF-16 surrogate code unit, try to combine it with the following
    // escape (if present) to form a valid Unicode scalar.
    if (0xD800..=0xDBFF).contains(&unit) {
        if let Some((low, consumed)) = parse_unicode_escape(bytes, *i, true) {
            if (0xDC00..=0xDFFF).contains(&low) {
                let high = (unit as u32) - 0xD800;
                let low = (low as u32) - 0xDC00;
                let codepoint = 0x10000 + ((high << 10) | low);
                if let Some(ch) = char::from_u32(codepoint) {
                    *i += consumed;
                    *span_end = *i;
                    return ch;
                }
            }
        }
        return '\u{FFFD}';
    }
    if (0xDC00..=0xDFFF).contains(&unit) {
        return '\u{FFFD}';
    }
    char::from_u32(unit as u32).unwrap_or('\u{FFFD}')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DigitsScan {
    digits: usize,
    leading_underscore: bool,
    trailing_underscore: bool,
    invalid_underscore: bool,
    invalid_digit: Option<u8>,
}

impl DigitsScan {
    fn empty() -> Self {
        Self {
            digits: 0,
            leading_underscore: false,
            trailing_underscore: false,
            invalid_underscore: false,
            invalid_digit: None,
        }
    }
}

fn is_hex_digit(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn is_ident_start(ch: char) -> bool {
    ch == '$' || ch == '_' || is_xid_start(ch)
}

fn is_ident_continue(ch: char) -> bool {
    ch == '$' || ch == '_' || is_xid_continue(ch)
}

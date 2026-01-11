use unicode_ident::{is_xid_continue, is_xid_start};

use crate::syntax_kind::SyntaxKind;
use crate::TextRange;

// NOTE: The JLS specifies that Unicode escape translation (`\\uXXXX`) happens *before*
// lexical analysis. Nova's lexer currently operates on the raw source text and does
// not perform this translation yet, so escapes inside identifiers/keywords/comments
// are treated as the literal bytes `\\`, `u`, etc.
//
// TODO: Implement full JLS Unicode escape translation with an offset mapping so
// diagnostics/ranges remain stable.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub message: String,
    pub range: TextRange,
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
}

pub fn lex(input: &str) -> Vec<Token> {
    lex_with_errors(input).0
}

pub fn lex_with_errors(input: &str) -> (Vec<Token>, Vec<LexError>) {
    Lexer::new(input).lex_with_errors()
}

pub struct Lexer<'a> {
    input: &'a str,
    pos: usize,
    errors: Vec<LexError>,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            input,
            pos: 0,
            errors: Vec::new(),
        }
    }

    pub fn lex(mut self) -> Vec<Token> {
        self.lex_with_errors().0
    }

    pub fn lex_with_errors(mut self) -> (Vec<Token>, Vec<LexError>) {
        let mut tokens = Vec::new();
        while !self.is_eof() {
            let start = self.pos;
            let kind = self.next_kind();
            let end = self.pos;
            tokens.push(Token {
                kind,
                range: TextRange::new(start, end),
            });
        }
        tokens.push(Token {
            kind: SyntaxKind::Eof,
            range: TextRange::new(self.pos, self.pos),
        });
        (tokens, self.errors)
    }

    fn next_kind(&mut self) -> SyntaxKind {
        let b = self.peek_byte(0).unwrap_or(b'\0');
        match b {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0C => self.scan_whitespace(),
            b'/' => self.scan_slash_or_comment(),
            b'"' => self.scan_quote(),
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
                    self.errors.push(LexError {
                        message: format!("unexpected character `{}`", ch.escape_debug()),
                        range: TextRange::new(start, self.pos),
                    });
                    SyntaxKind::Error
                }
            }
        }
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
        self.errors.push(LexError {
            message: "unterminated block comment".to_string(),
            range: TextRange::new(start, self.pos),
        });
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
                    self.bump_char();
                    // Escape sequence: consume next char if present, but do not swallow line
                    // terminators (Java does not support C-style `\\\n` line continuations in
                    // string literals).
                    match self.peek_char() {
                        Some('\n' | '\r') | None => {
                            self.errors.push(LexError {
                                message: "unterminated string literal".to_string(),
                                range: TextRange::new(start, self.pos),
                            });
                            return SyntaxKind::Error;
                        }
                        Some(_) => {
                            self.bump_char();
                        }
                    }
                }
                '\n' | '\r' => {
                    // Unterminated string.
                    self.errors.push(LexError {
                        message: "unterminated string literal".to_string(),
                        range: TextRange::new(start, self.pos),
                    });
                    return SyntaxKind::Error;
                }
                _ => {
                    self.bump_char();
                }
            }
        }
        self.errors.push(LexError {
            message: "unterminated string literal".to_string(),
            range: TextRange::new(start, self.pos),
        });
        SyntaxKind::Error
    }

    fn scan_char_literal(&mut self) -> SyntaxKind {
        let start = self.pos;
        self.pos += 1; // opening '
        while let Some(ch) = self.peek_char() {
            match ch {
                '\'' => {
                    self.bump_char();
                    return SyntaxKind::CharLiteral;
                }
                '\\' => {
                    self.bump_char();
                    match self.peek_char() {
                        Some('\n' | '\r') | None => {
                            self.errors.push(LexError {
                                message: "unterminated character literal".to_string(),
                                range: TextRange::new(start, self.pos),
                            });
                            return SyntaxKind::Error;
                        }
                        Some(_) => {
                            self.bump_char();
                        }
                    }
                }
                '\n' | '\r' => {
                    self.errors.push(LexError {
                        message: "unterminated character literal".to_string(),
                        range: TextRange::new(start, self.pos),
                    });
                    return SyntaxKind::Error;
                }
                _ => {
                    self.bump_char();
                }
            }
        }
        self.errors.push(LexError {
            message: "unterminated character literal".to_string(),
            range: TextRange::new(start, self.pos),
        });
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
            self.errors.push(LexError {
                message: "text block opening delimiter must be followed by a line terminator"
                    .to_string(),
                range: TextRange::new(start, self.pos),
            });
        }

        while !self.is_eof() {
            if self.peek_byte(0) == Some(b'"')
                && self.peek_byte(1) == Some(b'"')
                && self.peek_byte(2) == Some(b'"')
                && !self.is_escaped_quote()
            {
                self.pos += 3;
                return SyntaxKind::TextBlock;
            }
            self.bump_char();
        }
        self.errors.push(LexError {
            message: "unterminated text block".to_string(),
            range: TextRange::new(start, self.pos),
        });
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
                self.errors.push(LexError {
                    message,
                    range: TextRange::new(start, self.pos),
                });
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
            return Ok(kind);
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
                if matches!(self.peek_byte(0), Some(b'_'))
                    && matches!(
                        self.peek_byte(1),
                        Some(b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F')
                    )
                {
                    after = self.scan_digits_with_underscores(is_hex_digit, is_hex_digit);
                } else if self.peek_byte(0).is_some_and(is_hex_digit) {
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
                return Ok(kind);
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
        let mut trailing_underscore = false;
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

        trailing_underscore = prev_underscore;

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
    matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F')
}

fn is_ident_start(ch: char) -> bool {
    ch == '$' || ch == '_' || is_xid_start(ch)
}

fn is_ident_continue(ch: char) -> bool {
    ch == '$' || ch == '_' || is_xid_continue(ch)
}

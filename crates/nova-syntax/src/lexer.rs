use unicode_ident::{is_xid_continue, is_xid_start};

use crate::syntax_kind::SyntaxKind;
use crate::TextRange;

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
    Lexer::new(input).lex()
}

pub struct Lexer<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    pub fn lex(mut self) -> Vec<Token> {
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
        tokens
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
                let ch = self.peek_char().unwrap_or('\0');
                if is_ident_start(ch) {
                    self.scan_identifier_or_keyword()
                } else {
                    self.bump_char();
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
        let is_doc = self.peek_byte(2) == Some(b'*');
        self.pos += 2; // /*
        while !self.is_eof() {
            if self.peek_byte(0) == Some(b'*') && self.peek_byte(1) == Some(b'/') {
                self.pos += 2;
                break;
            }
            self.bump_char();
        }
        if is_doc {
            SyntaxKind::DocComment
        } else {
            SyntaxKind::BlockComment
        }
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
        self.pos += 1; // opening "
        while let Some(ch) = self.peek_char() {
            match ch {
                '"' => {
                    self.bump_char();
                    return SyntaxKind::StringLiteral;
                }
                '\\' => {
                    self.bump_char();
                    // Escape sequence: consume next char if present.
                    if !self.is_eof() {
                        self.bump_char();
                    }
                }
                '\n' | '\r' => {
                    // Unterminated string.
                    return SyntaxKind::Error;
                }
                _ => {
                    self.bump_char();
                }
            }
        }
        SyntaxKind::Error
    }

    fn scan_char_literal(&mut self) -> SyntaxKind {
        self.pos += 1; // opening '
        while let Some(ch) = self.peek_char() {
            match ch {
                '\'' => {
                    self.bump_char();
                    return SyntaxKind::CharLiteral;
                }
                '\\' => {
                    self.bump_char();
                    if !self.is_eof() {
                        self.bump_char();
                    }
                }
                '\n' | '\r' => return SyntaxKind::Error,
                _ => {
                    self.bump_char();
                }
            }
        }
        SyntaxKind::Error
    }

    fn scan_text_block(&mut self) -> SyntaxKind {
        // opening """
        self.pos += 3;
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
                .map_or(false, |s| s.starts_with("sealed"))
            {
                let sealed_end = after_dash + "sealed".len();
                if self
                    .input
                    .get(sealed_end..)
                    .and_then(|rest| rest.chars().next())
                    .map_or(true, |ch| !is_ident_continue(ch))
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
        if started_with_dot {
            self.pos += 1; // '.'
            self.consume_digits(10);
            self.consume_exponent(10);
            return self.finish_number_literal(start, true);
        }

        // Prefix: 0x, 0b.
        let mut base = 10;
        if self.peek_byte(0) == Some(b'0') {
            if matches!(self.peek_byte(1), Some(b'x' | b'X')) {
                base = 16;
                self.pos += 2;
                self.consume_digits(base);
            } else if matches!(self.peek_byte(1), Some(b'b' | b'B')) {
                base = 2;
                self.pos += 2;
                self.consume_digits(base);
            } else {
                // Octal-ish / decimal with leading zero.
                self.pos += 1;
                self.consume_digits(10);
            }
        } else {
            self.consume_digits(10);
        }

        let mut is_float = false;

        // Fractional part.
        if self.peek_byte(0) == Some(b'.') && self.peek_byte(1) != Some(b'.') {
            is_float = true;
            self.pos += 1;
            self.consume_digits(base.max(10));
        }

        // Exponent.
        if base == 16 {
            if matches!(self.peek_byte(0), Some(b'p' | b'P')) {
                is_float = true;
                self.consume_exponent(16);
            }
        } else if matches!(self.peek_byte(0), Some(b'e' | b'E')) {
            is_float = true;
            self.consume_exponent(10);
        }

        self.finish_number_literal(start, is_float)
    }

    fn consume_digits(&mut self, base: u8) {
        while let Some(b) = self.peek_byte(0) {
            match b {
                b'_' => {
                    self.pos += 1;
                }
                b'0'..=b'9' => {
                    self.pos += 1;
                }
                b'a'..=b'f' | b'A'..=b'F' if base == 16 => {
                    self.pos += 1;
                }
                _ => break,
            }
        }
    }

    fn consume_exponent(&mut self, base: u8) {
        match self.peek_byte(0) {
            Some(b'e' | b'E') if base == 10 => {}
            Some(b'p' | b'P') if base == 16 => {}
            _ => return,
        }
        self.pos += 1;
        if matches!(self.peek_byte(0), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        self.consume_digits(10);
    }

    fn finish_number_literal(&mut self, start: usize, is_float: bool) -> SyntaxKind {
        // Suffix.
        let suffix = self.peek_byte(0);
        if let Some(b) = suffix {
            match b {
                b'l' | b'L' => {
                    self.pos += 1;
                    return SyntaxKind::LongLiteral;
                }
                b'f' | b'F' => {
                    self.pos += 1;
                    return SyntaxKind::FloatLiteral;
                }
                b'd' | b'D' => {
                    self.pos += 1;
                    return SyntaxKind::DoubleLiteral;
                }
                _ => {}
            }
        }

        if is_float || self.input[start..self.pos].contains('.') {
            SyntaxKind::DoubleLiteral
        } else {
            SyntaxKind::IntLiteral
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

fn is_ident_start(ch: char) -> bool {
    ch == '$' || ch == '_' || is_xid_start(ch)
}

fn is_ident_continue(ch: char) -> bool {
    ch == '$' || ch == '_' || is_xid_continue(ch)
}

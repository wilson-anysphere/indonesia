use crate::FormatConfig;
use nova_syntax::{SyntaxKind, SyntaxTree};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
}

impl Span {
    fn text<'a>(self, source: &'a str) -> &'a str {
        // The lexer stores byte offsets, so slicing is always safe as long as the source is.
        &source[self.start..self.end]
    }
}

#[derive(Debug, Clone)]
enum Token {
    Word(Span),
    Number(Span),
    StringLiteral(Span),
    CharLiteral(Span),
    LineComment(Span),
    BlockComment(Span),
    DocComment(Span),
    Punct(Punct),
    BlankLine,
}

impl Token {
    fn is_trivia(&self) -> bool {
        matches!(self, Token::BlankLine)
    }

    fn is_line_comment(&self) -> bool {
        matches!(self, Token::LineComment(_))
    }

    fn display_len(&self) -> usize {
        match self {
            Token::Word(span)
            | Token::Number(span)
            | Token::StringLiteral(span)
            | Token::CharLiteral(span) => span.end.saturating_sub(span.start),
            Token::LineComment(span) | Token::BlockComment(span) | Token::DocComment(span) => {
                // When measuring we treat comment tokens as a single "word" (we'll handle internal
                // newlines separately during actual formatting).
                span.end.saturating_sub(span.start)
            }
            Token::Punct(p) => p.len(),
            Token::BlankLine => 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Punct {
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semicolon,
    Comma,
    Dot,
    Ellipsis,
    At,
    Question,
    Colon,
    DoubleColon,
    Arrow,
    Eq,
    EqEq,
    Bang,
    BangEq,
    Plus,
    PlusPlus,
    PlusEq,
    Minus,
    MinusMinus,
    MinusEq,
    Star,
    StarEq,
    Slash,
    SlashEq,
    Percent,
    PercentEq,
    Amp,
    AmpAmp,
    AmpEq,
    Pipe,
    PipePipe,
    PipeEq,
    Caret,
    CaretEq,
    Tilde,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    LeftShift,
    LeftShiftEq,
    RightShift,
    RightShiftEq,
    UnsignedRightShift,
    UnsignedRightShiftEq,
    Other(char),
}

impl fmt::Debug for Punct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Punct::Other(ch) => write!(f, "Other({ch:?})"),
            _ => write!(f, "{:?}", self.as_str()),
        }
    }
}

impl Punct {
    fn as_str(self) -> &'static str {
        match self {
            Punct::LBrace => "{",
            Punct::RBrace => "}",
            Punct::LParen => "(",
            Punct::RParen => ")",
            Punct::LBracket => "[",
            Punct::RBracket => "]",
            Punct::Semicolon => ";",
            Punct::Comma => ",",
            Punct::Dot => ".",
            Punct::Ellipsis => "...",
            Punct::At => "@",
            Punct::Question => "?",
            Punct::Colon => ":",
            Punct::DoubleColon => "::",
            Punct::Arrow => "->",
            Punct::Eq => "=",
            Punct::EqEq => "==",
            Punct::Bang => "!",
            Punct::BangEq => "!=",
            Punct::Plus => "+",
            Punct::PlusPlus => "++",
            Punct::PlusEq => "+=",
            Punct::Minus => "-",
            Punct::MinusMinus => "--",
            Punct::MinusEq => "-=",
            Punct::Star => "*",
            Punct::StarEq => "*=",
            Punct::Slash => "/",
            Punct::SlashEq => "/=",
            Punct::Percent => "%",
            Punct::PercentEq => "%=",
            Punct::Amp => "&",
            Punct::AmpAmp => "&&",
            Punct::AmpEq => "&=",
            Punct::Pipe => "|",
            Punct::PipePipe => "||",
            Punct::PipeEq => "|=",
            Punct::Caret => "^",
            Punct::CaretEq => "^=",
            Punct::Tilde => "~",
            Punct::Less => "<",
            Punct::LessEq => "<=",
            Punct::Greater => ">",
            Punct::GreaterEq => ">=",
            Punct::LeftShift => "<<",
            Punct::LeftShiftEq => "<<=",
            Punct::RightShift => ">>",
            Punct::RightShiftEq => ">>=",
            Punct::UnsignedRightShift => ">>>",
            Punct::UnsignedRightShiftEq => ">>>=",
            Punct::Other(_) => "",
        }
    }

    fn len(self) -> usize {
        match self {
            Punct::Other(ch) => ch.len_utf8(),
            _ => self.as_str().len(),
        }
    }

    fn push_to(self, out: &mut String) {
        match self {
            Punct::Other(ch) => out.push(ch),
            _ => out.push_str(self.as_str()),
        }
    }

    fn is_closing_delim(self) -> bool {
        matches!(
            self,
            Punct::RParen | Punct::RBracket | Punct::RBrace | Punct::Comma | Punct::Semicolon
        )
    }

    fn is_opening_delim(self) -> bool {
        matches!(self, Punct::LParen | Punct::LBracket | Punct::LBrace)
    }

    fn is_chain_separator(self) -> bool {
        matches!(self, Punct::Dot | Punct::DoubleColon)
    }
}

#[derive(Debug, Clone, Copy)]
struct ParenInfo {
    inside_len: usize,
    top_level_commas: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordKind {
    Control,
    Switch,
    For,
    Try,
    Case,
    Default,
    Modifier,
    New,
    Other,
}

#[derive(Debug, Clone, Copy)]
struct WordInfo {
    kind: WordKind,
    type_like: bool,
}

fn word_info(text: &str) -> WordInfo {
    let kind = match text {
        "if" | "while" | "catch" | "synchronized" => WordKind::Control,
        "for" => WordKind::For,
        "switch" => WordKind::Switch,
        "try" => WordKind::Try,
        "case" => WordKind::Case,
        "default" => WordKind::Default,
        "new" => WordKind::New,
        "public" | "protected" | "private" | "static" | "final" | "abstract" | "native"
        | "strictfp" | "transient" | "volatile" | "sealed" | "non" | "record" => WordKind::Modifier,
        _ => WordKind::Other,
    };

    WordInfo {
        kind,
        type_like: looks_like_type_name(text),
    }
}

fn is_join_keyword(text: &str) -> bool {
    matches!(text, "else" | "catch" | "finally" | "while")
}

fn looks_like_type_name(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
}

#[derive(Debug, Clone, Copy)]
enum SigToken {
    Word(WordInfo),
    Literal,
    Punct(Punct),
    GenericClose { after_dot: bool },
    Comment,
}

#[derive(Debug, Clone, Copy)]
struct GenericContext {
    after_dot: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParenKind {
    Normal,
    ForHeader,
    ResourceSpec,
}

#[derive(Debug, Clone)]
struct ParenCtx {
    kind: ParenKind,
    multiline: bool,
    base_indent: usize,
    content_indent: usize,
    annotation_args: bool,
    start_brace_depth: usize,
    start_bracket_depth: usize,
    start_generic_depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BraceKind {
    Normal,
    Switch,
}

#[derive(Debug, Clone)]
struct BraceCtx {
    kind: BraceKind,
}

#[derive(Debug, Clone)]
struct SwitchCtx {
    brace_depth: usize,
    in_case_body: bool,
}

#[derive(Debug)]
struct FormatState<'a> {
    config: &'a FormatConfig,
    source: &'a str,
    out: String,

    indent_level: usize,
    at_line_start: bool,
    pending_blank_line: bool,
    line_len: usize,

    last_sig: Option<SigToken>,

    generic_stack: Vec<GenericContext>,
    paren_stack: Vec<ParenCtx>,
    bracket_depth: usize,
    brace_stack: Vec<BraceCtx>,
    switch_stack: Vec<SwitchCtx>,

    pending_for: bool,
    pending_try: bool,
    pending_switch: bool,
    pending_case_label: bool,
}

impl<'a> FormatState<'a> {
    fn new(config: &'a FormatConfig, source: &'a str, initial_indent: usize) -> Self {
        Self {
            config,
            source,
            out: String::new(),

            indent_level: initial_indent,
            at_line_start: true,
            pending_blank_line: false,
            line_len: 0,

            last_sig: None,

            generic_stack: Vec::new(),
            paren_stack: Vec::new(),
            bracket_depth: 0,
            brace_stack: Vec::new(),
            switch_stack: Vec::new(),

            pending_for: false,
            pending_try: false,
            pending_switch: false,
            pending_case_label: false,
        }
    }

    fn ensure_newline(&mut self) {
        while matches!(self.out.as_bytes().last(), Some(b' ' | b'\t')) {
            self.out.pop();
            self.line_len = self.line_len.saturating_sub(1);
        }
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        self.at_line_start = true;
        self.line_len = 0;
        self.last_sig = None;
    }

    fn ensure_blank_line(&mut self) {
        if self.out.is_empty() {
            return;
        }
        self.ensure_newline();
        if !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
        self.at_line_start = true;
        self.line_len = 0;
        self.last_sig = None;
    }

    fn push_spaces(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.out.reserve(count);
        for _ in 0..count {
            self.out.push(' ');
        }
        self.line_len += count;
    }

    fn write_indent(&mut self) {
        if !self.at_line_start {
            return;
        }
        let indent_level = self.current_line_indent();
        self.push_spaces(indent_level.saturating_mul(self.config.indent_width));
        self.at_line_start = false;
    }

    fn write_indent_level(&mut self, indent_level: usize) {
        if !self.at_line_start {
            return;
        }
        self.push_spaces(indent_level.saturating_mul(self.config.indent_width));
        self.at_line_start = false;
    }

    fn ensure_space(&mut self) {
        if self.at_line_start {
            return;
        }
        if matches!(self.out.as_bytes().last(), Some(b' ' | b'\n' | b'\t')) {
            return;
        }
        self.out.push(' ');
        self.line_len += 1;
    }

    fn current_line_indent(&self) -> usize {
        if let Some(ctx) = self.paren_stack.last() {
            if ctx.multiline {
                return ctx.content_indent;
            }
        }
        self.indent_level
    }

    fn continuation_indent(&self) -> usize {
        self.current_line_indent().saturating_add(1)
    }

    fn set_pending_blank_line(&mut self) {
        self.pending_blank_line = true;
    }

    fn flush_pending_blank_line(&mut self) {
        if self.pending_blank_line {
            self.ensure_blank_line();
            self.pending_blank_line = false;
        }
    }

    fn next_non_trivia<'t>(tokens: &'t [Token], mut idx: usize) -> Option<&'t Token> {
        while idx < tokens.len() {
            if !tokens[idx].is_trivia() {
                return Some(&tokens[idx]);
            }
            idx += 1;
        }
        None
    }

    fn token_sig(&self, tok: &Token) -> SigToken {
        match tok {
            Token::Word(span) => SigToken::Word(word_info(span.text(self.source))),
            Token::Number(_) | Token::StringLiteral(_) | Token::CharLiteral(_) => SigToken::Literal,
            Token::LineComment(_) | Token::BlockComment(_) | Token::DocComment(_) => {
                SigToken::Comment
            }
            Token::Punct(p) => SigToken::Punct(*p),
            Token::BlankLine => SigToken::Comment,
        }
    }

    fn should_start_generic(&self, prev: Option<SigToken>, next: Option<&Token>) -> bool {
        let Some(next) = next else {
            return false;
        };

        // Wildcards (`<?>`) and annotated type arguments are always type-ish.
        let next_is_typeish = match next {
            Token::Punct(Punct::Question) | Token::Punct(Punct::At) => true,
            Token::Word(span) => looks_like_type_name(span.text(self.source)),
            _ => false,
        };
        if !next_is_typeish {
            return false;
        }

        match prev {
            None => true,
            Some(SigToken::Punct(Punct::Dot | Punct::DoubleColon)) => true,
            Some(SigToken::GenericClose { .. }) => true,
            Some(SigToken::Word(info)) => match info.kind {
                WordKind::Modifier | WordKind::New => true,
                WordKind::Other | WordKind::Switch | WordKind::For | WordKind::Try => {
                    info.type_like
                }
                WordKind::Case | WordKind::Default | WordKind::Control => false,
            },
            Some(SigToken::Punct(p)) => matches!(
                p,
                Punct::RBracket
                    | Punct::RParen
                    | Punct::Greater
                    | Punct::RightShift
                    | Punct::UnsignedRightShift
            ),
            Some(SigToken::Literal | SigToken::Comment) => false,
        }
    }

    fn generic_depth(&self) -> usize {
        self.generic_stack.len()
    }

    fn pop_generic(&mut self, count: usize) -> bool {
        let mut after_dot = false;
        for _ in 0..count {
            if let Some(ctx) = self.generic_stack.pop() {
                after_dot = ctx.after_dot;
            } else {
                break;
            }
        }
        after_dot
    }

    fn is_unary_context(prev: Option<SigToken>) -> bool {
        match prev {
            None => true,
            Some(SigToken::Punct(p)) => matches!(
                p,
                Punct::LParen
                    | Punct::LBracket
                    | Punct::LBrace
                    | Punct::Comma
                    | Punct::Eq
                    | Punct::Colon
                    | Punct::Question
                    | Punct::Arrow
            ),
            Some(SigToken::Word(info)) => {
                matches!(
                    info.kind,
                    WordKind::Control | WordKind::For | WordKind::Switch
                )
            }
            Some(SigToken::Comment) => true,
            Some(SigToken::Literal | SigToken::GenericClose { .. }) => false,
        }
    }

    fn needs_space_before(&self, prev: Option<SigToken>, curr: SigToken, curr_tok: &Token) -> bool {
        let Some(prev) = prev else {
            return false;
        };

        if self.at_line_start {
            return false;
        }

        match curr {
            SigToken::Comment => return true,
            SigToken::GenericClose { .. } => return false,
            SigToken::Punct(p) => {
                if matches!(
                    p,
                    Punct::RParen
                        | Punct::RBracket
                        | Punct::RBrace
                        | Punct::Comma
                        | Punct::Semicolon
                        | Punct::Dot
                        | Punct::DoubleColon
                ) {
                    return false;
                }
            }
            _ => {}
        }

        match prev {
            SigToken::Punct(p) => {
                if p.is_opening_delim() || p.is_chain_separator() || p == Punct::At {
                    return false;
                }

                if matches!(curr, SigToken::Punct(Punct::At))
                    && matches!(p, Punct::RParen | Punct::RBracket | Punct::RBrace)
                {
                    return true;
                }

                if matches!(curr, SigToken::Word(_) | SigToken::Literal)
                    && matches!(p, Punct::RParen | Punct::RBracket | Punct::RBrace)
                {
                    return true;
                }

                // `(... ) {`
                if matches!(curr, SigToken::Punct(Punct::LBrace))
                    && matches!(p, Punct::RParen | Punct::RBracket)
                {
                    return true;
                }
            }
            SigToken::Word(info) => {
                if matches!(curr, SigToken::Punct(Punct::LParen)) {
                    return matches!(
                        info.kind,
                        WordKind::Control | WordKind::For | WordKind::Switch | WordKind::Try
                    );
                }
                if matches!(curr, SigToken::Punct(Punct::Less)) {
                    // `public <T>` should keep a space, while `List<T>` should not.
                    return matches!(info.kind, WordKind::Modifier);
                }
                if matches!(curr, SigToken::Punct(Punct::At)) {
                    return true;
                }
                if matches!(curr, SigToken::Punct(Punct::LBrace)) {
                    return true;
                }
                if matches!(curr, SigToken::Word(_) | SigToken::Literal) {
                    return true;
                }
            }
            SigToken::Literal => {
                if matches!(curr, SigToken::Punct(Punct::At)) {
                    return true;
                }
                if matches!(curr, SigToken::Word(_) | SigToken::Literal) {
                    return true;
                }
            }
            SigToken::GenericClose { after_dot } => {
                if after_dot {
                    return false;
                }
                if matches!(
                    curr,
                    SigToken::Word(_) | SigToken::Literal | SigToken::Punct(Punct::At)
                ) {
                    // `List<String> foo` but not `List<String>()`.
                    return !matches!(
                        curr_tok,
                        Token::Punct(Punct::LParen) | Token::Punct(Punct::LBracket)
                    );
                }
                if matches!(curr, SigToken::Punct(Punct::LBrace)) {
                    return true;
                }
            }
            SigToken::Comment => return true,
        }

        false
    }

    fn is_binary_operator(punct: Punct, prev: Option<SigToken>, generic_depth: usize) -> bool {
        if generic_depth > 0 {
            match punct {
                Punct::Less | Punct::Greater | Punct::RightShift | Punct::UnsignedRightShift => {
                    return false;
                }
                Punct::Question => return false,
                _ => {}
            }
        }

        match punct {
            Punct::Eq
            | Punct::EqEq
            | Punct::BangEq
            | Punct::AmpAmp
            | Punct::PipePipe
            | Punct::AmpEq
            | Punct::PipeEq
            | Punct::CaretEq
            | Punct::PlusEq
            | Punct::MinusEq
            | Punct::StarEq
            | Punct::SlashEq
            | Punct::PercentEq
            | Punct::Amp
            | Punct::Pipe
            | Punct::Caret
            | Punct::LessEq
            | Punct::GreaterEq
            | Punct::LeftShift
            | Punct::LeftShiftEq
            | Punct::RightShift
            | Punct::RightShiftEq
            | Punct::UnsignedRightShift
            | Punct::UnsignedRightShiftEq => true,
            Punct::Plus | Punct::Minus => !Self::is_unary_context(prev),
            Punct::Star => !matches!(prev, Some(SigToken::Punct(Punct::Dot))),
            Punct::Slash | Punct::Percent => true,
            Punct::Less | Punct::Greater => true,
            Punct::Arrow => true,
            Punct::Question | Punct::Colon => generic_depth == 0,
            _ => false,
        }
    }

    fn wrap_if_needed(&mut self, break_indent: usize, extra_len: usize) {
        if self.config.max_line_length == 0 {
            return;
        }
        if self.line_len + extra_len <= self.config.max_line_length {
            return;
        }
        self.ensure_newline();
        self.write_indent_level(break_indent);
    }

    fn write_block_comment(&mut self, text: &str) {
        let mut parts = text.split_inclusive(['\n', '\r']);
        if let Some(first) = parts.next() {
            let trimmed = first.trim_end_matches(['\r', '\n']);
            self.out.push_str(trimmed);
            self.line_len += trimmed.len();
            if first.ends_with(['\n', '\r']) {
                self.ensure_newline();
            }
        }
        for part in parts {
            let trimmed = part.trim_end_matches(['\r', '\n']);
            self.write_indent();
            let rest = trimmed.trim_start_matches([' ', '\t']);
            self.out.push_str(rest);
            self.line_len += rest.len();
            if part.ends_with(['\n', '\r']) {
                self.ensure_newline();
            }
        }
    }
}

pub(crate) fn format_java_with_indent(
    tree: &SyntaxTree,
    source: &str,
    config: &FormatConfig,
    initial_indent: usize,
    ensure_final_newline: bool,
) -> String {
    let tokens = tokenize(tree, source);
    let paren_info = analyze_parens(&tokens, source, config);
    let mut state = FormatState::new(config, source, initial_indent);

    for idx in 0..tokens.len() {
        let tok = &tokens[idx];

        if matches!(tok, Token::BlankLine) {
            state.set_pending_blank_line();
            continue;
        }

        state.flush_pending_blank_line();

        let next = FormatState::next_non_trivia(&tokens, idx + 1);
        write_token(&mut state, &tokens, &paren_info, idx, tok, next);
    }

    if ensure_final_newline {
        state.ensure_newline();
    } else {
        while matches!(state.out.as_bytes().last(), Some(b' ' | b'\t' | b'\n')) {
            state.out.pop();
        }
    }

    state.out
}

fn tokenize(tree: &SyntaxTree, source: &str) -> Vec<Token> {
    let raw: Vec<nova_syntax::GreenToken> = tree.tokens().cloned().collect();
    let mut out: Vec<Token> = Vec::with_capacity(raw.len());
    let mut i = 0usize;

    while i < raw.len() {
        let tok = &raw[i];
        let span = Span {
            start: tok.range.start as usize,
            end: tok.range.end as usize,
        };

        match tok.kind {
            SyntaxKind::Whitespace => {
                if count_line_breaks(tok.text(source)) >= 2 {
                    if !matches!(out.last(), Some(Token::BlankLine)) {
                        out.push(Token::BlankLine);
                    }
                }
                i += 1;
            }
            SyntaxKind::Identifier => {
                out.push(Token::Word(span));
                i += 1;
            }
            SyntaxKind::Number => {
                out.push(Token::Number(span));
                i += 1;
            }
            SyntaxKind::StringLiteral => {
                out.push(Token::StringLiteral(span));
                i += 1;
            }
            SyntaxKind::CharLiteral => {
                out.push(Token::CharLiteral(span));
                i += 1;
            }
            SyntaxKind::LineComment => {
                out.push(Token::LineComment(span));
                i += 1;
            }
            SyntaxKind::DocComment => {
                out.push(Token::DocComment(span));
                i += 1;
            }
            SyntaxKind::BlockComment => {
                out.push(Token::BlockComment(span));
                i += 1;
            }
            SyntaxKind::Punctuation => {
                let (punct, consumed) = merge_punct(&raw, i, source);
                out.push(Token::Punct(punct));
                i += consumed;
            }
            _ => {
                // The token-level parser only produces the above kinds.
                i += 1;
            }
        }
    }

    out
}

fn merge_punct(tokens: &[nova_syntax::GreenToken], idx: usize, source: &str) -> (Punct, usize) {
    let ch = tokens[idx].text(source).chars().next().unwrap_or('\0');
    let next_ch = |offset: usize| -> Option<char> {
        tokens
            .get(idx + offset)
            .and_then(|t| (t.kind == SyntaxKind::Punctuation).then_some(t.text(source)))
            .and_then(|s| s.chars().next())
    };

    let punct = match (ch, next_ch(1), next_ch(2), next_ch(3)) {
        ('.', Some('.'), Some('.'), _) => (Punct::Ellipsis, 3),
        (':', Some(':'), _, _) => (Punct::DoubleColon, 2),
        ('-', Some('>'), _, _) => (Punct::Arrow, 2),
        ('=', Some('='), _, _) => (Punct::EqEq, 2),
        ('!', Some('='), _, _) => (Punct::BangEq, 2),
        ('&', Some('&'), _, _) => (Punct::AmpAmp, 2),
        ('|', Some('|'), _, _) => (Punct::PipePipe, 2),
        ('+', Some('+'), _, _) => (Punct::PlusPlus, 2),
        ('-', Some('-'), _, _) => (Punct::MinusMinus, 2),
        ('<', Some('='), _, _) => (Punct::LessEq, 2),
        ('>', Some('='), _, _) => (Punct::GreaterEq, 2),
        ('+', Some('='), _, _) => (Punct::PlusEq, 2),
        ('-', Some('='), _, _) => (Punct::MinusEq, 2),
        ('*', Some('='), _, _) => (Punct::StarEq, 2),
        ('/', Some('='), _, _) => (Punct::SlashEq, 2),
        ('%', Some('='), _, _) => (Punct::PercentEq, 2),
        ('&', Some('='), _, _) => (Punct::AmpEq, 2),
        ('|', Some('='), _, _) => (Punct::PipeEq, 2),
        ('^', Some('='), _, _) => (Punct::CaretEq, 2),
        ('<', Some('<'), Some('='), _) => (Punct::LeftShiftEq, 3),
        ('>', Some('>'), Some('>'), Some('=')) => (Punct::UnsignedRightShiftEq, 4),
        ('>', Some('>'), Some('>'), _) => (Punct::UnsignedRightShift, 3),
        ('>', Some('>'), Some('='), _) => (Punct::RightShiftEq, 3),
        ('>', Some('>'), _, _) => (Punct::RightShift, 2),
        ('<', Some('<'), _, _) => (Punct::LeftShift, 2),
        _ => (punct_from_char(ch), 1),
    };

    punct
}

fn punct_from_char(ch: char) -> Punct {
    match ch {
        '{' => Punct::LBrace,
        '}' => Punct::RBrace,
        '(' => Punct::LParen,
        ')' => Punct::RParen,
        '[' => Punct::LBracket,
        ']' => Punct::RBracket,
        ';' => Punct::Semicolon,
        ',' => Punct::Comma,
        '.' => Punct::Dot,
        '@' => Punct::At,
        '?' => Punct::Question,
        ':' => Punct::Colon,
        '=' => Punct::Eq,
        '!' => Punct::Bang,
        '+' => Punct::Plus,
        '-' => Punct::Minus,
        '*' => Punct::Star,
        '/' => Punct::Slash,
        '%' => Punct::Percent,
        '&' => Punct::Amp,
        '|' => Punct::Pipe,
        '^' => Punct::Caret,
        '~' => Punct::Tilde,
        '<' => Punct::Less,
        '>' => Punct::Greater,
        _ => Punct::Other(ch),
    }
}

fn analyze_parens(tokens: &[Token], source: &str, config: &FormatConfig) -> Vec<Option<ParenInfo>> {
    let mut info = vec![None; tokens.len()];
    let mut state = FormatState::new(config, source, 0);
    let mut stack: Vec<(usize, usize, usize, usize, usize)> = Vec::new();
    // (open_idx, after_open_pos, start_brace_depth, start_bracket_depth, start_generic_depth)
    let mut commas: Vec<usize> = Vec::new();

    for idx in 0..tokens.len() {
        let tok = &tokens[idx];
        let sig = state.token_sig(tok);
        let prev = state.last_sig;

        if let Token::BlankLine = tok {
            state.ensure_space();
            state.last_sig = Some(SigToken::Comment);
            continue;
        }

        if state.needs_space_before(prev, sig, tok) {
            state.ensure_space();
        }

        match tok {
            Token::Punct(Punct::LParen) => {
                state.out.push('(');
                state.line_len += 1;
                stack.push((
                    idx,
                    state.line_len,
                    state.brace_stack.len(),
                    state.bracket_depth,
                    state.generic_depth(),
                ));
                commas.push(0);
                state.last_sig = Some(SigToken::Punct(Punct::LParen));
                continue;
            }
            Token::Punct(Punct::RParen) => {
                if let Some((open_idx, after_open, brace_depth, bracket_depth, generic_depth)) =
                    stack.pop()
                {
                    let comma_count = commas.pop().unwrap_or(0);
                    let inside_len = state.line_len.saturating_sub(after_open);
                    // Only record if the delimiter depths match (i.e. we didn't underflow).
                    if brace_depth <= state.brace_stack.len()
                        && bracket_depth <= state.bracket_depth
                        && generic_depth <= state.generic_depth()
                    {
                        info[open_idx] = Some(ParenInfo {
                            inside_len,
                            top_level_commas: comma_count,
                        });
                    }
                }
                state.out.push(')');
                state.line_len += 1;
                state.last_sig = Some(SigToken::Punct(Punct::RParen));
                continue;
            }
            Token::Punct(Punct::Comma) => {
                if let Some(last) = commas.last_mut() {
                    if let Some((_, _, brace_depth, bracket_depth, generic_depth)) = stack.last() {
                        if *brace_depth == state.brace_stack.len()
                            && *bracket_depth == state.bracket_depth
                            && *generic_depth == state.generic_depth()
                        {
                            *last += 1;
                        }
                    }
                }
            }
            Token::Punct(Punct::LBrace) => state.brace_stack.push(BraceCtx {
                kind: BraceKind::Normal,
            }),
            Token::Punct(Punct::RBrace) => {
                state.brace_stack.pop();
            }
            Token::Punct(Punct::LBracket) => state.bracket_depth += 1,
            Token::Punct(Punct::RBracket) => {
                state.bracket_depth = state.bracket_depth.saturating_sub(1)
            }
            _ => {}
        }

        // Update generic tracking for flat analysis.
        if let Token::Punct(p) = tok {
            match p {
                Punct::Less => {
                    if state
                        .should_start_generic(prev, FormatState::next_non_trivia(tokens, idx + 1))
                    {
                        state.generic_stack.push(GenericContext {
                            after_dot: matches!(
                                prev,
                                Some(SigToken::Punct(Punct::Dot | Punct::DoubleColon))
                            ),
                        });
                    }
                }
                Punct::Greater => {
                    if state.generic_depth() > 0 {
                        state.pop_generic(1);
                    }
                }
                Punct::RightShift => {
                    if state.generic_depth() > 0 {
                        state.pop_generic(2);
                    }
                }
                Punct::UnsignedRightShift => {
                    if state.generic_depth() > 0 {
                        state.pop_generic(3);
                    }
                }
                _ => {}
            }
        }

        match tok {
            Token::Word(span)
            | Token::Number(span)
            | Token::StringLiteral(span)
            | Token::CharLiteral(span)
            | Token::LineComment(span)
            | Token::BlockComment(span)
            | Token::DocComment(span) => {
                state.out.push_str(span.text(source));
                state.line_len += span.end.saturating_sub(span.start);
            }
            Token::Punct(p) => {
                p.push_to(&mut state.out);
                state.line_len += p.len();
            }
            Token::BlankLine => {}
        }
        state.last_sig = Some(sig);
    }

    info
}

fn write_token(
    state: &mut FormatState<'_>,
    tokens: &[Token],
    paren_info: &[Option<ParenInfo>],
    idx: usize,
    tok: &Token,
    next: Option<&Token>,
) {
    if state.pending_try && !matches!(tok, Token::Punct(Punct::LParen)) {
        // `try` without resources.
        state.pending_try = false;
    }

    match tok {
        Token::LineComment(span) => {
            state.write_indent();
            if state.last_sig.is_some() && !state.at_line_start {
                state.ensure_space();
            }
            let text = span.text(state.source).trim_end_matches(['\r', '\n']);
            state.out.push_str(text);
            state.line_len += text.len();
            state.ensure_newline();
            state.pending_for = false;
            state.pending_case_label = false;
        }
        Token::DocComment(span) => {
            state.write_indent();
            if state.last_sig.is_some() && !state.at_line_start {
                state.ensure_space();
            }
            state.write_block_comment(span.text(state.source));
            state.ensure_newline();
            state.pending_for = false;
            state.pending_case_label = false;
        }
        Token::BlockComment(span) => {
            state.write_indent();
            if state.last_sig.is_some() && !state.at_line_start {
                state.ensure_space();
            }
            state.write_block_comment(span.text(state.source));
            state.last_sig = Some(SigToken::Comment);
            state.pending_for = false;
            state.pending_case_label = false;
        }
        Token::Word(span) => {
            let text = span.text(state.source);
            let info = word_info(text);
            let kind = info.kind;

            if kind == WordKind::Case || kind == WordKind::Default {
                // Case labels should start on their own line.
                if let Some(ctx) = state.switch_stack.last_mut() {
                    if state.brace_stack.len() == ctx.brace_depth {
                        if ctx.in_case_body {
                            state.indent_level = state.indent_level.saturating_sub(1);
                            ctx.in_case_body = false;
                        }
                        state.ensure_newline();
                    }
                }
            }

            state.write_indent();
            let sig = SigToken::Word(info);
            if state.needs_space_before(state.last_sig, sig, tok) {
                state.ensure_space();
            }
            state.out.push_str(text);
            state.line_len += span.end.saturating_sub(span.start);
            state.last_sig = Some(sig);

            state.pending_for = kind == WordKind::For;
            state.pending_try = kind == WordKind::Try;
            if kind == WordKind::Switch {
                state.pending_switch = true;
            }

            if matches!(kind, WordKind::Case | WordKind::Default) {
                state.pending_case_label = true;
            }
        }
        Token::Number(span) | Token::StringLiteral(span) | Token::CharLiteral(span) => {
            state.write_indent();
            let sig = SigToken::Literal;
            if state.needs_space_before(state.last_sig, sig, tok) {
                state.ensure_space();
            }
            state.out.push_str(span.text(state.source));
            state.line_len += span.end.saturating_sub(span.start);
            state.last_sig = Some(sig);
            state.pending_for = false;
        }
        Token::Punct(punct) => match punct {
            Punct::LBrace => {
                state.write_indent();
                let sig = SigToken::Punct(*punct);
                if state.needs_space_before(state.last_sig, sig, tok) {
                    state.ensure_space();
                }
                punct.push_to(&mut state.out);
                state.line_len += punct.len();
                state.ensure_newline();
                state.indent_level = state.indent_level.saturating_add(1);

                let brace_kind = if state.pending_switch {
                    state.pending_switch = false;
                    let ctx = SwitchCtx {
                        brace_depth: state.brace_stack.len() + 1,
                        in_case_body: false,
                    };
                    state.switch_stack.push(ctx);
                    BraceKind::Switch
                } else {
                    BraceKind::Normal
                };
                state.brace_stack.push(BraceCtx { kind: brace_kind });
                state.last_sig = None;
                state.pending_for = false;
            }
            Punct::RBrace => {
                let closing_switch =
                    matches!(state.brace_stack.last(), Some(ctx) if ctx.kind == BraceKind::Switch);
                if closing_switch {
                    if let Some(ctx) = state.switch_stack.last_mut() {
                        if ctx.in_case_body {
                            state.indent_level = state.indent_level.saturating_sub(1);
                            ctx.in_case_body = false;
                        }
                    }
                }
                state.indent_level = state.indent_level.saturating_sub(1);
                state.ensure_newline();
                state.write_indent_level(state.indent_level);
                punct.push_to(&mut state.out);
                state.line_len += punct.len();

                if closing_switch {
                    state.brace_stack.pop();
                    state.switch_stack.pop();
                } else {
                    state.brace_stack.pop();
                }

                let join_next = match next {
                    Some(Token::Word(span)) => is_join_keyword(span.text(state.source)),
                    _ => false,
                } || matches!(
                    next,
                    Some(Token::Punct(
                        Punct::Semicolon | Punct::Comma | Punct::RParen | Punct::RBracket
                    ))
                ) || matches!(next, Some(Token::LineComment(_)));

                if let Some(Token::Word(span)) = next {
                    if is_join_keyword(span.text(state.source)) {
                        state.ensure_space();
                    }
                }
                if !join_next {
                    state.ensure_newline();
                }

                state.last_sig = Some(SigToken::Punct(Punct::RBrace));
                state.pending_for = false;
            }
            Punct::Semicolon => {
                state.write_indent();
                punct.push_to(&mut state.out);
                state.line_len += punct.len();

                let in_header = state.paren_stack.last().is_some_and(|ctx| {
                    matches!(ctx.kind, ParenKind::ForHeader | ParenKind::ResourceSpec)
                        && ctx.start_brace_depth == state.brace_stack.len()
                        && ctx.start_bracket_depth == state.bracket_depth
                        && ctx.start_generic_depth == state.generic_depth()
                });

                let next_is_comment = next.is_some_and(|t| t.is_line_comment());
                if in_header {
                    if matches!(next, Some(Token::Punct(Punct::RParen | Punct::Semicolon))) {
                        // No trailing space.
                    } else {
                        state.ensure_space();
                    }
                } else if !next_is_comment {
                    state.ensure_newline();
                }

                state.last_sig = Some(SigToken::Punct(Punct::Semicolon));
                state.pending_for = false;
                state.pending_case_label = false;
            }
            Punct::Comma => {
                state.write_indent();
                punct.push_to(&mut state.out);
                state.line_len += punct.len();

                let mut broke_line = false;
                if let Some(ctx) = state.paren_stack.last() {
                    if ctx.multiline
                        && ctx.start_brace_depth == state.brace_stack.len()
                        && ctx.start_bracket_depth == state.bracket_depth
                        && ctx.start_generic_depth == state.generic_depth()
                    {
                        state.ensure_newline();
                        broke_line = true;
                    }
                }

                if !broke_line
                    && matches!(next, Some(Token::Punct(Punct::RParen | Punct::RBracket)))
                {
                    // No space before closing.
                } else if !broke_line && next.is_some() {
                    state.ensure_space();
                }

                state.last_sig = Some(SigToken::Punct(Punct::Comma));
                state.pending_for = false;
            }
            Punct::LParen => {
                state.write_indent();
                let sig = SigToken::Punct(Punct::LParen);
                if state.needs_space_before(state.last_sig, sig, tok) {
                    state.ensure_space();
                }

                punct.push_to(&mut state.out);
                state.line_len += punct.len();

                let kind = if state.pending_for {
                    state.pending_for = false;
                    ParenKind::ForHeader
                } else if state.pending_try {
                    state.pending_try = false;
                    ParenKind::ResourceSpec
                } else {
                    ParenKind::Normal
                };

                let mut multiline = false;
                if let Some(info) = paren_info.get(idx).and_then(|i| *i) {
                    if info.top_level_commas > 0 {
                        let projected = state.line_len + info.inside_len + 1;
                        multiline = projected > state.config.max_line_length;
                    }
                }

                let base_indent = state.current_line_indent();
                let annotation_args = is_annotation_args(tokens, idx);
                let ctx = ParenCtx {
                    kind,
                    multiline,
                    base_indent,
                    content_indent: base_indent.saturating_add(1),
                    annotation_args,
                    start_brace_depth: state.brace_stack.len(),
                    start_bracket_depth: state.bracket_depth,
                    start_generic_depth: state.generic_depth(),
                };
                state.paren_stack.push(ctx);
                state.last_sig = Some(sig);

                if multiline {
                    state.ensure_newline();
                }
            }
            Punct::RParen => {
                let ctx = state.paren_stack.pop();
                if ctx.as_ref().is_some_and(|c| c.multiline) {
                    state.ensure_newline();
                    let base_indent = ctx
                        .as_ref()
                        .map(|c| c.base_indent)
                        .unwrap_or(state.indent_level);
                    state.write_indent_level(base_indent);
                } else {
                    state.write_indent();
                }

                punct.push_to(&mut state.out);
                state.line_len += punct.len();

                if ctx
                    .as_ref()
                    .is_some_and(|c| c.multiline && c.annotation_args)
                    && matches!(next, Some(Token::Word(_) | Token::Punct(Punct::At)))
                {
                    state.ensure_newline();
                }

                state.last_sig = Some(SigToken::Punct(Punct::RParen));
            }
            Punct::LBracket => {
                state.write_indent();
                let sig = SigToken::Punct(Punct::LBracket);
                if state.needs_space_before(state.last_sig, sig, tok) {
                    state.ensure_space();
                }
                punct.push_to(&mut state.out);
                state.line_len += punct.len();
                state.bracket_depth = state.bracket_depth.saturating_add(1);
                state.last_sig = Some(sig);
            }
            Punct::RBracket => {
                state.write_indent();
                punct.push_to(&mut state.out);
                state.line_len += punct.len();
                state.bracket_depth = state.bracket_depth.saturating_sub(1);
                state.last_sig = Some(SigToken::Punct(Punct::RBracket));
            }
            Punct::Dot | Punct::DoubleColon => {
                // Avoid wrapping decimal literals like `3.14`.
                let prev_is_number = matches!(state.last_sig, Some(SigToken::Literal))
                    && matches!(tokens.get(idx.wrapping_sub(1)), Some(Token::Number(_)));
                let next_is_number = matches!(next, Some(Token::Number(_)));
                let should_wrap = !prev_is_number && !next_is_number;

                if should_wrap {
                    let next_len = next.map(|t| t.display_len()).unwrap_or(0);
                    state.wrap_if_needed(state.continuation_indent(), punct.len() + next_len + 1);
                }

                punct.push_to(&mut state.out);
                state.line_len += punct.len();
                state.last_sig = Some(SigToken::Punct(*punct));
            }
            Punct::Ellipsis => {
                state.write_indent();
                punct.push_to(&mut state.out);
                state.line_len += punct.len();
                if matches!(
                    next,
                    Some(
                        Token::Word(_)
                            | Token::Number(_)
                            | Token::StringLiteral(_)
                            | Token::CharLiteral(_)
                            | Token::Punct(Punct::At)
                    )
                ) {
                    state.ensure_space();
                }
                state.last_sig = Some(SigToken::Punct(Punct::Ellipsis));
            }
            Punct::At => {
                state.write_indent();
                let sig = SigToken::Punct(Punct::At);
                if state.needs_space_before(state.last_sig, sig, tok) {
                    state.ensure_space();
                }
                punct.push_to(&mut state.out);
                state.line_len += punct.len();
                state.last_sig = Some(sig);
            }
            Punct::Less => {
                let prev = state.last_sig;
                let starts_generic = state.should_start_generic(prev, next);
                let sig = SigToken::Punct(Punct::Less);

                state.write_indent();
                if starts_generic {
                    if state.needs_space_before(prev, sig, tok) {
                        state.ensure_space();
                    }
                    punct.push_to(&mut state.out);
                    state.line_len += punct.len();
                    state.generic_stack.push(GenericContext {
                        after_dot: matches!(
                            prev,
                            Some(SigToken::Punct(Punct::Dot | Punct::DoubleColon))
                        ),
                    });
                    state.last_sig = Some(sig);
                } else {
                    // Treat as comparison operator.
                    state.ensure_space();
                    let next_len = next.map(|t| t.display_len()).unwrap_or(0);
                    state.wrap_if_needed(state.continuation_indent(), punct.len() + next_len + 1);
                    punct.push_to(&mut state.out);
                    state.line_len += punct.len();
                    if next.is_some()
                        && !matches!(
                            next,
                            Some(Token::Punct(p)) if p.is_closing_delim() || p.is_chain_separator()
                        )
                    {
                        state.ensure_space();
                    }
                    state.last_sig = Some(sig);
                }
            }
            Punct::Greater | Punct::RightShift | Punct::UnsignedRightShift => {
                let sig = SigToken::Punct(*punct);
                state.write_indent();

                if state.generic_depth() > 0 {
                    if state.needs_space_before(state.last_sig, sig, tok) {
                        state.ensure_space();
                    }
                    punct.push_to(&mut state.out);
                    state.line_len += punct.len();
                    let after_dot = match punct {
                        Punct::Greater => state.pop_generic(1),
                        Punct::RightShift => state.pop_generic(2),
                        Punct::UnsignedRightShift => state.pop_generic(3),
                        _ => false,
                    };
                    state.last_sig = Some(SigToken::GenericClose { after_dot });
                } else {
                    let prev = state.last_sig;
                    state.ensure_space();
                    let next_len = next.map(|t| t.display_len()).unwrap_or(0);
                    state.wrap_if_needed(state.continuation_indent(), punct.len() + next_len + 1);
                    punct.push_to(&mut state.out);
                    state.line_len += punct.len();
                    if prev.is_some()
                        && next.is_some()
                        && !matches!(
                            next,
                            Some(Token::Punct(p)) if p.is_closing_delim() || p.is_chain_separator()
                        )
                    {
                        state.ensure_space();
                    }
                    state.last_sig = Some(sig);
                }
            }
            Punct::Question => {
                state.write_indent();
                let sig = SigToken::Punct(Punct::Question);
                if state.generic_depth() > 0 {
                    // Wildcard: no leading space after `<`.
                    if state.needs_space_before(state.last_sig, sig, tok) {
                        state.ensure_space();
                    }
                    punct.push_to(&mut state.out);
                    state.line_len += punct.len();
                    if matches!(next, Some(Token::Word(_) | Token::Punct(Punct::At))) {
                        // `? extends` / `? super` / `? @Ann`.
                        state.ensure_space();
                    }
                } else {
                    if state.needs_space_before(state.last_sig, sig, tok) {
                        state.ensure_space();
                    }
                    let next_len = next.map(|t| t.display_len()).unwrap_or(0);
                    state.wrap_if_needed(state.continuation_indent(), punct.len() + next_len + 1);
                    punct.push_to(&mut state.out);
                    state.line_len += punct.len();
                }
                state.last_sig = Some(sig);
            }
            Punct::Colon => {
                state.write_indent();
                if state.pending_case_label {
                    punct.push_to(&mut state.out);
                    state.line_len += punct.len();
                    let next_is_line_comment = matches!(next, Some(Token::LineComment(_)));
                    if !next_is_line_comment {
                        state.ensure_newline();
                    }
                    state.pending_case_label = false;
                    if let Some(ctx) = state.switch_stack.last_mut() {
                        if state.brace_stack.len() == ctx.brace_depth && !ctx.in_case_body {
                            state.indent_level = state.indent_level.saturating_add(1);
                            ctx.in_case_body = true;
                        }
                    }
                    state.last_sig = if next_is_line_comment {
                        Some(SigToken::Punct(Punct::Colon))
                    } else {
                        None
                    };
                } else {
                    punct.push_to(&mut state.out);
                    state.line_len += punct.len();
                    if matches!(
                        next,
                        Some(
                            Token::Word(_)
                                | Token::Number(_)
                                | Token::StringLiteral(_)
                                | Token::CharLiteral(_)
                        )
                    ) {
                        state.ensure_space();
                    }
                    state.last_sig = Some(SigToken::Punct(Punct::Colon));
                }
            }
            Punct::Arrow => {
                state.write_indent();
                let sig = SigToken::Punct(Punct::Arrow);
                state.ensure_space();
                let next_len = next.map(|t| t.display_len()).unwrap_or(0);
                state.wrap_if_needed(state.continuation_indent(), punct.len() + next_len + 1);
                punct.push_to(&mut state.out);
                state.line_len += punct.len();
                state.ensure_space();
                state.last_sig = Some(sig);
                state.pending_case_label = false;
            }
            Punct::PlusPlus | Punct::MinusMinus | Punct::Bang | Punct::Tilde => {
                state.write_indent();
                let sig = SigToken::Punct(*punct);
                if state.needs_space_before(state.last_sig, sig, tok) {
                    state.ensure_space();
                }
                punct.push_to(&mut state.out);
                state.line_len += punct.len();
                state.last_sig = Some(sig);
            }
            _ => {
                let sig = SigToken::Punct(*punct);
                state.write_indent();
                let prev = state.last_sig;
                let binary = FormatState::is_binary_operator(*punct, prev, state.generic_depth());
                if binary {
                    state.ensure_space();
                    let next_len = next.map(|t| t.display_len()).unwrap_or(0);
                    state.wrap_if_needed(state.continuation_indent(), punct.len() + next_len + 1);
                }

                punct.push_to(&mut state.out);
                state.line_len += punct.len();

                // Operators generally want spaces after them when followed by something word-like.
                if binary
                    && next.is_some()
                    && !matches!(next, Some(Token::Punct(p)) if p.is_closing_delim() || p.is_chain_separator())
                {
                    state.ensure_space();
                }

                state.last_sig = Some(sig);
            }
        },
        Token::BlankLine => {}
    }
}

fn is_annotation_args(tokens: &[Token], l_paren_idx: usize) -> bool {
    let mut idx = l_paren_idx;
    let mut saw_name = false;

    while idx > 0 {
        idx -= 1;
        match tokens.get(idx) {
            Some(Token::BlankLine) => continue,
            Some(Token::Word(_)) => {
                saw_name = true;
                continue;
            }
            Some(Token::Punct(Punct::Dot)) => continue,
            Some(Token::Punct(Punct::At)) => return saw_name,
            _ => break,
        }
    }

    false
}

fn count_line_breaks(text: &str) -> u32 {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut count = 0u32;

    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                count += 1;
                i += 1;
            }
            b'\r' => {
                count += 1;
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    count
}

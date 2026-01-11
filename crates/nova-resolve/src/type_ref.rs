//! Java source type reference parsing + name resolution.
//!
//! Nova's early syntax layer (`nova-syntax`) stores type references as a raw
//! (often whitespace-stripped) string. This module turns those strings into
//! `nova_types::Type` by parsing Java type syntax and then resolving names
//! through `nova_resolve::Resolver` + the scope graph.

use std::collections::HashMap;
use std::ops::Range;

use nova_core::{Name, QualifiedName};
use nova_types::{Diagnostic, PrimitiveType, Span, Type, TypeEnv, TypeVarId, WildcardBound};

use crate::{Resolution, Resolver, ScopeGraph, ScopeId};

#[derive(Debug, Clone)]
pub struct ResolvedType {
    pub ty: Type,
    pub diagnostics: Vec<Diagnostic>,
}

/// Parse + resolve a Java source type reference from `text`.
///
/// The parser is intentionally whitespace-insensitive. It is designed to work
/// on `nova_syntax::ast::TypeRef.text`, which is currently whitespace-stripped.
///
/// Diagnostics are best-effort:
/// - Parse errors use `code = "invalid-type-ref"`.
/// - Unresolved names use `code = "unresolved-type"`.
pub fn resolve_type_ref_text<'idx>(
    resolver: &Resolver<'idx>,
    scopes: &ScopeGraph,
    scope: ScopeId,
    env: &dyn TypeEnv,
    type_vars: &HashMap<String, TypeVarId>,
    text: &str,
    base_span: Option<Span>,
) -> ResolvedType {
    let mut parser = Parser::new(resolver, scopes, scope, env, type_vars, text, base_span);
    let ty = parser.parse_type_ref();
    ResolvedType {
        ty,
        diagnostics: parser.diagnostics,
    }
}

struct Parser<'a, 'idx> {
    resolver: &'a Resolver<'idx>,
    scopes: &'a ScopeGraph,
    scope: ScopeId,
    env: &'a dyn TypeEnv,
    type_vars: &'a HashMap<String, TypeVarId>,
    text: &'a str,
    pos: usize,
    diagnostics: Vec<Diagnostic>,
    base_span: Option<Span>,
}

impl<'a, 'idx> Parser<'a, 'idx> {
    fn new(
        resolver: &'a Resolver<'idx>,
        scopes: &'a ScopeGraph,
        scope: ScopeId,
        env: &'a dyn TypeEnv,
        type_vars: &'a HashMap<String, TypeVarId>,
        text: &'a str,
        base_span: Option<Span>,
    ) -> Self {
        Self {
            resolver,
            scopes,
            scope,
            env,
            type_vars,
            text,
            pos: 0,
            diagnostics: Vec::new(),
            base_span,
        }
    }

    fn parse_type_ref(&mut self) -> Type {
        let ty = self.parse_type();

        self.skip_ws();
        if !self.is_eof() {
            let start = self.pos;
            // Consume the rest so we don't risk loops in callers that re-use `pos`.
            self.pos = self.text.len();
            self.push_error(
                "invalid-type-ref",
                "unexpected trailing tokens in type reference",
                start..self.pos,
            );
        }

        ty
    }

    fn parse_type(&mut self) -> Type {
        self.skip_ws();
        let start = self.pos;
        if self.is_eof() {
            self.push_error("invalid-type-ref", "expected a type", start..start);
            return Type::Unknown;
        }

        // In nested contexts (type args), certain chars indicate "no type here".
        if self.is_stop_char() {
            self.push_error("invalid-type-ref", "expected a type", start..start);
            return Type::Unknown;
        }

        let mut ty = if self.consume_char('?') {
            self.parse_wildcard_type(start)
        } else {
            self.parse_non_wildcard_type()
        };

        ty = self.parse_suffixes(ty, start);
        ty
    }

    fn parse_wildcard_type(&mut self, start: usize) -> Type {
        // `?`, `? extends T`, `? super T`
        self.skip_ws();

        let bound = if self.consume_keyword_glued("extends") {
            self.skip_ws();
            let b = self.parse_type();
            WildcardBound::Extends(Box::new(b))
        } else if self.consume_keyword_glued("super") {
            self.skip_ws();
            let b = self.parse_type();
            WildcardBound::Super(Box::new(b))
        } else {
            WildcardBound::Unbounded
        };

        let span = start..self.pos;
        let out = Type::Wildcard(bound);

        // `void` is never valid as wildcard bound (JLS), but we can spot the
        // common error `? extends void`.
        if let Type::Wildcard(WildcardBound::Extends(b) | WildcardBound::Super(b)) = &out {
            if matches!(b.as_ref(), Type::Void) {
                self.push_error(
                    "invalid-type-ref",
                    "`void` cannot be used as a wildcard bound",
                    span,
                );
            }
        }

        out
    }

    fn parse_non_wildcard_type(&mut self) -> Type {
        let Some((ident, ident_range)) = self.parse_ident() else {
            // Error recovery: consume one char so we always make progress.
            let err_start = self.pos;
            self.bump_char();
            self.push_error(
                "invalid-type-ref",
                "expected an identifier or primitive type",
                err_start..self.pos,
            );
            return Type::Unknown;
        };

        if let Some(prim) = primitive_from_str(&ident) {
            // Primitives cannot have type arguments.
            if self.peek_non_ws_char() == Some('<') {
                self.push_error(
                    "invalid-type-ref",
                    "primitive types cannot have type arguments",
                    ident_range.clone(),
                );
                // Attempt to skip the `<...>` so we can continue parsing suffixes.
                self.skip_angle_group();
            }
            return Type::Primitive(prim);
        }

        if ident == "void" {
            // Parseable everywhere for resilience; we only error on syntactically
            // impossible constructs like `void[]` or `List<void>`.
            if self.peek_non_ws_char() == Some('<') {
                self.push_error(
                    "invalid-type-ref",
                    "`void` cannot have type arguments",
                    ident_range.clone(),
                );
                self.skip_angle_group();
            }
            return Type::Void;
        }

        // Qualified name (dot-separated).
        let mut segments = vec![ident];
        let mut name_range = ident_range;
        loop {
            self.skip_ws();
            // `...` is a varargs suffix, not a qualified name separator.
            if self.rest().starts_with("...") {
                break;
            }
            if !self.consume_char('.') {
                break;
            }
            self.skip_ws();
            let Some((seg, seg_range)) = self.parse_ident() else {
                self.push_error(
                    "invalid-type-ref",
                    "expected identifier after `.`",
                    self.pos..self.pos,
                );
                break;
            };
            name_range.end = seg_range.end;
            segments.push(seg);
        }

        // Type arguments apply to the last segment (`List<String>`).
        let args = if self.consume_char('<') {
            self.parse_type_args()
        } else {
            Vec::new()
        };

        self.resolve_named_type(segments, args, name_range)
    }

    fn resolve_named_type(
        &mut self,
        segments: Vec<String>,
        args: Vec<Type>,
        name_range: Range<usize>,
    ) -> Type {
        let dotted = segments.join(".");
        let qname = QualifiedName::from_dotted(&dotted);

        if let Some(type_name) =
            self.resolver
                .resolve_qualified_type_in_scope(self.scopes, self.scope, &qname)
        {
            let resolved_name = type_name.as_str();
            if let Some(class_id) = self.env.lookup_class(resolved_name) {
                return Type::class(class_id, args);
            }

            // No class definition in the env; drop args (best effort).
            return Type::Named(resolved_name.to_string());
        }

        // Fall back to in-scope type variables (only for simple names).
        if segments.len() == 1 {
            if let Some(tv) = self.type_vars.get(&segments[0]) {
                if !args.is_empty() {
                    self.push_error(
                        "invalid-type-ref",
                        "type variables cannot have type arguments",
                        name_range.clone(),
                    );
                }
                return Type::TypeVar(*tv);
            }
        }

        let mut best_guess = dotted.clone();
        // Best-effort: if the first segment resolves to a type in the current
        // scope, treat remaining segments as nested class qualifiers and build a
        // binary-style name (`Outer$Inner`).
        if segments.len() > 1 {
            let first = Name::from(segments[0].as_str());
            if let Some(Resolution::Type(owner)) =
                self.resolver.resolve_name(self.scopes, self.scope, &first)
            {
                let mut candidate = owner.as_str().to_string();
                for seg in &segments[1..] {
                    candidate.push('$');
                    candidate.push_str(seg);
                }
                best_guess = candidate;
            }
        }

        self.diagnostics.push(Diagnostic::error(
            "unresolved-type",
            format!("unresolved type `{dotted}`"),
            self.anchor_span(name_range),
        ));

        Type::Named(best_guess)
    }

    fn parse_type_args(&mut self) -> Vec<Type> {
        let mut args = Vec::new();

        loop {
            self.skip_ws();
            if self.consume_char('>') {
                if args.is_empty() {
                    self.push_error(
                        "invalid-type-ref",
                        "expected at least one type argument",
                        self.pos.saturating_sub(1)..self.pos,
                    );
                }
                break;
            }

            if self.is_eof() {
                self.push_error(
                    "invalid-type-ref",
                    "unterminated type argument list (missing `>`)",
                    self.pos..self.pos,
                );
                break;
            }

            let arg = self.parse_type();
            // `void` is never allowed as a type argument.
            if matches!(arg, Type::Void) {
                self.push_error(
                    "invalid-type-ref",
                    "`void` cannot be used as a type argument",
                    self.pos.saturating_sub(4)..self.pos, // best-effort
                );
            }
            args.push(arg);

            self.skip_ws();
            if self.consume_char(',') {
                continue;
            }
            if self.consume_char('>') {
                break;
            }

            if self.is_eof() {
                self.push_error(
                    "invalid-type-ref",
                    "unterminated type argument list (missing `>`)",
                    self.pos..self.pos,
                );
                break;
            }

            // Error recovery: skip until `,` or `>` (but don't consume the terminator).
            self.push_error(
                "invalid-type-ref",
                "expected `,` or `>` in type argument list",
                self.pos..self.pos,
            );
            self.skip_until(|ch| ch == ',' || ch == '>');
        }

        args
    }

    fn parse_suffixes(&mut self, mut ty: Type, type_start: usize) -> Type {
        loop {
            self.skip_ws();
            if self.consume_str("[]") {
                // `void[]` is syntactically impossible.
                if matches!(ty, Type::Void) {
                    self.push_error(
                        "invalid-type-ref",
                        "`void` cannot be an array element type",
                        type_start..self.pos,
                    );
                    ty = Type::Unknown;
                } else {
                    ty = Type::Array(Box::new(ty));
                }
                continue;
            }

            if self.consume_str("...") {
                // Varargs are represented as an extra array dimension.
                if matches!(ty, Type::Void) {
                    self.push_error(
                        "invalid-type-ref",
                        "`void` cannot be a varargs element type",
                        type_start..self.pos,
                    );
                    ty = Type::Unknown;
                } else {
                    ty = Type::Array(Box::new(ty));
                }
                continue;
            }

            break;
        }

        ty
    }

    // --- lexing helpers ------------------------------------------------------

    fn is_eof(&self) -> bool {
        self.pos >= self.text.len()
    }

    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn peek_char(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_non_ws_char(&self) -> Option<char> {
        let mut idx = self.pos;
        while idx < self.text.len() {
            let ch = self.text[idx..].chars().next()?;
            if !ch.is_whitespace() {
                return Some(ch);
            }
            idx += ch.len_utf8();
        }
        None
    }

    fn bump_char(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn skip_ws(&mut self) {
        while let Some(ch) = self.peek_char() {
            if ch.is_whitespace() {
                self.bump_char();
            } else {
                break;
            }
        }
    }

    fn consume_char(&mut self, expected: char) -> bool {
        self.skip_ws();
        if self.peek_char() == Some(expected) {
            self.bump_char();
            true
        } else {
            false
        }
    }

    fn consume_str(&mut self, s: &str) -> bool {
        self.skip_ws();
        if self.rest().starts_with(s) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn consume_keyword_glued(&mut self, kw: &str) -> bool {
        // This intentionally does *not* require a token boundary. `TypeRef.text`
        // is whitespace-stripped, so `? extends T` becomes `?extendsT`.
        self.skip_ws();
        if self.rest().starts_with(kw) {
            self.pos += kw.len();
            true
        } else {
            false
        }
    }

    fn parse_ident(&mut self) -> Option<(String, Range<usize>)> {
        self.skip_ws();
        let start = self.pos;
        let mut chars = self.rest().char_indices();
        let (_, first) = chars.next()?;

        if !is_ident_start(first) {
            return None;
        }

        let mut end = start + first.len_utf8();
        for (idx, ch) in chars {
            if is_ident_part(ch) {
                end = start + idx + ch.len_utf8();
            } else {
                break;
            }
        }

        self.pos = end;
        Some((self.text[start..end].to_string(), start..end))
    }

    fn is_stop_char(&self) -> bool {
        matches!(self.peek_char(), Some('>') | Some(',') | Some(')'))
    }

    // --- error recovery helpers ---------------------------------------------

    fn skip_until(&mut self, mut predicate: impl FnMut(char) -> bool) {
        while let Some(ch) = self.peek_char() {
            if predicate(ch) {
                break;
            }
            self.bump_char();
        }
    }

    fn skip_angle_group(&mut self) {
        // Called when we see `<` at the current position (after skipping ws).
        self.skip_ws();
        if self.peek_char() != Some('<') {
            return;
        }
        let start = self.pos;
        self.bump_char(); // '<'
        let mut depth = 1usize;
        while let Some(ch) = self.peek_char() {
            match ch {
                '<' => {
                    depth += 1;
                    self.bump_char();
                }
                '>' => {
                    depth = depth.saturating_sub(1);
                    self.bump_char();
                    if depth == 0 {
                        break;
                    }
                }
                _ => {
                    self.bump_char();
                }
            }
        }
        if depth != 0 {
            self.push_error(
                "invalid-type-ref",
                "unterminated type argument list (missing `>`)",
                start..self.pos,
            );
        }
    }

    // --- diagnostics ---------------------------------------------------------

    fn push_error(&mut self, code: &'static str, message: impl Into<String>, range: Range<usize>) {
        self.diagnostics
            .push(Diagnostic::error(code, message, self.anchor_span(range)));
    }

    fn anchor_span(&self, range: Range<usize>) -> Option<Span> {
        let base = self.base_span?;
        let mut start = base.start.saturating_add(range.start);
        let mut end = base.start.saturating_add(range.end);
        if end > base.end {
            end = base.end;
        }
        if start > end {
            start = end;
        }
        Some(Span::new(start, end))
    }
}

fn primitive_from_str(s: &str) -> Option<PrimitiveType> {
    Some(match s {
        "boolean" => PrimitiveType::Boolean,
        "byte" => PrimitiveType::Byte,
        "short" => PrimitiveType::Short,
        "char" => PrimitiveType::Char,
        "int" => PrimitiveType::Int,
        "long" => PrimitiveType::Long,
        "float" => PrimitiveType::Float,
        "double" => PrimitiveType::Double,
        _ => return None,
    })
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

fn is_ident_part(ch: char) -> bool {
    is_ident_start(ch) || ch.is_ascii_digit()
}

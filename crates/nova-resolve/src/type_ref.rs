//! Java source type reference parsing + name resolution.
//!
//! Nova's early syntax layer (`nova-syntax`) stores type references as a raw
//! (often whitespace-stripped) string. This module turns those strings into
//! `nova_types::Type` by parsing Java type syntax and then resolving names
//! through `nova_resolve::Resolver` + the scope graph.

use std::collections::HashMap;
use std::ops::Range;

use nova_core::{Name, QualifiedName};
use nova_types::{
    lub, ClassDef, ClassKind, Diagnostic, PrimitiveType, Span, Type, TypeEnv, TypeVarId,
    WildcardBound,
};

use crate::{Resolver, ScopeGraph, ScopeId, TypeNameResolution};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
enum AnnotationSkipContext {
    /// Skipping annotations before a type (e.g. `@A String`).
    BeforeType,
    /// Skipping annotations before a qualified name segment (e.g. `Outer.@A Inner`).
    BeforeQualifiedSegment,
    /// Skipping annotations before suffixes like `[]`/`...` (e.g. `String @A []`).
    BeforeSuffix,
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
        self.parse_union_type()
    }

    fn parse_union_type(&mut self) -> Type {
        // Union types (`A|B|C`) can appear in Java multi-catch (`catch (A|B e)`).
        // We model them as the least-upper-bound of the alternatives.
        let mut ty = self.parse_intersection_type();
        loop {
            self.skip_ws();
            if !self.consume_char('|') {
                break;
            }
            let rhs = self.parse_intersection_type();
            ty = lub(self.env, &ty, &rhs);
        }
        ty
    }

    fn parse_intersection_type(&mut self) -> Type {
        // Intersection types (`A&B&C`) are common in bounds and appear in some
        // best-effort type representations.
        let mut types = Vec::new();
        let first = self.parse_single_type();
        types.push(first);
        loop {
            self.skip_ws();
            if !self.consume_char('&') {
                break;
            }
            let next = self.parse_single_type();
            types.push(next);
        }

        if types.len() == 1 {
            types.pop().unwrap_or(Type::Unknown)
        } else {
            Type::Intersection(types)
        }
    }

    fn parse_single_type(&mut self) -> Type {
        self.skip_ws();
        self.skip_annotations(AnnotationSkipContext::BeforeType);
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

        // `ClassOrInterfaceType` (JLS 4.3.2):
        //   Ident [TypeArgs] ('.' Ident [TypeArgs])*
        //
        // Java allows type arguments on qualifying types (e.g. `Outer<String>.Inner`).
        // Nova's `Type` model does not represent owner types directly, so we parse
        // per-segment args and then flatten them in outer-to-inner order during
        // resolution (matching `nova-types-signature`).
        let mut segments = vec![ident];
        let mut per_segment_args: Vec<Vec<Type>> = Vec::new();
        let mut name_range = ident_range;

        // Parse type arguments for the first segment.
        let first_args = if self.consume_char('<') {
            self.parse_type_args()
        } else {
            Vec::new()
        };
        per_segment_args.push(first_args);

        loop {
            self.skip_ws();
            // `...` is a varargs suffix, not a qualified name separator.
            if self.rest().starts_with("...") {
                break;
            }
            if !self.consume_char('.') {
                break;
            }
            self.skip_annotations(AnnotationSkipContext::BeforeQualifiedSegment);
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

            let seg_args = if self.consume_char('<') {
                self.parse_type_args()
            } else {
                Vec::new()
            };
            per_segment_args.push(seg_args);
        }

        self.resolve_named_type(segments, per_segment_args, name_range)
    }

    fn resolve_named_type(
        &mut self,
        segments: Vec<String>,
        per_segment_args: Vec<Vec<Type>>,
        name_range: Range<usize>,
    ) -> Type {
        // In-scope type variables take precedence over types (JLS 6.5).
        if segments.len() == 1 {
            if let Some(tv) = self.type_vars.get(&segments[0]) {
                let has_args = per_segment_args.iter().any(|a| !a.is_empty());
                if has_args {
                    self.push_error(
                        "invalid-type-ref",
                        "type variables cannot have type arguments",
                        name_range.clone(),
                    );
                }
                return Type::TypeVar(*tv);
            }
        }

        let dotted = segments.join(".");
        let flattened_args: Vec<Type> = per_segment_args.iter().flatten().cloned().collect();
        let args_for_class = |class_id| {
            // Avoid turning raw types / diamond (`List` / `List<>`) into
            // `List<Unknown>`. Source type refs commonly omit type args, and we
            // intentionally model that as "raw" (`args = []`).
            if flattened_args.is_empty() {
                return Vec::new();
            }
            match self.env.class(class_id) {
                Some(class_def) => reconcile_class_args(
                    class_def.type_params.len(),
                    &per_segment_args,
                    flattened_args.clone(),
                ),
                None => flattened_args.clone(),
            }
        };

        // Resolve simple names in the type namespace and preserve ambiguity for diagnostics.
        if segments.len() == 1 {
            let ident = Name::from(segments[0].as_str());
            match self
                .resolver
                .resolve_type_name_detailed(self.scopes, self.scope, &ident)
            {
                TypeNameResolution::Resolved(resolution) => {
                    if let Some(type_name) = self
                        .resolver
                        .type_name_for_resolution(self.scopes, &resolution)
                    {
                        let resolved_name = type_name.as_str();
                        if let Some(class_id) = self.env.lookup_class(resolved_name) {
                            return Type::class(class_id, args_for_class(class_id));
                        }

                        // No class definition in the env; drop args (best effort).
                        return Type::Named(resolved_name.to_string());
                    }
                }
                TypeNameResolution::Ambiguous(candidates) => {
                    let mut candidate_names: Vec<String> = candidates
                        .iter()
                        .filter_map(|c| {
                            self.resolver
                                .type_name_for_resolution(self.scopes, c)
                                .map(|n| n.as_str().to_string())
                        })
                        .collect();
                    candidate_names.sort();
                    candidate_names.dedup();

                    let mut msg = format!("ambiguous type `{}`", segments[0]);
                    if !candidate_names.is_empty() {
                        msg.push_str(": ");
                        msg.push_str(&candidate_names.join(", "));
                    }

                    self.diagnostics.push(Diagnostic::error(
                        "ambiguous-type",
                        msg,
                        self.anchor_span(name_range.clone()),
                    ));

                    // Best-effort: prefer `java.lang.*` if present, otherwise pick the first
                    // candidate. This keeps behavior stable while still surfacing diagnostics.
                    let best = candidate_names
                        .iter()
                        .find(|c| c.starts_with("java.lang."))
                        .or_else(|| candidate_names.first())
                        .cloned();

                    if let Some(best) = best {
                        if let Some(class_id) = self.env.lookup_class(&best) {
                            return Type::class(class_id, args_for_class(class_id));
                        }
                        return Type::Named(best);
                    }

                    // If we failed to compute any candidate names, fall back to a stable best-effort
                    // resolution without also reporting an unresolved-type diagnostic (we already
                    // know the reference is ambiguous).
                    let java_lang = format!("java.lang.{}", segments[0]);
                    if let Some(class_id) = self.env.lookup_class(&java_lang) {
                        return Type::class(class_id, args_for_class(class_id));
                    }
                    return Type::Named(dotted);
                }
                TypeNameResolution::Unresolved => {}
            }
        }

        // Qualified name resolution (and simple-name fallback).
        let qname = QualifiedName::from_dotted(&dotted);
        if let Some(type_name) =
            self.resolver
                .resolve_qualified_type_in_scope(self.scopes, self.scope, &qname)
        {
            let resolved_name = type_name.as_str();
            if let Some(class_id) = self.env.lookup_class(resolved_name) {
                return Type::class(class_id, args_for_class(class_id));
            }

            // No class definition in the env; drop args (best effort).
            return Type::Named(resolved_name.to_string());
        }

        // If the resolver can't map the name, fall back to the `TypeEnv` for the
        // implicit `java.lang.*` universe scope (JLS 7.5.5).
        //
        // Note: `TypeStore::intern_class_id` inserts a placeholder `ClassDef` that
        // is visible through `TypeEnv::lookup_class`. We intentionally ignore
        // those placeholders here so pre-interning external types does not bypass
        // name-resolution rules (e.g. JPMS readability/exports checks enforced by
        // the resolver).
        //
        // IMPORTANT: Avoid falling back to `env.lookup_class` for qualified
        // names. In JPMS mode the resolver is module-aware and will intentionally
        // return `None` for unreadable/unexported types. The `TypeEnv`/`TypeStore`
        // may still contain those types (pre-interned from a flattened classpath
        // index), so resolving qualified names here would bypass module-access
        // restrictions and suppress `unresolved-type` diagnostics.
        if segments.len() == 1 {
            let java_lang = format!("java.lang.{}", segments[0]);
            if let Some(class_id) = self.lookup_non_placeholder_class(&java_lang) {
                return Type::class(class_id, args_for_class(class_id));
            }
        }

        // Best-effort: if the first segment resolves to a type in the current
        // scope, treat remaining segments as nested class qualifiers and build a
        // binary-style name (`Outer$Inner`).
        let mut best_guess = dotted.clone();
        if segments.len() > 1 {
            let first = Name::from(segments[0].as_str());
            if let Some(owner) = self
                .resolver
                .resolve_type_name(self.scopes, self.scope, &first)
            {
                if let Some(owner_name) =
                    self.resolver.type_name_for_resolution(self.scopes, &owner)
                {
                    let mut candidate = owner_name.as_str().to_string();
                    for seg in &segments[1..] {
                        candidate.push('$');
                        candidate.push_str(seg);
                    }
                    best_guess = candidate;
                }
            }
        }

        // If we successfully guessed a binary nested name, try resolving it in
        // the environment before reporting an unresolved-type diagnostic.
        //
        // IMPORTANT: Only consult the environment for the binary nested-name
        // guess (`Outer$Inner`). Never resolve the original dotted spelling via
        // the environment, since that can bypass JPMS/module-access restrictions.
        if best_guess != dotted {
            if let Some(class_id) = self.lookup_non_placeholder_class(&best_guess) {
                return Type::class(class_id, args_for_class(class_id));
            }
        }

        self.diagnostics.push(Diagnostic::error(
            "unresolved-type",
            format!("unresolved type `{dotted}`"),
            self.anchor_span(name_range),
        ));

        Type::Named(best_guess)
    }

    fn lookup_non_placeholder_class(&self, name: &str) -> Option<nova_types::ClassId> {
        let id = self.env.lookup_class(name)?;
        let def = self.env.class(id)?;
        (!is_placeholder_class_def(def)).then_some(id)
    }

    fn parse_type_args(&mut self) -> Vec<Type> {
        let mut args = Vec::new();

        loop {
            self.skip_ws();
            if self.consume_char('>') {
                // Accept empty `<>` (diamond) syntax with no diagnostics. This
                // can appear in `TypeRef.text` for `new Foo<>()`.
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
            self.skip_annotations(AnnotationSkipContext::BeforeSuffix);
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
        // `self.pos` is maintained as a byte offset into `self.text`, and should always stay on a
        // UTF-8 char boundary. However, this parser is used in best-effort IDE paths and should
        // never panic if a stale/corrupted offset slips through (e.g. due to parse recovery or
        // mismatched memoization).
        self.text.get(self.pos..).unwrap_or("")
    }

    fn peek_char(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_non_ws_char(&self) -> Option<char> {
        let mut idx = self.pos;
        while idx < self.text.len() {
            let ch = self.text.get(idx..).and_then(|s| s.chars().next())?;
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

    // --- annotation skipping -------------------------------------------------

    fn skip_annotations(&mut self, ctx: AnnotationSkipContext) {
        loop {
            self.skip_ws();
            if self.peek_char() != Some('@') {
                break;
            }

            let before = self.pos;
            self.skip_annotation_use(ctx);
            if self.pos == before {
                // Ensure progress even on malformed inputs.
                self.bump_char();
            }
        }
    }

    fn skip_annotation_use(&mut self, ctx: AnnotationSkipContext) {
        self.skip_ws();
        if self.peek_char() != Some('@') {
            return;
        }
        self.bump_char(); // '@'
        self.skip_ws();

        let name_start = self.pos;
        let Some(first) = self.peek_char() else {
            return;
        };
        if !is_ident_start(first) {
            // Malformed annotation; best-effort skip just the `@`.
            return;
        }

        // Greedily consume a qualified identifier, but don't accidentally swallow
        // varargs `...` (only accept `.` when followed by another identifier).
        let greedy_end = self.scan_greedy_qualified_ident_end();

        // Greedy path: consume the full qualified name + optional arg list.
        self.pos = greedy_end;
        self.skip_ws();
        if self.peek_char() == Some('(') {
            self.skip_paren_group();
        }
        self.skip_ws();

        if ctx == AnnotationSkipContext::BeforeSuffix {
            // Array/varargs suffixes provide a delimiter, so we don't need to do
            // any heuristic splitting.
            self.resolve_annotation_name(name_start..greedy_end);
            return;
        }

        // In `TypeRef.text` whitespace is stripped, so annotation names can be glued to the next
        // token. One particularly tricky case is when a *suffix* annotation follows the type,
        // e.g. `@A String @B []` -> `@AString@B[]` or `Outer.@A Inner @B []` -> `Outer.@AInner@B[]`.
        //
        // A purely greedy parse would treat `AString` / `AInner` as the annotation name, leaving
        // `@B[]` without a base type/segment. If the next token is another `@`, run the splitting
        // heuristic even if the greedy parse looks plausible.
        if self.annotation_follow_is_ok(ctx) && self.peek_char() != Some('@') {
            self.resolve_annotation_name(name_start..greedy_end);
            return;
        }

        // Heuristic recovery: in `TypeRef.text` whitespace is stripped, so
        // `@A String` becomes `@AString`. The greedy parse above would treat
        // `AString` as a single identifier, leaving nothing (or only `<...>` /
        // `[]`) for the actual type. Try to split the identifier so the
        // remainder parses as a type/segment.
        let name_end = self.find_best_annotation_name_end(name_start, greedy_end, ctx);
        self.pos = name_end;
        self.skip_ws();
        if self.peek_char() == Some('(') {
            self.skip_paren_group();
        }
        self.skip_ws();
        self.resolve_annotation_name(name_start..name_end);
    }

    fn scan_greedy_qualified_ident_end(&self) -> usize {
        let mut pos = self.pos;

        // First segment.
        let mut chars = self.text.get(pos..).unwrap_or("").char_indices();
        let Some((_, first)) = chars.next() else {
            return pos;
        };
        if !is_ident_start(first) {
            return pos;
        }
        pos += first.len_utf8();
        for (idx, ch) in chars {
            if is_ident_part(ch) {
                pos = self.pos + idx + ch.len_utf8();
            } else {
                break;
            }
        }

        // Additional segments (`.` + ident) as long as the dot is followed by
        // another identifier segment.
        loop {
            if pos >= self.text.len() || self.text.as_bytes()[pos] != b'.' {
                break;
            }
            // Look ahead one char after '.'
            let after_dot = pos + 1;
            let Some(next) = self.text.get(after_dot..).and_then(|s| s.chars().next()) else {
                break;
            };
            if !is_ident_start(next) {
                break;
            }

            // Consume '.' and the next segment.
            pos += 1;
            pos += next.len_utf8();
            while pos < self.text.len() {
                let Some(ch) = self.text.get(pos..).and_then(|s| s.chars().next()) else {
                    break;
                };
                if is_ident_part(ch) {
                    pos += ch.len_utf8();
                } else {
                    break;
                }
            }
        }

        pos
    }

    fn annotation_follow_is_ok(&self, ctx: AnnotationSkipContext) -> bool {
        let mut idx = self.pos;
        while idx < self.text.len() {
            let Some(ch) = self.text.get(idx..).and_then(|s| s.chars().next()) else {
                break;
            };
            if ch.is_whitespace() {
                idx += ch.len_utf8();
            } else {
                break;
            }
        }

        let Some(ch) = self.text.get(idx..).and_then(|s| s.chars().next()) else {
            return false;
        };

        match ctx {
            AnnotationSkipContext::BeforeType => ch == '@' || ch == '?' || is_ident_start(ch),
            AnnotationSkipContext::BeforeQualifiedSegment => ch == '@' || is_ident_start(ch),
            AnnotationSkipContext::BeforeSuffix => true,
        }
    }

    fn resolve_annotation_name(&mut self, name_range: Range<usize>) {
        // Type-use annotation names can appear anywhere inside a type reference (`@A String`,
        // `Outer.@A Inner`, `String @A []`, ...). Nova does not currently model these in
        // `nova_types::Type`, so type refs should parse/resolve as if the annotation were absent.
        //
        // We *do* emit best-effort diagnostics for the annotation's *type name* when parsing an
        // anchored source range (e.g. via `nova_db` / IDE diagnostics), because the annotation type
        // being missing from the classpath is still actionable for users.
        //
        // However, callers frequently parse detached `TypeRef.text` snippets with
        // `base_span = None`, and `TypeRef.text` may be whitespace-stripped. If we can't reliably
        // map offsets back into the original source text, suppress these diagnostics to avoid
        // mis-anchored spans and noisy errors.
        let Some(base_span) = self.base_span else {
            return;
        };
        if base_span.len() != self.text.len() {
            return;
        }
        if name_range.is_empty() {
            return;
        }
        let text = self.text.get(name_range.clone()).unwrap_or("");
        if text.is_empty() {
            return;
        }

        let segments: Vec<String> = text
            .split('.')
            .filter(|seg| !seg.is_empty())
            .map(|seg| seg.to_string())
            .collect();
        if segments.is_empty() {
            return;
        }

        let per_segment_args = vec![Vec::new(); segments.len()];
        let _ = self.resolve_named_type(segments, per_segment_args, name_range);
    }

    fn find_best_annotation_name_end(
        &self,
        name_start: usize,
        greedy_end: usize,
        ctx: AnnotationSkipContext,
    ) -> usize {
        let mut best_end = greedy_end;
        // Key layout documented in `consider_annotation_split_candidate`.
        let mut best_key: Option<(usize, usize, usize, u8, bool, usize)> = None;

        // Scan through the greedy qualified identifier and consider every
        // possible boundary that produces a syntactically valid qualified name
        // prefix. We allow splitting inside an identifier segment because
        // whitespace stripping may have glued the next token onto it.
        let mut idx = name_start;
        let mut at_segment_start = true;
        let mut seen_any_ident = false;

        while idx < greedy_end {
            let Some(ch) = self.text.get(idx..).and_then(|s| s.chars().next()) else {
                break;
            };
            let next_idx = idx + ch.len_utf8();

            if at_segment_start {
                if !is_ident_start(ch) {
                    break;
                }
                at_segment_start = false;
                seen_any_ident = true;
                // Candidate end after the first char of a segment.
                let end = next_idx;
                self.consider_annotation_split_candidate(ctx, end, &mut best_end, &mut best_key);
                idx = next_idx;
                continue;
            }

            if ch == '.' {
                // Qualified names cannot have empty segments.
                at_segment_start = true;
                idx = next_idx;
                continue;
            }

            if !is_ident_part(ch) {
                break;
            }

            if seen_any_ident {
                let end = next_idx;
                self.consider_annotation_split_candidate(ctx, end, &mut best_end, &mut best_key);
            }

            idx = next_idx;
        }

        best_end
    }

    fn consider_annotation_split_candidate(
        &self,
        ctx: AnnotationSkipContext,
        name_end: usize,
        best_end: &mut usize,
        best_key: &mut Option<(usize, usize, usize, u8, bool, usize)>,
    ) {
        // Evaluate the remainder if we stop the annotation name at `name_end`.
        let mut look = Parser {
            resolver: self.resolver,
            scopes: self.scopes,
            scope: self.scope,
            env: self.env,
            type_vars: self.type_vars,
            text: self.text,
            pos: name_end,
            diagnostics: Vec::new(),
            // This parser is only used for ranking annotation-splitting candidates; it should not
            // attempt to resolve type-use annotations and introduce extra (unanchored) diagnostics
            // that would skew the heuristic.
            base_span: None,
        };

        look.skip_ws();
        if look.peek_char() == Some('(') {
            look.skip_paren_group();
        }
        look.skip_ws();
        let type_start = look.pos;

        let start_ch = look.peek_char();
        let starts_upper = start_ch.is_some_and(|c| c.is_ascii_uppercase());

        // In `BeforeQualifiedSegment` contexts, we must land on an identifier (or another
        // annotation), not on `?` etc.
        let ctx_start_penalty = match (ctx, start_ch) {
            (AnnotationSkipContext::BeforeQualifiedSegment, Some('?')) => 1usize,
            (AnnotationSkipContext::BeforeQualifiedSegment, Some(ch))
                if ch != '@' && !is_ident_start(ch) =>
            {
                1
            }
            (AnnotationSkipContext::BeforeQualifiedSegment, None) => 1,
            _ => 0,
        };

        let ty = look.parse_type();
        let invalid = look
            .diagnostics
            .iter()
            .filter(|d| d.code.as_ref() == "invalid-type-ref")
            .count();
        let unresolved = look
            .diagnostics
            .iter()
            .filter(|d| d.code.as_ref() == "unresolved-type")
            .count();

        let kind_rank = type_rank(&ty);

        // Key layout:
        // - ctx_start_penalty: ensure we don't pick obviously invalid follow positions in
        //   qualified-segment context.
        // - invalid: prefer parses that don't produce `invalid-type-ref`.
        // - unresolved: prefer parses that resolve (for `@AString` -> `String`).
        // - kind_rank: prefer "real" types over unknown/named.
        // - !starts_upper: for segment contexts, prefer starts that look like type names.
        // - Reverse(type_start): prefer consuming as much as possible as annotation.
        let key = match ctx {
            AnnotationSkipContext::BeforeType => (
                ctx_start_penalty,
                invalid,
                unresolved,
                kind_rank,
                false, // unused
                usize::MAX - type_start,
            ),
            AnnotationSkipContext::BeforeQualifiedSegment => (
                ctx_start_penalty,
                invalid,
                0, // unresolved is noisy here; ignore it for ranking
                kind_rank,
                !starts_upper,
                usize::MAX - type_start,
            ),
            AnnotationSkipContext::BeforeSuffix => unreachable!("suffix mode does not split"),
        };

        if best_key.is_none_or(|best| key < best) {
            *best_key = Some(key);
            *best_end = name_end;
        }
    }

    fn skip_paren_group(&mut self) {
        self.skip_ws();
        if self.peek_char() != Some('(') {
            return;
        }

        self.bump_char(); // '('
        let mut depth = 1usize;
        while let Some(ch) = self.peek_char() {
            match ch {
                '(' => {
                    depth += 1;
                    self.bump_char();
                }
                ')' => {
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

fn is_placeholder_class_def(def: &ClassDef) -> bool {
    def.kind == ClassKind::Class
        && def.name != "java.lang.Object"
        && def.super_class.is_none()
        && def.type_params.is_empty()
        && def.interfaces.is_empty()
        && def.fields.is_empty()
        && def.constructors.is_empty()
        && def.methods.is_empty()
}

fn type_rank(ty: &Type) -> u8 {
    match ty {
        Type::Array(elem) => type_rank(elem.as_ref()),
        Type::Named(_) => 2,
        Type::Unknown | Type::Error => 3,
        _ => 0,
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
    ch == '_' || ch == '$' || unicode_ident::is_xid_start(ch)
}

fn is_ident_part(ch: char) -> bool {
    ch == '$' || ch == '_' || unicode_ident::is_xid_continue(ch)
}

fn reconcile_class_args(
    expected_len: usize,
    per_segment_args: &[Vec<Type>],
    flattened: Vec<Type>,
) -> Vec<Type> {
    // Keep behavior consistent with `nova-types-signature` for nested generics
    // while staying resilient to partial / broken inputs.
    //
    // Note: Callers should generally avoid this for raw types (`args.is_empty()`),
    // since `nova_types::Type` uses `args = []` to represent raw references.
    if expected_len == 0 {
        return flattened;
    }
    if flattened.len() == expected_len {
        return flattened;
    }

    if let Some(last) = per_segment_args.last() {
        if last.len() == expected_len {
            return last.clone();
        }
    }

    if flattened.len() > expected_len {
        let start = flattened.len().saturating_sub(expected_len);
        return flattened[start..].to_vec();
    }

    let missing = expected_len.saturating_sub(flattened.len());
    let mut out = Vec::with_capacity(expected_len);
    out.extend(std::iter::repeat_n(Type::Unknown, missing));
    out.extend(flattened);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scopes::build_scopes_for_item_tree;
    use nova_core::FileId;
    use nova_hir::item_tree::ItemTree;
    use nova_jdk::JdkIndex;
    use nova_types::TypeStore;

    #[test]
    fn parser_helpers_are_panic_free_on_non_char_boundary_offsets() {
        let jdk = JdkIndex::new();
        let resolver = Resolver::new(&jdk);
        let tree = ItemTree::default();
        let scope_result = build_scopes_for_item_tree(FileId::new(0), &tree);
        let env = TypeStore::default();
        let type_vars: HashMap<String, TypeVarId> = HashMap::new();

        // `Ā` is a 2-byte UTF-8 character. Force `pos` onto a non-char boundary to ensure helper
        // routines never panic on corrupted offsets.
        let text = "Ā.B";
        let mut parser = Parser::new(
            &resolver,
            &scope_result.scopes,
            scope_result.file_scope,
            &env,
            &type_vars,
            text,
            None,
        );
        parser.pos = 1;

        assert_eq!(parser.rest(), "");
        assert_eq!(parser.peek_char(), None);
        assert_eq!(parser.peek_non_ws_char(), None);

        let _ = parser.scan_greedy_qualified_ident_end();
        let _ = parser.annotation_follow_is_ok(AnnotationSkipContext::BeforeType);
        let _ = parser.find_best_annotation_name_end(
            parser.pos,
            text.len(),
            AnnotationSkipContext::BeforeType,
        );
    }
}

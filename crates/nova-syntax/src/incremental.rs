use rowan::{NodeOrToken, TokenAtOffset};

use std::collections::HashMap;

use crate::parser::{
    parse_annotation_element_value_pair_list_fragment, parse_argument_list_fragment,
    parse_block_fragment, parse_class_body_fragment, parse_class_member_fragment,
    parse_parameter_list_fragment, parse_switch_block_fragment, parse_type_arguments_fragment,
    parse_type_parameters_fragment, StatementContext, SwitchContext,
};
use crate::{lex, parse_java, JavaParseResult, ParseError, SyntaxKind, TextEdit, TextRange};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReparseTarget {
    Block(StatementContext),
    SwitchBlock {
        stmt_ctx: StatementContext,
        switch_ctx: SwitchContext,
    },
    ClassBody(SyntaxKind),
    ClassMember,
    ArgumentList,
    AnnotationElementValuePairList,
    ParameterList,
    TypeArguments,
    TypeParameters,
}

#[derive(Debug)]
struct ReparsePlan {
    target: ReparseTarget,
    target_node: crate::SyntaxNode,
    /// Byte range of the reparsed node in the *old* text.
    old_range: TextRange,
    /// Byte range of the reparsed node in the *new* text.
    new_range: TextRange,
}

/// Incrementally reparse a Java file using a single text edit.
///
/// This attempts to reparse a small syntactic region (block, statement/member list,
/// class member) and then splices the new green subtree into the previous tree,
/// reusing unchanged green nodes.
///
/// If the edit touches lexically ambiguous tokens (strings/comments/text blocks),
/// or no suitable reparse root can be found, this falls back to a full reparse.
pub fn reparse_java(
    old: &JavaParseResult,
    old_text: &str,
    edit: TextEdit,
    new_text: &str,
) -> JavaParseResult {
    debug_assert_eq!(
        old_text.len() as u32,
        u32::from(old.syntax().text_range().end()),
        "old_text length must match syntax tree"
    );

    let Some(plan) = build_reparse_plan(old, old_text, &edit, new_text) else {
        return parse_java(new_text);
    };

    let fragment_start = plan.new_range.start as usize;
    let fragment_end = plan.new_range.end as usize;

    if fragment_start > fragment_end || fragment_end > new_text.len() {
        return parse_java(new_text);
    }
    if !new_text.is_char_boundary(fragment_start) || !new_text.is_char_boundary(fragment_end) {
        return parse_java(new_text);
    }

    let fragment_text = &new_text[fragment_start..fragment_end];

    let fragment = match plan.target {
        ReparseTarget::Block(stmt_ctx) => parse_block_fragment(fragment_text, stmt_ctx),
        ReparseTarget::SwitchBlock {
            stmt_ctx,
            switch_ctx,
        } => parse_switch_block_fragment(fragment_text, stmt_ctx, switch_ctx),
        ReparseTarget::ClassBody(kind) => parse_class_body_fragment(fragment_text, kind),
        ReparseTarget::ClassMember => parse_class_member_fragment(fragment_text),
        ReparseTarget::ArgumentList => parse_argument_list_fragment(fragment_text),
        ReparseTarget::AnnotationElementValuePairList => {
            parse_annotation_element_value_pair_list_fragment(fragment_text)
        }
        ReparseTarget::ParameterList => parse_parameter_list_fragment(fragment_text),
        ReparseTarget::TypeArguments => parse_type_arguments_fragment(fragment_text),
        ReparseTarget::TypeParameters => parse_type_parameters_fragment(fragment_text),
    };

    // Fragment parsing must be lossless. Some fragment parsers may stop early for recovery (e.g.
    // mis-indented braces), leaving trailing tokens unconsumed. Splicing a truncated fragment into
    // the previous tree would silently drop source text. Detect this by comparing the fragment
    // syntax tree length to the fragment slice length and fall back to a full parse if they
    // differ.
    let fragment_syntax = fragment.syntax();
    if u32::from(fragment_syntax.text_range().end()) as usize != fragment_text.len() {
        return parse_java(new_text);
    }

    // `parse_node_fragment`-based fragment parsers ensure losslessness by wrapping any trailing,
    // unconsumed tokens in a top-level `Error` node without producing diagnostics for them.
    //
    // Those tokens may instead be interpreted by the surrounding grammar in a full parse,
    // producing different syntax and/or diagnostics (e.g. `>>` being parsed as a shift operator
    // after an early-exited type-arguments parse).
    //
    // If we detect such trailing tokens, fall back to a full parse to keep the incremental result
    // consistent.
    if fragment_has_trailing_unparsed_tokens(plan.target, &fragment) {
        return parse_java(new_text);
    }

    // If the fragment ends in an unterminated string/comment/text block, the lexer would normally
    // continue tokenizing into the following text. Splicing the fragment into the previous tree
    // would leave the preserved portion tokenized under the old lexer state, producing an
    // inconsistent syntax tree. Fall back to a full reparse in that case.
    let fragment_reaches_eof = plan.new_range.end as usize == new_text.len();
    if !fragment_reaches_eof && fragment_has_unterminated_lex_error(&fragment) {
        return parse_java(new_text);
    }
    if !fragment_reaches_eof
        && fragment_ends_in_line_comment(fragment_text)
        && !matches!(new_text.as_bytes()[fragment_end], b'\n' | b'\r')
    {
        return parse_java(new_text);
    }
    // Fragment parsers treat the end of the fragment slice as EOF. If they report an error "found
    // end of file" but the reparsed region does not actually reach the end of the document, error
    // recovery can differ from a full parse (which would see the following tokens). Fall back to a
    // full parse in that case to keep diagnostics consistent.
    if !fragment_reaches_eof && fragment_has_eof_parse_error(&fragment) {
        return parse_java(new_text);
    }

    if fragment_syntax.kind() != plan.target_node.kind() {
        return parse_java(new_text);
    }

    let new_green = plan.target_node.replace_with(fragment.green);

    let preserved_errors = shift_preserved_errors(
        old,
        &old.errors,
        plan.old_range,
        plan.target_node.kind(),
        edit.delta(),
    );
    let fragment_errors = offset_errors(fragment.errors, plan.new_range.start);
    let mut errors = Vec::new();
    // Preserve the parser's natural error ordering at identical offsets: errors emitted while
    // parsing the fragment (typically inner constructs) should precede errors from preserved outer
    // contexts.
    errors.extend(fragment_errors);
    errors.extend(preserved_errors);
    crate::util::sort_parse_errors(&mut errors);

    let result = JavaParseResult {
        green: new_green,
        errors,
    };

    #[cfg(debug_assertions)]
    {
        // Cheap-ish sanity check: incremental reparsing must remain lossless.
        debug_assert_eq!(result.syntax().text().to_string(), new_text);
    }

    result
}

fn fragment_has_unterminated_lex_error(fragment: &JavaParseResult) -> bool {
    fragment
        .errors
        .iter()
        .any(|e| e.message.starts_with("unterminated "))
}

fn fragment_has_eof_parse_error(fragment: &JavaParseResult) -> bool {
    fragment
        .errors
        .iter()
        .any(|e| e.message.contains("found end of file"))
}

fn fragment_ends_in_line_comment(fragment_text: &str) -> bool {
    let tokens = lex(fragment_text);
    if tokens.len() < 2 {
        return false;
    }

    let last = &tokens[tokens.len().saturating_sub(2)];
    last.kind == SyntaxKind::LineComment
}

fn fragment_has_trailing_unparsed_tokens(
    target: ReparseTarget,
    fragment: &JavaParseResult,
) -> bool {
    // This only applies to fragment parsers built on `parse_node_fragment` (lists and type
    // parameter/argument nodes). Block/class-body fragment parsers either parse to a delimiter or
    // rely on a separate losslessness check.
    match target {
        ReparseTarget::ArgumentList
        | ReparseTarget::AnnotationElementValuePairList
        | ReparseTarget::ParameterList
        | ReparseTarget::TypeArguments
        | ReparseTarget::TypeParameters => {}
        _ => return false,
    }

    let root = fragment.syntax();
    let mut last_child = None;
    for child in root.children() {
        last_child = Some(child);
    }
    let Some(last_child) = last_child else {
        return false;
    };
    if last_child.kind() != SyntaxKind::Error {
        return false;
    }
    if last_child.text_range().end() != root.text_range().end() {
        return false;
    }
    // Ignore empty error nodes (shouldn't happen, but be defensive).
    if last_child.text_range().is_empty() {
        return false;
    }

    // The wrapper `Error` node always contains at least one non-trivia token.
    last_child
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .any(|t| !t.kind().is_trivia())
}

/// Convenience wrapper used by query integrations: reparse when an edit + old parse is available,
/// otherwise parse from scratch.
pub fn parse_java_incremental(
    old: Option<(&JavaParseResult, &str)>,
    edit: Option<TextEdit>,
    new_text: &str,
) -> JavaParseResult {
    match (old, edit) {
        (Some((old_parse, old_text)), Some(edit)) => {
            reparse_java(old_parse, old_text, edit, new_text)
        }
        _ => parse_java(new_text),
    }
}

fn build_reparse_plan(
    old: &JavaParseResult,
    old_text: &str,
    edit: &TextEdit,
    new_text: &str,
) -> Option<ReparsePlan> {
    if edit_intersects_ambiguous_token(old, edit) {
        return None;
    }

    let anchor = anchor_node(old, edit);

    let (target_node, target_kind) = select_reparse_node(anchor, edit)?;

    let old_range = syntax_text_range(&target_node);
    debug_assert!(old_range.start <= edit.range.start && old_range.end >= edit.range.end);

    let delta = edit.delta();
    let new_end = (old_range.end as isize + delta) as i64;
    if new_end < old_range.start as i64 {
        return None;
    }
    let new_end = new_end as u32;

    // Validate that the caller-provided new_text matches the edit length delta.
    if (old_text.len() as isize + delta) != new_text.len() as isize {
        return None;
    }

    let new_range = TextRange {
        start: old_range.start,
        end: new_end,
    };

    Some(ReparsePlan {
        target: target_kind,
        target_node,
        old_range,
        new_range,
    })
}

fn syntax_text_range(node: &crate::SyntaxNode) -> TextRange {
    let range = node.text_range();
    TextRange {
        start: u32::from(range.start()),
        end: u32::from(range.end()),
    }
}

fn ranges_intersect(a: TextRange, b: TextRange) -> bool {
    a.start < b.end && b.start < a.end
}

fn edit_intersects_ambiguous_token(old: &JavaParseResult, edit: &TextEdit) -> bool {
    if edit.range.is_empty() {
        let tokens = old.token_at_offset(edit.range.start);
        return match tokens {
            TokenAtOffset::None => false,
            TokenAtOffset::Single(tok) => is_ambiguous_token(&tok),
            TokenAtOffset::Between(left, right) => {
                is_ambiguous_token(&left) || is_ambiguous_token(&right)
            }
        };
    }

    let start_offset = edit.range.start;
    let end_offset = edit.range.end;

    let mut token = match old.token_at_offset(start_offset) {
        TokenAtOffset::None => return false,
        TokenAtOffset::Single(tok) => Some(tok),
        TokenAtOffset::Between(_, right) => Some(right),
    };

    while let Some(tok) = token {
        let range = tok.text_range();
        let tok_range = TextRange {
            start: u32::from(range.start()),
            end: u32::from(range.end()),
        };

        if tok_range.start >= end_offset {
            break;
        }

        if ranges_intersect(edit.range, tok_range) && is_ambiguous_token(&tok) {
            return true;
        }

        token = tok.next_token();
    }

    false
}

fn is_ambiguous_token(tok: &crate::SyntaxToken) -> bool {
    if is_ambiguous_token_kind(tok.kind()) {
        return true;
    }

    // String templates are lexed using a mode stack; edits inside interpolations can shift where
    // the lexer returns to template mode (and vice versa) without intersecting a dedicated template
    // token kind. Conservatively force a full reparse for any edit within a string template node.
    let mut cur = tok.parent();
    while let Some(node) = cur {
        if node.kind() == SyntaxKind::StringTemplate {
            return true;
        }
        cur = node.parent();
    }

    false
}

fn is_ambiguous_token_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::StringLiteral
            | SyntaxKind::TextBlock
            | SyntaxKind::StringTemplateStart
            | SyntaxKind::StringTemplateText
            | SyntaxKind::StringTemplateExprStart
            | SyntaxKind::StringTemplateExprEnd
            | SyntaxKind::StringTemplateEnd
            | SyntaxKind::CharLiteral
            | SyntaxKind::LineComment
            | SyntaxKind::BlockComment
            | SyntaxKind::DocComment
    )
}

fn anchor_node(old: &JavaParseResult, edit: &TextEdit) -> crate::SyntaxNode {
    if !edit.range.is_empty() {
        let element = old.covering_element(edit.range);
        return match element {
            NodeOrToken::Node(node) => node,
            NodeOrToken::Token(tok) => tok.parent().unwrap_or_else(|| old.syntax()),
        };
    }

    // Insertion: pick a nearby token and use its parent as the anchor.
    let offset = edit.range.start;
    match old.token_at_offset(offset) {
        TokenAtOffset::None => old.syntax(),
        TokenAtOffset::Single(tok) => tok.parent().unwrap_or_else(|| old.syntax()),
        TokenAtOffset::Between(left, right) => {
            // Prefer the token to the *right* of the insertion point. Insertions occur before
            // `right`, and the smallest syntactic context that contains `right` is more likely to
            // also contain the inserted text. This avoids selecting a node that ends exactly at the
            // insertion offset (which would cause us to slice extra trailing text into the
            // reparsed fragment and potentially drop it during parsing).
            let chosen = if !right.kind().is_trivia() {
                right
            } else if !left.kind().is_trivia() {
                left
            } else {
                right
            };
            chosen.parent().unwrap_or_else(|| old.syntax())
        }
    }
}

fn select_reparse_node(
    anchor: crate::SyntaxNode,
    edit: &TextEdit,
) -> Option<(crate::SyntaxNode, ReparseTarget)> {
    let insertion_offset = if edit.range.is_empty() {
        Some(edit.range.start)
    } else {
        None
    };

    let mut node = Some(anchor);
    while let Some(cur) = node {
        let kind = cur.kind();

        if let Some(offset) = insertion_offset {
            // For insertions we intentionally avoid reparsing a node when the insertion point is
            // at the node boundary. Many parse functions stop once the node's closing delimiter is
            // reached; if we were to include inserted text after the delimiter in the fragment
            // slice, it would be silently dropped from the rebuilt subtree.
            //
            // By requiring the insertion offset to be *strictly* inside the node, we ensure the
            // inserted text is part of the reparsed region.
            let range = cur.text_range();
            let start = u32::from(range.start());
            let end = u32::from(range.end());
            if offset <= start || offset >= end {
                if kind == SyntaxKind::CompilationUnit {
                    break;
                }
                node = cur.parent();
                continue;
            }
        }

        if let Some(target) = classify_list_or_block(&cur) {
            if !edit_overlaps_list_delimiters(&cur, edit) {
                return Some((cur, target));
            }
        }

        if is_class_member_kind(kind) && !edit_overlaps_node_end(&cur, edit) {
            return Some((cur, ReparseTarget::ClassMember));
        }

        if kind == SyntaxKind::CompilationUnit {
            break;
        }
        node = cur.parent();
    }
    None
}

fn classify_list_or_block(node: &crate::SyntaxNode) -> Option<ReparseTarget> {
    let kind = node.kind();
    Some(match kind {
        SyntaxKind::Block => ReparseTarget::Block(statement_context_for_block(node)),
        SyntaxKind::SwitchBlock => {
            let (stmt_ctx, switch_ctx) = switch_context_for_block(node);
            ReparseTarget::SwitchBlock {
                stmt_ctx,
                switch_ctx,
            }
        }
        SyntaxKind::ClassBody
        | SyntaxKind::InterfaceBody
        | SyntaxKind::EnumBody
        | SyntaxKind::RecordBody
        | SyntaxKind::AnnotationBody => ReparseTarget::ClassBody(kind),
        SyntaxKind::ArgumentList => ReparseTarget::ArgumentList,
        SyntaxKind::AnnotationElementValuePairList => ReparseTarget::AnnotationElementValuePairList,
        SyntaxKind::ParameterList => ReparseTarget::ParameterList,
        SyntaxKind::TypeArguments => ReparseTarget::TypeArguments,
        SyntaxKind::TypeParameters => ReparseTarget::TypeParameters,
        _ => return None,
    })
}

fn statement_context_for_block(node: &crate::SyntaxNode) -> StatementContext {
    // `yield` is only a statement inside switch *expressions* (JEP 361). When reparsing a block
    // fragment, derive the same statement-context the full parser would have used based on the
    // surrounding syntax.
    //
    // The parser explicitly resets to `Normal` when parsing lambda bodies and class bodies, so we
    // treat those nodes as context boundaries: even if they appear inside a switch expression, a
    // `yield` inside the nested construct should be parsed as an identifier-like expression.
    let mut cur = node.parent();
    while let Some(parent) = cur {
        match parent.kind() {
            // Context boundaries: these constructs parse their nested blocks using the normal
            // statement grammar even if they appear within a switch expression.
            SyntaxKind::SwitchStatement
            | SyntaxKind::LambdaExpression
            | SyntaxKind::ClassBody
            | SyntaxKind::InterfaceBody
            | SyntaxKind::EnumBody
            | SyntaxKind::RecordBody
            | SyntaxKind::AnnotationBody => return StatementContext::Normal,

            SyntaxKind::SwitchExpression => return StatementContext::SwitchExpression,
            _ => {}
        }
        cur = parent.parent();
    }
    StatementContext::Normal
}

fn switch_context_for_block(node: &crate::SyntaxNode) -> (StatementContext, SwitchContext) {
    let is_expression = node
        .parent()
        .is_some_and(|parent| parent.kind() == SyntaxKind::SwitchExpression);
    if is_expression {
        (
            StatementContext::SwitchExpression,
            SwitchContext::Expression,
        )
    } else {
        (StatementContext::Normal, SwitchContext::Statement)
    }
}

fn is_class_member_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::FieldDeclaration
            | SyntaxKind::MethodDeclaration
            | SyntaxKind::ConstructorDeclaration
            | SyntaxKind::CompactConstructorDeclaration
            | SyntaxKind::InitializerBlock
            | SyntaxKind::EmptyDeclaration
            | SyntaxKind::ClassDeclaration
            | SyntaxKind::InterfaceDeclaration
            | SyntaxKind::EnumDeclaration
            | SyntaxKind::RecordDeclaration
            | SyntaxKind::AnnotationTypeDeclaration
    )
}

fn edit_overlaps_list_delimiters(node: &crate::SyntaxNode, edit: &TextEdit) -> bool {
    let Some(open) = first_non_trivia_token(node) else {
        return false;
    };
    let Some(close) = last_non_trivia_token(node) else {
        return false;
    };
    edit_overlaps_token(edit, &open) || edit_overlaps_token(edit, &close)
}

fn edit_overlaps_node_end(node: &crate::SyntaxNode, edit: &TextEdit) -> bool {
    let Some(end) = last_non_trivia_token(node) else {
        return false;
    };
    edit_overlaps_token(edit, &end)
}

fn edit_overlaps_token(edit: &TextEdit, token: &crate::SyntaxToken) -> bool {
    let range = token.text_range();
    ranges_intersect(
        edit.range,
        TextRange {
            start: u32::from(range.start()),
            end: u32::from(range.end()),
        },
    )
}

fn first_non_trivia_token(node: &crate::SyntaxNode) -> Option<crate::SyntaxToken> {
    let node_range = node.text_range();
    let mut tok = node.first_token()?;
    while tok.kind().is_trivia() {
        let next = tok.next_token()?;
        if next.text_range().start() >= node_range.end() {
            return None;
        }
        tok = next;
    }
    Some(tok)
}

fn last_non_trivia_token(node: &crate::SyntaxNode) -> Option<crate::SyntaxToken> {
    let node_range = node.text_range();
    let mut tok = node.last_token()?;
    while tok.kind().is_trivia() {
        let prev = tok.prev_token()?;
        if prev.text_range().end() <= node_range.start() {
            return None;
        }
        tok = prev;
    }
    Some(tok)
}

fn offset_errors(mut errors: Vec<ParseError>, offset: u32) -> Vec<ParseError> {
    if offset == 0 {
        return errors;
    }
    for err in &mut errors {
        err.range.start = err.range.start.saturating_add(offset);
        err.range.end = err.range.end.saturating_add(offset);
    }
    errors
}

fn shift_preserved_errors(
    old: &JavaParseResult,
    errors: &[ParseError],
    reparsed_old_range: TextRange,
    reparsed_kind: SyntaxKind,
    delta: isize,
) -> Vec<ParseError> {
    #[derive(Debug, Clone, Copy)]
    enum BoundaryOwner {
        Block,
        ClassBody,
        EnumBody,
        ModuleBody,
        AnnotationArrayInitializer,
        ArrayInitializer,
        TemplateExpression,
    }

    fn boundary_owner_from_message(msg: &str) -> Option<BoundaryOwner> {
        if msg.starts_with("expected `}` to close block") {
            Some(BoundaryOwner::Block)
        } else if msg.starts_with("expected `}` to close class body") {
            Some(BoundaryOwner::ClassBody)
        } else if msg.starts_with("expected `}` to close enum body") {
            Some(BoundaryOwner::EnumBody)
        } else if msg.starts_with("expected `}` to close module body") {
            Some(BoundaryOwner::ModuleBody)
        } else if msg.starts_with("expected `}` to close annotation array initializer") {
            Some(BoundaryOwner::AnnotationArrayInitializer)
        } else if msg.starts_with("expected `}` to close array initializer") {
            Some(BoundaryOwner::ArrayInitializer)
        } else if msg.starts_with("expected `}` to close template expression") {
            Some(BoundaryOwner::TemplateExpression)
        } else {
            None
        }
    }

    fn boundary_owner_matches(owner: BoundaryOwner, kind: SyntaxKind) -> bool {
        match owner {
            BoundaryOwner::Block => kind == SyntaxKind::Block,
            BoundaryOwner::ClassBody => matches!(
                kind,
                SyntaxKind::ClassBody
                    | SyntaxKind::InterfaceBody
                    | SyntaxKind::EnumBody
                    | SyntaxKind::RecordBody
                    | SyntaxKind::AnnotationBody
            ),
            BoundaryOwner::EnumBody => kind == SyntaxKind::EnumBody,
            BoundaryOwner::ModuleBody => kind == SyntaxKind::ModuleBody,
            BoundaryOwner::AnnotationArrayInitializer => {
                kind == SyntaxKind::AnnotationElementValueArrayInitializer
            }
            BoundaryOwner::ArrayInitializer => kind == SyntaxKind::ArrayInitializer,
            BoundaryOwner::TemplateExpression => kind == SyntaxKind::StringTemplateExpression,
        }
    }

    fn count_nodes_starting_before(
        old: &JavaParseResult,
        offset: u32,
        owner: BoundaryOwner,
        before: u32,
    ) -> u32 {
        // Prefer the token to the *left* of the offset. Boundary errors at the end of a node often
        // occur at the same byte offset as the file's `Eof` token; using the right token can land
        // us outside the construct that actually triggered the error.
        let token = match old.token_at_offset(offset) {
            TokenAtOffset::None => None,
            TokenAtOffset::Single(tok) => {
                if tok.kind() == SyntaxKind::Eof {
                    tok.prev_token()
                } else {
                    Some(tok)
                }
            }
            TokenAtOffset::Between(left, _) => Some(left),
        };

        let mut node = token
            .and_then(|tok| tok.parent())
            .unwrap_or_else(|| old.syntax());
        let mut count = 0u32;
        loop {
            if boundary_owner_matches(owner, node.kind()) {
                let start = u32::from(node.text_range().start());
                if start < before {
                    count = count.saturating_add(1);
                }
            }
            node = match node.parent() {
                Some(parent) => parent,
                None => break,
            };
        }
        count
    }

    // Preserve errors that are *entirely* outside the reparsed region. Empty ranges anchored at the
    // region boundary are tricky: many "missing token" diagnostics use a zero-length range at the
    // expected token position (often exactly `reparsed_old_range.end`).
    //
    // Some of these boundary errors belong to the reparsed fragment and should be dropped and
    // recomputed (to avoid stale/duplicated diagnostics), while others correspond to *outer*
    // constructs that started before the reparsed region and must be preserved.
    //
    // For a handful of common "expected `}` to close ..." diagnostics we approximate ownership by:
    // - finding the innermost matching node at the boundary offset, and
    // - preserving the error only when that node starts before `reparsed_old_range.start`.
    //
    // If we cannot attribute the error to a known construct, we fall back to preserving
    // class-body-level boundary diagnostics when we are not reparsing the class body itself.
    let reparsing_class_body = matches!(
        reparsed_kind,
        SyntaxKind::ClassBody
            | SyntaxKind::InterfaceBody
            | SyntaxKind::EnumBody
            | SyntaxKind::RecordBody
            | SyntaxKind::AnnotationBody
            // Type declarations *contain* a body; when reparsing the whole declaration we must not
            // preserve class-body-level EOF diagnostics from the previous parse.
            | SyntaxKind::ClassDeclaration
            | SyntaxKind::InterfaceDeclaration
            | SyntaxKind::EnumDeclaration
            | SyntaxKind::RecordDeclaration
            | SyntaxKind::AnnotationTypeDeclaration
    );

    let is_class_body_error = |e: &ParseError| {
        e.message.contains("class body")
            || e.message.contains("interface body")
            || e.message.contains("enum body")
            || e.message.contains("record body")
            || e.message.contains("annotation body")
            || e.message.contains("member name")
    };

    let delta_i64 = delta as i64;
    let mut preserved = Vec::new();

    // Track how many boundary errors we must skip for each `(range, message)` key so we preserve
    // the *outer* diagnostics.
    //
    // The Java parser emits EOF boundary errors from inner to outer as it unwinds the parse stack.
    // When reparsing an inner region, we want to drop stale diagnostics for constructs fully
    // contained in the reparsed fragment, while preserving diagnostics for outer constructs that
    // started before the reparsed range.
    //
    // Because those outer diagnostics appear *later* in the original error list, we preserve the
    // last `limit` occurrences for each key (equivalently: skip the first `total - limit`).
    let mut boundary_skip: HashMap<(TextRange, &str), u32> = {
        let mut totals: HashMap<(TextRange, &str), (u32, u32)> = HashMap::new(); // (limit, total)
        for e in errors {
            if !(e.range.is_empty() && e.range.start == reparsed_old_range.end) {
                continue;
            }

            let key = (e.range, e.message.as_str());
            let entry = totals.entry(key).or_insert_with(|| {
                let limit = if let Some(owner) = boundary_owner_from_message(&e.message) {
                    count_nodes_starting_before(old, e.range.start, owner, reparsed_old_range.start)
                } else if !reparsing_class_body && is_class_body_error(e) {
                    u32::MAX
                } else {
                    0
                };
                (limit, 0)
            });
            entry.1 = entry.1.saturating_add(1);
        }

        totals
            .into_iter()
            .map(|(key, (limit, total))| (key, total.saturating_sub(limit)))
            .collect()
    };

    for e in errors {
        let is_before = e.range.end < reparsed_old_range.start
            || (e.range.end == reparsed_old_range.start
                && e.range.start < reparsed_old_range.start);
        if is_before {
            preserved.push(e.clone());
            continue;
        }

        let is_after = e.range.start > reparsed_old_range.end
            || (e.range.start == reparsed_old_range.end && e.range.end > reparsed_old_range.end);
        if is_after {
            if delta == 0 {
                preserved.push(e.clone());
            } else {
                let start = (e.range.start as i64 + delta_i64) as u32;
                let end = (e.range.end as i64 + delta_i64) as u32;
                preserved.push(ParseError {
                    message: e.message.clone(),
                    range: TextRange { start, end },
                });
            }
            continue;
        }

        // Empty range exactly at the end boundary: preserve only when it is attributable to an
        // outer construct that started before the reparsed region.
        if e.range.is_empty() && e.range.start == reparsed_old_range.end {
            let key = (e.range, e.message.as_str());
            let skip = boundary_skip.get_mut(&key);
            if let Some(skip) = skip {
                if *skip > 0 {
                    *skip = skip.saturating_sub(1);
                    continue;
                }
            } else {
                continue;
            }

            if delta == 0 {
                preserved.push(e.clone());
            } else {
                let start = (e.range.start as i64 + delta_i64) as u32;
                let end = (e.range.end as i64 + delta_i64) as u32;
                preserved.push(ParseError {
                    message: e.message.clone(),
                    range: TextRange { start, end },
                });
            }
        }
    }

    preserved
}

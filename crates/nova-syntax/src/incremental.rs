use rowan::{NodeOrToken, TokenAtOffset};

use crate::parser::{
    parse_block_fragment, parse_class_body_fragment, parse_class_member_fragment,
    parse_switch_block_fragment,
};
use crate::{lex, parse_java, JavaParseResult, ParseError, SyntaxKind, TextEdit, TextRange};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReparseTarget {
    Block,
    SwitchBlock,
    ClassBody(SyntaxKind),
    ClassMember,
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
        ReparseTarget::Block => parse_block_fragment(fragment_text),
        ReparseTarget::SwitchBlock => parse_switch_block_fragment(fragment_text),
        ReparseTarget::ClassBody(kind) => parse_class_body_fragment(fragment_text, kind),
        ReparseTarget::ClassMember => parse_class_member_fragment(fragment_text),
    };

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

    if fragment.syntax().kind() != plan.target_node.kind() {
        return parse_java(new_text);
    }

    let new_green = plan.target_node.replace_with(fragment.green);

    let mut errors = Vec::new();
    errors.extend(shift_preserved_errors(
        &old.errors,
        plan.old_range,
        edit.delta(),
    ));
    errors.extend(offset_errors(fragment.errors, plan.new_range.start));
    errors.sort_by_key(|e| (e.range.start, e.range.end));

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

fn fragment_ends_in_line_comment(fragment_text: &str) -> bool {
    let tokens = lex(fragment_text);
    if tokens.len() < 2 {
        return false;
    }

    let last = &tokens[tokens.len().saturating_sub(2)];
    last.kind == SyntaxKind::LineComment
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
    if edit.range.len() == 0 {
        let tokens = old.token_at_offset(edit.range.start);
        return match tokens {
            TokenAtOffset::None => false,
            TokenAtOffset::Single(tok) => is_ambiguous_token_kind(tok.kind()),
            TokenAtOffset::Between(left, right) => {
                is_ambiguous_token_kind(left.kind()) || is_ambiguous_token_kind(right.kind())
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

        if ranges_intersect(edit.range, tok_range) && is_ambiguous_token_kind(tok.kind()) {
            return true;
        }

        token = tok.next_token();
    }

    false
}

fn is_ambiguous_token_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::StringLiteral
            | SyntaxKind::TextBlock
            | SyntaxKind::CharLiteral
            | SyntaxKind::LineComment
            | SyntaxKind::BlockComment
            | SyntaxKind::DocComment
    )
}

fn anchor_node(old: &JavaParseResult, edit: &TextEdit) -> crate::SyntaxNode {
    if edit.range.len() > 0 {
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
    let insertion_offset = if edit.range.len() == 0 {
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

        if let Some(target) = classify_list_or_block(kind) {
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

fn classify_list_or_block(kind: SyntaxKind) -> Option<ReparseTarget> {
    Some(match kind {
        SyntaxKind::Block => ReparseTarget::Block,
        SyntaxKind::SwitchBlock => ReparseTarget::SwitchBlock,
        SyntaxKind::ClassBody
        | SyntaxKind::InterfaceBody
        | SyntaxKind::EnumBody
        | SyntaxKind::RecordBody
        | SyntaxKind::AnnotationBody => ReparseTarget::ClassBody(kind),
        _ => return None,
    })
}

fn is_class_member_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::FieldDeclaration
            | SyntaxKind::MethodDeclaration
            | SyntaxKind::ConstructorDeclaration
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
    errors: &[ParseError],
    reparsed_old_range: TextRange,
    delta: isize,
) -> Vec<ParseError> {
    if delta == 0 {
        return errors
            .iter()
            .filter(|e| {
                e.range.end <= reparsed_old_range.start || e.range.start >= reparsed_old_range.end
            })
            .cloned()
            .collect();
    }

    let delta_i64 = delta as i64;
    errors
        .iter()
        .filter_map(|e| {
            if e.range.end <= reparsed_old_range.start {
                return Some(e.clone());
            }
            if e.range.start >= reparsed_old_range.end {
                let start = (e.range.start as i64 + delta_i64) as u32;
                let end = (e.range.end as i64 + delta_i64) as u32;
                return Some(ParseError {
                    message: e.message.clone(),
                    range: TextRange { start, end },
                });
            }
            None
        })
        .collect()
}

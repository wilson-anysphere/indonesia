use nova_types::{Diagnostic, Span};

use crate::language_level::{FeatureAvailability, JavaFeature, JavaLanguageLevel};
use crate::{SyntaxKind, SyntaxNode, SyntaxToken};

pub(crate) fn feature_gate_diagnostics(root: &SyntaxNode, level: JavaLanguageLevel) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    gate_records(root, level, &mut diagnostics);
    gate_sealed_classes(root, level, &mut diagnostics);
    gate_text_blocks(root, level, &mut diagnostics);
    gate_switch_expressions(root, level, &mut diagnostics);
    gate_pattern_matching_switch(root, level, &mut diagnostics);
    gate_record_patterns(root, level, &mut diagnostics);
    gate_pattern_matching_instanceof(root, level, &mut diagnostics);
    gate_var_local_inference(root, level, &mut diagnostics);

    diagnostics
}

fn gate_records(root: &SyntaxNode, level: JavaLanguageLevel, out: &mut Vec<Diagnostic>) {
    if level.is_enabled(JavaFeature::Records) {
        return;
    }

    for node in root.descendants().filter(|n| n.kind() == SyntaxKind::RecordDeclaration) {
        let Some(record_kw) = node
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == SyntaxKind::RecordKw)
        else {
            continue;
        };
        out.push(feature_error(level, JavaFeature::Records, &record_kw));
    }
}

fn gate_sealed_classes(root: &SyntaxNode, level: JavaLanguageLevel, out: &mut Vec<Diagnostic>) {
    if level.is_enabled(JavaFeature::SealedClasses) {
        return;
    }

    for decl in root.descendants().filter(|n| {
        matches!(
            n.kind(),
            SyntaxKind::ClassDeclaration | SyntaxKind::InterfaceDeclaration
        )
    }) {
        let sealed_kw = decl
            .children()
            .find(|n| n.kind() == SyntaxKind::Modifiers)
            .and_then(|mods| {
                mods.children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .find(|t| matches!(t.kind(), SyntaxKind::SealedKw | SyntaxKind::NonSealedKw))
            });
        if let Some(tok) = sealed_kw {
            out.push(feature_error(level, JavaFeature::SealedClasses, &tok));
            continue;
        }

        let permits_kw = decl
            .children()
            .find(|n| n.kind() == SyntaxKind::PermitsClause)
            .and_then(|permits| {
                permits
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .find(|t| t.kind() == SyntaxKind::PermitsKw)
            });
        if let Some(tok) = permits_kw {
            out.push(feature_error(level, JavaFeature::SealedClasses, &tok));
        }
    }
}

fn gate_text_blocks(root: &SyntaxNode, level: JavaLanguageLevel, out: &mut Vec<Diagnostic>) {
    if level.is_enabled(JavaFeature::TextBlocks) {
        return;
    }

    for tok in root
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == SyntaxKind::TextBlock)
    {
        out.push(feature_error(level, JavaFeature::TextBlocks, &tok));
    }
}

fn gate_switch_expressions(root: &SyntaxNode, level: JavaLanguageLevel, out: &mut Vec<Diagnostic>) {
    if level.is_enabled(JavaFeature::SwitchExpressions) {
        return;
    }

    // We only gate `->` *in a switch label*. (Plain lambdas are Java 8.)
    for tok in root
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == SyntaxKind::Arrow)
    {
        if tok.parent().map_or(false, |p| p.kind() == SyntaxKind::SwitchLabel) {
            out.push(feature_error(level, JavaFeature::SwitchExpressions, &tok));
        }
    }
}

fn gate_pattern_matching_switch(root: &SyntaxNode, level: JavaLanguageLevel, out: &mut Vec<Diagnostic>) {
    if level.is_enabled(JavaFeature::PatternMatchingSwitch) {
        return;
    }

    // Gate switch labels that use pattern-matching-only constructs:
    // - patterns (`case String s ->`)
    // - null labels (`case null ->`) / `case null, default ->`
    // - `default` as a case label element (distinct from a `default:` label)
    // - guards (`when <expr>`)
    for label in root.descendants().filter(|n| n.kind() == SyntaxKind::SwitchLabel) {
        let Some(first_tok) = label
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| !t.kind().is_trivia())
        else {
            continue;
        };

        // Only `case ...` labels participate; plain `default:` is always allowed.
        if first_tok.kind() != SyntaxKind::CaseKw {
            continue;
        }

        let pattern_tok = label
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::Pattern)
            .find_map(|n| first_token(&n));
        if let Some(tok) = pattern_tok {
            out.push(feature_error(level, JavaFeature::PatternMatchingSwitch, &tok));
            continue;
        }

        let guard_tok = label
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::Guard)
            .find_map(|n| first_token(&n));
        if let Some(tok) = guard_tok {
            out.push(feature_error(level, JavaFeature::PatternMatchingSwitch, &tok));
            continue;
        }

        // `case null` and `case null, default`.
        // Only consider `null`/`default` as *case label elements* (not occurrences inside guards).
        for element in label
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::CaseLabelElement)
        {
            // If this element includes a Pattern, it would have been caught above; ignore `null`
            // literals inside e.g. guard expressions.
            if element.descendants().any(|n| n.kind() == SyntaxKind::Pattern) {
                continue;
            }

            let tok = element
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .find(|t| matches!(t.kind(), SyntaxKind::NullKw | SyntaxKind::DefaultKw));
            if let Some(tok) = tok {
                out.push(feature_error(level, JavaFeature::PatternMatchingSwitch, &tok));
                break;
            }
        }
    }
}

fn gate_record_patterns(root: &SyntaxNode, level: JavaLanguageLevel, out: &mut Vec<Diagnostic>) {
    if level.is_enabled(JavaFeature::RecordPatterns) {
        return;
    }

    for node in root.descendants().filter(|n| n.kind() == SyntaxKind::RecordPattern) {
        let Some(tok) = first_token(&node) else {
            continue;
        };
        out.push(feature_error(level, JavaFeature::RecordPatterns, &tok));
    }
}

fn gate_pattern_matching_instanceof(
    root: &SyntaxNode,
    level: JavaLanguageLevel,
    out: &mut Vec<Diagnostic>,
) {
    if level.is_enabled(JavaFeature::PatternMatchingInstanceof) {
        return;
    }

    // `x instanceof Type binding`
    for pattern in root.descendants().filter(|n| n.kind() == SyntaxKind::Pattern) {
        let Some(parent) = pattern.parent() else {
            continue;
        };
        if parent.kind() != SyntaxKind::BinaryExpression {
            continue;
        }

        let is_instanceof = parent
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == SyntaxKind::InstanceofKw);
        if !is_instanceof {
            continue;
        }

        let Some(tok) = first_token(&pattern) else {
            continue;
        };
        out.push(feature_error(level, JavaFeature::PatternMatchingInstanceof, &tok));
    }
}

fn gate_var_local_inference(root: &SyntaxNode, level: JavaLanguageLevel, out: &mut Vec<Diagnostic>) {
    if level.is_enabled(JavaFeature::VarLocalInference) {
        return;
    }

    // LocalVariableDeclarationStatement: `var x = ...;`
    for node in root
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::LocalVariableDeclarationStatement)
    {
        let Some(var_kw) = var_type_keyword(&node) else {
            continue;
        };
        out.push(feature_error(level, JavaFeature::VarLocalInference, &var_kw));
    }

    // try-with-resources: `try (var x = ...) { ... }`
    for node in root.descendants().filter(|n| n.kind() == SyntaxKind::Resource) {
        let Some(var_kw) = var_type_keyword(&node) else {
            continue;
        };
        out.push(feature_error(level, JavaFeature::VarLocalInference, &var_kw));
    }

    // for headers: `for (var i = 0; ...; ...)` / `for (var x : xs)`
    for node in root.descendants().filter(|n| n.kind() == SyntaxKind::ForHeader) {
        let Some(var_kw) = var_type_keyword(&node) else {
            continue;
        };
        out.push(feature_error(level, JavaFeature::VarLocalInference, &var_kw));
    }
}

fn var_type_keyword(container: &SyntaxNode) -> Option<SyntaxToken> {
    // We only want the *declaration type* (`Type` node that is a direct child of
    // the container), not nested `Type` nodes within e.g. cast expressions.
    let ty = container.children().find(|n| n.kind() == SyntaxKind::Type)?;

    let first = ty
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| !t.kind().is_trivia())?;

    if first.kind() != SyntaxKind::VarKw {
        return None;
    }
    Some(first)
}

fn feature_error(level: JavaLanguageLevel, feature: JavaFeature, token: &SyntaxToken) -> Diagnostic {
    Diagnostic::error(feature.diagnostic_code(), feature_message(level, feature), Some(span(token)))
}

fn first_token(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| !t.kind().is_trivia())
}

fn span(token: &SyntaxToken) -> Span {
    let range = token.text_range();
    let start: usize = u32::from(range.start()) as usize;
    let end: usize = u32::from(range.end()) as usize;
    Span::new(start, end)
}

fn feature_message(level: JavaLanguageLevel, feature: JavaFeature) -> String {
    match level.availability(feature) {
        FeatureAvailability::Stable => {
            // Only called for disabled features.
            format!("{} is enabled in this language level", feature.display_name())
        }
        FeatureAvailability::Preview => format!(
            "{} is a preview feature in Java {} and requires --enable-preview",
            feature.display_name(),
            level.major
        ),
        FeatureAvailability::Unavailable => match feature.stable_since() {
            Some(min) => format!("{} requires Java {}+", feature.display_name(), min),
            None => format!(
                "{} is not available in Java {}",
                feature.display_name(),
                level.major
            ),
        },
    }
}

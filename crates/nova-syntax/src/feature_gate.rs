use nova_types::{Diagnostic, Span};

use crate::language_level::{FeatureAvailability, JavaFeature, JavaLanguageLevel};
use crate::{SyntaxKind, SyntaxNode, SyntaxToken};

pub(crate) fn feature_gate_diagnostics(root: &SyntaxNode, level: JavaLanguageLevel) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    gate_records(root, level, &mut diagnostics);
    gate_sealed_classes(root, level, &mut diagnostics);
    gate_text_blocks(root, level, &mut diagnostics);
    gate_switch_expressions(root, level, &mut diagnostics);
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

    for tok in root.descendants_with_tokens().filter_map(|e| e.into_token()) {
        match tok.kind() {
            SyntaxKind::SealedKw | SyntaxKind::NonSealedKw => {
                if tok.parent().map_or(false, |p| p.kind() == SyntaxKind::Modifiers) {
                    out.push(feature_error(level, JavaFeature::SealedClasses, &tok));
                }
            }
            // `permits` in a type header.
            SyntaxKind::PermitsKw => {
                if tok
                    .parent()
                    .map_or(false, |p| p.kind() == SyntaxKind::PermitsClause)
                {
                    out.push(feature_error(level, JavaFeature::SealedClasses, &tok));
                }
            }
            _ => {}
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

use std::collections::{HashMap, HashSet};

use nova_framework_parse::{
    clean_type, parse_annotation_text, parse_class_literal, ParsedAnnotation,
};
use nova_syntax::{parse_java, SyntaxKind, SyntaxNode, SyntaxToken};
use nova_types::{Diagnostic, Span};

pub const JPA_PARSE_ERROR: &str = "JPA_PARSE_ERROR";
pub const JPA_MISSING_ID: &str = "JPA_MISSING_ID";
pub const JPA_NO_NOARG_CTOR: &str = "JPA_NO_NOARG_CTOR";
pub const JPA_REL_INVALID_TARGET_TYPE: &str = "JPA_REL_INVALID_TARGET_TYPE";
pub const JPA_REL_TARGET_UNKNOWN: &str = "JPA_REL_TARGET_UNKNOWN";
pub const JPA_REL_TARGET_NOT_ENTITY: &str = "JPA_REL_TARGET_NOT_ENTITY";
pub const JPA_MAPPEDBY_MISSING: &str = "JPA_MAPPEDBY_MISSING";
pub const JPA_MAPPEDBY_NOT_RELATIONSHIP: &str = "JPA_MAPPEDBY_NOT_RELATIONSHIP";
pub const JPA_MAPPEDBY_WRONG_TARGET: &str = "JPA_MAPPEDBY_WRONG_TARGET";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceDiagnostic {
    pub source: usize,
    pub diagnostic: Diagnostic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub ty: String,
    pub span: Span,
    pub is_transient: bool,
    pub is_static: bool,
    pub is_id: bool,
    pub is_embedded_id: bool,
    pub relationship: Option<Relationship>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entity {
    pub name: String,
    /// JPQL entity name (defaults to the class name, but can be overridden via
    /// `@Entity(name = "...")`).
    pub jpql_name: String,
    pub table: String,
    pub span: Span,
    pub source: usize,
    pub fields: Vec<Field>,
    pub has_explicit_ctor: bool,
    pub has_no_arg_ctor: bool,
}

impl Entity {
    pub fn id_fields(&self) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(|f| f.is_id || f.is_embedded_id)
    }

    pub fn field_named(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityModel {
    pub entities: HashMap<String, Entity>,
}

impl EntityModel {
    pub fn entity(&self, name: &str) -> Option<&Entity> {
        self.entities.get(name)
    }

    pub fn entity_names(&self) -> impl Iterator<Item = &String> {
        self.entities.keys()
    }

    pub fn jpql_entity_names(&self) -> impl Iterator<Item = &String> + '_ {
        self.entities.values().map(|e| &e.jpql_name)
    }

    pub fn entity_by_jpql_name(&self, name: &str) -> Option<&Entity> {
        self.entities.values().find(|e| e.jpql_name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelationshipKind {
    OneToMany,
    ManyToOne,
    ManyToMany,
    OneToOne,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relationship {
    pub kind: RelationshipKind,
    pub field_name: String,
    pub target_entity: Option<String>,
    pub mapped_by: Option<String>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisResult {
    pub model: EntityModel,
    pub diagnostics: Vec<SourceDiagnostic>,
}

/// Parse and validate entities across multiple Java sources.
pub(crate) fn analyze_entities(sources: &[&str]) -> AnalysisResult {
    let mut entities: Vec<Entity> = Vec::new();
    let mut diagnostics: Vec<SourceDiagnostic> = Vec::new();

    for (source_idx, src) in sources.iter().enumerate() {
        match parse_entities(src, source_idx) {
            Ok(mut parsed) => entities.append(&mut parsed),
            Err(err) => diagnostics.push(SourceDiagnostic {
                source: source_idx,
                diagnostic: Diagnostic::error(
                    JPA_PARSE_ERROR,
                    format!("Failed to parse Java source: {err}"),
                    None,
                ),
            }),
        }
    }

    let mut model = EntityModel {
        entities: entities.into_iter().map(|e| (e.name.clone(), e)).collect(),
    };

    // Validate entities and relationships now that we have the full model.
    diagnostics.extend(validate_model(&model));

    // relationship validation can benefit from knowing derived targets, so we
    // update them based on the final model.
    hydrate_relationship_targets(&mut model);
    diagnostics.extend(validate_relationships(&model));

    AnalysisResult { model, diagnostics }
}

fn parse_entities(source: &str, source_idx: usize) -> Result<Vec<Entity>, String> {
    // Fast path: avoid running the full Java parser for files that clearly
    // cannot contain entities.
    if !(source.contains("@Entity")
        || source.contains("@jakarta.persistence.Entity")
        || source.contains("@javax.persistence.Entity"))
    {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();

    let parse = parse_java(source);
    let root = parse.syntax();
    for node in root
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::ClassDeclaration)
    {
        if let Some(entity) = parse_entity_class(&node, source, source_idx) {
            out.push(entity);
        }
    }

    Ok(out)
}

fn parse_entity_class(node: &SyntaxNode, source: &str, source_idx: usize) -> Option<Entity> {
    let modifiers = node.children().find(|n| n.kind() == SyntaxKind::Modifiers);
    let annotations = modifiers
        .as_ref()
        .map(|m| collect_annotations(m, source))
        .unwrap_or_else(Vec::new);

    let is_entity = annotations.iter().any(|ann| ann.simple_name == "Entity");
    if !is_entity {
        return None;
    }

    let name_tok = class_name_token(node)?;
    let name = name_tok.text().to_string();
    let jpql_name = annotations
        .iter()
        .find(|ann| ann.simple_name == "Entity")
        .and_then(|ann| ann.args.get("name").cloned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| name.clone());

    let table = annotations
        .iter()
        .find(|ann| ann.simple_name == "Table")
        .and_then(|ann| ann.args.get("name").cloned())
        .unwrap_or_else(|| name.clone());

    // Use the class name span for diagnostics/navigation rather than the full
    // class declaration range.
    let span = span_of_token(&name_tok);

    let body = node
        .children()
        .find(|n| n.kind() == SyntaxKind::ClassBody)?;
    let (fields, has_explicit_ctor, mut has_no_arg_ctor) = parse_class_body(&body, source);
    if !has_no_arg_ctor
        && annotations
            .iter()
            .any(|ann| ann.simple_name == "NoArgsConstructor")
    {
        // Best-effort: treat Lombok's `@NoArgsConstructor` as satisfying the JPA requirement.
        has_no_arg_ctor = true;
    }

    Some(Entity {
        name,
        jpql_name,
        table,
        span,
        source: source_idx,
        fields,
        has_explicit_ctor,
        has_no_arg_ctor,
    })
}

fn parse_class_body(body: &SyntaxNode, source: &str) -> (Vec<Field>, bool, bool) {
    let mut fields = Vec::new();
    let mut method_properties = Vec::new();
    let mut has_explicit_ctor = false;
    let mut has_no_arg_ctor = false;

    for child in body.children() {
        match child.kind() {
            SyntaxKind::FieldDeclaration => fields.extend(parse_field_declaration(&child, source)),
            SyntaxKind::MethodDeclaration => {
                if let Some(field) = parse_method_property(&child, source) {
                    method_properties.push(field);
                }
            }
            SyntaxKind::ConstructorDeclaration => {
                has_explicit_ctor = true;
                if is_no_arg_constructor(&child) {
                    has_no_arg_ctor = true;
                }
            }
            _ => {}
        }
    }

    // If there is no explicit constructor then Java provides an implicit no-arg
    // constructor.
    if !has_explicit_ctor {
        has_no_arg_ctor = true;
    }

    if !method_properties.is_empty() {
        let mut by_name: HashMap<String, usize> = fields
            .iter()
            .enumerate()
            .map(|(idx, f)| (f.name.clone(), idx))
            .collect();

        for method_field in method_properties {
            if let Some(&idx) = by_name.get(&method_field.name) {
                let existing = &mut fields[idx];
                existing.is_id |= method_field.is_id;
                existing.is_embedded_id |= method_field.is_embedded_id;
                if method_field.relationship.is_some() {
                    existing.relationship = method_field.relationship.clone();
                }
                if existing.ty.is_empty() && !method_field.ty.is_empty() {
                    existing.ty = method_field.ty.clone();
                }
                if method_field.is_id
                    || method_field.is_embedded_id
                    || method_field.relationship.is_some()
                {
                    existing.span = method_field.span;
                }
            } else {
                by_name.insert(method_field.name.clone(), fields.len());
                fields.push(method_field);
            }
        }
    }

    (fields, has_explicit_ctor, has_no_arg_ctor)
}

fn is_no_arg_constructor(node: &SyntaxNode) -> bool {
    // Best-effort check: parameter list is empty and constructor isn't explicitly `private`.
    let modifiers = node.children().find(|n| n.kind() == SyntaxKind::Modifiers);
    if modifiers
        .as_ref()
        .is_some_and(|m| modifier_contains(m, SyntaxKind::PrivateKw))
    {
        return false;
    }

    let params = node
        .children()
        .find(|n| n.kind() == SyntaxKind::ParameterList);
    let Some(params) = params else {
        return false;
    };
    params
        .children()
        .filter(|n| n.kind() == SyntaxKind::Parameter)
        .count()
        == 0
}

fn parse_field_declaration(node: &SyntaxNode, source: &str) -> Vec<Field> {
    let modifiers = node.children().find(|n| n.kind() == SyntaxKind::Modifiers);
    let annotations = modifiers
        .as_ref()
        .map(|m| collect_annotations(m, source))
        .unwrap_or_else(Vec::new);
    let is_static = modifiers
        .as_ref()
        .is_some_and(|m| modifier_contains(m, SyntaxKind::StaticKw));
    let is_transient_kw = modifiers
        .as_ref()
        .is_some_and(|m| modifier_contains(m, SyntaxKind::TransientKw));

    let is_transient =
        is_transient_kw || annotations.iter().any(|ann| ann.simple_name == "Transient");
    if is_static || is_transient {
        return Vec::new();
    }
    let is_id = annotations.iter().any(|ann| ann.simple_name == "Id");
    let is_embedded_id = annotations
        .iter()
        .any(|ann| ann.simple_name == "EmbeddedId");

    let relationship = annotations
        .iter()
        .find_map(|ann| relationship_from_annotation(ann, source));

    let ty_node = node.children().find(|n| n.kind() == SyntaxKind::Type);
    let Some(ty_node) = ty_node else {
        return Vec::new();
    };
    let ty = clean_type(node_text(source, &ty_node));

    let mut fields = Vec::new();
    let Some(declarators) = node
        .children()
        .find(|n| n.kind() == SyntaxKind::VariableDeclaratorList)
    else {
        return fields;
    };

    for declarator in declarators
        .children()
        .filter(|n| n.kind() == SyntaxKind::VariableDeclarator)
    {
        let Some(name_tok) = declarator
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind().is_identifier_like())
        else {
            continue;
        };
        let name = name_tok.text().to_string();
        let span = span_of_token(&name_tok);

        fields.push(Field {
            name: name.clone(),
            ty: ty.clone(),
            span,
            is_transient,
            is_static,
            is_id,
            is_embedded_id,
            relationship: relationship.as_ref().map(|rel| Relationship {
                field_name: name.clone(),
                ..rel.clone()
            }),
        });
    }

    fields
}

fn parse_method_property(node: &SyntaxNode, source: &str) -> Option<Field> {
    // Best-effort support for JPA property access where annotations are placed
    // on getter methods rather than fields.
    //
    // We only treat no-arg getter-like methods as persistent properties to avoid
    // pulling in arbitrary business methods.
    let modifiers = node.children().find(|n| n.kind() == SyntaxKind::Modifiers);
    let annotations = modifiers
        .as_ref()
        .map(|m| collect_annotations(m, source))
        .unwrap_or_else(Vec::new);
    let is_static = modifiers
        .as_ref()
        .is_some_and(|m| modifier_contains(m, SyntaxKind::StaticKw));

    if is_static {
        return None;
    }

    let params = node
        .children()
        .find(|n| n.kind() == SyntaxKind::ParameterList);
    if params
        .as_ref()
        .is_some_and(|p| p.children().any(|n| n.kind() == SyntaxKind::Parameter))
    {
        return None;
    }

    let name_tok = method_name_token(node)?;
    let method_name = name_tok.text().trim().to_string();

    let ty_node = node.children().find(|n| n.kind() == SyntaxKind::Type)?;
    let ty = clean_type(node_text(source, &ty_node));

    let prop_name = getter_property_name(&method_name, &ty)?;

    let is_transient = annotations.iter().any(|ann| ann.simple_name == "Transient");
    if is_transient {
        return None;
    }

    let is_id = annotations.iter().any(|ann| ann.simple_name == "Id");
    let is_embedded_id = annotations
        .iter()
        .any(|ann| ann.simple_name == "EmbeddedId");

    let relationship = annotations
        .iter()
        .find_map(|ann| relationship_from_annotation(ann, source));

    let span = span_of_token(&name_tok);

    Some(Field {
        name: prop_name.clone(),
        ty,
        span,
        is_transient,
        is_static,
        is_id,
        is_embedded_id,
        relationship: relationship.as_ref().map(|rel| Relationship {
            field_name: prop_name,
            ..rel.clone()
        }),
    })
}

fn getter_property_name(method_name: &str, return_type: &str) -> Option<String> {
    let return_type = return_type.trim();
    if return_type.is_empty() || return_type == "void" {
        return None;
    }

    if let Some(rest) = method_name.strip_prefix("get") {
        if rest.is_empty() {
            return None;
        }
        if rest.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            return Some(decapitalize_java_bean(rest));
        }
    }

    if let Some(rest) = method_name.strip_prefix("is") {
        if rest.is_empty() {
            return None;
        }
        if rest.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            return Some(decapitalize_java_bean(rest));
        }
    }

    None
}

fn decapitalize_java_bean(name: &str) -> String {
    // JavaBeans decapitalize rules: if the first two chars are both uppercase,
    // do not change the name (e.g. "URL" stays "URL").
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let second = chars.clone().next();

    if first.is_ascii_uppercase() && second.is_some_and(|c| c.is_ascii_uppercase()) {
        return name.to_string();
    }

    let mut out = String::new();
    out.push(first.to_ascii_lowercase());
    out.push_str(chars.as_str());
    out
}

fn relationship_from_annotation(ann: &ParsedAnnotation, _source: &str) -> Option<Relationship> {
    let kind = match ann.simple_name.as_str() {
        "OneToMany" => RelationshipKind::OneToMany,
        "ManyToOne" => RelationshipKind::ManyToOne,
        "ManyToMany" => RelationshipKind::ManyToMany,
        "OneToOne" => RelationshipKind::OneToOne,
        _ => return None,
    };

    let target_entity = ann
        .args
        .get("targetEntity")
        .and_then(|value| parse_class_literal(value));

    Some(Relationship {
        kind,
        field_name: String::new(),
        target_entity,
        mapped_by: ann.args.get("mappedBy").cloned(),
        span: ann.span,
    })
}

fn collect_annotations(modifiers: &SyntaxNode, source: &str) -> Vec<ParsedAnnotation> {
    let mut anns = Vec::new();
    for child in modifiers.children() {
        if child.kind() == SyntaxKind::Annotation {
            let text = node_text(source, &child);
            let span = span_of_node(&child);
            if let Some(ann) = parse_annotation_text(text, span) {
                anns.push(ann);
            }
        }
    }
    anns
}

fn node_text<'a>(source: &'a str, node: &SyntaxNode) -> &'a str {
    let range = node.text_range();
    let start: usize = u32::from(range.start()) as usize;
    let end: usize = u32::from(range.end()) as usize;
    &source[start..end]
}

fn span_of_node(node: &SyntaxNode) -> Span {
    let range = node.text_range();
    let start: usize = u32::from(range.start()) as usize;
    let end: usize = u32::from(range.end()) as usize;
    Span::new(start, end)
}

fn span_of_token(token: &SyntaxToken) -> Span {
    let range = token.text_range();
    let start: usize = u32::from(range.start()) as usize;
    let end: usize = u32::from(range.end()) as usize;
    Span::new(start, end)
}

fn modifier_contains(modifiers: &SyntaxNode, kind: SyntaxKind) -> bool {
    modifiers
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| t.kind() == kind)
}

fn class_name_token(node: &SyntaxNode) -> Option<SyntaxToken> {
    let mut saw_class_kw = false;
    for element in node.children_with_tokens() {
        let Some(token) = element.into_token() else {
            continue;
        };
        match token.kind() {
            SyntaxKind::ClassKw => saw_class_kw = true,
            k if saw_class_kw && k.is_identifier_like() => return Some(token),
            _ => {}
        }
    }
    None
}

fn method_name_token(node: &SyntaxNode) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind().is_identifier_like())
}

fn validate_model(model: &EntityModel) -> Vec<SourceDiagnostic> {
    let mut diags = Vec::new();

    for entity in model.entities.values() {
        if entity.id_fields().next().is_none() {
            diags.push(SourceDiagnostic {
                source: entity.source,
                diagnostic: Diagnostic::error(
                    JPA_MISSING_ID,
                    format!(
                        "Entity `{}` does not declare an @Id or @EmbeddedId field",
                        entity.name
                    ),
                    Some(entity.span),
                ),
            });
        }

        if entity.has_explicit_ctor && !entity.has_no_arg_ctor {
            diags.push(SourceDiagnostic {
                source: entity.source,
                diagnostic: Diagnostic::warning(
                    JPA_NO_NOARG_CTOR,
                    format!(
                        "Entity `{}` does not declare a non-private no-arg constructor",
                        entity.name
                    ),
                    Some(entity.span),
                ),
            });
        }
    }

    diags
}

fn hydrate_relationship_targets(model: &mut EntityModel) {
    // Derive the relationship target type from the field type once all entities
    // are known.
    let entity_names: HashSet<String> = model.entities.keys().cloned().collect();
    for entity in model.entities.values_mut() {
        for field in &mut entity.fields {
            let Some(rel) = field.relationship.as_mut() else {
                continue;
            };
            if rel.target_entity.is_some() {
                continue;
            }
            rel.target_entity = relationship_target_from_type(&rel.kind, &field.ty, &entity_names);
        }
    }
}

fn relationship_target_from_type(
    kind: &RelationshipKind,
    ty: &str,
    entity_names: &HashSet<String>,
) -> Option<String> {
    let ty = ty.trim();
    if ty.is_empty() {
        return None;
    }

    let simple = |s: &str| s.rsplit('.').next().unwrap_or(s).to_string();

    match kind {
        RelationshipKind::ManyToOne | RelationshipKind::OneToOne => {
            let name = simple(ty.trim_end_matches("[]"));
            Some(name)
        }
        RelationshipKind::OneToMany | RelationshipKind::ManyToMany => {
            // collection relationship; try to extract the first generic argument.
            if let Some((base, arg)) = split_generic_type(ty) {
                let _ = base;
                let arg = arg.trim();
                let arg = arg
                    .strip_prefix("?extends")
                    .or_else(|| arg.strip_prefix("?super"))
                    .unwrap_or(arg);
                let arg = arg.trim();
                let name = simple(arg.trim_end_matches("[]"));
                return Some(name);
            }
            // Non-generic collections are ambiguous; if the raw type matches an
            // entity we keep it, otherwise return None.
            let name = simple(ty.trim_end_matches("[]"));
            if entity_names.contains(&name) {
                Some(name)
            } else {
                None
            }
        }
    }
}

fn split_generic_type(ty: &str) -> Option<(String, String)> {
    let start = ty.find('<')?;
    let end = ty.rfind('>')?;
    if end <= start {
        return None;
    }
    let base = ty[..start].to_string();
    // Only return first type arg.
    let inner = &ty[start + 1..end];
    let first = inner.split(',').next().unwrap_or(inner).trim();
    Some((base, first.to_string()))
}

fn validate_relationships(model: &EntityModel) -> Vec<SourceDiagnostic> {
    let mut diags = Vec::new();

    for entity in model.entities.values() {
        for field in &entity.fields {
            let Some(rel) = &field.relationship else {
                continue;
            };

            if !relationship_type_matches_field(&rel.kind, &field.ty) {
                diags.push(SourceDiagnostic {
                    source: entity.source,
                    diagnostic: Diagnostic::error(
                        JPA_REL_INVALID_TARGET_TYPE,
                        format!(
                            "Relationship `{}`.{} has incompatible field type `{}` for {:?}",
                            entity.name, field.name, field.ty, rel.kind
                        ),
                        Some(rel.span),
                    ),
                });
            }

            let Some(target) = &rel.target_entity else {
                diags.push(SourceDiagnostic {
                    source: entity.source,
                    diagnostic: Diagnostic::warning(
                        JPA_REL_TARGET_UNKNOWN,
                        format!(
                            "Unable to determine relationship target for `{}`.{}",
                            entity.name, field.name
                        ),
                        Some(rel.span),
                    ),
                });
                continue;
            };

            if model.entity(target).is_none() {
                diags.push(SourceDiagnostic {
                    source: entity.source,
                    diagnostic: Diagnostic::error(
                        JPA_REL_TARGET_NOT_ENTITY,
                        format!(
                            "Relationship `{}`.{} targets `{}`, which is not a known @Entity",
                            entity.name, field.name, target
                        ),
                        Some(rel.span),
                    ),
                });
            }

            if let Some(mapped_by) = &rel.mapped_by {
                if let Some(target_entity) = model.entity(target) {
                    let Some(mapped_field) = target_entity.field_named(mapped_by) else {
                        diags.push(SourceDiagnostic {
                            source: entity.source,
                            diagnostic: Diagnostic::error(
                                JPA_MAPPEDBY_MISSING,
                                format!(
                                    "`mappedBy=\"{}\"` on `{}`.{} does not exist on target entity `{}`",
                                    mapped_by, entity.name, field.name, target
                                ),
                                Some(rel.span),
                            ),
                        });
                        continue;
                    };

                    // Best-effort: validate that the mappedBy field looks like a
                    // relationship back to the declaring entity.
                    let Some(mapped_rel) = &mapped_field.relationship else {
                        diags.push(SourceDiagnostic {
                            source: entity.source,
                            diagnostic: Diagnostic::warning(
                                JPA_MAPPEDBY_NOT_RELATIONSHIP,
                                format!(
                                    "`mappedBy=\"{}\"` on `{}`.{} refers to `{}`.{}, which is not a relationship field",
                                    mapped_by, entity.name, field.name, target, mapped_by
                                ),
                                Some(rel.span),
                            ),
                        });
                        continue;
                    };

                    if let Some(mapped_target) = &mapped_rel.target_entity {
                        if mapped_target != &entity.name {
                            diags.push(SourceDiagnostic {
                                source: entity.source,
                                diagnostic: Diagnostic::warning(
                                    JPA_MAPPEDBY_WRONG_TARGET,
                                    format!(
                                        "`mappedBy=\"{}\"` on `{}`.{} points at `{}`.{} which targets `{}`, expected `{}`",
                                        mapped_by,
                                        entity.name,
                                        field.name,
                                        target,
                                        mapped_by,
                                        mapped_target,
                                        entity.name
                                    ),
                                    Some(rel.span),
                                ),
                            });
                        }
                    }
                }
            }
        }
    }

    diags
}

fn relationship_type_matches_field(kind: &RelationshipKind, field_ty: &str) -> bool {
    if field_ty.trim().is_empty() {
        return true;
    }
    match kind {
        RelationshipKind::OneToMany | RelationshipKind::ManyToMany => {
            is_collection_like_type(field_ty)
        }
        RelationshipKind::ManyToOne | RelationshipKind::OneToOne => {
            !is_collection_like_type(field_ty)
        }
    }
}

fn is_collection_like_type(ty: &str) -> bool {
    let ty = ty.trim();
    debug_assert!(!ty.is_empty());
    if ty.ends_with("[]") {
        return true;
    }

    let base = split_generic_type(ty)
        .map(|(b, _)| b)
        .unwrap_or_else(|| ty.to_string());
    let base = base.trim();
    let simple = base.rsplit('.').next().unwrap_or(base);

    matches!(simple, "List" | "Set" | "Collection" | "Iterable" | "Map")
}

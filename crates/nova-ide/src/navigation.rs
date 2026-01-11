use lsp_types::{Location, Position, Uri};
use nova_types::Span;

use crate::db::DatabaseSnapshot;
use crate::parse::{CallSite, ParsedFile, TypeKind};
use crate::text::{position_to_offset, span_to_lsp_range};

impl DatabaseSnapshot {
    /// Best-effort `textDocument/implementation`.
    #[must_use]
    pub fn implementation(&self, file: &Uri, position: Position) -> Vec<Location> {
        let Some(parsed) = self.file(file) else {
            return Vec::new();
        };
        let Some(offset) = position_to_offset(&parsed.text, position) else {
            return Vec::new();
        };

        if let Some(call) = parsed
            .calls
            .iter()
            .find(|call| span_contains(call.method_span, offset))
        {
            return self.implementation_for_call(parsed, offset, call);
        }

        if let Some((ty_name, method_name)) = self.method_decl_at(parsed, offset) {
            return self.implementation_for_abstract_method(&ty_name, &method_name);
        }

        if let Some(type_name) = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.name_span, offset))
            .map(|ty| ty.name.clone())
        {
            return self.implementation_for_type(&type_name);
        }

        Vec::new()
    }

    /// Best-effort `textDocument/declaration`.
    #[must_use]
    pub fn declaration(&self, file: &Uri, position: Position) -> Option<Location> {
        let parsed = self.file(file)?;
        let offset = position_to_offset(&parsed.text, position)?;

        if let Some((ty_name, method_name)) = self.method_decl_at(parsed, offset) {
            return self.declaration_for_override(&ty_name, &method_name);
        }

        let (ident, _span) = identifier_at(&parsed.text, offset)?;
        if let Some((decl_file, decl_span)) = self.variable_declaration(parsed, offset, &ident) {
            let decl_parsed = self.file(&decl_file)?;
            return Some(Location {
                uri: decl_file,
                range: span_to_lsp_range(&decl_parsed.text, decl_span),
            });
        }

        None
    }

    /// Best-effort `textDocument/typeDefinition`.
    #[must_use]
    pub fn type_definition(&self, file: &Uri, position: Position) -> Option<Location> {
        let parsed = self.file(file)?;
        let offset = position_to_offset(&parsed.text, position)?;

        if let Some((ident, _ident_span)) = identifier_at(&parsed.text, offset) {
            if let Some(type_info) = self.type_info(&ident) {
                let def_file = self.file(&type_info.uri)?;
                return Some(Location {
                    uri: type_info.uri.clone(),
                    range: span_to_lsp_range(&def_file.text, type_info.def.name_span),
                });
            }

            let ty = self.resolve_name_type(parsed, offset, &ident)?;
            let type_info = self.type_info(&ty)?;
            let def_file = self.file(&type_info.uri)?;
            return Some(Location {
                uri: type_info.uri.clone(),
                range: span_to_lsp_range(&def_file.text, type_info.def.name_span),
            });
        }

        None
    }

    fn implementation_for_type(&self, type_name: &str) -> Vec<Location> {
        let mut out = Vec::new();
        for subtype in self.inheritance().all_subtypes(type_name) {
            if let Some(type_info) = self.type_info(&subtype) {
                if let Some(parsed) = self.file(&type_info.uri) {
                    out.push(Location {
                        uri: type_info.uri.clone(),
                        range: span_to_lsp_range(&parsed.text, type_info.def.name_span),
                    });
                }
            }
        }
        out
    }

    fn implementation_for_abstract_method(
        &self,
        ty_name: &str,
        method_name: &str,
    ) -> Vec<Location> {
        let Some(type_info) = self.type_info(ty_name) else {
            return Vec::new();
        };

        let method = type_info.def.methods.iter().find(|m| m.name == method_name);
        let is_abstract = type_info.def.kind == TypeKind::Interface
            || method.is_some_and(|m| m.is_abstract || m.body_span.is_none());

        if !is_abstract {
            return Vec::new();
        }

        let mut out = Vec::new();
        for subtype in self.inheritance().all_subtypes(ty_name) {
            let Some((uri, span)) = self.resolve_method_definition(&subtype, method_name) else {
                continue;
            };
            let Some(parsed) = self.file(&uri) else {
                continue;
            };
            out.push(Location {
                uri,
                range: span_to_lsp_range(&parsed.text, span),
            });
        }

        out.sort_by(|a, b| a.uri.to_string().cmp(&b.uri.to_string()));
        out.dedup_by(|a, b| a.uri == b.uri && a.range.start == b.range.start);
        out
    }

    fn implementation_for_call(
        &self,
        parsed: &ParsedFile,
        offset: usize,
        call: &CallSite,
    ) -> Vec<Location> {
        let Some(containing_type) = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))
        else {
            return Vec::new();
        };

        let receiver_ty = if call.receiver == "this" {
            Some(containing_type.name.clone())
        } else if call.receiver == "super" {
            containing_type.super_class.clone()
        } else {
            self.resolve_name_type(parsed, offset, &call.receiver)
        };

        let Some(receiver_ty) = receiver_ty else {
            return Vec::new();
        };

        let Some(receiver_info) = self.type_info(&receiver_ty) else {
            return Vec::new();
        };

        let receiver_is_interface = receiver_info.def.kind == TypeKind::Interface;
        let receiver_is_abstract = receiver_info.def.modifiers.is_abstract;
        let receiver_is_final = receiver_info.def.modifiers.is_final;

        let mut candidates: Vec<(Uri, Span)> = Vec::new();

        if let Some(def) = self.resolve_method_definition(&receiver_ty, &call.method) {
            candidates.push(def);
        }

        if !receiver_is_final {
            if receiver_is_interface || receiver_is_abstract {
                for subtype in self.inheritance().all_subtypes(&receiver_ty) {
                    if let Some(def) = self.resolve_method_definition(&subtype, &call.method) {
                        candidates.push(def);
                    }
                }
            } else {
                for subtype in self.inheritance().all_subtypes(&receiver_ty) {
                    if let Some(def) = self.resolve_method_definition(&subtype, &call.method) {
                        candidates.push(def);
                    }
                }
            }
        }

        candidates.sort_by(|a, b| {
            a.0.to_string()
                .cmp(&b.0.to_string())
                .then(a.1.start.cmp(&b.1.start))
        });
        candidates.dedup_by(|a, b| a.0 == b.0 && a.1.start == b.1.start);

        candidates
            .into_iter()
            .filter_map(|(uri, span)| {
                let file = self.file(&uri)?;
                Some(Location {
                    uri,
                    range: span_to_lsp_range(&file.text, span),
                })
            })
            .collect()
    }

    fn resolve_method_definition(&self, ty_name: &str, method_name: &str) -> Option<(Uri, Span)> {
        let type_info = self.type_info(ty_name)?;
        if let Some(method) = type_info
            .def
            .methods
            .iter()
            .find(|m| m.name == method_name && m.body_span.is_some())
        {
            return Some((type_info.uri.clone(), method.name_span));
        }

        let super_name = type_info.def.super_class.as_deref()?;
        self.resolve_method_definition(super_name, method_name)
    }

    fn declaration_for_override(&self, ty_name: &str, method_name: &str) -> Option<Location> {
        let type_info = self.type_info(ty_name)?;

        // Prefer interface declarations.
        for iface in &type_info.def.interfaces {
            if let Some(loc) = self.declaration_in_type(iface, method_name) {
                return Some(loc);
            }
        }

        // Then walk the superclass chain.
        let mut cur = type_info.def.super_class.clone();
        while let Some(next) = cur {
            if let Some(loc) = self.declaration_in_type(&next, method_name) {
                return Some(loc);
            }
            cur = self
                .type_info(&next)
                .and_then(|info| info.def.super_class.clone());
        }

        // Locals/fields/methods default: declaration == definition.
        let file = self.file(&type_info.uri)?;
        let method = type_info
            .def
            .methods
            .iter()
            .find(|m| m.name == method_name)?;
        Some(Location {
            uri: type_info.uri.clone(),
            range: span_to_lsp_range(&file.text, method.name_span),
        })
    }

    fn declaration_in_type(&self, ty_name: &str, method_name: &str) -> Option<Location> {
        let type_info = self.type_info(ty_name)?;
        let method = type_info
            .def
            .methods
            .iter()
            .find(|m| m.name == method_name)?;

        let is_declaration = type_info.def.kind == TypeKind::Interface
            || method.is_abstract
            || method.body_span.is_none();
        if !is_declaration {
            return None;
        }

        let file = self.file(&type_info.uri)?;
        Some(Location {
            uri: type_info.uri.clone(),
            range: span_to_lsp_range(&file.text, method.name_span),
        })
    }

    fn method_decl_at(&self, parsed: &ParsedFile, offset: usize) -> Option<(String, String)> {
        for ty in &parsed.types {
            for m in &ty.methods {
                if span_contains(m.name_span, offset) {
                    return Some((ty.name.clone(), m.name.clone()));
                }
            }
        }
        None
    }

    fn resolve_name_type(&self, parsed: &ParsedFile, offset: usize, name: &str) -> Option<String> {
        let ty = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))?;

        if let Some(method) = ty
            .methods
            .iter()
            .find(|m| m.body_span.is_some_and(|r| span_contains(r, offset)))
        {
            if let Some(local) = method.locals.iter().find(|v| v.name == name) {
                return Some(local.ty.clone());
            }
        }

        if let Some(field) = ty.fields.iter().find(|f| f.name == name) {
            return Some(field.ty.clone());
        }

        None
    }

    fn variable_declaration(
        &self,
        parsed: &ParsedFile,
        offset: usize,
        name: &str,
    ) -> Option<(Uri, Span)> {
        let ty = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.body_span, offset))?;

        if let Some(method) = ty
            .methods
            .iter()
            .find(|m| m.body_span.is_some_and(|r| span_contains(r, offset)))
        {
            if let Some(local) = method.locals.iter().find(|v| v.name == name) {
                return Some((parsed.uri.clone(), local.name_span));
            }
        }

        if let Some(field) = ty.fields.iter().find(|f| f.name == name) {
            return Some((parsed.uri.clone(), field.name_span));
        }

        None
    }
}

fn span_contains(span: Span, offset: usize) -> bool {
    span.start <= offset && offset < span.end
}

fn identifier_at(text: &str, offset: usize) -> Option<(String, Span)> {
    if offset > text.len() {
        return None;
    }

    let bytes = text.as_bytes();
    let mut start = offset;
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' {
            start -= 1;
        } else {
            break;
        }
    }

    let mut end = offset;
    while end < bytes.len() {
        let ch = bytes[end] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' {
            end += 1;
        } else {
            break;
        }
    }

    if start == end {
        return None;
    }

    Some((text[start..end].to_string(), Span::new(start, end)))
}

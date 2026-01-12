use lsp_types::{Location, Position, Uri};

use crate::db::DatabaseSnapshot;
use crate::nav_core;
use crate::text::{position_to_offset_with_index, span_to_lsp_range_with_index};

impl DatabaseSnapshot {
    /// Best-effort `textDocument/implementation`.
    #[must_use]
    pub fn implementation(&self, file: &Uri, position: Position) -> Vec<Location> {
        let Some(parsed) = self.file(file) else {
            return Vec::new();
        };
        let Some(offset) =
            position_to_offset_with_index(&parsed.line_index, &parsed.text, position)
        else {
            return Vec::new();
        };

        let lookup_type_info = |name: &str| self.type_info(name);
        let lookup_file = |uri: &Uri| self.file(uri);
        let no_fallback = |_: &str, _: &str| None;

        if let Some(call) = parsed
            .calls
            .iter()
            .find(|call| nav_core::span_contains(call.method_span, offset))
        {
            return nav_core::implementation_for_call(
                self.inheritance(),
                &lookup_type_info,
                &lookup_file,
                parsed,
                offset,
                call,
                &no_fallback,
            );
        }

        if let Some((ty_name, method_name)) = nav_core::method_decl_at(parsed, offset) {
            return nav_core::implementation_for_abstract_method(
                self.inheritance(),
                &lookup_type_info,
                &lookup_file,
                &ty_name,
                &method_name,
            );
        }

        if let Some(type_name) = parsed
            .types
            .iter()
            .find(|ty| nav_core::span_contains(ty.name_span, offset))
            .map(|ty| ty.name.clone())
        {
            return nav_core::implementation_for_type(
                self.inheritance(),
                &lookup_type_info,
                &lookup_file,
                &type_name,
            );
        }

        Vec::new()
    }

    /// Best-effort `textDocument/declaration`.
    #[must_use]
    pub fn declaration(&self, file: &Uri, position: Position) -> Option<Location> {
        let parsed = self.file(file)?;
        let offset = position_to_offset_with_index(&parsed.line_index, &parsed.text, position)?;

        let lookup_type_info = |name: &str| self.type_info(name);
        let lookup_file = |uri: &Uri| self.file(uri);

        if let Some((ty_name, method_name)) = nav_core::method_decl_at(parsed, offset) {
            return nav_core::declaration_for_override(
                &lookup_type_info,
                &lookup_file,
                &ty_name,
                &method_name,
            );
        }

        let (ident, _span) = nav_core::identifier_at(&parsed.text, offset)?;
        if let Some((decl_file, decl_span)) = nav_core::variable_declaration(parsed, offset, &ident)
        {
            let decl_parsed = self.file(&decl_file)?;
            return Some(Location {
                uri: decl_file,
                range: span_to_lsp_range_with_index(&decl_parsed.line_index, &decl_parsed.text, decl_span),
            });
        }

        None
    }

    /// Best-effort `textDocument/typeDefinition`.
    #[must_use]
    pub fn type_definition(&self, file: &Uri, position: Position) -> Option<Location> {
        let parsed = self.file(file)?;
        let offset = position_to_offset_with_index(&parsed.line_index, &parsed.text, position)?;

        if let Some((ident, _ident_span)) = nav_core::identifier_at(&parsed.text, offset) {
            if let Some(type_info) = self.type_info(&ident) {
                let def_file = self.file(&type_info.uri)?;
                return Some(Location {
                    uri: type_info.uri.clone(),
                    range: span_to_lsp_range_with_index(
                        &def_file.line_index,
                        &def_file.text,
                        type_info.def.name_span,
                    ),
                });
            }

            let ty = nav_core::resolve_name_type(parsed, offset, &ident)?;
            let type_info = self.type_info(&ty)?;
            let def_file = self.file(&type_info.uri)?;
            return Some(Location {
                uri: type_info.uri.clone(),
                range: span_to_lsp_range_with_index(
                    &def_file.line_index,
                    &def_file.text,
                    type_info.def.name_span,
                ),
            });
        }

        None
    }
}


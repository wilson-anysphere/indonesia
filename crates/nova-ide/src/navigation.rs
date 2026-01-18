use std::path::Path;
use std::str::FromStr;

use lsp_types::{Location, Position, Uri};

use crate::db::DatabaseSnapshot;
use crate::framework_cache;
use crate::nav_core;
use crate::text::{position_to_offset_with_index, span_to_lsp_range_with_index};
use nova_core::LineIndex;
use nova_db::Database as _;
use nova_framework_mapstruct::NavigationTarget as MapStructNavigationTarget;

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

        let mut locations = if let Some(call) = parsed
            .calls
            .iter()
            .find(|call| nav_core::span_contains(call.method_span, offset))
        {
            nav_core::implementation_for_call(
                self.inheritance(),
                &lookup_type_info,
                &lookup_file,
                parsed,
                offset,
                call,
                &no_fallback,
            )
        } else if let Some((ty_name, method_name)) = nav_core::method_decl_at(parsed, offset) {
            nav_core::implementation_for_abstract_method(
                self.inheritance(),
                &lookup_type_info,
                &lookup_file,
                &ty_name,
                &method_name,
            )
        } else if let Some(type_name) = parsed
            .types
            .iter()
            .find(|ty| nav_core::span_contains(ty.name_span, offset))
            .map(|ty| ty.name.clone())
        {
            nav_core::implementation_for_type(
                self.inheritance(),
                &lookup_type_info,
                &lookup_file,
                &type_name,
            )
        } else {
            Vec::new()
        };

        if locations.is_empty() {
            if let Some(file_id) = self.file_id_for_uri(file) {
                locations = mapstruct_fallback_locations(self, file_id, &parsed.text, offset);
            }
        }

        locations
    }

    /// Best-effort `textDocument/declaration`.
    #[must_use]
    pub fn declaration(&self, file: &Uri, position: Position) -> Option<Location> {
        let parsed = self.file(file)?;
        let offset = position_to_offset_with_index(&parsed.line_index, &parsed.text, position)?;

        let lookup_type_info = |name: &str| self.type_info(name);
        let lookup_file = |uri: &Uri| self.file(uri);

        let mut location =
            if let Some((ty_name, method_name)) = nav_core::method_decl_at(parsed, offset) {
                nav_core::declaration_for_override(
                    &lookup_type_info,
                    &lookup_file,
                    &ty_name,
                    &method_name,
                )
            } else {
                let (ident, _span) = nav_core::identifier_at(&parsed.text, offset)?;
                if let Some((decl_file, decl_span)) =
                    nav_core::variable_declaration(parsed, offset, &ident)
                {
                    let decl_parsed = self.file(&decl_file)?;
                    Some(Location {
                        uri: decl_file,
                        range: span_to_lsp_range_with_index(
                            &decl_parsed.line_index,
                            &decl_parsed.text,
                            decl_span,
                        ),
                    })
                } else {
                    None
                }
            };

        if location.is_none() {
            if let Some(file_id) = self.file_id_for_uri(file) {
                location = mapstruct_fallback_locations(self, file_id, &parsed.text, offset)
                    .into_iter()
                    .next();
            }
        }

        location
    }

    /// Best-effort `textDocument/typeDefinition`.
    #[must_use]
    pub fn type_definition(&self, file: &Uri, position: Position) -> Option<Location> {
        let parsed = self.file(file)?;
        let offset = position_to_offset_with_index(&parsed.line_index, &parsed.text, position)?;

        let lookup_type_info = |name: &str| self.type_info(name);
        let lookup_file = |uri: &Uri| self.file(uri);
        nav_core::type_definition_best_effort(&lookup_type_info, &lookup_file, parsed, offset)
    }
}

fn mapstruct_fallback_locations(
    db: &DatabaseSnapshot,
    file_id: nova_db::FileId,
    text: &str,
    offset: usize,
) -> Vec<Location> {
    let Some(path) = db.file_path(file_id) else {
        return Vec::new();
    };
    if path.extension().and_then(|e| e.to_str()) != Some("java") {
        return Vec::new();
    }
    if !nova_framework_mapstruct::looks_like_mapstruct_source(text) {
        return Vec::new();
    }

    let root = framework_cache::project_root_for_path(path);
    let targets =
        match nova_framework_mapstruct::goto_definition_in_source(&root, path, text, offset) {
            Ok(targets) => targets,
            Err(_) => return Vec::new(),
        };

    targets
        .into_iter()
        .filter_map(|target| mapstruct_target_location(db, target))
        .collect()
}

fn mapstruct_target_location(
    db: &DatabaseSnapshot,
    target: MapStructNavigationTarget,
) -> Option<Location> {
    let uri = uri_for_path(&target.file).unwrap_or_else(fallback_unknown_uri);

    if let Some(parsed) = db.file(&uri) {
        return Some(Location {
            uri,
            range: span_to_lsp_range_with_index(&parsed.line_index, &parsed.text, target.span),
        });
    }

    if let Some(file_id) = db.file_id(&target.file) {
        let text = db.file_content(file_id);
        let line_index = LineIndex::new(text);
        return Some(Location {
            uri,
            range: span_to_lsp_range_with_index(&line_index, text, target.span),
        });
    }

    let text = match std::fs::read_to_string(&target.file) {
        Ok(text) => text,
        Err(err) => {
            // The target may be outside the current workspace and could race with deletion.
            // Treat as best-effort and only log unexpected filesystem errors.
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    target = "nova.ide",
                    file = %target.file.display(),
                    error = %err,
                    "failed to read mapstruct navigation target file"
                );
            }
            return None;
        }
    };
    let line_index = LineIndex::new(&text);
    Some(Location {
        uri,
        range: span_to_lsp_range_with_index(&line_index, &text, target.span),
    })
}

fn uri_for_path(path: &Path) -> Option<Uri> {
    crate::uri::uri_from_path_best_effort(path, "navigation.uri_for_path")
}

fn fallback_unknown_uri() -> Uri {
    Uri::from_str("file:///unknown").expect("fallback URI is valid")
}

use crate::stdio_text::{ident_range_at, position_to_offset_utf16};
use crate::ServerState;

use nova_db::Database;
use nova_vfs::VfsPath;
use std::collections::HashMap;
use std::sync::Arc;

pub(super) fn handle_definition(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::TextDocumentPositionParams =
        crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let location = nova_lsp::goto_definition(&state.analysis, file_id, params.position)
        .or_else(|| goto_definition_jdk(state, file_id, params.position));
    match location {
        Some(loc) => serde_json::to_value(loc).map_err(|e| e.to_string()),
        None => Ok(serde_json::Value::Null),
    }
}

pub(super) fn handle_implementation(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::TextDocumentPositionParams =
        crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let locations = nova_lsp::implementation(&state.analysis, file_id, params.position);
    if locations.is_empty() {
        Ok(serde_json::Value::Null)
    } else {
        serde_json::to_value(locations).map_err(|e| e.to_string())
    }
}

pub(super) fn handle_declaration(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::TextDocumentPositionParams =
        crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let location = nova_lsp::declaration(&state.analysis, file_id, params.position);
    match location {
        Some(loc) => serde_json::to_value(loc).map_err(|e| e.to_string()),
        None => Ok(serde_json::Value::Null),
    }
}

pub(super) fn handle_type_definition(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::TextDocumentPositionParams =
        crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let location = nova_lsp::type_definition(&state.analysis, file_id, params.position)
        .or_else(|| type_definition_jdk(state, file_id, params.position));
    match location {
        Some(loc) => serde_json::to_value(loc).map_err(|e| e.to_string()),
        None => Ok(serde_json::Value::Null),
    }
}

fn parse_java_imports(text: &str) -> (HashMap<String, String>, Vec<String>) {
    let mut explicit_imports = HashMap::<String, String>::new();
    let mut wildcard_imports = Vec::<String>::new();

    for raw_line in text.lines() {
        let line = raw_line.trim_start();
        let Some(rest) = line.strip_prefix("import") else {
            continue;
        };
        // Ensure `import` is a standalone keyword.
        let mut rest_chars = rest.chars();
        if !rest_chars.next().is_some_and(|c| c.is_whitespace()) {
            continue;
        }
        let rest = rest.trim_start();

        // Ignore static imports for type navigation.
        if let Some(after_static) = rest.strip_prefix("static") {
            if after_static.is_empty()
                || after_static
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_whitespace())
            {
                continue;
            }
        }

        let import_path = rest.split_once(';').map(|(path, _)| path).unwrap_or(rest);
        let import_path = import_path.trim();
        if import_path.is_empty() {
            continue;
        }

        if let Some(pkg) = import_path.strip_suffix(".*") {
            let pkg = pkg.trim();
            if !pkg.is_empty() {
                wildcard_imports.push(pkg.to_owned());
            }
            continue;
        }

        let Some((_pkg, simple)) = import_path.rsplit_once('.') else {
            continue;
        };
        if simple.is_empty() {
            continue;
        }

        explicit_imports.insert(simple.to_owned(), import_path.to_owned());
    }

    (explicit_imports, wildcard_imports)
}

fn lookup_jdk_type_best_effort(
    jdk: &nova_jdk::JdkIndex,
    name: &str,
) -> Option<Arc<nova_jdk::JdkClassStub>> {
    let mut stub = match jdk.lookup_type(name) {
        Ok(stub) => stub,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                name,
                err = ?err,
                "jdk type lookup failed"
            );
            None
        }
    };
    if stub.is_some() {
        return stub;
    }

    // Best-effort support for nested type references written with dots in source/imports
    // (`java.util.Map.Entry`). The JDK index expects `$` for nested types
    // (`java.util.Map$Entry`).
    if !name.contains('/') && name.contains('.') {
        let mut candidate = name.to_owned();
        while let Some(dot) = candidate.rfind('.') {
            candidate.replace_range(dot..dot + 1, "$");
            stub = match jdk.lookup_type(&candidate) {
                Ok(stub) => stub,
                Err(err) => {
                    tracing::debug!(
                        target = "nova.lsp",
                        name,
                        candidate = %candidate,
                        err = ?err,
                        "jdk type lookup failed for nested-type candidate"
                    );
                    None
                }
            };
            if stub.is_some() {
                break;
            }
        }
    }

    stub
}

fn goto_definition_jdk(
    state: &mut ServerState,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Option<lsp_types::Location> {
    if state.jdk_index.is_none() {
        // Try to honor workspace JDK overrides (nova.toml `[jdk]`) when present. If the configured
        // JDK is invalid/unavailable, fall back to environment-based discovery so the feature keeps
        // working in partially configured environments.
        let configured = state.project_root.as_deref().and_then(|root| {
            let workspace_root =
                nova_project::workspace_root(root).unwrap_or_else(|| root.to_path_buf());
            let (config, _path) = match nova_config::load_for_workspace(&workspace_root) {
                Ok(loaded) => loaded,
                Err(err) => {
                    tracing::debug!(
                        target = "nova.lsp",
                        workspace_root = %workspace_root.display(),
                        err = %err,
                        "failed to load workspace config; falling back to environment JDK discovery"
                    );
                    return None;
                }
            };
            let jdk_config = config.jdk_config();
            match nova_jdk::JdkIndex::discover(Some(&jdk_config)) {
                Ok(index) => Some(index),
                Err(err) => {
                    tracing::debug!(
                        target = "nova.lsp",
                        workspace_root = %workspace_root.display(),
                        err = ?err,
                        "failed to discover configured JDK index; falling back to environment discovery"
                    );
                    None
                }
            }
        });

        state.jdk_index = configured.or_else(|| match nova_jdk::JdkIndex::discover(None) {
            Ok(index) => Some(index),
            Err(err) => {
                tracing::debug!(
                    target = "nova.lsp",
                    err = ?err,
                    "failed to discover JDK index from environment"
                );
                None
            }
        });
    }
    let jdk = state.jdk_index.as_ref()?;
    let text = state.analysis.file_content(file);
    let offset = position_to_offset_utf16(text, position)?;
    let (start, end) = ident_range_at(text, offset)?;
    let ident = text.get(start..end)?;

    fn is_ident_continue(b: u8) -> bool {
        (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$'
    }

    fn looks_like_type_name(receiver: &str) -> bool {
        receiver.contains('.')
            || receiver
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
    }

    fn infer_variable_type(text: &str, var_name: &str, before: usize) -> Option<String> {
        if var_name.is_empty() {
            return None;
        }

        let scope = text.get(..before)?;
        let bytes = scope.as_bytes();
        let var_len = var_name.len();

        let mut search_from = 0usize;
        let mut best: Option<String> = None;

        while let Some(found_rel) = scope.get(search_from..)?.find(var_name) {
            let found = search_from + found_rel;

            // Ensure we matched an identifier.
            let before_ok = found == 0 || !is_ident_continue(bytes[found - 1]);
            let after_ok =
                found + var_len >= bytes.len() || !is_ident_continue(bytes[found + var_len]);
            if !before_ok || !after_ok {
                search_from = found + 1;
                continue;
            }

            // Declarations should not be immediately followed by `.` (member access).
            let mut after = found + var_len;
            while after < bytes.len() && bytes[after].is_ascii_whitespace() {
                after += 1;
            }
            if after < bytes.len() && bytes[after] == b'.' {
                search_from = found + 1;
                continue;
            }

            // Grab the token immediately before the variable name.
            let mut ty_end = found;
            while ty_end > 0 && bytes[ty_end - 1].is_ascii_whitespace() {
                ty_end -= 1;
            }
            if ty_end == 0 {
                search_from = found + 1;
                continue;
            }

            let mut ty_start = ty_end;
            while ty_start > 0 {
                let b = bytes[ty_start - 1];
                if is_ident_continue(b) || b == b'.' {
                    ty_start -= 1;
                } else {
                    break;
                }
            }
            if ty_start == ty_end {
                search_from = found + 1;
                continue;
            }

            let candidate = scope.get(ty_start..ty_end)?;
            if looks_like_type_name(candidate) {
                best = Some(candidate.to_string());
            }

            search_from = found + 1;
        }

        best
    }

    fn resolve_jdk_type(
        jdk: &nova_jdk::JdkIndex,
        text: &str,
        name: &str,
    ) -> Option<Arc<nova_jdk::JdkClassStub>> {
        let mut stub = lookup_jdk_type_best_effort(jdk, name);
        if stub.is_none() && !name.contains('.') && !name.contains('/') {
            let (explicit_imports, wildcard_imports) = parse_java_imports(text);

            if let Some(fq_name) = explicit_imports.get(name) {
                stub = lookup_jdk_type_best_effort(jdk, fq_name);
            }

            if stub.is_none() {
                for pkg in wildcard_imports {
                    let candidate = format!("{pkg}.{name}");
                    stub = lookup_jdk_type_best_effort(jdk, &candidate);
                    if stub.is_some() {
                        break;
                    }
                }
            }

            if stub.is_none() {
                let suffix = format!(".{name}");
                match jdk.iter_binary_class_names() {
                    Ok(names) => {
                        let mut found: Option<&str> = None;
                        for candidate in names {
                            if candidate.ends_with(&suffix) {
                                if found.is_some() {
                                    // Ambiguous; stop early.
                                    found = None;
                                    break;
                                }
                                found = Some(candidate);
                            }
                        }

                        if let Some(binary_name) = found {
                            stub = lookup_jdk_type_best_effort(jdk, binary_name);
                        }
                    }
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.lsp",
                            error = ?err,
                            "failed to iterate JDK binary class names"
                        );
                    }
                }
            }
        }
        stub
    }

    let bytes = text.as_bytes();

    // Detect member access (`receiver.ident`) and resolve into the receiver's JDK class.
    //
    // Best-effort: Only attempt this when the character immediately preceding the identifier (after
    // skipping whitespace) is `.`.
    let mut before_start = start;
    while before_start > 0 && bytes[before_start - 1].is_ascii_whitespace() {
        before_start -= 1;
    }

    let mut stub: Option<Arc<nova_jdk::JdkClassStub>> = None;
    let mut member_symbol: Option<nova_decompile::SymbolKey> = None;

    if before_start > 0 && bytes[before_start - 1] == b'.' {
        let dot = before_start - 1;

        // Parse receiver expression just before the `.`.
        let mut recv_end = dot;
        while recv_end > 0 && bytes[recv_end - 1].is_ascii_whitespace() {
            recv_end -= 1;
        }

        if recv_end == 0 {
            return None;
        }

        let is_method_call = {
            let mut after_ident = end;
            while after_ident < bytes.len() && bytes[after_ident].is_ascii_whitespace() {
                after_ident += 1;
            }
            after_ident < bytes.len() && bytes[after_ident] == b'('
        };

        // Optional: `"x".method()` treated as `String.method()`.
        if bytes[recv_end - 1] == b'"' {
            if let Some(receiver_stub) = resolve_jdk_type(jdk, text, "String") {
                member_symbol = if is_method_call {
                    receiver_stub
                        .methods
                        .iter()
                        .find(|m| m.name == ident)
                        .map(|m| nova_decompile::SymbolKey::Method {
                            name: m.name.clone(),
                            descriptor: m.descriptor.clone(),
                        })
                } else {
                    receiver_stub
                        .fields
                        .iter()
                        .find(|f| f.name == ident)
                        .map(|f| nova_decompile::SymbolKey::Field {
                            name: f.name.clone(),
                            descriptor: f.descriptor.clone(),
                        })
                };
                stub = Some(receiver_stub);
            }
        } else {
            let mut recv_start = recv_end;
            while recv_start > 0 {
                let b = bytes[recv_start - 1];
                if is_ident_continue(b) || b == b'.' {
                    recv_start -= 1;
                } else {
                    break;
                }
            }
            if recv_start == recv_end {
                return None;
            }

            let receiver = text.get(recv_start..recv_end)?;

            let receiver_type_name = if looks_like_type_name(receiver) {
                Some(receiver.to_string())
            } else {
                infer_variable_type(text, receiver, dot)
            };
            let receiver_stub = receiver_type_name
                .as_deref()
                .and_then(|ty| resolve_jdk_type(jdk, text, ty));

            if let Some(stub_value) = receiver_stub.as_ref() {
                member_symbol = if is_method_call {
                    stub_value
                        .methods
                        .iter()
                        .find(|m| m.name == ident)
                        .map(|m| nova_decompile::SymbolKey::Method {
                            name: m.name.clone(),
                            descriptor: m.descriptor.clone(),
                        })
                } else {
                    stub_value.fields.iter().find(|f| f.name == ident).map(|f| {
                        nova_decompile::SymbolKey::Field {
                            name: f.name.clone(),
                            descriptor: f.descriptor.clone(),
                        }
                    })
                };
            }

            // If member resolution fails, also treat `receiver.ident` as a fully-qualified type
            // name (e.g. `java.util.List`).
            if member_symbol.is_some() {
                stub = receiver_stub;
            } else {
                let qualified = format!("{receiver}.{ident}");
                stub = resolve_jdk_type(jdk, text, &qualified)
                    .or_else(|| {
                        // Best-effort support for nested types referenced via an imported outer
                        // type, e.g. `Map.Entry` where `Map` is imported as `java.util.Map`.
                        if !looks_like_type_name(receiver) {
                            return None;
                        }
                        if !ident
                            .chars()
                            .next()
                            .is_some_and(|c: char| c.is_ascii_uppercase())
                        {
                            return None;
                        }
                        let receiver_stub = receiver_stub.as_ref()?;
                        let nested = format!("{}.{}", receiver_stub.binary_name, ident);
                        resolve_jdk_type(jdk, text, &nested)
                    })
                    .or(receiver_stub);
            }
        }
    }

    // Existing behavior: resolve identifier as a type name.
    let stub = stub.or_else(|| resolve_jdk_type(jdk, text, ident))?;
    let bytes = match jdk.read_class_bytes(&stub.internal_name) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return None,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                internal_name = %stub.internal_name,
                err = ?err,
                "failed to read classfile bytes"
            );
            return None;
        }
    };

    let uri_string = nova_decompile::decompiled_uri_for_classfile(&bytes, &stub.internal_name);

    let class_symbol = nova_decompile::SymbolKey::Class {
        internal_name: stub.internal_name.clone(),
    };

    // Store the virtual document so follow-up requests can read it via `Vfs::read_to_string`.
    let uri: lsp_types::Uri = match uri_string.parse() {
        Ok(uri) => uri,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                uri = %uri_string,
                err = %err,
                "failed to parse decompiled class uri"
            );
            return None;
        }
    };
    let vfs_path = VfsPath::from(&uri);

    // Best-effort: try to use the persisted decompiled-document store so we can compute a precise
    // symbol range without re-running the decompiler.
    let store = state.analysis.decompiled_store.as_ref();
    if let Some((text, mappings)) =
        vfs_path
            .as_decompiled()
            .and_then(|(content_hash, binary_name)| {
                match store.load_document(content_hash, binary_name) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.lsp",
                            content_hash,
                            binary_name,
                            err = ?err,
                            "failed to load decompiled document from store"
                        );
                        None
                    }
                }
            })
    {
        let cached_range = member_symbol
            .as_ref()
            .and_then(|symbol| {
                mappings
                    .iter()
                    .find(|m| &m.symbol == symbol)
                    .map(|m| m.range)
            })
            .or_else(|| {
                mappings
                    .iter()
                    .find(|m| &m.symbol == &class_symbol)
                    .map(|m| m.range)
            });

        if let Some(range) = cached_range {
            state.analysis.vfs.store_virtual_document(vfs_path, text);
            // Virtual documents are cached outside the "open documents" set; refresh our coarse
            // memory accounting so they still contribute to memory pressure and can trigger
            // eviction elsewhere.
            state.refresh_document_memory();

            return Some(lsp_types::Location {
                uri,
                range: lsp_types::Range::new(
                    lsp_types::Position::new(range.start.line, range.start.character),
                    lsp_types::Position::new(range.end.line, range.end.character),
                ),
            });
        }
    }

    let decompiled = match nova_decompile::decompile_classfile(&bytes) {
        Ok(decompiled) => decompiled,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                internal_name = %stub.internal_name,
                err = %err,
                "failed to decompile classfile bytes"
            );
            return None;
        }
    };

    // Persist the decompiled output (text + mappings) for future requests.
    // Ignore errors and fall back to the in-memory virtual document store.
    if let Some((content_hash, binary_name)) = vfs_path.as_decompiled() {
        if let Err(err) = store.store_document(
            content_hash,
            binary_name,
            &decompiled.text,
            &decompiled.mappings,
        ) {
            tracing::warn!(
                target = "nova.lsp",
                uri = %uri_string,
                error = %err,
                "failed to persist decompiled document"
            );
        }
    }

    let range = member_symbol
        .as_ref()
        .and_then(|symbol| decompiled.range_for(symbol))
        .or_else(|| decompiled.range_for(&class_symbol))?;

    state
        .analysis
        .vfs
        .store_virtual_document(vfs_path, decompiled.text);
    // Virtual documents are cached outside the "open documents" set; refresh our coarse memory
    // accounting so they still contribute to memory pressure and can trigger eviction elsewhere.
    state.refresh_document_memory();

    Some(lsp_types::Location {
        uri,
        range: lsp_types::Range::new(
            lsp_types::Position::new(range.start.line, range.start.character),
            lsp_types::Position::new(range.end.line, range.end.character),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{EnvVarGuard, ENV_LOCK};
    use lsp_types::TextDocumentPositionParams;
    use nova_memory::MemoryBudgetOverrides;
    use nova_vfs::{FileSystem as _, VfsPath};
    use tempfile::TempDir;

    #[test]
    fn go_to_definition_into_jdk_returns_canonical_virtual_uri_and_is_readable() {
        let _lock = crate::poison::lock(&ENV_LOCK, "stdio_goto/test/go_to_definition_jdk");

        // Point JDK discovery at the tiny fake JDK shipped in this repository.
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fake_jdk_root = manifest_dir.join("../nova-jdk/testdata/fake-jdk");
        let _java_home = EnvVarGuard::set("JAVA_HOME", &fake_jdk_root);

        let cache_dir = TempDir::new().expect("cache dir");
        let _cache_dir = EnvVarGuard::set("NOVA_CACHE_DIR", cache_dir.path());

        let mut state = ServerState::new(
            nova_config::NovaConfig::default(),
            None,
            MemoryBudgetOverrides::default(),
        );
        let dir = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(dir.path().join("Main.java")).unwrap();
        let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs).unwrap().parse().unwrap();

        let text = "class Main { void m() { String s = \"\"; } }".to_string();
        state.analysis.open_document(uri.clone(), text.clone(), 1);

        let offset = text.find("String").expect("String token exists");
        let position = nova_lsp::text_pos::lsp_position(&text, offset).expect("position");
        let params = TextDocumentPositionParams {
            text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            position,
        };
        let value = serde_json::to_value(params).unwrap();
        let resp = handle_definition(value, &mut state).unwrap();
        let loc: lsp_types::Location = serde_json::from_value(resp).unwrap();

        assert!(loc.uri.as_str().starts_with("nova:///decompiled/"));
        let vfs_path = VfsPath::from(&loc.uri);
        assert_eq!(vfs_path.to_uri().unwrap(), loc.uri.to_string());

        let loaded = state
            .analysis
            .vfs
            .read_to_string(&vfs_path)
            .expect("read virtual document");
        assert!(
            loaded.contains("class String"),
            "unexpected decompiled text: {loaded}"
        );
    }
}

fn type_definition_jdk(
    state: &mut ServerState,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Option<lsp_types::Location> {
    fn is_ident_continue(b: u8) -> bool {
        (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$'
    }

    fn dotted_ident_range(text: &str, start: usize, end: usize) -> Option<(usize, usize)> {
        let bytes = text.as_bytes();
        if start > bytes.len() || end > bytes.len() || start > end {
            return None;
        }

        let mut dotted_start = start;
        while dotted_start > 0 {
            if bytes.get(dotted_start.wrapping_sub(1)) != Some(&b'.') {
                break;
            }
            let seg_end = dotted_start - 1;
            if seg_end == 0 || !is_ident_continue(bytes[seg_end - 1]) {
                break;
            }
            let mut seg_start = seg_end;
            while seg_start > 0 && is_ident_continue(bytes[seg_start - 1]) {
                seg_start -= 1;
            }
            dotted_start = seg_start;
        }

        let mut dotted_end = end;
        while dotted_end < bytes.len() {
            if bytes.get(dotted_end) != Some(&b'.') {
                break;
            }
            let seg_start = dotted_end + 1;
            if seg_start >= bytes.len() || !is_ident_continue(bytes[seg_start]) {
                break;
            }
            let mut seg_end = seg_start + 1;
            while seg_end < bytes.len() && is_ident_continue(bytes[seg_end]) {
                seg_end += 1;
            }
            dotted_end = seg_end;
        }

        Some((dotted_start, dotted_end))
    }

    fn normalize_type_token(token: &str) -> Option<String> {
        let mut token = token.trim();
        if token.is_empty() {
            return None;
        }

        // Strip generic arguments.
        if let Some((head, _)) = token.split_once('<') {
            token = head;
        }

        // Strip array suffixes.
        let mut out = token.to_string();
        while out.ends_with("[]") {
            out.truncate(out.len() - 2);
        }

        // Strip varargs suffix.
        if out.ends_with("...") {
            out.truncate(out.len() - 3);
        }

        let out = out.trim();
        if out.is_empty() {
            None
        } else {
            Some(out.to_string())
        }
    }

    fn infer_type_from_var_initializer(text: &str, name_end: usize) -> Option<String> {
        let rest = text.get(name_end..)?;
        let statement = rest.split(';').next().unwrap_or(rest);
        let eq_offset = statement.find('=')?;
        let after_eq = statement.get(eq_offset + 1..)?.trim_start();
        let after_new = after_eq.strip_prefix("new")?.trim_start();

        let bytes = after_new.as_bytes();
        let mut end = 0usize;
        while end < bytes.len() {
            let b = bytes[end];
            if (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'.' {
                end += 1;
            } else {
                break;
            }
        }

        let ty = after_new.get(0..end)?.trim();
        normalize_type_token(ty)
    }

    fn declared_type_for_variable(text: &str, var: &str, cursor_offset: usize) -> Option<String> {
        let bytes = text.as_bytes();
        let var_bytes = var.as_bytes();
        if var_bytes.is_empty() {
            return None;
        }

        fn is_type_token_char(b: u8) -> bool {
            (b as char).is_ascii_alphanumeric()
                || b == b'_'
                || b == b'$'
                || b == b'.'
                || b == b'<'
                || b == b'>'
                || b == b'['
                || b == b']'
        }

        let mut best_before: Option<(usize, String)> = None;
        let mut best_after: Option<(usize, String)> = None;

        let mut search = 0usize;
        while search <= text.len() {
            let Some(found_rel) = text.get(search..)?.find(var) else {
                break;
            };
            let found = search + found_rel;

            let name_start = found;
            let name_end = found + var_bytes.len();

            // Ensure identifier boundaries.
            if name_start > 0 && is_ident_continue(bytes[name_start - 1]) {
                search = name_end;
                continue;
            }
            if name_end < bytes.len() && is_ident_continue(bytes[name_end]) {
                search = name_end;
                continue;
            }

            // Find the previous token (type) immediately preceding `<ws><name>`.
            let mut i = name_start;
            while i > 0 && bytes[i - 1].is_ascii_whitespace() {
                i -= 1;
            }
            let type_end = i;
            if type_end == 0 {
                search = name_end;
                continue;
            }

            let mut type_start = type_end;
            while type_start > 0 && is_type_token_char(bytes[type_start - 1]) {
                type_start -= 1;
            }
            if type_start == type_end {
                search = name_end;
                continue;
            }

            let raw_type = match text.get(type_start..type_end) {
                Some(slice) => slice.trim(),
                None => {
                    tracing::debug!(
                        target = "nova.lsp",
                        text_len = text.len(),
                        type_start,
                        type_end,
                        "failed to slice inferred variable type token"
                    );
                    search = name_end;
                    continue;
                }
            };
            let Some(mut ty) = normalize_type_token(raw_type) else {
                search = name_end;
                continue;
            };

            if ty == "var" {
                let Some(inferred) = infer_type_from_var_initializer(text, name_end) else {
                    search = name_end;
                    continue;
                };
                ty = inferred;
            }

            if name_start <= cursor_offset {
                best_before = Some((name_start, ty));
            } else if best_after.is_none() {
                best_after = Some((name_start, ty));
            }

            search = name_end;
        }

        best_before.or(best_after).map(|(_, ty)| ty)
    }

    fn resolve_jdk_type(
        jdk: &nova_jdk::JdkIndex,
        text: &str,
        name: &str,
    ) -> Option<Arc<nova_jdk::JdkClassStub>> {
        let mut stub = lookup_jdk_type_best_effort(jdk, name);
        if stub.is_none() && !name.contains('.') && !name.contains('/') {
            let (explicit_imports, wildcard_imports) = parse_java_imports(text);

            if let Some(fq_name) = explicit_imports.get(name) {
                stub = lookup_jdk_type_best_effort(jdk, fq_name);
            }

            if stub.is_none() {
                for pkg in wildcard_imports {
                    let candidate = format!("{pkg}.{name}");
                    stub = lookup_jdk_type_best_effort(jdk, &candidate);
                    if stub.is_some() {
                        break;
                    }
                }
            }

            if stub.is_none() {
                let suffix = format!(".{name}");
                match jdk.iter_binary_names() {
                    Ok(names) => {
                        let mut found: Option<&str> = None;
                        for candidate in names {
                            if candidate.ends_with(&suffix) {
                                if found.is_some() {
                                    // Ambiguous; stop early.
                                    found = None;
                                    break;
                                }
                                found = Some(candidate);
                            }
                        }

                        if let Some(binary_name) = found {
                            stub = lookup_jdk_type_best_effort(jdk, binary_name);
                        }
                    }
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.lsp",
                            error = ?err,
                            "failed to iterate JDK binary names"
                        );
                    }
                }
            }
        }
        stub
    }

    if state.jdk_index.is_none() {
        // Try to honor workspace JDK overrides (nova.toml `[jdk]`) when present. If the configured
        // JDK is invalid/unavailable, fall back to environment-based discovery so the feature keeps
        // working in partially configured environments.
        let configured = state.project_root.as_deref().and_then(|root| {
            let workspace_root =
                nova_project::workspace_root(root).unwrap_or_else(|| root.to_path_buf());
            let (config, _path) = match nova_config::load_for_workspace(&workspace_root) {
                Ok(loaded) => loaded,
                Err(err) => {
                    tracing::debug!(
                        target = "nova.lsp",
                        workspace_root = %workspace_root.display(),
                        err = %err,
                        "failed to load workspace config; falling back to environment JDK discovery"
                    );
                    return None;
                }
            };
            let jdk_config = config.jdk_config();
            match nova_jdk::JdkIndex::discover(Some(&jdk_config)) {
                Ok(index) => Some(index),
                Err(err) => {
                    tracing::debug!(
                        target = "nova.lsp",
                        workspace_root = %workspace_root.display(),
                        err = ?err,
                        "failed to discover configured JDK index; falling back to environment discovery"
                    );
                    None
                }
            }
        });

        state.jdk_index = configured.or_else(|| match nova_jdk::JdkIndex::discover(None) {
            Ok(index) => Some(index),
            Err(err) => {
                tracing::debug!(
                    target = "nova.lsp",
                    err = ?err,
                    "failed to discover JDK index from environment"
                );
                None
            }
        });
    }
    let jdk = state.jdk_index.as_ref()?;

    let text = state.analysis.file_content(file);
    let offset = position_to_offset_utf16(text, position)?;
    let (start, end) = ident_range_at(text, offset)?;
    let ident = text.get(start..end)?;

    // 1) If the cursor is already on a type token (including qualified names), resolve that.
    if let Some((dotted_start, dotted_end)) = dotted_ident_range(text, start, end) {
        if let Some(type_token) = text.get(dotted_start..dotted_end) {
            if let Some(stub) = resolve_jdk_type(jdk, text, type_token) {
                let bytes = match jdk.read_class_bytes(&stub.internal_name) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => return None,
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.lsp",
                            internal_name = %stub.internal_name,
                            err = ?err,
                            "failed to read classfile bytes"
                        );
                        return None;
                    }
                };
                let uri_string =
                    nova_decompile::decompiled_uri_for_classfile(&bytes, &stub.internal_name);
                let decompiled = match nova_decompile::decompile_classfile(&bytes) {
                    Ok(decompiled) => decompiled,
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.lsp",
                            internal_name = %stub.internal_name,
                            err = %err,
                            "failed to decompile classfile bytes"
                        );
                        return None;
                    }
                };
                let class_symbol = nova_decompile::SymbolKey::Class {
                    internal_name: stub.internal_name.clone(),
                };
                let range = decompiled.range_for(&class_symbol)?;

                let uri: lsp_types::Uri = match uri_string.parse() {
                    Ok(uri) => uri,
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.lsp",
                            uri = %uri_string,
                            err = %err,
                            "failed to parse decompiled class uri"
                        );
                        return None;
                    }
                };
                let vfs_path = VfsPath::from(&uri);

                if let VfsPath::Decompiled {
                    content_hash,
                    binary_name,
                } = &vfs_path
                {
                    if let Err(err) = state.analysis.decompiled_store.store_text(
                        content_hash,
                        binary_name,
                        &decompiled.text,
                    ) {
                        tracing::warn!(
                            target = "nova.lsp",
                            uri = %uri_string,
                            error = %err,
                            "failed to persist decompiled document"
                        );
                    }
                }
                state
                    .analysis
                    .vfs
                    .store_virtual_document(vfs_path, decompiled.text);
                state.refresh_document_memory();

                return Some(lsp_types::Location {
                    uri,
                    range: lsp_types::Range::new(
                        lsp_types::Position::new(range.start.line, range.start.character),
                        lsp_types::Position::new(range.end.line, range.end.character),
                    ),
                });
            }
        }
    }

    // 2) Cursor is on a variable identifier; try to infer its declared type (`Type name`).
    let type_name = declared_type_for_variable(text, ident, offset)?;
    let stub = resolve_jdk_type(jdk, text, &type_name)?;
    let bytes = match jdk.read_class_bytes(&stub.internal_name) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return None,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                internal_name = %stub.internal_name,
                err = ?err,
                "failed to read classfile bytes"
            );
            return None;
        }
    };

    let uri_string = nova_decompile::decompiled_uri_for_classfile(&bytes, &stub.internal_name);
    let decompiled = match nova_decompile::decompile_classfile(&bytes) {
        Ok(decompiled) => decompiled,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                internal_name = %stub.internal_name,
                err = %err,
                "failed to decompile classfile bytes"
            );
            return None;
        }
    };

    let class_symbol = nova_decompile::SymbolKey::Class {
        internal_name: stub.internal_name.clone(),
    };
    let range = decompiled.range_for(&class_symbol)?;

    let uri: lsp_types::Uri = match uri_string.parse() {
        Ok(uri) => uri,
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                uri = %uri_string,
                err = %err,
                "failed to parse decompiled class uri"
            );
            return None;
        }
    };
    let vfs_path = VfsPath::from(&uri);

    if let VfsPath::Decompiled {
        content_hash,
        binary_name,
    } = &vfs_path
    {
        if let Err(err) =
            state
                .analysis
                .decompiled_store
                .store_text(content_hash, binary_name, &decompiled.text)
        {
            tracing::warn!(
                target = "nova.lsp",
                uri = %uri_string,
                error = %err,
                "failed to persist decompiled document"
            );
        }
    }
    state
        .analysis
        .vfs
        .store_virtual_document(vfs_path, decompiled.text);
    state.refresh_document_memory();

    Some(lsp_types::Location {
        uri,
        range: lsp_types::Range::new(
            lsp_types::Position::new(range.start.line, range.start.character),
            lsp_types::Position::new(range.end.line, range.end.character),
        ),
    })
}

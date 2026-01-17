use crate::ServerState;

use nova_vfs::FileSystem;
use nova_vfs::VfsPath;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub(super) fn open_document_files(state: &ServerState) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    for file_id in state.analysis.vfs.open_documents().snapshot() {
        let Some(path) = state.analysis.vfs.path_for_id(file_id) else {
            continue;
        };
        let Some(uri) = path.to_uri() else {
            continue;
        };
        let Some(text) = state.analysis.file_contents.get(&file_id) else {
            continue;
        };
        files.insert(uri, text.as_str().to_owned());
    }
    files
}

pub(super) fn load_document_text(state: &ServerState, uri: &str) -> Option<String> {
    let path = VfsPath::uri(uri.to_string());
    let overlay = state.analysis.vfs.overlay().document_text(&path);
    if overlay.is_some() {
        return overlay;
    }

    match state.analysis.vfs.read_to_string(&path) {
        Ok(text) => Some(text),
        Err(err) => {
            tracing::debug!(
                target = "nova.lsp",
                uri,
                error = ?err,
                "failed to load document text via VFS"
            );
            None
        }
    }
}

pub(super) fn path_from_uri(uri: &str) -> Option<PathBuf> {
    match VfsPath::uri(uri.to_string()) {
        VfsPath::Local(path) => Some(path),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_from_uri_decodes_percent_encoding() {
        #[cfg(not(windows))]
        {
            let uri = "file:///tmp/My%20File.java";
            let path = path_from_uri(uri).expect("path");
            assert_eq!(path, PathBuf::from("/tmp/My File.java"));
        }

        #[cfg(windows)]
        {
            let uri = "file:///C:/tmp/My%20File.java";
            let path = path_from_uri(uri).expect("path");
            assert_eq!(path, PathBuf::from(r"C:\tmp\My File.java"));
        }
    }
}

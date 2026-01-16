use crate::stdio_paths::path_from_uri;
use crate::ServerState;

use lsp_types::Uri as LspUri;
use nova_lsp::refactor_workspace::RefactorWorkspaceSnapshot;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) struct CachedRefactorWorkspaceSnapshot {
    pub(super) project_root: PathBuf,
    pub(super) overlay_generation: u64,
    pub(super) snapshot: Arc<RefactorWorkspaceSnapshot>,
}

impl ServerState {
    pub(super) fn note_refactor_overlay_change(&mut self, uri: &str) {
        self.refactor_overlay_generation = self.refactor_overlay_generation.wrapping_add(1);

        let Some(cache) = &self.refactor_snapshot_cache else {
            return;
        };

        let Some(path) = path_from_uri(uri) else {
            self.refactor_snapshot_cache = None;
            return;
        };

        if path.starts_with(&cache.project_root) {
            self.refactor_snapshot_cache = None;
        }
    }

    pub(super) fn refactor_snapshot(
        &mut self,
        uri: &LspUri,
    ) -> Result<Arc<RefactorWorkspaceSnapshot>, String> {
        let project_root =
            RefactorWorkspaceSnapshot::project_root_for_uri(uri).map_err(|e| e.to_string())?;

        if let Some(cache) = &self.refactor_snapshot_cache {
            if cache.project_root == project_root
                && cache.overlay_generation == self.refactor_overlay_generation
                && cache.snapshot.is_disk_uptodate()
            {
                return Ok(cache.snapshot.clone());
            }
        }

        let mut overlays: HashMap<String, Arc<str>> = HashMap::new();
        for file_id in self.analysis.vfs.open_documents().snapshot() {
            let Some(path) = self.analysis.vfs.path_for_id(file_id) else {
                continue;
            };
            let Some(uri) = path.to_uri() else {
                continue;
            };
            let Some(text) = self.analysis.file_contents.get(&file_id) else {
                continue;
            };
            overlays.insert(uri, Arc::<str>::from(text.as_str()));
        }
        let snapshot =
            RefactorWorkspaceSnapshot::build(uri, &overlays).map_err(|e| e.to_string())?;
        let project_root = snapshot.project_root().to_path_buf();
        let snapshot = Arc::new(snapshot);
        self.refactor_snapshot_cache = Some(CachedRefactorWorkspaceSnapshot {
            project_root,
            overlay_generation: self.refactor_overlay_generation,
            snapshot: snapshot.clone(),
        });
        Ok(snapshot)
    }
}

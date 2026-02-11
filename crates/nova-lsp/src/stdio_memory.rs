use crate::ServerState;

pub(super) fn memory_status_payload(state: &mut ServerState) -> Result<serde_json::Value, String> {
    // Force an enforcement pass so the response reflects the current
    // pressure state and triggers evictions in registered components.
    let report = state.memory.enforce();
    let mut top_components = state.memory.report_detailed().1;
    top_components.truncate(10);
    serde_json::to_value(nova_lsp::MemoryStatusResponse {
        report,
        top_components,
    })
    .map_err(|err| crate::stdio_sanitize::sanitize_serde_json_error(&err))
}

impl ServerState {
    pub(super) fn refresh_document_memory(&mut self) {
        let total = self.analysis.vfs.estimated_bytes() as u64;
        self.documents_memory.tracker().set_bytes(total);
        self.memory.enforce();
    }
}

use crate::rpc_out::RpcOut;
use crate::ServerState;

use lsp_server::RequestId;
use lsp_types::{ApplyWorkspaceEditParams, WorkspaceEdit};

pub(super) fn send_workspace_apply_edit(
    state: &mut ServerState,
    out: &impl RpcOut,
    label: &str,
    edit: &WorkspaceEdit,
) -> Result<(), (i32, String)> {
    let id: RequestId = state.next_outgoing_id().into();

    let params = serde_json::to_value(ApplyWorkspaceEditParams {
        label: Some(label.to_string()),
        edit: edit.clone(),
    })
    .map_err(|e| (-32603, e.to_string()))?;
    out.send_request(id, "workspace/applyEdit", params)
        .map_err(|e| (-32603, e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::rpc_out::WriteRpcOut;

    #[test]
    fn send_workspace_apply_edit_emits_apply_edit_request() {
        let mut state = ServerState::new(
            nova_config::NovaConfig::default(),
            None,
            nova_memory::MemoryBudgetOverrides::default(),
        );
        let out = WriteRpcOut::new(Vec::<u8>::new());

        send_workspace_apply_edit(&mut state, &out, "Test label", &WorkspaceEdit::default())
            .expect("applyEdit request should succeed");

        let bytes = out.into_inner();
        let mut reader = std::io::BufReader::new(bytes.as_slice());
        let msg = crate::codec::read_json_message(&mut reader).expect("jsonrpc message");
        assert_eq!(
            msg.get("method").and_then(|m| m.as_str()),
            Some("workspace/applyEdit")
        );
        let params_value = msg
            .get("params")
            .cloned()
            .expect("missing applyEdit params");
        let params: ApplyWorkspaceEditParams =
            serde_json::from_value(params_value).expect("applyEdit params");
        assert_eq!(params.label.as_deref(), Some("Test label"));
    }
}

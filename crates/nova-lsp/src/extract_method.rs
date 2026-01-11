use lsp_types::{Range, Uri, WorkspaceEdit as LspWorkspaceEdit};
use nova_ide::code_action::ExtractMethodCommandArgs;
use nova_refactor::{
    extract_method::ExtractMethod, workspace_edit_to_lsp, FileId, TextDatabase,
    WorkspaceEdit as RefactorWorkspaceEdit, WorkspaceTextEdit,
};

pub fn code_action(source: &str, uri: Uri, range: Range) -> Option<lsp_types::CodeAction> {
    nova_ide::code_action::extract_method_code_action(source, uri, range)
}

pub fn execute(source: &str, args: ExtractMethodCommandArgs) -> Result<LspWorkspaceEdit, String> {
    let selection = nova_refactor::TextRange::new(
        position_to_offset(source, args.range.start).ok_or("invalid range start")?,
        position_to_offset(source, args.range.end).ok_or("invalid range end")?,
    );

    let file = args.uri.to_string();
    let refactoring = ExtractMethod {
        file: file.clone(),
        selection,
        name: args.name,
        visibility: args.visibility,
        insertion_strategy: args.insertion_strategy,
    };

    let db = TextDatabase::new([(FileId::new(file.clone()), source.to_string())]);
    let edits = refactoring.apply(source)?;
    let edit = RefactorWorkspaceEdit::new(
        edits
            .into_iter()
            .map(WorkspaceTextEdit::from)
            .collect::<Vec<_>>(),
    );
    workspace_edit_to_lsp(&db, &edit).map_err(|e| e.to_string())
}

fn position_to_offset(text: &str, pos: lsp_types::Position) -> Option<usize> {
    crate::text_pos::byte_offset(text, pos)
}

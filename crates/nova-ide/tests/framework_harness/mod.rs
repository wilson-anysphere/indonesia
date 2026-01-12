use std::path::PathBuf;
use std::sync::Arc;

use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::ProjectId;
use nova_ide::extensions::IdeExtensions;

pub const CARET: &str = crate::text_fixture::CARET;

pub use crate::text_fixture::offset_to_position;

pub struct Fixture {
    pub db: Arc<InMemoryFileStore>,
    pub ide: IdeExtensions<InMemoryFileStore>,
    pub file: nova_db::FileId,
    pub position: lsp_types::Position,
    pub text: String,
}

pub fn ide_with_default_registry(
    db: Arc<InMemoryFileStore>,
) -> IdeExtensions<InMemoryFileStore> {
    IdeExtensions::<InMemoryFileStore>::with_default_registry(
        db,
        Arc::new(NovaConfig::default()),
        ProjectId::new(0),
    )
}

pub fn fixture_multi(
    primary_path: PathBuf,
    primary_text_with_caret: &str,
    extra_files: Vec<(PathBuf, String)>,
) -> Fixture {
    let (primary_text, position) = match primary_text_with_caret.find(CARET) {
        Some(caret_offset) => {
            let primary_text = primary_text_with_caret.replace(CARET, "");
            let pos = offset_to_position(&primary_text, caret_offset);
            (primary_text, pos)
        }
        None => (
            primary_text_with_caret.to_string(),
            lsp_types::Position::new(0, 0),
        ),
    };

    let mut db = InMemoryFileStore::new();
    let primary_file = db.file_id_for_path(&primary_path);
    db.set_file_text(primary_file, primary_text.clone());
    for (path, text) in extra_files {
        let id = db.file_id_for_path(&path);
        db.set_file_text(id, text);
    }

    let db = Arc::new(db);
    let ide = ide_with_default_registry(Arc::clone(&db));

    Fixture {
        db,
        ide,
        file: primary_file,
        position,
        text: primary_text,
    }
}

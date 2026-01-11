use std::sync::Arc;

use nova_vfs::{FileSystem, VfsPath};
use nova_workspace::live::{Workspace, WorkspaceClient, WorkspaceConfig};

#[derive(Default)]
struct NoopClient;

impl WorkspaceClient for NoopClient {
    fn show_status(&self, _message: String) {}

    fn show_error(&self, _message: String) {}
}

#[test]
fn live_workspace_open_and_new_do_not_panic() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let file_path = root.join("Main.java");
    std::fs::write(&file_path, "class Main {}").expect("write");

    let client: Arc<dyn WorkspaceClient> = Arc::new(NoopClient::default());

    let ws = Workspace::open(&file_path, Arc::clone(&client)).expect("open live workspace");
    let overlay = ws.overlay_fs();
    let vfs_path = VfsPath::local(file_path.clone());
    assert!(overlay.exists(&vfs_path));
    assert_eq!(
        overlay.read_to_string(&vfs_path).expect("read"),
        "class Main {}"
    );

    let config = WorkspaceConfig::new(
        root.to_path_buf(),
        vec![root.to_path_buf()],
        Vec::new(),
        vec![root.to_path_buf()],
    );
    let ws = Workspace::new(config, client);
    let _ = ws.overlay_fs();
}

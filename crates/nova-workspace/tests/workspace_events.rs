use nova_vfs::{ContentChange, VfsPath};
use nova_workspace::{Workspace, WorkspaceEvent, WorkspaceStatus};
use tokio::time::{timeout, Duration};

#[tokio::test]
async fn diagnostics_event_on_open_and_change() {
    let workspace = Workspace::new_in_memory();
    let events = workspace.subscribe();

    let file = VfsPath::uri("file:///Main.java");
    workspace.open_document(file.clone(), "class Main { error }".to_string(), 1);

    loop {
        let ev = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("diagnostics event")
            .expect("channel open");
        if let WorkspaceEvent::DiagnosticsUpdated { file: got, diagnostics } = ev {
            if got == file {
                assert!(!diagnostics.is_empty());
                break;
            }
        }
    }

    workspace
        .apply_changes(&file, 2, &[ContentChange::full("class Main {}".to_string())])
        .unwrap();

    loop {
        let ev = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("diagnostics event")
            .expect("channel open");
        if let WorkspaceEvent::DiagnosticsUpdated { file: got, diagnostics } = ev {
            if got == file {
                assert!(diagnostics.is_empty());
                break;
            }
        }
    }
}

#[tokio::test]
async fn indexing_emits_progress_and_ready() {
    let workspace = Workspace::new_in_memory();
    let events = workspace.subscribe();

    let a = VfsPath::uri("file:///A.java");
    let b = VfsPath::uri("file:///B.java");
    workspace.open_document(a, "class A {}".into(), 1);
    workspace.open_document(b, "class B {}".into(), 1);

    workspace.trigger_indexing();

    let mut saw_progress = false;
    let mut saw_ready = false;

    while !saw_ready {
        let ev = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("event")
            .expect("channel open");

        match ev {
            WorkspaceEvent::IndexProgress(progress) => {
                saw_progress = true;
                assert!(progress.current <= progress.total);
            }
            WorkspaceEvent::Status(WorkspaceStatus::IndexingReady) => {
                saw_ready = true;
            }
            _ => {}
        }
    }

    assert!(saw_progress, "expected at least one progress update");
}

use std::fs;

use nova_db::Database as _;
use nova_memory::{MemoryBudget, MemoryManager, MemoryPressureThresholds};
use nova_vfs::VfsPath;
use nova_workspace::Workspace;

fn find_component_bytes(components: &[nova_memory::ComponentUsage], name: &str) -> Option<u64> {
    components.iter().find(|c| c.name == name).map(|c| c.bytes)
}

#[test]
fn closed_file_texts_evict_and_reload_while_open_docs_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    let open_path = root.join("Open.java");
    let closed_a_path = root.join("ClosedA.java");
    let closed_b_path = root.join("ClosedB.java");

    fs::write(&open_path, "class Open { /* disk */ }").unwrap();

    // Make closed-file texts large enough to exceed the small memory budget.
    let closed_a_text = format!("class ClosedA {{ /* {} */ }}", "a".repeat(256 * 1024));
    let closed_b_text = format!("class ClosedB {{ /* {} */ }}", "b".repeat(256 * 1024));
    fs::write(&closed_a_path, &closed_a_text).unwrap();
    fs::write(&closed_b_path, &closed_b_text).unwrap();

    // Use a small budget but custom pressure thresholds so process RSS doesn't immediately push us
    // into `Critical` (which would evict during initial workspace load). We want to observe
    // resident file texts first, then create pressure deterministically.
    let memory = MemoryManager::with_thresholds(
        MemoryBudget::from_total(2 * 1024 * 1024),
        MemoryPressureThresholds {
            medium: 1000.0,
            high: 1000.0,
            critical: 1000.0,
        },
    );
    let ws = Workspace::open_with_memory_manager(&root, memory).unwrap();

    // Open one document with an unsaved overlay; it must stay pinned across eviction.
    let overlay_text = "class Open { /* overlay */ }".to_string();
    let open_id = ws.open_document(VfsPath::local(open_path.clone()), overlay_text.clone(), 1);

    // Discover the closed file ids.
    let snap = ws.snapshot();
    let closed_a_id = snap.file_id(&closed_a_path).expect("ClosedA id");
    let closed_b_id = snap.file_id(&closed_b_path).expect("ClosedB id");
    drop(snap);

    // Ensure we're starting with resident texts for closed files.
    assert!(
        ws.debug_salsa_file_content(closed_a_id)
            .as_ref()
            .is_some_and(|text| !text.is_empty()),
        "expected ClosedA content to be resident prior to eviction"
    );
    assert_eq!(
        ws.debug_salsa_file_content(open_id)
            .expect("open doc content")
            .as_str(),
        overlay_text
    );

    let (_report_before, components_before) = ws.memory_report_detailed();
    let tracked_before = find_component_bytes(&components_before, "workspace_closed_file_texts")
        .expect("workspace_closed_file_texts component registered");
    assert!(
        tracked_before > 0,
        "expected workspace_closed_file_texts tracker to report >0 bytes"
    );

    // Create enough non-evictable pressure (open document overlay text) to force eviction of
    // closed-file texts.
    let pressure_text = format!("class Pressure {{ /* {} */ }}", "p".repeat(2 * 1024 * 1024));
    let pressure_path = VfsPath::local(root.join("Pressure.java"));
    ws.open_document(pressure_path.clone(), pressure_text, 1);

    // Trigger eviction.
    ws.enforce_memory();

    let (_report_after, components_after) = ws.memory_report_detailed();
    let tracked_after = find_component_bytes(&components_after, "workspace_closed_file_texts")
        .expect("workspace_closed_file_texts component registered");
    assert!(
        tracked_after < tracked_before,
        "expected eviction to reduce tracked closed-file text bytes"
    );

    // Closed files should be evicted (their Salsa input is replaced with an empty placeholder).
    assert_eq!(
        ws.debug_salsa_file_content(closed_a_id)
            .expect("ClosedA content")
            .as_str(),
        ""
    );
    assert_eq!(
        ws.debug_salsa_file_content(closed_b_id)
            .expect("ClosedB content")
            .as_str(),
        ""
    );

    // Open document must remain intact (unsaved overlay wins over disk).
    assert_eq!(
        ws.debug_salsa_file_content(open_id)
            .expect("open doc content")
            .as_str(),
        overlay_text
    );

    // Drop the pressure-inducing open document so the on-demand reload can "stick" without being
    // immediately re-evicted by background enforcement tasks.
    ws.close_document(&pressure_path);

    // `Workspace::snapshot()` should still surface the real on-disk contents even when the Salsa
    // input has been evicted to an empty placeholder.
    let snap_after_evict = ws.snapshot();
    assert_eq!(snap_after_evict.file_content(closed_a_id), closed_a_text.as_str());
    assert_eq!(snap_after_evict.file_content(closed_b_id), closed_b_text.as_str());

    // On-demand reload: a Salsa query that needs `file_content` should transparently reload the
    // evicted text.
    let parsed = ws.salsa_parse_java(closed_a_id);
    let parsed_len: u32 = parsed.syntax().text_range().end().into();
    assert_eq!(
        parsed_len as usize,
        closed_a_text.len(),
        "expected parse_java to run against reloaded disk contents"
    );
    assert_eq!(
        ws.debug_salsa_file_content(closed_a_id)
            .expect("ClosedA content")
            .len(),
        closed_a_text.len(),
        "expected workspace to restore file_content after on-demand reload"
    );

    // Regression: ensure the open overlay doc is not reloaded from disk during on-demand reload
    // of closed files.
    let parsed_open = ws.salsa_parse_java(open_id);
    let parsed_open_len: u32 = parsed_open.syntax().text_range().end().into();
    assert_eq!(parsed_open_len as usize, overlay_text.len());
    assert_eq!(
        ws.debug_salsa_file_content(open_id)
            .expect("open doc content")
            .as_str(),
        overlay_text
    );
}

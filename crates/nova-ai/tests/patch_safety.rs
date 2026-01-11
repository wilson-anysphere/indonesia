use nova_ai::patch::{
    JsonPatch, Patch, Position, Range, TextEdit, UnifiedDiffFile, UnifiedDiffHunk, UnifiedDiffLine,
    UnifiedDiffPatch,
};
use nova_ai::safety::{enforce_patch_safety, PatchSafetyConfig, SafetyError};
use nova_ai::workspace::VirtualWorkspace;

fn zero_range() -> Range {
    Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end: Position {
            line: 0,
            character: 0,
        },
    }
}

#[test]
fn rejects_windows_drive_backslash_paths() {
    let workspace = VirtualWorkspace::default();
    let patch = Patch::Json(JsonPatch {
        edits: vec![TextEdit {
            file: r"C:\Windows\system32\evil.java".to_string(),
            range: zero_range(),
            text: "x".to_string(),
        }],
        ops: Vec::new(),
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    match err {
        SafetyError::NonRelativePath { path } => {
            assert_eq!(path, r"C:\Windows\system32\evil.java");
        }
        other => panic!("expected NonRelativePath, got {other:?}"),
    }
}

#[test]
fn rejects_windows_drive_forwardslash_paths() {
    let workspace = VirtualWorkspace::default();
    let patch = Patch::Json(JsonPatch {
        edits: vec![TextEdit {
            file: "C:/Windows/system32/evil.java".to_string(),
            range: zero_range(),
            text: "x".to_string(),
        }],
        ops: Vec::new(),
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    match err {
        SafetyError::NonRelativePath { path } => {
            assert_eq!(path, "C:/Windows/system32/evil.java");
        }
        other => panic!("expected NonRelativePath, got {other:?}"),
    }
}

#[test]
fn rejects_unc_paths() {
    let workspace = VirtualWorkspace::default();
    let patch = Patch::Json(JsonPatch {
        edits: vec![TextEdit {
            file: r"\\server\share\evil.java".to_string(),
            range: zero_range(),
            text: "x".to_string(),
        }],
        ops: Vec::new(),
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    match err {
        SafetyError::NonRelativePath { path } => {
            assert_eq!(path, r"\\server\share\evil.java");
        }
        other => panic!("expected NonRelativePath, got {other:?}"),
    }
}

#[test]
fn rejects_parent_directory_segments() {
    let workspace = VirtualWorkspace::default();
    let patch = Patch::Json(JsonPatch {
        edits: vec![TextEdit {
            file: "foo/../bar.java".to_string(),
            range: zero_range(),
            text: "x".to_string(),
        }],
        ops: Vec::new(),
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    match err {
        SafetyError::NonRelativePath { path } => {
            assert_eq!(path, "foo/../bar.java");
        }
        other => panic!("expected NonRelativePath, got {other:?}"),
    }
}

#[test]
fn rejects_new_files_in_json_edits_by_default() {
    let workspace = VirtualWorkspace::new(vec![(
        "Existing.java".to_string(),
        "class Existing {}".to_string(),
    )]);

    let patch = Patch::Json(JsonPatch {
        edits: vec![TextEdit {
            file: "New.java".to_string(),
            range: zero_range(),
            text: "class New {}".to_string(),
        }],
        ops: Vec::new(),
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    match err {
        SafetyError::NewFileNotAllowed { file } => {
            assert_eq!(file, "New.java");
        }
        other => panic!("expected NewFileNotAllowed, got {other:?}"),
    }
}

#[test]
fn rejects_new_files_in_unified_diff_by_default() {
    let workspace = VirtualWorkspace::new(vec![(
        "Existing.java".to_string(),
        "class Existing {}".to_string(),
    )]);

    let patch = Patch::UnifiedDiff(UnifiedDiffPatch {
        files: vec![UnifiedDiffFile {
            old_path: "/dev/null".to_string(),
            new_path: "New.java".to_string(),
            hunks: vec![UnifiedDiffHunk {
                old_start: 0,
                old_len: 0,
                new_start: 1,
                new_len: 1,
                lines: vec![UnifiedDiffLine::Add("class New {}".to_string())],
            }],
        }],
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    match err {
        SafetyError::NewFileNotAllowed { file } => {
            assert_eq!(file, "New.java");
        }
        other => panic!("expected NewFileNotAllowed, got {other:?}"),
    }
}

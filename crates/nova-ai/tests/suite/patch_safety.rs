use nova_ai::patch::{
    JsonPatch, Patch, Position, Range, TextEdit, UnifiedDiffFile, UnifiedDiffHunk, UnifiedDiffLine,
    UnifiedDiffPatch,
};
use nova_ai::safety::{enforce_patch_safety, PatchSafetyConfig, SafetyError};
use nova_ai::workspace::{PatchApplyConfig, VirtualWorkspace};

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
fn rejects_windows_drive_relative_paths() {
    let workspace = VirtualWorkspace::default();
    let patch = Patch::Json(JsonPatch {
        edits: vec![TextEdit {
            file: "C:evil.java".to_string(),
            range: zero_range(),
            text: "x".to_string(),
        }],
        ops: Vec::new(),
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    match err {
        SafetyError::NonRelativePath { path } => {
            assert_eq!(path, "C:evil.java");
        }
        other => panic!("expected NonRelativePath, got {other:?}"),
    }
}

#[test]
fn rejects_backslash_separated_relative_paths() {
    let workspace = VirtualWorkspace::default();
    let patch = Patch::Json(JsonPatch {
        edits: vec![TextEdit {
            file: r"src\Main.java".to_string(),
            range: zero_range(),
            text: "x".to_string(),
        }],
        ops: Vec::new(),
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    match err {
        SafetyError::NonRelativePath { path } => {
            assert_eq!(path, r"src\Main.java");
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
fn rejects_windows_drive_forwardslash_paths_in_unified_diff() {
    let workspace = VirtualWorkspace::default();
    let patch = Patch::UnifiedDiff(UnifiedDiffPatch {
        files: vec![UnifiedDiffFile {
            old_path: "C:/Windows/system32/evil.java".to_string(),
            new_path: "C:/Windows/system32/evil.java".to_string(),
            hunks: vec![UnifiedDiffHunk {
                old_start: 0,
                old_len: 0,
                new_start: 1,
                new_len: 1,
                lines: vec![UnifiedDiffLine::Add("x".to_string())],
            }],
        }],
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    assert!(
        matches!(err, SafetyError::NonRelativePath { path } if path == "C:/Windows/system32/evil.java")
    );
}

#[test]
fn rejects_unc_paths_in_unified_diff() {
    let workspace = VirtualWorkspace::default();
    let patch = Patch::UnifiedDiff(UnifiedDiffPatch {
        files: vec![UnifiedDiffFile {
            old_path: r"\\server\share\evil.java".to_string(),
            new_path: r"\\server\share\evil.java".to_string(),
            hunks: vec![UnifiedDiffHunk {
                old_start: 0,
                old_len: 0,
                new_start: 1,
                new_len: 1,
                lines: vec![UnifiedDiffLine::Add("x".to_string())],
            }],
        }],
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    assert!(matches!(err, SafetyError::NonRelativePath { .. }));
}

#[test]
fn rejects_backslash_separated_paths_in_unified_diff() {
    let workspace = VirtualWorkspace::default();
    let patch = Patch::UnifiedDiff(UnifiedDiffPatch {
        files: vec![UnifiedDiffFile {
            old_path: r"src\Main.java".to_string(),
            new_path: r"src\Main.java".to_string(),
            hunks: vec![UnifiedDiffHunk {
                old_start: 0,
                old_len: 0,
                new_start: 1,
                new_len: 1,
                lines: vec![UnifiedDiffLine::Add("x".to_string())],
            }],
        }],
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    assert!(matches!(err, SafetyError::NonRelativePath { .. }));
}

#[test]
fn rejects_windows_device_paths() {
    let workspace = VirtualWorkspace::default();
    let patch = Patch::Json(JsonPatch {
        edits: vec![TextEdit {
            file: r"\\?\C:\Windows\system32\evil.java".to_string(),
            range: zero_range(),
            text: "x".to_string(),
        }],
        ops: Vec::new(),
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    assert!(matches!(err, SafetyError::NonRelativePath { .. }));
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
fn rejects_dot_and_empty_segments() {
    let workspace = VirtualWorkspace::default();
    let patch = Patch::Json(JsonPatch {
        edits: vec![TextEdit {
            file: "src/./Main.java".to_string(),
            range: zero_range(),
            text: "x".to_string(),
        }],
        ops: Vec::new(),
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    assert!(matches!(err, SafetyError::NonRelativePath { .. }));

    let patch = Patch::Json(JsonPatch {
        edits: vec![TextEdit {
            file: "src//Main.java".to_string(),
            range: zero_range(),
            text: "x".to_string(),
        }],
        ops: Vec::new(),
    });
    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    assert!(matches!(err, SafetyError::NonRelativePath { .. }));
}

#[test]
fn invalid_excluded_path_globs_do_not_echo_pattern_values_in_errors() {
    let secret = "super-secret-excluded-glob";
    let workspace = VirtualWorkspace::default();
    let patch = Patch::Json(JsonPatch {
        edits: Vec::new(),
        ops: Vec::new(),
    });

    let mut config = PatchSafetyConfig::default();
    config.excluded_path_globs = vec![format!("{secret}[unterminated")];

    let err = enforce_patch_safety(&patch, &workspace, &config).expect_err("expected invalid glob");
    let message = err.to_string();
    assert!(
        !message.contains(secret),
        "SafetyError should not echo raw excluded_path_globs values: {message}"
    );
    assert!(
        message.contains("excluded_path_globs[0]"),
        "SafetyError should include the failing index: {message}"
    );

    let debug = format!("{err:?}");
    assert!(
        !debug.contains(secret),
        "SafetyError debug should not echo raw excluded_path_globs values: {debug}"
    );
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

#[test]
fn allows_new_files_when_enabled() {
    let workspace = VirtualWorkspace::default();
    let patch = Patch::Json(JsonPatch {
        edits: vec![TextEdit {
            file: "New.java".to_string(),
            range: zero_range(),
            text: "class New {}".to_string(),
        }],
        ops: Vec::new(),
    });

    let mut cfg = PatchSafetyConfig::default();
    cfg.allow_new_files = true;
    enforce_patch_safety(&patch, &workspace, &cfg).expect("patch safety");

    let applied = workspace
        .apply_patch_with_config(
            &patch,
            &PatchApplyConfig {
                allow_new_files: true,
            },
        )
        .expect("apply patch");
    assert_eq!(applied.workspace.get("New.java"), Some("class New {}"));
    assert!(applied.created_files.contains("New.java"));
}

#[test]
fn allowlist_blocks_paths_outside_prefixes() {
    let workspace = VirtualWorkspace::new(vec![(
        "src/Main.java".to_string(),
        "class Main {}".to_string(),
    )]);
    let patch = Patch::Json(JsonPatch {
        edits: vec![TextEdit {
            file: "Main.java".to_string(),
            range: zero_range(),
            text: "x".to_string(),
        }],
        ops: Vec::new(),
    });

    let mut cfg = PatchSafetyConfig::default();
    cfg.allowed_path_prefixes = vec!["src/".to_string()];
    let err = enforce_patch_safety(&patch, &workspace, &cfg).unwrap_err();
    assert!(matches!(err, SafetyError::NotAllowedPath { .. }));
}

#[test]
fn rejects_file_deletes_by_default() {
    let workspace = VirtualWorkspace::new(vec![(
        "Delete.java".to_string(),
        "class Delete {}".to_string(),
    )]);

    let patch = Patch::Json(JsonPatch {
        edits: Vec::new(),
        ops: vec![nova_ai::patch::JsonPatchOp::Delete {
            file: "Delete.java".to_string(),
        }],
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    assert!(matches!(err, SafetyError::DeleteNotAllowed { file } if file == "Delete.java"));

    let patch = Patch::UnifiedDiff(UnifiedDiffPatch {
        files: vec![UnifiedDiffFile {
            old_path: "Delete.java".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: Vec::new(),
        }],
    });
    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    assert!(matches!(err, SafetyError::DeleteNotAllowed { file } if file == "Delete.java"));
}

#[test]
fn rejects_file_renames_by_default() {
    let workspace =
        VirtualWorkspace::new(vec![("Old.java".to_string(), "class Old {}".to_string())]);

    let patch = Patch::Json(JsonPatch {
        edits: Vec::new(),
        ops: vec![nova_ai::patch::JsonPatchOp::Rename {
            from: "Old.java".to_string(),
            to: "New.java".to_string(),
        }],
    });

    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    assert!(
        matches!(err, SafetyError::RenameNotAllowed { from, to } if from == "Old.java" && to == "New.java")
    );

    let patch = Patch::UnifiedDiff(UnifiedDiffPatch {
        files: vec![UnifiedDiffFile {
            old_path: "Old.java".to_string(),
            new_path: "New.java".to_string(),
            hunks: Vec::new(),
        }],
    });
    let err = enforce_patch_safety(&patch, &workspace, &PatchSafetyConfig::default()).unwrap_err();
    assert!(
        matches!(err, SafetyError::RenameNotAllowed { from, to } if from == "Old.java" && to == "New.java")
    );
}

#[test]
fn allows_file_deletes_and_renames_when_enabled() {
    let workspace = VirtualWorkspace::new(vec![
        ("Old.java".to_string(), "class Old {}".to_string()),
        ("Delete.java".to_string(), "class Delete {}".to_string()),
    ]);

    let delete = Patch::Json(JsonPatch {
        edits: Vec::new(),
        ops: vec![nova_ai::patch::JsonPatchOp::Delete {
            file: "Delete.java".to_string(),
        }],
    });
    let rename = Patch::Json(JsonPatch {
        edits: Vec::new(),
        ops: vec![nova_ai::patch::JsonPatchOp::Rename {
            from: "Old.java".to_string(),
            to: "New.java".to_string(),
        }],
    });

    let mut cfg = PatchSafetyConfig::default();
    cfg.allow_delete_files = true;
    enforce_patch_safety(&delete, &workspace, &cfg).expect("delete safety");

    cfg.allow_rename_files = true;
    enforce_patch_safety(&rename, &workspace, &cfg).expect("rename safety");
}

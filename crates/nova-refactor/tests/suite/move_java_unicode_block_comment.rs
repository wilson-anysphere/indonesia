use nova_refactor::{move_package_workspace_edit, MovePackageParams};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[test]
fn move_package_workspace_edit_does_not_panic_on_unicode_in_block_comment() {
    let files: BTreeMap<PathBuf, String> = BTreeMap::from([(
        PathBuf::from("src/main/java/com/old/A.java"),
        "/* 😀 */\npackage com.old;\n\npublic class A {}\n".to_string(),
    )]);

    move_package_workspace_edit(
        &files,
        MovePackageParams {
            old_package: "com.old".into(),
            new_package: "com.newpkg".into(),
        },
    )
    .expect("refactoring succeeds");
}

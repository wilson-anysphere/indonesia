use std::collections::BTreeMap;

use nova_refactor::{apply_workspace_edit, rename, FileId, RefactorJavaDatabase, RenameParams};

#[test]
fn rename_type_updates_inheritance_and_body_type_occurrences() {
    let base_file = FileId::new("p/Base.java");
    let derived_file = FileId::new("p/Derived.java");

    let base_src = "package p; public class Base {}\n";
    let derived_src = "package p; public class Derived extends Base implements java.io.Serializable { void m(Object o){ if(o instanceof Base b){ Base x = (Base) o; } try{ throw new Exception(); } catch (Exception e) {} } }\n";

    let db = RefactorJavaDatabase::new([
        (base_file.clone(), base_src.to_string()),
        (derived_file.clone(), derived_src.to_string()),
    ]);

    let base_offset = base_src.find("Base").expect("Base in Base.java") + 1;
    let symbol = db
        .symbol_at(&base_file, base_offset)
        .expect("symbol at Base type definition");

    let edit = rename(
        &db,
        RenameParams {
            symbol,
            new_name: "Renamed".into(),
        },
    )
    .expect("rename succeeds");

    let files = BTreeMap::from([
        (base_file.clone(), base_src.to_string()),
        (derived_file.clone(), derived_src.to_string()),
    ]);
    let updated = apply_workspace_edit(&files, &edit).expect("workspace edit applies");

    let derived_after = updated
        .get(&derived_file)
        .expect("Derived.java updated");

    assert!(
        derived_after.contains("extends Renamed"),
        "expected extends clause updated: {derived_after}"
    );
    assert!(
        derived_after.contains("instanceof Renamed b"),
        "expected instanceof/pattern updated: {derived_after}"
    );
    assert!(
        derived_after.contains("Renamed x"),
        "expected local variable type updated: {derived_after}"
    );
    assert!(
        derived_after.contains("(Renamed) o"),
        "expected cast updated: {derived_after}"
    );

    // Ensure unrelated identifiers remain unchanged.
    assert!(
        derived_after.contains("java.io.Serializable"),
        "expected implements clause to remain unchanged: {derived_after}"
    );
    assert!(
        derived_after.contains("throw new Exception()"),
        "expected thrown exception to remain unchanged: {derived_after}"
    );
    assert!(
        derived_after.contains("catch (Exception e)"),
        "expected catch type to remain unchanged: {derived_after}"
    );
}


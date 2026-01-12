use std::collections::HashSet;

use nova_types::{TypeEnv, TypeStore, MINIMAL_JDK_BINARY_NAMES};

#[test]
fn minimal_jdk_binary_names_are_in_sync_with_type_store() {
    let store = TypeStore::with_minimal_jdk();

    // The list should be free of duplicates so consumers (like the workspace loader)
    // can rely on it as an identity-map seed.
    let unique: HashSet<&str> = MINIMAL_JDK_BINARY_NAMES.iter().copied().collect();
    assert_eq!(
        unique.len(),
        MINIMAL_JDK_BINARY_NAMES.len(),
        "MINIMAL_JDK_BINARY_NAMES must not contain duplicates"
    );

    for &name in MINIMAL_JDK_BINARY_NAMES {
        assert!(
            store.lookup_class(name).is_some(),
            "TypeStore::with_minimal_jdk() is missing {name}"
        );
    }

    // Ensure the list stays an exact match for the minimal JDK types, so additions
    // to `TypeStore::with_minimal_jdk()` can't silently drift from the published list.
    let expected: HashSet<String> = MINIMAL_JDK_BINARY_NAMES.iter().map(|n| (*n).to_string()).collect();
    let actual: HashSet<String> = store
        .iter_classes()
        .map(|(_, def)| def.name.clone())
        .collect();
    assert_eq!(actual, expected);
}


/// Canonical list of JDK type binary names that are included in `JdkIndex::new()`'s built-in
/// dependency-free index.
///
/// This list is also reused by `nova-db` to seed a small set of stable `ClassId`s for core JDK
/// types without enumerating a full on-disk JDK.
///
/// The ordering is **lexicographic** to keep it deterministic.
pub const BUILTIN_JDK_BINARY_NAMES: &[&str] = &[
    // java.io
    "java.io.PrintStream",
    "java.io.Serializable",
    // java.lang
    "java.lang.Boolean",
    "java.lang.Byte",
    "java.lang.Character",
    "java.lang.Class",
    "java.lang.Cloneable",
    "java.lang.Double",
    "java.lang.Float",
    "java.lang.Integer",
    "java.lang.Iterable",
    "java.lang.Long",
    "java.lang.Math",
    "java.lang.Number",
    "java.lang.Object",
    "java.lang.Runnable",
    "java.lang.Short",
    "java.lang.String",
    "java.lang.System",
    "java.lang.Throwable",
    // java.util
    "java.util.ArrayList",
    "java.util.Collections",
    "java.util.List",
    // Keep a few nested-type examples around so resolver tests can validate
    // `Outer.Inner` → `Outer$Inner` translation without relying on an
    // on-disk JDK index.
    "java.util.Map",
    "java.util.Map$Entry",
    // java.util.function
    "java.util.function.Consumer",
    "java.util.function.Function",
    "java.util.function.Predicate",
    "java.util.function.Supplier",
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::BUILTIN_JDK_BINARY_NAMES;
    use crate::JdkIndex;

    #[test]
    fn builtin_jdk_names_are_sorted_and_unique() {
        assert!(
            BUILTIN_JDK_BINARY_NAMES.windows(2).all(|w| w[0] < w[1]),
            "expected BUILTIN_JDK_BINARY_NAMES to be strictly sorted and deduplicated"
        );
    }

    #[test]
    fn builtin_index_contains_exactly_builtin_names() {
        let index = JdkIndex::new();

        let names: Vec<&str> = index
            .iter_binary_names()
            .expect("builtin JdkIndex should enumerate names without errors")
            .collect();

        assert_eq!(
            names, BUILTIN_JDK_BINARY_NAMES,
            "expected JdkIndex::new() to populate its built-in index from BUILTIN_JDK_BINARY_NAMES"
        );

        // Also assert membership via a set so failures are easier to interpret for missing entries.
        let name_set: HashSet<&str> = names.into_iter().collect();
        for &name in BUILTIN_JDK_BINARY_NAMES {
            assert!(
                name_set.contains(name),
                "expected builtin index to contain {name}"
            );
        }
    }
}

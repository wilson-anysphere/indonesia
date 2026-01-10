//! JPA applicability detection.

/// Returns `true` when JPA is likely in use.
///
/// The upstream Nova project can look at build tooling/classpaths. In this kata
/// repo we use a lightweight heuristic:
///
/// - Known dependency coordinates contain `jakarta.persistence` or
///   `javax.persistence`
/// - Source files reference those packages
pub fn is_jpa_applicable(dependencies: &[&str], sources: &[&str]) -> bool {
    let dep_hit = dependencies.iter().any(|dep| {
        dep.contains("jakarta.persistence")
            || dep.contains("javax.persistence")
            || dep.contains("jakarta.persistence-api")
            || dep.contains("javax.persistence-api")
    });
    if dep_hit {
        return true;
    }

    sources.iter().any(|src| {
        src.contains("jakarta.persistence.")
            || src.contains("javax.persistence.")
            || src.contains("@Entity")
            || src.contains("@javax.persistence.Entity")
            || src.contains("@jakarta.persistence.Entity")
    })
}

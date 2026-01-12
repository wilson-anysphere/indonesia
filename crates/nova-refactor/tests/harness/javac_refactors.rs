// Dedicated integration test target for the `#[ignore]` suites that invoke `javac`.
//
// This keeps the stable entrypoint used by `.github/workflows/javac.yml`:
//   cargo test --locked -p nova-refactor --test javac_refactors -- --ignored

#[path = "../suite/javac_refactors.rs"]
mod javac_refactors;


// A single integration-test harness that pulls in all test modules.
//
// Keeping these as modules (instead of separate `tests/*.rs` crates) reduces the
// number of test binaries Cargo needs to compile.

#[cfg(feature = "bazel")]
#[path = "cases/bazel_model.rs"]
mod bazel_model;

#[path = "cases/discovery.rs"]
mod discovery;

#[path = "cases/jpms.rs"]
mod jpms;

#[path = "cases/real_projects.rs"]
mod real_projects;

#[path = "cases/workspace_config.rs"]
mod workspace_config;

#[path = "cases/workspace_root.rs"]
mod workspace_root;

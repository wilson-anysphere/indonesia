#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

cargo_agent() {
  bash "${ROOT_DIR}/scripts/cargo_agent.sh" "$@"
}

# Run Nova repository invariants enforced by `nova-devtools`.
#
# This is the local/dev convenience equivalent of the CI "repo invariants" step.
#
# Usage:
#   ./scripts/check-repo-invariants.sh

# Some environments configure a global rustc wrapper (commonly `sccache`) via cargo config.
# This can be flaky in multi-agent sandboxes. Mirror `scripts/cargo_agent.sh` and disable
# rustc wrappers by default for reliability; callers that want to keep them can set
# `NOVA_CARGO_KEEP_RUSTC_WRAPPER=1`.
if [[ -z "${NOVA_CARGO_KEEP_RUSTC_WRAPPER:-}" ]]; then
  export RUSTC_WRAPPER=""
  export RUSTC_WORKSPACE_WRAPPER=""
fi

# Use a template with trailing Xs for portability (BSD `mktemp` requires it).
tmp="$(mktemp -t nova-devtools-metadata.XXXXXX)"
trap 'rm -f "$tmp"' EXIT

# Generate metadata once and reuse it across all checks.
cargo_agent metadata --format-version=1 --no-deps --locked >"$tmp"

# Build once, then run the binary directly to avoid repeated `cargo run` overhead in CI.
cargo_agent build -p nova-devtools --locked

target_dir="${CARGO_TARGET_DIR:-target}"
bin="${target_dir}/debug/nova-devtools"
if [[ "${OS:-}" == "Windows_NT" ]]; then
  bin="${bin}.exe"
fi

"${bin}" check-repo-invariants --metadata-path "$tmp"

# Keep duplicated fuzz corpora in sync (Java seed inputs shared across multiple fuzz targets).
bash "${ROOT_DIR}/scripts/check-fuzz-java-corpus-sync.sh"

# Guard the crate-boundary refactor that removed the `nova-build -> nova-project` dependency.
#
# `nova-project` is the workspace loader crate; `nova-build` should consume the shared semantic
# model types from `nova-build-model` without depending on the loader.
#
# This prevents accidental reintroduction of the dependency edge (which risks dependency cycles and
# heavier builds).
nova_project_dep_pattern='^[[:space:]]*(nova-project([[:space:]]|=|\\.)|\\[[^]]*nova-project[^]]*\\])'
if git grep -n -E -- "${nova_project_dep_pattern}" -- crates/nova-build/Cargo.toml >/dev/null; then
  echo "repo invariant failed: nova-build must not depend on nova-project (use nova-build-model instead)" >&2
  git grep -n -E -- "${nova_project_dep_pattern}" -- crates/nova-build/Cargo.toml >&2
  exit 1
fi

# Guard the crate-boundary refactor that extracted project model types into `nova-build-model`.
#
# These crates are expected to depend directly on `nova-build-model` for `ProjectConfig`,
# `SourceRoot`, etc., and should not pull in the heavier `nova-project` loader crate.
model_only_crates=(
  "crates/nova-index/Cargo.toml"
  "crates/nova-resolve/Cargo.toml"
  "crates/nova-framework-spring/Cargo.toml"
)

for manifest in "${model_only_crates[@]}"; do
  if git grep -n -E -- "${nova_project_dep_pattern}" -- "${manifest}" >/dev/null; then
    crate_name="$(basename "$(dirname "${manifest}")")"
    echo "repo invariant failed: ${crate_name} must not depend on nova-project (use nova-build-model instead)" >&2
    git grep -n -E -- "${nova_project_dep_pattern}" -- "${manifest}" >&2
    exit 1
  fi
done

# Enforce the AGENTS.md integration test harness pattern for `nova-dap`.
#
# Each `tests/*.rs` file becomes a separate Cargo integration test binary, which is expensive
# under the agent RLIMIT_AS constraints. `nova-dap` intentionally consolidates its integration
# tests into a single harness, `tests/tests.rs`, which `mod`s the rest of the suite.
nova_dap_root_tests=()
while IFS= read -r file; do
  nova_dap_root_tests+=("$file")
done < <(find crates/nova-dap/tests -maxdepth 1 -name '*.rs' -print)

if [[ ${#nova_dap_root_tests[@]} -ne 1 || "${nova_dap_root_tests[0]}" != "crates/nova-dap/tests/tests.rs" ]]; then
  echo "repo invariant failed: nova-dap integration tests must be consolidated into crates/nova-dap/tests/tests.rs" >&2
  if [[ ${#nova_dap_root_tests[@]} -eq 0 ]]; then
    echo "  found: <none>" >&2
  else
    echo "  found:" >&2
    for file in "${nova_dap_root_tests[@]}"; do
      echo "    - ${file}" >&2
  done
  fi
  echo "  suggestion: move additional files into crates/nova-dap/tests/suite/ and add them to crates/nova-dap/tests/suite/mod.rs" >&2
  exit 1
fi

# Enforce the single-harness integration test layout for framework crates.
#
# These crates intentionally consolidate their integration tests into a single root harness for
# compile-time/memory efficiency (each `tests/*.rs` file is its own integration test binary).
framework_harness_checks=(
  "crates/nova-framework-spring/tests:crates/nova-framework-spring/tests/harness.rs|crates/nova-framework-spring/tests/integration.rs:move additional files into crates/nova-framework-spring/tests/suite/ and add them to crates/nova-framework-spring/tests/suite/mod.rs"
  "crates/nova-framework-builtins/tests:crates/nova-framework-builtins/tests/builtins_tests.rs:move additional files into crates/nova-framework-builtins/tests/builtins/ and add them to crates/nova-framework-builtins/tests/builtins/mod.rs"
  "crates/nova-framework-micronaut/tests:crates/nova-framework-micronaut/tests/integration_tests.rs:move additional files into crates/nova-framework-micronaut/tests/integration_tests/ and add them to crates/nova-framework-micronaut/tests/integration_tests/mod.rs"
)

for check in "${framework_harness_checks[@]}"; do
  IFS=":" read -r test_dir expected_file suggestion <<<"${check}"

  root_tests=()
  while IFS= read -r file; do
    root_tests+=("$file")
  done < <(find "${test_dir}" -maxdepth 1 -name '*.rs' -print)

  expected_ok=false
  IFS="|" read -r -a expected_files <<<"${expected_file}"
  for expected in "${expected_files[@]}"; do
    if [[ "${root_tests[0]:-}" == "${expected}" ]]; then
      expected_ok=true
      break
    fi
  done

  if [[ ${#root_tests[@]} -ne 1 || "${expected_ok}" != "true" ]]; then
    echo "repo invariant failed: integration tests in ${test_dir} must be consolidated into ${expected_file}" >&2
    if [[ ${#root_tests[@]} -eq 0 ]]; then
      echo "  found: <none>" >&2
    else
      echo "  found:" >&2
      for file in "${root_tests[@]}"; do
        echo "    - ${file}" >&2
      done
    fi
    echo "  suggestion: ${suggestion}" >&2
    exit 1
  fi
done

# Enforce the single-harness integration test layout for `nova-project`.
#
# `nova-project` has a large test surface area and its integration tests are intentionally
# consolidated into a single `tests/harness.rs` binary for compile-time/memory efficiency.
nova_project_root_tests=()
while IFS= read -r file; do
  nova_project_root_tests+=("$file")
done < <(find crates/nova-project/tests -maxdepth 1 -name '*.rs' -print)

if [[ ${#nova_project_root_tests[@]} -ne 1 || "${nova_project_root_tests[0]}" != "crates/nova-project/tests/harness.rs" ]]; then
  echo "repo invariant failed: nova-project integration tests must be consolidated into crates/nova-project/tests/harness.rs" >&2
  if [[ ${#nova_project_root_tests[@]} -eq 0 ]]; then
    echo "  found: <none>" >&2
  else
    echo "  found:" >&2
    for file in "${nova_project_root_tests[@]}"; do
      echo "    - ${file}" >&2
    done
  fi
  echo "  suggestion: move additional files into crates/nova-project/tests/cases/ and add them to crates/nova-project/tests/harness.rs" >&2
  exit 1
fi

# Enforce the `nova-types` integration test harness naming.
#
# CI and docs rely on the stable entrypoint:
#   cargo test --locked -p nova-types --test javac_differential
#
# So the harness file must remain: `crates/nova-types/tests/javac_differential.rs`.
nova_types_root_tests=()
while IFS= read -r file; do
  nova_types_root_tests+=("$file")
done < <(find crates/nova-types/tests -maxdepth 1 -name '*.rs' -print)

if [[ ${#nova_types_root_tests[@]} -ne 1 || "${nova_types_root_tests[0]}" != "crates/nova-types/tests/javac_differential.rs" ]]; then
  echo "repo invariant failed: nova-types integration tests must be consolidated into crates/nova-types/tests/javac_differential.rs" >&2
  if [[ ${#nova_types_root_tests[@]} -eq 0 ]]; then
    echo "  found: <none>" >&2
  else
    echo "  found:" >&2
    for file in "${nova_types_root_tests[@]}"; do
      echo "    - ${file}" >&2
    done
  fi
  echo "  suggestion: move additional files into crates/nova-types/tests/suite/ and add them to crates/nova-types/tests/suite/mod.rs" >&2
  exit 1
fi

# Enforce consolidated integration test harness usage in docs/scripts.
#
# After folding many per-file integration test binaries into single harnesses, the old `--test=<name>`
# entrypoints are removed (or at least deprecated). Keep docs/examples aligned with the current harness +
# filter pattern: `cargo test --locked -p <crate> --test=<harness> <filter>`.
#
# NOTE: Use `git grep` so we only check tracked files (avoids local scratch noise).
#
# These patterns are intentionally written to match *invocations* like:
#   cargo test ... --test=<name>
#   cargo test ... --test=<name> <filter>
# so we can keep the patterns in this script without self-matching.
banned_test_target_patterns=(
  # `nova-lsp` navigation tests were folded into `--test=stdio` (run with a test-name filter).
  '--test(=|[[:space:]]+)navigation([^[:alnum:]_-]|$)'
  # `nova-lsp` stdio server tests were folded into the `stdio` harness (a `[[test]]` target).
  '--test(=|[[:space:]]+)stdio_server([^[:alnum:]_-]|$)'
  # `nova-format` formatter tests are consolidated into `--test=harness`.
  '--test(=|[[:space:]]+)format_fixtures([^[:alnum:]_-]|$)'
  '--test(=|[[:space:]]+)format_snapshots([^[:alnum:]_-]|$)'
  # `nova-syntax` suites were folded into the `harness` test binary.
  '--test(=|[[:space:]]+)javac_corpus([^[:alnum:]_-]|$)'
  '--test(=|[[:space:]]+)golden_corpus([^[:alnum:]_-]|$)'
  # `nova-dap` real JVM tests are part of the consolidated `tests` harness (run with a test-name filter),
  # so there is no standalone integration test target named `real_jvm`.
  '--test(=|[[:space:]]+)real_jvm([^[:alnum:]_-]|$)'
  # `nova-cli` real-project tests are part of the consolidated `harness`.
  '--test(=|[[:space:]]+)real_projects([^[:alnum:]_-]|$)'
)

for pat in "${banned_test_target_patterns[@]}"; do
  # Exclude this script itself: the patterns are listed here intentionally.
  if git grep -n -E -- "${pat}" -- ':!scripts/check-repo-invariants.sh' >/dev/null; then
    echo "repo invariant failed: found reference to removed integration test target (${pat})" >&2
    git grep -n -E -- "${pat}" -- ':!scripts/check-repo-invariants.sh' >&2
    exit 1
  fi
done

#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

cargo_agent() {
  bash "${ROOT_DIR}/scripts/cargo_agent.sh" "$@"
}

# Ensure `scripts/cargo_agent.sh` toolchain behavior stays consistent across merges.
bash "${ROOT_DIR}/scripts/check-cargo-agent-toolchain.sh"

# Fast TOML parse check for all tracked Cargo manifests.
#
# This catches duplicate keys / invalid TOML early with a clear error message,
# before invoking Cargo (which would otherwise fail during manifest parsing).
#
# Optional: only runs when `python3` + `tomllib` are available.
if command -v python3 >/dev/null 2>&1 && python3 -c 'import tomllib' >/dev/null 2>&1; then
  python3 - <<'PY'
import subprocess
import sys
import tomllib

paths = subprocess.check_output(["git", "ls-files"], text=True).splitlines()
paths = [p for p in paths if p.endswith("Cargo.toml")]

errors = []
for path in paths:
    try:
        with open(path, "rb") as f:
            tomllib.load(f)
    except Exception as e:
        errors.append((path, e))

if errors:
    for path, err in errors:
        print(f"repo invariant failed: invalid TOML in {path}: {err}", file=sys.stderr)
    sys.exit(1)
PY
fi

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
  "crates/nova-classpath/Cargo.toml"
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

# Guard the test-architecture cleanup that removed the `nova-project -> nova-workspace`
# dev-dependency edge.
#
# `nova-project` lives in the semantic layer, while `nova-workspace` is a protocol-layer
# integration crate that pulls in `nova-ide` and many other crates. Keeping `nova-project`
# test/dev dependencies free of `nova-workspace` ensures `cargo test -p nova-project --lib`
# remains fast and isolated from higher-stack build churn.
nova_workspace_dep_pattern='^[[:space:]]*nova-workspace[[:space:]]*='
if git grep -n -E -- "${nova_workspace_dep_pattern}" -- crates/nova-project/Cargo.toml >/dev/null; then
  echo "repo invariant failed: nova-project must not depend on nova-workspace (move integration tests to nova-workspace instead)" >&2
  git grep -n -E -- "${nova_workspace_dep_pattern}" -- crates/nova-project/Cargo.toml >&2
  exit 1
fi

# Enforce the AGENTS.md integration test harness pattern for `nova-dap`.
#
# Each `tests/*.rs` file becomes a separate Cargo integration test binary, which is expensive
# under the agent RLIMIT_AS constraints. `nova-dap` intentionally consolidates its integration
# tests into a single harness, `tests/real_jvm.rs`, which `mod`s the rest of the suite.
nova_dap_root_tests=()
while IFS= read -r file; do
  nova_dap_root_tests+=("$file")
done < <(find crates/nova-dap/tests -maxdepth 1 -name '*.rs' -print | sort)

if ! printf '%s\n' "${nova_dap_root_tests[@]}" | grep -Fxq "crates/nova-dap/tests/real_jvm.rs"; then
  echo "repo invariant failed: nova-dap integration tests must include crates/nova-dap/tests/real_jvm.rs" >&2
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

if [[ ${#nova_dap_root_tests[@]} -gt 2 ]]; then
  echo "repo invariant failed: nova-dap integration tests must be capped at 2 root test binaries (tests/*.rs)" >&2
  echo "  found:" >&2
  for file in "${nova_dap_root_tests[@]}"; do
    echo "    - ${file}" >&2
  done
  echo "  suggestion: move additional files into crates/nova-dap/tests/suite/ and add them to crates/nova-dap/tests/suite/mod.rs" >&2
  exit 1
fi

# Enforce the AGENTS.md integration test harness pattern for `nova-ide`.
#
# Each `tests/*.rs` file becomes a separate Cargo integration test binary, which is expensive
# under the agent RLIMIT_AS constraints. `nova-ide` keeps its main integration suite in
# `tests/tests.rs`. A second root harness is allowed only when there's a strong reason (see
# `nova-devtools check-test-layout`, which warns at 2 and errors at >2).
nova_ide_root_tests=()
while IFS= read -r file; do
  nova_ide_root_tests+=("$file")
done < <(find crates/nova-ide/tests -maxdepth 1 -name '*.rs' -print | sort)

if ! printf '%s\n' "${nova_ide_root_tests[@]}" | grep -Fxq "crates/nova-ide/tests/tests.rs"; then
  echo "repo invariant failed: nova-ide integration tests must include crates/nova-ide/tests/tests.rs" >&2
  if [[ ${#nova_ide_root_tests[@]} -eq 0 ]]; then
    echo "  found: <none>" >&2
  else
    echo "  found:" >&2
    for file in "${nova_ide_root_tests[@]}"; do
      echo "    - ${file}" >&2
    done
  fi
  echo "  suggestion: keep tests/tests.rs as the main harness and move other tests into crates/nova-ide/tests/suite/ (add them to crates/nova-ide/tests/suite/mod.rs)" >&2
  exit 1
fi

if [[ ${#nova_ide_root_tests[@]} -gt 2 ]]; then
  echo "repo invariant failed: nova-ide integration tests must be capped at 2 root test binaries (tests/*.rs)" >&2
  echo "  found:" >&2
  for file in "${nova_ide_root_tests[@]}"; do
    echo "    - ${file}" >&2
  done
  echo "  suggestion: move additional files into crates/nova-ide/tests/suite/ and add them to crates/nova-ide/tests/suite/mod.rs" >&2
  exit 1
fi

# Ensure `nova-dap`'s real-JVM integration tests remain opt-in.
#
# These tests require external tooling (`java` + `javac`) and are only intended to run when
# explicitly enabled with `--features real-jvm-tests`.
#
# We enforce this at the source level so the tests aren't even compiled into the default
# `nova-dap` integration test binary.
nova_dap_real_jvm_test="crates/nova-dap/tests/suite/real_jvm.rs"
if [[ -f "${nova_dap_real_jvm_test}" ]]; then
  # Allow an optional leading doc comment, then require an inner `#![cfg(...)]` gate near
  # the top of the module.
  if ! head -n 10 "${nova_dap_real_jvm_test}" | grep -q -E '^#!\[cfg\(feature[[:space:]]*=[[:space:]]*"real-jvm-tests"\)\]'; then
    echo "repo invariant failed: nova-dap real JVM tests must be gated behind the real-jvm-tests feature" >&2
    echo "  expected an inner attribute like: #![cfg(feature = \"real-jvm-tests\")]" >&2
    echo "  file: ${nova_dap_real_jvm_test}" >&2
    echo "  top of file:" >&2
    head -n 10 "${nova_dap_real_jvm_test}" >&2
    exit 1
  fi
fi

# Enforce the single-harness integration test layout for framework crates.
#
# These crates intentionally consolidate their integration tests into a single root harness for
# compile-time/memory efficiency (each `tests/*.rs` file is its own integration test binary).
framework_harness_checks=(
  "crates/nova-framework-spring/tests:crates/nova-framework-spring/tests/integration.rs:move additional files into crates/nova-framework-spring/tests/suite/ and add them to crates/nova-framework-spring/tests/suite/mod.rs"
  "crates/nova-framework-builtins/tests:crates/nova-framework-builtins/tests/builtins_tests.rs:move additional files into crates/nova-framework-builtins/tests/builtins/ and add them to crates/nova-framework-builtins/tests/builtins/mod.rs"
  "crates/nova-framework-dagger/tests:crates/nova-framework-dagger/tests/integration_tests.rs:move additional files into crates/nova-framework-dagger/tests/integration_tests/ and add them to crates/nova-framework-dagger/tests/integration_tests/mod.rs"
  "crates/nova-framework-jpa/tests:crates/nova-framework-jpa/tests/integration_tests.rs:move additional files into crates/nova-framework-jpa/tests/integration_tests/ and add them to crates/nova-framework-jpa/tests/integration_tests/mod.rs"
  "crates/nova-framework-mapstruct/tests:crates/nova-framework-mapstruct/tests/integration_tests.rs:move additional files into crates/nova-framework-mapstruct/tests/integration_tests/ and add them to crates/nova-framework-mapstruct/tests/integration_tests/mod.rs"
  "crates/nova-framework-quarkus/tests:crates/nova-framework-quarkus/tests/integration.rs:move additional files into crates/nova-framework-quarkus/tests/suite/ and add them to crates/nova-framework-quarkus/tests/suite/mod.rs"
  "crates/nova-framework-micronaut/tests:crates/nova-framework-micronaut/tests/integration_tests.rs:move additional files into crates/nova-framework-micronaut/tests/integration_tests/ and add them to crates/nova-framework-micronaut/tests/integration_tests/mod.rs"
  "crates/nova-framework-web/tests:crates/nova-framework-web/tests/endpoints.rs:move additional files into crates/nova-framework-web/tests/endpoints/ and add them to crates/nova-framework-web/tests/endpoints.rs"
)

for check in "${framework_harness_checks[@]}"; do
  IFS=":" read -r test_dir expected_file suggestion <<<"${check}"

  mapfile -t root_tests < <(find "${test_dir}" -maxdepth 1 -name '*.rs' -print | sort)

  # `expected_file` can include multiple acceptable sets:
  # - alternative groups are separated by `|`
  # - within each group, multiple expected harnesses can be listed with `,`
  #
  # Examples:
  #   "tests:harness.rs:..."                               => {harness.rs}
  #   "tests:harness.rs|workspace_events.rs:..."           => {harness.rs} OR {workspace_events.rs}
  #   "tests:harness.rs,typeck.rs:..."                     => {harness.rs, typeck.rs}
  expected_ok=false

  IFS="|" read -r -a expected_groups <<<"${expected_file}"
  for group in "${expected_groups[@]}"; do
    IFS="," read -r -a expected_files <<<"${group}"
    group_ok=true
    for expected in "${expected_files[@]}"; do
      if ! printf '%s\n' "${root_tests[@]}" | grep -Fxq "${expected}"; then
        group_ok=false
        break
      fi
    done

    if [[ "${group_ok}" == "true" ]]; then
      expected_ok=true
      break
    fi
  done

  if [[ "${expected_ok}" != "true" ]]; then
    echo "repo invariant failed: integration tests in ${test_dir} must include expected harness file(s): ${expected_file}" >&2
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

  if [[ ${#root_tests[@]} -gt 2 ]]; then
    echo "repo invariant failed: integration tests in ${test_dir} must be capped at 2 root test binaries (tests/*.rs)" >&2
    echo "  found:" >&2
    for file in "${root_tests[@]}"; do
      echo "    - ${file}" >&2
    done
    echo "  suggestion: ${suggestion}" >&2
    exit 1
  fi
done

# Enforce stable integration test harness entrypoints for selected crates.
#
# Each `tests/*.rs` file becomes a separate Cargo integration test binary, which is expensive under
# the agent RLIMIT_AS constraints. These crates consolidate their integration tests into a single
# root-level harness file, and CI/docs/scripts rely on stable `--test <harness>` entrypoints.
stable_harness_checks=(
  # `nova-db` integration tests are consolidated into a single root harness binary.
  #
  # Backwards-compatible `cargo test -p nova-db --test typeck` is provided via
  # `[[test]] name = "typeck"` in `crates/nova-db/Cargo.toml` (not via an additional root
  # `tests/typeck.rs` file, which would create a second expensive integration test binary).
  "crates/nova-db/tests:crates/nova-db/tests/harness.rs:move additional files into crates/nova-db/tests/suite/ and add them to crates/nova-db/tests/suite/mod.rs"
  # `scripts/run-real-project-tests.sh` + docs invoke these by name.
  "crates/nova-project/tests:crates/nova-project/tests/harness.rs:move additional files into crates/nova-project/tests/suite/ and add them to crates/nova-project/tests/suite/mod.rs"
  "crates/nova-cli/tests:crates/nova-cli/tests/real_projects.rs:move additional files into crates/nova-cli/tests/suite/ and add them to crates/nova-cli/tests/suite/mod.rs"
  # `nova-testing` docs instruct updating fixtures by running the schema harness by name.
  "crates/nova-testing/tests:crates/nova-testing/tests/schema_json.rs:move additional files into crates/nova-testing/tests/suite/ and add them to crates/nova-testing/tests/suite/mod.rs"
  # The workspace integration tests have historically used a few harness names; allow either to
  # keep `--test <harness>` entrypoints stable across refactors.
  "crates/nova-workspace/tests:crates/nova-workspace/tests/workspace_events.rs|crates/nova-workspace/tests/harness.rs:move additional files into crates/nova-workspace/tests/suite/ and add them to crates/nova-workspace/tests/suite/mod.rs"
  "crates/nova-resolve/tests:crates/nova-resolve/tests/resolve.rs:move additional files into crates/nova-resolve/tests/suite/ and add them to crates/nova-resolve/tests/suite/mod.rs"
)

for check in "${stable_harness_checks[@]}"; do
  IFS=":" read -r test_dir expected_file suggestion <<<"${check}"

  mapfile -t root_tests < <(find "${test_dir}" -maxdepth 1 -name '*.rs' -print | sort)

  # `expected_file` can include multiple acceptable sets:
  # - alternative groups are separated by `|`
  # - within each group, multiple expected harnesses can be listed with `,`
  expected_ok=false

  IFS="|" read -r -a expected_groups <<<"${expected_file}"
  for group in "${expected_groups[@]}"; do
    IFS="," read -r -a expected_files <<<"${group}"
    group_ok=true
    for expected in "${expected_files[@]}"; do
      if ! printf '%s\n' "${root_tests[@]}" | grep -Fxq "${expected}"; then
        group_ok=false
        break
      fi
    done

    if [[ "${group_ok}" == "true" ]]; then
      expected_ok=true
      break
    fi
  done

  if [[ "${expected_ok}" != "true" ]]; then
    echo "repo invariant failed: integration tests in ${test_dir} must include expected harness file(s): ${expected_file}" >&2
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

  if [[ ${#root_tests[@]} -gt 2 ]]; then
    echo "repo invariant failed: integration tests in ${test_dir} must be capped at 2 root test binaries (tests/*.rs)" >&2
    echo "  found:" >&2
    for file in "${root_tests[@]}"; do
      echo "    - ${file}" >&2
    done
    echo "  suggestion: ${suggestion}" >&2
    exit 1
  fi
done

# Enforce the `nova-types` integration test harness naming.
#
# CI and docs rely on the stable entrypoint:
#   cargo test --locked -p nova-types --test javac_differential
#
# So the harness file must remain: `crates/nova-types/tests/javac_differential.rs`.
nova_types_root_tests=()
while IFS= read -r file; do
  nova_types_root_tests+=("$file")
done < <(find crates/nova-types/tests -maxdepth 1 -name '*.rs' -print | sort)

if ! printf '%s\n' "${nova_types_root_tests[@]}" | grep -Fxq "crates/nova-types/tests/javac_differential.rs"; then
  echo "repo invariant failed: nova-types integration tests must include crates/nova-types/tests/javac_differential.rs" >&2
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

if [[ ${#nova_types_root_tests[@]} -gt 2 ]]; then
  echo "repo invariant failed: nova-types integration tests must be capped at 2 root test binaries (tests/*.rs)" >&2
  echo "  found:" >&2
  for file in "${nova_types_root_tests[@]}"; do
    echo "    - ${file}" >&2
  done
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
  # `nova-lsp` navigation tests were folded into `--test=tests` (run with a test-name filter).
  '--test(=|[[:space:]]+)navigation([^[:alnum:]_-]|$)'
  # `nova-lsp` stdio server tests were folded into the `tests` harness.
  '--test(=|[[:space:]]+)stdio_server([^[:alnum:]_-]|$)'
  # `nova-format` formatter tests are consolidated into `--test=harness`.
  '--test(=|[[:space:]]+)format_fixtures([^[:alnum:]_-]|$)'
  '--test(=|[[:space:]]+)format_snapshots([^[:alnum:]_-]|$)'
  # `nova-syntax` suites were folded into the `harness` test binary.
  '--test(=|[[:space:]]+)javac_corpus([^[:alnum:]_-]|$)'
  '--test(=|[[:space:]]+)golden_corpus([^[:alnum:]_-]|$)'
)

for pat in "${banned_test_target_patterns[@]}"; do
  # Exclude this script itself: the patterns are listed here intentionally.
  if git grep -n -E -- "${pat}" -- ':!scripts/check-repo-invariants.sh' >/dev/null; then
    echo "repo invariant failed: found reference to removed integration test target (${pat})" >&2
    git grep -n -E -- "${pat}" -- ':!scripts/check-repo-invariants.sh' >&2
    exit 1
  fi
done

# Enforce Cargo.lock reproducibility in command examples.
#
# Any package-scoped `cargo test` invocation should include `--locked` so CI + local runs resolve
# the same dependency graph and fail fast when Cargo.lock is stale.
#
# NOTE: We intentionally keep this check narrow (only `cargo test`) to avoid false positives in
# scripts that reference other cargo subcommands in error messages.
bad_locked_test_examples="$(
  git grep -n -E -- 'cargo test[^\n]*[[:space:]](-p|--package)[[:space:]]' -- \
    ':!scripts/check-repo-invariants.sh' \
    | grep -v -- '--locked' \
    || true
)"
if [[ -n "${bad_locked_test_examples}" ]]; then
  echo "repo invariant failed: found \`cargo test\` example(s) missing \`--locked\` (CI requires \`--locked\`):" >&2
  echo "${bad_locked_test_examples}" >&2
  exit 1
fi

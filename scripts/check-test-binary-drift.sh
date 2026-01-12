#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

base_sha="${GITHUB_BASE_SHA:-${BASE_SHA:-}}"
if [[ -z "${base_sha}" ]]; then
  echo "check-test-binary-drift: GITHUB_BASE_SHA (or BASE_SHA) not set; skipping." >&2
  exit 0
fi

if ! git cat-file -e "${base_sha}^{commit}" 2>/dev/null; then
  echo "check-test-binary-drift: base commit ${base_sha} not found in the local checkout." >&2
  echo "Hint: in GitHub Actions, set actions/checkout fetch-depth: 0 (or fetch the base SHA explicitly)." >&2
  exit 2
fi

head_sha="$(git rev-parse HEAD)"

# Some workflows/docs rely on stable `cargo test --test=<name>` entrypoints. Renaming these test
# harness files breaks CI even if the overall number of test binaries stays constant.
#
# Only enforce pinning if the harness existed in the base commit (so new crates/targets are still
# allowed to introduce a first harness).
pinned_harnesses=(
  "nova-syntax:harness"
  "nova-types:javac_differential"
  "nova-refactor:tests"
  "nova-project:harness"
)

for entry in "${pinned_harnesses[@]}"; do
  crate="${entry%%:*}"
  test_name="${entry##*:}"
  test_path="crates/${crate}/tests/${test_name}.rs"

  if git cat-file -e "${base_sha}:${test_path}" 2>/dev/null; then
    if ! git cat-file -e "${head_sha}:${test_path}" 2>/dev/null; then
      cat >&2 <<EOF
ERROR: Pinned integration test harness missing.

${test_path} existed in the PR base commit but is missing in HEAD.

This file is a stable CI/docs entrypoint (cargo test --locked -p ${crate} --test=${test_name}).
Do not rename/move it; instead, keep the harness and add new tests under a subdirectory (e.g.
crates/${crate}/tests/suite/) and include them via a module.
EOF
      exit 1
    fi
  fi
done

# Only run the heavier per-crate counting if the PR touched top-level integration test files.
#
# "Top-level" means exactly: crates/<crate>/tests/<name>.rs
changed_crates="$(
  git diff --name-status "${base_sha}" "${head_sha}" |
    awk -F'\t' '
      $1 ~ /^[RC]/ { print $2; print $3; next }
      { print $2 }
    ' |
    grep -E '^crates/[^/]+/tests/[^/]+\.rs$' || true
)"

if [[ -z "${changed_crates}" ]]; then
  echo "check-test-binary-drift: no top-level crates/*/tests/*.rs changes detected." >&2
  exit 0
fi

changed_crates="$(
  echo "${changed_crates}" |
    sed -E 's|^crates/([^/]+)/tests/[^/]+\.rs$|\1|' |
    sort -u
)"

count_top_level_tests() {
  local rev="$1"
  local crate="$2"
  local tree="${rev}:crates/${crate}/tests"

  local entries
  if ! entries="$(git ls-tree --name-only "${tree}" 2>/dev/null)"; then
    echo 0
    return 0
  fi

  awk '/\.rs$/ { c++ } END { print c+0 }' <<<"${entries}"
}

violations=()
while IFS= read -r crate; do
  [[ -n "${crate}" ]] || continue

  # Only enforce drift for crates that existed in the base commit. (New crates are allowed.)
  if ! git cat-file -e "${base_sha}:crates/${crate}/Cargo.toml" 2>/dev/null; then
    continue
  fi

  base_count="$(count_top_level_tests "${base_sha}" "${crate}")"
  head_count="$(count_top_level_tests "${head_sha}" "${crate}")"

  allowed="${base_count}"
  # Special case: crates with no top-level harnesses in the base commit may add exactly one.
  if [[ "${base_count}" -eq 0 ]]; then
    allowed=1
  fi

  if [[ "${head_count}" -gt "${allowed}" ]]; then
    violations+=("${crate}: base=${base_count}, head=${head_count} (allowed ≤ ${allowed})")
  fi
done <<<"${changed_crates}"

if (( ${#violations[@]} > 0 )); then
  cat >&2 <<'EOF'
ERROR: Integration test binary drift detected.

This PR increases the number of *top-level* integration test binaries for one or more existing crates.

Top-level means exactly:
  crates/<crate>/tests/*.rs

Each .rs file directly under tests/ becomes a separate test binary, which slows builds and increases CI
memory pressure. Instead, extend an existing harness and put new tests in module files under a
subdirectory (e.g. tests/<harness>/...).

See: AGENTS.md → "Test Organization"
EOF

  printf '\nViolations:\n' >&2
  printf '  - %s\n' "${violations[@]}" >&2

  printf '\n' >&2
  exit 1
fi

echo "check-test-binary-drift: ok" >&2

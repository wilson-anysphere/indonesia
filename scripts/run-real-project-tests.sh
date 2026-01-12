#!/usr/bin/env bash
set -euo pipefail

# Convenience wrapper for the ignored "real project" integration tests.
#
# By default it clones all fixtures and runs all ignored tests:
#   ./scripts/run-real-project-tests.sh
#
# To focus on a subset of fixtures/tests:
#   ./scripts/run-real-project-tests.sh --only guava,spring-petclinic
#   # or:
#   NOVA_TEST_PROJECTS=guava,spring-petclinic ./scripts/run-real-project-tests.sh
#   # or (alias):
#   NOVA_REAL_PROJECT=guava,spring-petclinic ./scripts/run-real-project-tests.sh

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "${ROOT_DIR}"

usage() {
  cat <<'EOF'
Usage: ./scripts/run-real-project-tests.sh [--only <csv>]

Runs the ignored real-project tests against fixture repos under `test-projects/`.

Options:
  --only <csv>   Run tests only for the given fixtures (comma-separated).

Environment:
  NOVA_TEST_PROJECTS  Same as --only (comma-separated).
  NOVA_REAL_PROJECT   Alias for NOVA_TEST_PROJECTS.
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

ONLY_CSV=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --only)
      shift
      [[ $# -gt 0 ]] || die "--only requires a comma-separated value"
      ONLY_CSV="$1"
      shift
      ;;
    --only=*)
      ONLY_CSV="${1#--only=}"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      die "unknown argument: $1"
      ;;
  esac
done

ENV_ONLY=""
if [[ -n "${NOVA_TEST_PROJECTS:-}" ]]; then
  ENV_ONLY="${NOVA_TEST_PROJECTS//[[:space:]]/}"
fi
if [[ -n "${NOVA_REAL_PROJECT:-}" ]]; then
  real_only="${NOVA_REAL_PROJECT//[[:space:]]/}"
  if [[ -n "${ENV_ONLY}" && "${real_only}" != "${ENV_ONLY}" ]]; then
    die "both NOVA_TEST_PROJECTS and NOVA_REAL_PROJECT are set but differ; choose one"
  fi
  ENV_ONLY="${ENV_ONLY:-${real_only}}"
fi

if [[ -n "${ENV_ONLY}" ]]; then
  [[ -z "${ONLY_CSV}" ]] || die "both --only and NOVA_TEST_PROJECTS/NOVA_REAL_PROJECT are set; choose one"
  ONLY_CSV="${ENV_ONLY}"
fi

declare -a ONLY_PROJECTS=()
if [[ -n "${ONLY_CSV}" ]]; then
  ONLY_CSV="${ONLY_CSV//[[:space:]]/}"
  IFS=',' read -r -a ONLY_PROJECTS <<<"${ONLY_CSV}"

  declare -a filtered=()
  for project in "${ONLY_PROJECTS[@]}"; do
    [[ -n "${project}" ]] && filtered+=("${project}")
  done
  ONLY_PROJECTS=("${filtered[@]}")

  [[ ${#ONLY_PROJECTS[@]} -gt 0 ]] || die "--only/NOVA_TEST_PROJECTS/NOVA_REAL_PROJECT resolved to an empty list"
fi

if [[ ${#ONLY_PROJECTS[@]} -gt 0 ]]; then
  # Always pass the selection via --only to avoid surprising interactions if
  # `NOVA_TEST_PROJECTS`/`NOVA_REAL_PROJECT` is set in the environment.
  NOVA_TEST_PROJECTS= NOVA_REAL_PROJECT= ./scripts/clone-test-projects.sh --only "${ONLY_CSV}"
else
  ./scripts/clone-test-projects.sh
fi

echo "==> Running ignored real-project tests"

failures=0

run_test() {
  # With `set -e` enabled, wrap in `if ! ...` so failures don't abort the script;
  # we want to run both nova-workspace and nova-cli suites and report all failures.
  if ! "$@"; then
    failures=1
  fi
}

if [[ ${#ONLY_PROJECTS[@]} -eq 0 ]]; then
  run_test NOVA_TEST_PROJECTS= NOVA_REAL_PROJECT= bash ./scripts/cargo_agent.sh test --locked -p nova-workspace --test workspace_events -- --ignored
  run_test NOVA_TEST_PROJECTS= NOVA_REAL_PROJECT= bash ./scripts/cargo_agent.sh test --locked -p nova-cli --test real_projects -- --ignored
else
  # Pass fixture selection via environment variables; the individual tests will skip
  # fixtures not included in the list.
  run_test NOVA_TEST_PROJECTS="${ONLY_CSV}" NOVA_REAL_PROJECT= bash ./scripts/cargo_agent.sh test --locked -p nova-workspace --test workspace_events -- --ignored
  run_test NOVA_TEST_PROJECTS="${ONLY_CSV}" NOVA_REAL_PROJECT= bash ./scripts/cargo_agent.sh test --locked -p nova-cli --test real_projects -- --ignored
fi

if [[ $failures -ne 0 ]]; then
  echo "==> Real-project tests FAILED" >&2
  exit 1
fi

echo "==> Done"

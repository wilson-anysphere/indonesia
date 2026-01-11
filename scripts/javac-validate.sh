#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_PROJECTS_DIR="${ROOT_DIR}/test-projects"

usage() {
  cat <<'EOF'
Usage: ./scripts/javac-validate.sh [--only <csv>]

Best-effort helper to compile fixture projects with their build toolchain.

Options:
  --only <csv>   Compile only the named fixtures (comma-separated).

Environment:
  NOVA_TEST_PROJECTS  Same as --only (comma-separated). If both are provided,
                      the script exits with an error.
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

if [[ -n "${NOVA_TEST_PROJECTS:-}" ]]; then
  [[ -z "${ONLY_CSV}" ]] || die "both --only and NOVA_TEST_PROJECTS are set; choose one"
  ONLY_CSV="${NOVA_TEST_PROJECTS}"
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

  [[ ${#ONLY_PROJECTS[@]} -gt 0 ]] || die "--only/NOVA_TEST_PROJECTS resolved to an empty list"
fi

contains() {
  local needle="$1"
  shift
  local item
  for item in "$@"; do
    [[ "${item}" == "${needle}" ]] && return 0
  done
  return 1
}

is_selected() {
  local name="$1"
  [[ ${#ONLY_PROJECTS[@]} -eq 0 ]] && return 0
  contains "${name}" "${ONLY_PROJECTS[@]}"
}

run_maven_compile() {
  local name="$1"
  shift || true
  local extra_args=("$@")
  local dir="${TEST_PROJECTS_DIR}/${name}"

  if [[ ! -d "${dir}" ]]; then
    echo "Missing ${dir}; run ./scripts/clone-test-projects.sh first" >&2
    return 1
  fi

  echo "==> Building ${name} (best-effort)"
  (
    cd "${dir}"
    if [[ -x "./mvnw" ]]; then
      ./mvnw -q -DskipTests "${extra_args[@]}" compile
    elif command -v mvn >/dev/null 2>&1; then
      mvn -q -DskipTests "${extra_args[@]}" compile
    else
      echo "No mvn/mvnw found; skipping ${name}" >&2
    fi
  )
}

if [[ ${#ONLY_PROJECTS[@]} -eq 0 ]]; then
  run_maven_compile "spring-petclinic"
  # Guava's full multi-module build can be sensitive to local JDK/Maven versions.
  # For a lightweight "javac sanity" check we build only the main `guava` module.
  run_maven_compile "guava" -pl guava -am
else
  is_selected "spring-petclinic" && run_maven_compile "spring-petclinic"
  is_selected "guava" && run_maven_compile "guava" -pl guava -am
  is_selected "maven-resolver" && run_maven_compile "maven-resolver"
fi

echo "==> Done"

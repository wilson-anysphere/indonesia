#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_PROJECTS_DIR="${ROOT_DIR}/test-projects"
PINS_FILE="${TEST_PROJECTS_DIR}/pins.toml"

mkdir -p "${TEST_PROJECTS_DIR}"

usage() {
  cat <<'EOF'
Usage: ./scripts/clone-test-projects.sh [--only <csv>]

Clone (or update) local-only fixture repos under `test-projects/` based on
`test-projects/pins.toml`.

Options:
  --only <csv>   Clone/update only the named fixtures (comma-separated).
                 Example: --only guava,spring-petclinic

Environment:
  NOVA_TEST_PROJECTS  Same as --only (comma-separated). If both are provided,
                      the script exits with an error.
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
  # Split on commas and trim whitespace.
  ONLY_CSV="${ONLY_CSV//[[:space:]]/}"
  IFS=',' read -r -a ONLY_PROJECTS <<<"${ONLY_CSV}"

  # Filter out empty entries (e.g., trailing commas).
  declare -a filtered=()
  for project in "${ONLY_PROJECTS[@]}"; do
    [[ -n "${project}" ]] && filtered+=("${project}")
  done
  ONLY_PROJECTS=("${filtered[@]}")

  [[ ${#ONLY_PROJECTS[@]} -gt 0 ]] || die "--only/NOVA_TEST_PROJECTS/NOVA_REAL_PROJECT resolved to an empty list"
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

parse_pins() {
  if command -v python3 >/dev/null 2>&1 && python3 -c 'import tomllib' >/dev/null 2>&1; then
    python3 - "${PINS_FILE}" <<'PY'
import sys
import tomllib
from pathlib import Path

path = Path(sys.argv[1])
with path.open("rb") as f:
    data = tomllib.load(f)

for name, cfg in data.items():
    if not isinstance(cfg, dict):
        continue
    url = cfg.get("url")
    rev = cfg.get("rev")
    if not url or not rev:
        raise SystemExit(f"error: {path}: section [{name}] must have url and rev")
    print(f"{name}\t{url}\t{rev}")
PY
    return
  fi

  # POSIX awk fallback (no TOML edge-case support; sufficient for our simple pins file).
  awk '
  function ltrim(s) { sub(/^[ \t\r\n]+/, "", s); return s }
  function rtrim(s) { sub(/[ \t\r\n]+$/, "", s); return s }
  function trim(s) { return rtrim(ltrim(s)) }
  function emit() {
    if (section == "") return
    if (url == "" || rev == "") {
      printf("error: %s: section [%s] missing url or rev\n", FILENAME, section) > "/dev/stderr"
      exit 1
    }
    print section "\t" url "\t" rev
  }
  {
    line = $0
    sub(/#.*/, "", line)
    line = trim(line)
    if (line == "") next

    if (line ~ /^\[[^]]+\]$/) {
      emit()
      section = line
      sub(/^\[/, "", section)
      sub(/\]$/, "", section)
      url = ""
      rev = ""
      next
    }

    if (section == "") next

    if (line ~ /^url[ \t]*=/) {
      val = line
      sub(/^url[ \t]*=[ \t]*/, "", val)
      if (val !~ /^".*"$/) {
        printf("error: %s: expected url = \"...\" in section [%s]\n", FILENAME, section) > "/dev/stderr"
        exit 1
      }
      sub(/^"/, "", val)
      sub(/"$/, "", val)
      url = val
      next
    }

    if (line ~ /^rev[ \t]*=/) {
      val = line
      sub(/^rev[ \t]*=[ \t]*/, "", val)
      if (val !~ /^".*"$/) {
        printf("error: %s: expected rev = \"...\" in section [%s]\n", FILENAME, section) > "/dev/stderr"
        exit 1
      }
      sub(/^"/, "", val)
      sub(/"$/, "", val)
      rev = val
      next
    }
  }
  END { emit() }
  ' "${PINS_FILE}"
}

clone_or_update() {
  local name="$1"
  local url="$2"
  local rev="$3"

  local dir="${TEST_PROJECTS_DIR}/${name}"

  if [[ ! -d "${dir}/.git" ]]; then
    echo "==> Cloning ${name}"
    # Use a shallow clone + blob filtering to keep fixture downloads reasonable.
    # The pinned revision is fetched explicitly below.
    git clone --filter=blob:none --depth 1 "${url}" "${dir}"
  fi

  echo "==> Checking out ${name} @ ${rev}"
  (
    cd "${dir}"
    git fetch --prune origin || true
    # Best-effort: fetch only the requested rev (works for SHAs; tag support is optional).
    git fetch --depth 1 origin "${rev}"
    git checkout --detach FETCH_HEAD
  )
}

[[ -f "${PINS_FILE}" ]] || die "missing pins file: ${PINS_FILE}"

PINS_DATA="$(parse_pins)"
[[ -n "${PINS_DATA}" ]] || die "no fixtures found in ${PINS_FILE}"

declare -a AVAILABLE_PROJECTS=()
while IFS=$'\t' read -r name url rev; do
  AVAILABLE_PROJECTS+=("${name}")
done <<<"${PINS_DATA}"

if [[ ${#ONLY_PROJECTS[@]} -gt 0 ]]; then
  declare -a unknown=()
  for requested in "${ONLY_PROJECTS[@]}"; do
    contains "${requested}" "${AVAILABLE_PROJECTS[@]}" || unknown+=("${requested}")
  done

  if [[ ${#unknown[@]} -gt 0 ]]; then
    echo "error: unknown fixture(s): ${unknown[*]}" >&2
    echo "available fixtures: ${AVAILABLE_PROJECTS[*]}" >&2
    exit 1
  fi

  echo "==> Cloning selected fixtures: ${ONLY_PROJECTS[*]}"
fi

while IFS=$'\t' read -r name url rev; do
  if is_selected "${name}"; then
    clone_or_update "${name}" "${url}" "${rev}"
  fi
done <<<"${PINS_DATA}"

echo "==> Done"

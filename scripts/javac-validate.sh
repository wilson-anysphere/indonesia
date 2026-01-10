#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_PROJECTS_DIR="${ROOT_DIR}/test-projects"

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

run_maven_compile "spring-petclinic"
# Guava's full multi-module build can be sensitive to local JDK/Maven versions.
# For a lightweight "javac sanity" check we build only the main `guava` module.
run_maven_compile "guava" -pl guava -am

echo "==> Done"

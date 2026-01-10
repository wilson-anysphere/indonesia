#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_PROJECTS_DIR="${ROOT_DIR}/test-projects"

mkdir -p "${TEST_PROJECTS_DIR}"

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

# Pinned revisions live in `test-projects/pins.toml`.
clone_or_update \
  "spring-petclinic" \
  "https://github.com/spring-projects/spring-petclinic.git" \
  "ab1d5364a0a49d260b52bea2bfdfe804d8a36f7a"

clone_or_update \
  "guava" \
  "https://github.com/google/guava.git" \
  "8868c096cfdabbe38170b6e395369c315cfb72a1"

clone_or_update \
  "maven-resolver" \
  "https://github.com/apache/maven-resolver.git" \
  "13e91652bbebdb33ad3df81d22f568eeb4e66e34"

echo "==> Done"

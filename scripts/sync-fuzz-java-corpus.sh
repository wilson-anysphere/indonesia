#!/usr/bin/env bash
set -euo pipefail

# Mirrors the tracked Java seed corpus from `fuzz_syntax_parse` into `fuzz_format`.
# See `docs/fuzzing.md` for rationale and policy.

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -z "${repo_root}" ]]; then
  echo "error: must be run from within a git repository" >&2
  exit 2
fi

cd "${repo_root}"

src="fuzz/corpus/fuzz_syntax_parse"
dst="fuzz/corpus/fuzz_format"

if [[ ! -d "${src}" ]]; then
  echo "error: missing source corpus directory: ${src}" >&2
  exit 2
fi
if [[ ! -d "${dst}" ]]; then
  echo "error: missing destination corpus directory: ${dst}" >&2
  exit 2
fi

# Use templates with trailing Xs for portability (BSD `mktemp` requires it).
src_list="$(mktemp -t fuzz-java-corpus-src.XXXXXX)"
dst_list="$(mktemp -t fuzz-java-corpus-dst.XXXXXX)"
trap 'rm -f "${src_list}" "${dst_list}"' EXIT

git ls-files -- "${src}" |
  awk -v dir="${src}" '/\.java$/ { sub("^" dir "/", ""); print }' |
  LC_ALL=C sort >"${src_list}"

git ls-files -- "${dst}" |
  awk -v dir="${dst}" '/\.java$/ { sub("^" dir "/", ""); print }' |
  LC_ALL=C sort >"${dst_list}"

echo "Syncing Java corpus:"
echo "  source:      ${src}"
echo "  destination: ${dst}"
echo

# Remove any tracked Java seeds in the destination that no longer exist in the source.
comm -13 "${src_list}" "${dst_list}" | while IFS= read -r rel; do
  echo "Removing: ${dst}/${rel}"
  rm -f "${dst}/${rel}"
done

# Copy (or overwrite) all tracked Java seeds from source -> destination.
while IFS= read -r rel; do
  mkdir -p "$(dirname "${dst}/${rel}")"
  cp "${src}/${rel}" "${dst}/${rel}"
done <"${src_list}"

echo
bash scripts/check-fuzz-java-corpus-sync.sh


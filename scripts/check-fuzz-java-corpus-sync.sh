#!/usr/bin/env bash
set -euo pipefail

# Ensures that the checked-in Java seed corpora for `fuzz_syntax_parse` and `fuzz_format`
# stay identical (same filenames + same contents). These two corpora are duplicated on
# purpose so both fuzz targets start from the same set of real-world Java snippets.

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -z "${repo_root}" ]]; then
  echo "error: must be run from within a git repository" >&2
  exit 2
fi

cd "${repo_root}"

corpus_a="fuzz/corpus/fuzz_syntax_parse"
corpus_b="fuzz/corpus/fuzz_format"

if [[ ! -d "${corpus_a}" ]]; then
  echo "error: missing corpus directory: ${corpus_a}" >&2
  exit 2
fi
if [[ ! -d "${corpus_b}" ]]; then
  echo "error: missing corpus directory: ${corpus_b}" >&2
  exit 2
fi

corpus_java_hashes() {
  local corpus_dir="$1"

  # We intentionally compare *tracked* files to avoid noisy failures due to local
  # untracked scratch inputs.
  git ls-files -- "${corpus_dir}" |
    awk -v dir="${corpus_dir}" '
      /\.java$/ {
        sub("^" dir "/", "")
        print
      }
    ' |
    LC_ALL=C sort |
    while IFS= read -r rel; do
      printf '%s  %s\n' "$(git hash-object "${corpus_dir}/${rel}")" "${rel}"
    done
}

# Use a template with trailing Xs for portability (BSD `mktemp` requires it).
diff_tmp="$(mktemp -t fuzz-java-corpus-sync.XXXXXX)"
trap 'rm -f "${diff_tmp}"' EXIT

if diff -u <(corpus_java_hashes "${corpus_a}") <(corpus_java_hashes "${corpus_b}") >"${diff_tmp}"; then
  echo "OK: Java fuzz seed corpora are in sync (${corpus_a} == ${corpus_b})"
  exit 0
fi

echo "ERROR: Java fuzz seed corpora are out of sync:" >&2
echo "  - ${corpus_a}" >&2
echo "  - ${corpus_b}" >&2
echo >&2
echo "The diff below compares '<git hash-object>  <relative-path>' for tracked *.java files:" >&2
echo >&2
cat "${diff_tmp}" >&2
echo >&2
echo "To re-sync, run: bash scripts/sync-fuzz-java-corpus.sh" >&2
exit 1


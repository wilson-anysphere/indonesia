#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

warning_line="cargo_agent: ignoring RUSTUP_TOOLCHAIN in favor of rust-toolchain.toml"

# Extract the pinned toolchain version so this check stays in sync with `rust-toolchain.toml`.
# Expected format (see rust-toolchain.toml):
#   channel = "1.92.0"
pinned_toolchain="$(
  sed -n -E 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' rust-toolchain.toml | head -n 1
)"

if [[ -z "${pinned_toolchain}" ]]; then
  echo "check-cargo-agent-toolchain: unable to read pinned toolchain from rust-toolchain.toml" >&2
  exit 1
fi

tmp_dir="$(mktemp -d -t cargo-agent-toolchain.XXXXXX)"
trap 'rm -rf "${tmp_dir}"' EXIT

# Provide a tiny external cargo subcommand so we can observe what environment `cargo_agent.sh`
# passes through to the `cargo` process (and therefore to subcommands).
cat >"${tmp_dir}/cargo-print-rustup-toolchain" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${RUSTUP_TOOLCHAIN+x}" ]]; then
  echo "RUSTUP_TOOLCHAIN=${RUSTUP_TOOLCHAIN}"
else
  echo "RUSTUP_TOOLCHAIN is unset"
fi
SH
chmod +x "${tmp_dir}/cargo-print-rustup-toolchain"

run_and_capture() {
  local out_file="$1"
  local err_file="$2"
  shift 2

  set +e
  "$@" >"${out_file}" 2>"${err_file}"
  local status=$?
  set -e
  return "${status}"
}

dump_failure() {
  local context="$1"
  local out_file="$2"
  local err_file="$3"

  echo "check-cargo-agent-toolchain: ${context}" >&2
  echo "---- stdout ----" >&2
  cat "${out_file}" >&2 || true
  echo "---- stderr ----" >&2
  cat "${err_file}" >&2 || true
}

assert_warning_count() {
  local err_file="$1"
  local expected="$2"

  local count
  count="$(grep -Fxc "${warning_line}" "${err_file}" 2>/dev/null || true)"
  if [[ "${count}" -ne "${expected}" ]]; then
    echo "check-cargo-agent-toolchain: expected ${expected} toolchain warning(s), found ${count}" >&2
    echo "---- stderr ----" >&2
    cat "${err_file}" >&2
    exit 1
  fi
}

assert_stdout_eq() {
  local out_file="$1"
  local expected="$2"

  local actual
  actual="$(cat "${out_file}")"
  if [[ "${actual}" != "${expected}" ]]; then
    echo "check-cargo-agent-toolchain: unexpected stdout" >&2
    echo "  expected: ${expected}" >&2
    echo "  actual:   ${actual}" >&2
    exit 1
  fi
}

out="$(mktemp -t cargo-agent-toolchain-out.XXXXXX)"
err="$(mktemp -t cargo-agent-toolchain-err.XXXXXX)"

# -----------------------------------------------------------------------------
# Default behavior: ignore ambient `RUSTUP_TOOLCHAIN` so rustup honors
# `rust-toolchain.toml`, and emit a warning exactly once.
# -----------------------------------------------------------------------------
if PATH="${tmp_dir}:${PATH}" \
  RUSTUP_TOOLCHAIN="__nova_invalid_toolchain__" \
  run_and_capture "${out}" "${err}" bash scripts/cargo_agent.sh print-rustup-toolchain
then
  :
else
  dump_failure "default invocation failed (expected success)" "${out}" "${err}"
  exit 1
fi

assert_warning_count "${err}" 1
assert_stdout_eq "${out}" "RUSTUP_TOOLCHAIN is unset"

# -----------------------------------------------------------------------------
# Escape hatch: keep `RUSTUP_TOOLCHAIN` when explicitly requested, with no warning.
# -----------------------------------------------------------------------------
if PATH="${tmp_dir}:${PATH}" \
  RUSTUP_TOOLCHAIN="${pinned_toolchain}" \
  NOVA_CARGO_KEEP_RUSTUP_TOOLCHAIN=1 \
  run_and_capture "${out}" "${err}" bash scripts/cargo_agent.sh print-rustup-toolchain
then
  :
else
  dump_failure "keep invocation failed (expected success)" "${out}" "${err}"
  exit 1
fi

assert_warning_count "${err}" 0
assert_stdout_eq "${out}" "RUSTUP_TOOLCHAIN=${pinned_toolchain}"

# -----------------------------------------------------------------------------
# Explicit override: when the caller passes `+<toolchain>` we must not touch
# `RUSTUP_TOOLCHAIN` and must not print warnings.
# -----------------------------------------------------------------------------
if PATH="${tmp_dir}:${PATH}" \
  RUSTUP_TOOLCHAIN="__nova_invalid_toolchain__" \
  run_and_capture "${out}" "${err}" bash scripts/cargo_agent.sh "+${pinned_toolchain}" print-rustup-toolchain
then
  :
else
  dump_failure "explicit +toolchain invocation failed (expected success)" "${out}" "${err}"
  exit 1
fi

assert_warning_count "${err}" 0
assert_stdout_eq "${out}" "RUSTUP_TOOLCHAIN=__nova_invalid_toolchain__"

rm -f "${out}" "${err}"

#!/usr/bin/env bash
set -euo pipefail

# High-throughput cargo wrapper for multi-agent hosts.
#
# Goals:
# - Maximize utilization on big machines (many cores)
# - Avoid cargo/rustc stampedes when many agents run commands concurrently
# - Enforce a per-command RAM ceiling via RLIMIT_AS
#
# Usage:
#   scripts/cargo_agent.sh build --release
#   scripts/cargo_agent.sh check -p nova-syntax
#   scripts/cargo_agent.sh test -p nova-core --lib
#   scripts/cargo_agent.sh test -p nova-types --test harness -- --ignored
#
# Tuning knobs (env vars):
#   NOVA_CARGO_SLOTS        Max concurrent cargo commands (default: auto from CPU)
#   NOVA_CARGO_JOBS         cargo build jobs per command (default: cargo's default)
#   NOVA_CARGO_LIMIT_AS     Address-space cap (default: 4G)
#   NOVA_CARGO_LOCK_DIR     Lock directory (default: target/.cargo_agent_locks)
#   NOVA_RUST_TEST_THREADS  Default RUST_TEST_THREADS for cargo test (default: min(nproc, 32))

usage() {
  cat <<'EOF'
usage: scripts/cargo_agent.sh <cargo-subcommand> [args...]

Examples:
  scripts/cargo_agent.sh check -p nova-syntax --quiet
  scripts/cargo_agent.sh build --release
  scripts/cargo_agent.sh test -p nova-core --lib
  scripts/cargo_agent.sh test -p nova-types --test harness -- --ignored

Environment:
  NOVA_CARGO_SLOTS        Max concurrent cargo commands (default: auto)
  NOVA_CARGO_JOBS         cargo build jobs (default: cargo's default)
  NOVA_CARGO_LIMIT_AS     Address-space cap (default: 4G)
  NOVA_CARGO_LOCK_DIR     Lock directory
  NOVA_RUST_TEST_THREADS  RUST_TEST_THREADS for cargo test (default: min(nproc, 32))
  NOVA_CARGO_ALLOW_UNSCOPED_TEST=1  Allow running unscoped `cargo test` (not recommended)

Notes:
  - This wrapper enforces RAM caps via RLIMIT_AS (through scripts/run_limited.sh).
  - Set NOVA_CARGO_LIMIT_AS=unlimited to disable the cap.
  - `cargo test` is blocked unless scoped with `-p/--package` or `--manifest-path`.
    To override (rare): NOVA_CARGO_ALLOW_UNSCOPED_TEST=1 scripts/cargo_agent.sh test ...
  - For faster iteration, further scope tests with --lib, --bin <name>, or --test <name>.
EOF
}

deny_unscoped_cargo_test() {
  # Guardrail: block unscoped `cargo test` by default.
  #
  # Agent rules prohibit running workspace-wide `cargo test` because it can lead to huge builds and
  # OOMs under the RLIMIT_AS ceiling. We enforce the simplest safe rule here: require an explicit
  # package selector (-p/--package) or a manifest path (--manifest-path).
  #
  # Anything after `--` is forwarded to the test binary and must NOT be considered for scoping.
  if [[ "${NOVA_CARGO_ALLOW_UNSCOPED_TEST:-}" == "1" ]]; then
    return 0
  fi

  local args=("$@")
  local idx=0
  if [[ "${#args[@]}" -lt 1 ]]; then
    return 0
  fi
  if [[ "${args[0]}" == +* ]]; then
    idx=1
  fi
  if [[ "${#args[@]}" -le "${idx}" ]]; then
    return 0
  fi

  local subcommand="${args[${idx}]}"
  if [[ "${subcommand}" != "test" ]]; then
    return 0
  fi

  local has_scope_selector=""
  local arg
  local i
  for ((i = idx + 1; i < ${#args[@]}; i++)); do
    arg="${args[${i}]}"
    if [[ "${arg}" == "--" ]]; then
      break
    fi
    case "${arg}" in
      -p|--package|--manifest-path)
        has_scope_selector=1
        break
        ;;
      -p?*)
        has_scope_selector=1
        break
        ;;
      --package=*|--manifest-path=*)
        has_scope_selector=1
        break
        ;;
    esac
  done

  if [[ -z "${has_scope_selector}" ]]; then
    cat >&2 <<'EOF'
error: refusing to run unscoped `cargo test` via scripts/cargo_agent.sh

This repository's agent rules prohibit workspace-wide test runs because they can trigger huge builds
and OOM under the memory cap.

Re-run with an explicit scope selector:
  -p <crate> / --package <crate>
  --manifest-path <path>

Example:
  scripts/cargo_agent.sh test -p nova-testing --lib

To override (rare):
  NOVA_CARGO_ALLOW_UNSCOPED_TEST=1 scripts/cargo_agent.sh test [...]
EOF
    return 2
  fi
}

if [[ $# -lt 1 ]]; then
  usage
  exit 2
fi

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

deny_unscoped_cargo_test "$@"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Some environments (including multi-agent sandboxes) ship with a global Cargo config that enables
# `sccache` as a `rustc` wrapper. When the daemon isn't available this causes all builds/tests to
# fail at `sccache rustc -vV`.
#
# Prefer correctness/reliability here: allow callers to opt back in by explicitly setting
# `RUSTC_WRAPPER` in their environment, but default to no wrapper.
export RUSTC_WRAPPER="${RUSTC_WRAPPER:-}"

# Get CPU count
nproc="${NOVA_CARGO_NPROC:-}"
if [[ -z "${nproc}" ]]; then
  if command -v nproc >/dev/null 2>&1; then
    nproc="$(nproc 2>/dev/null || true)"
  fi
  if ! [[ "${nproc}" =~ ^[0-9]+$ ]] || [[ "${nproc}" -lt 1 ]]; then
    nproc="$(getconf _NPROCESSORS_ONLN 2>/dev/null || true)"
  fi
  if ! [[ "${nproc}" =~ ^[0-9]+$ ]] || [[ "${nproc}" -lt 1 ]]; then
    if command -v sysctl >/dev/null 2>&1; then
      nproc="$(sysctl -n hw.logicalcpu 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || true)"
    fi
  fi
  if ! [[ "${nproc}" =~ ^[0-9]+$ ]] || [[ "${nproc}" -lt 1 ]]; then
    nproc="${NUMBER_OF_PROCESSORS:-1}"
  fi
  if ! [[ "${nproc}" =~ ^[0-9]+$ ]] || [[ "${nproc}" -lt 1 ]]; then
    nproc=1
  fi
fi

# Default slots: keep concurrency low since each command uses many cores
slots="${NOVA_CARGO_SLOTS:-}"
if [[ -z "${slots}" ]]; then
  # ~1 concurrent cargo per 48 hw threads (clamped)
  slots=$(( nproc / 48 ))
  if [[ "${slots}" -lt 1 ]]; then slots=1; fi
  if [[ "${slots}" -gt 8 ]]; then slots=8; fi
fi

jobs="${NOVA_CARGO_JOBS:-}"
if [[ -n "${jobs}" ]]; then
  if ! [[ "${jobs}" =~ ^[0-9]+$ ]] || [[ "${jobs}" -lt 1 ]]; then
    echo "invalid NOVA_CARGO_JOBS: ${jobs}" >&2
    exit 2
  fi
fi

limit_as="${NOVA_CARGO_LIMIT_AS:-${LIMIT_AS:-4G}}"

lock_dir="${NOVA_CARGO_LOCK_DIR:-${repo_root}/target/.cargo_agent_locks}"
mkdir -p "${lock_dir}"

run_cargo() {
  local cargo_cmd=(cargo)
  local toolchain_arg=""
  local subcommand=""

  if [[ $# -lt 1 ]]; then
    echo "missing cargo subcommand" >&2
    return 2
  fi

  # When running under the default 4G RLIMIT_AS ceiling, large crates (notably `nova-lsp`)
  # can hit link-time OOM with lld. Both GNU ld and lld support `--no-keep-memory`, which
  # trades some additional disk I/O for lower peak address-space usage during linking.
  #
  # Only enable this under Linux and only when an address-space cap is active; this keeps
  # local/dev builds fast while making constrained CI/agent builds reliable.
  if [[ "$(uname -s)" == "Linux" ]] \
    && [[ -n "${limit_as}" && "${limit_as}" != "0" && "${limit_as}" != "off" && "${limit_as}" != "unlimited" ]] \
    && [[ -z "${NOVA_CARGO_NO_LINK_NO_KEEP_MEMORY:-}" ]]
  then
    if ! [[ "${RUSTFLAGS:-}" =~ no-keep-memory ]]; then
      export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,--no-keep-memory"
    fi
  fi

  # Handle toolchain spec (+nightly, etc.)
  if [[ "$1" == +* ]]; then
    toolchain_arg="$1"
    cargo_cmd+=("$1")
    shift
    if [[ $# -lt 1 ]]; then
      echo "missing cargo subcommand after toolchain spec" >&2
      return 2
    fi
  fi

  subcommand="$1"
  cargo_cmd+=("${subcommand}")
  shift

  # Cap RUST_TEST_THREADS for test runs
  if [[ "${subcommand}" == "test" && -z "${RUST_TEST_THREADS:-}" ]]; then
    local rust_test_threads="${NOVA_RUST_TEST_THREADS:-}"
    if [[ -z "${rust_test_threads}" ]]; then
      rust_test_threads=$(( nproc < 32 ? nproc : 32 ))
    fi
    export RUST_TEST_THREADS="${rust_test_threads}"
  fi

  # On multi-agent hosts we apply strict RLIMIT_AS ceilings. Some environments
  # also configure a global rustc wrapper (commonly `sccache`) via cargo config.
  # This can crash in low address-space environments and cause failures like:
  #   `sccache rustc -vV` -> "memory allocation ... failed"
  #
  # Default to disabling any rustc wrapper for reliability. Consumers that want
  # to opt back in can set `NOVA_CARGO_KEEP_RUSTC_WRAPPER=1`.
  if [[ -z "${NOVA_CARGO_KEEP_RUSTC_WRAPPER:-}" ]]; then
    export RUSTC_WRAPPER=""
    export RUSTC_WORKSPACE_WRAPPER=""
  fi

  if [[ -n "${jobs}" ]]; then
    cargo_cmd+=(-j "${jobs}")
  fi

  cargo_cmd+=("$@")

  if [[ -z "${limit_as}" || "${limit_as}" == "0" || "${limit_as}" == "off" || "${limit_as}" == "unlimited" ]]; then
    "${cargo_cmd[@]}"
    return $?
  fi

  bash "${repo_root}/scripts/run_limited.sh" --as "${limit_as}" -- "${cargo_cmd[@]}"
  return $?
}

# Skip slot acquisition if already in a slot (nested invocation)
if [[ -n "${NOVA_CARGO_SLOT:-}" ]]; then
  jobs_label="${jobs:-auto}"
  echo "cargo_agent: nested slot=${NOVA_CARGO_SLOT} jobs=${jobs_label} as=${limit_as}" >&2
  run_cargo "$@"
  exit $?
fi

# Check for flock
if ! command -v flock >/dev/null 2>&1; then
  echo "warning: flock not available; running cargo without slot throttling" >&2
  run_cargo "$@"
  exit $?
fi

# Test flock works
exec 198>&2
exec 2>/dev/null
if ! exec 199>"${lock_dir}/.flock_probe.lock"; then
  exec 2>&198
  exec 198>&-
  echo "warning: unable to open flock probe lock; running cargo without slot throttling" >&2
  run_cargo "$@"
  exit $?
fi
exec 2>&198
exec 198>&-
if ! flock -n 199 >/dev/null 2>&1; then
  echo "warning: flock appears unusable; running cargo without slot throttling" >&2
  exec 199>&- || true
  run_cargo "$@"
  exit $?
fi
exec 199>&-

acquire_slot() {
  local i k start lockfile fd
  start=$(( ($$ + RANDOM) % slots ))
  for ((k = 0; k < slots; k++)); do
    i=$(( (start + k) % slots ))
    lockfile="${lock_dir}/slot.${i}.lock"
    fd=$((200 + i))
    eval "exec ${fd}>\"${lockfile}\"" || continue
    if flock -n "${fd}"; then
      echo "${fd}:${i}"
      return 0
    fi
    eval "exec ${fd}>&-" || true
  done
  return 1
}

slot=""
while [[ -z "${slot}" ]]; do
  if s="$(acquire_slot)"; then
    slot="${s}"
    break
  fi
  sleep 0.1
done

slot_fd="${slot%%:*}"
slot_idx="${slot#*:}"
export NOVA_CARGO_SLOT="${slot_idx}"

jobs_label="${jobs:-auto}"
echo "cargo_agent: slot=${slot_idx}/${slots} jobs=${jobs_label} as=${limit_as}" >&2

set +e
run_cargo "$@"
status=$?
set -e

# Release lock
eval "exec ${slot_fd}>&-" || true
exit "${status}"

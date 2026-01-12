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
#   bash scripts/cargo_agent.sh build --locked --release
#   bash scripts/cargo_agent.sh check --locked -p nova-syntax
#   bash scripts/cargo_agent.sh test --locked -p nova-core --lib
#   bash scripts/cargo_agent.sh test --locked -p nova-types --test javac_differential -- --ignored
#
# Tuning knobs (env vars):
#   NOVA_CARGO_SLOTS        Max concurrent cargo commands (default: auto from CPU)
#   NOVA_CARGO_JOBS         cargo build jobs per command (default: cargo's default)
#   NOVA_CARGO_LIMIT_AS     Address-space cap (default: 4G)
#   NOVA_CARGO_LOCK_DIR     Lock directory (default: target/.cargo_agent_locks)
#   NOVA_RUST_TEST_THREADS  Default RUST_TEST_THREADS for cargo test (default: min(nproc, 8))
#   NOVA_CARGO_ALLOW_UNSCOPED_TEST=1  Allow unscoped `cargo test` (not recommended)

usage() {
  cat <<'EOF'
usage: bash scripts/cargo_agent.sh <cargo-subcommand> [args...]

Examples:
  bash scripts/cargo_agent.sh check --locked -p nova-syntax --quiet
  bash scripts/cargo_agent.sh build --locked --release
  bash scripts/cargo_agent.sh test --locked -p nova-core --lib
  bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_fixtures
  bash scripts/cargo_agent.sh test --locked -p nova-types --test javac_differential -- --ignored

Environment:
  NOVA_CARGO_SLOTS        Max concurrent cargo commands (default: auto)
  NOVA_CARGO_JOBS         cargo build jobs (default: cargo's default)
  NOVA_CARGO_LIMIT_AS     Address-space cap (default: 4G)
  NOVA_CARGO_LOCK_DIR     Lock directory
  NOVA_RUST_TEST_THREADS  RUST_TEST_THREADS for cargo test (default: min(nproc, 8))
  NOVA_CARGO_ALLOW_UNSCOPED_TEST=1  Allow running unscoped `cargo test` (not recommended)

Notes:
  - This wrapper enforces RAM caps via RLIMIT_AS (through scripts/run_limited.sh).
  - Set NOVA_CARGO_LIMIT_AS=unlimited to disable the cap.
  - `cargo test` is blocked unless scoped with `-p/--package` or `--manifest-path`.
    To override (rare): set `NOVA_CARGO_ALLOW_UNSCOPED_TEST=1` and re-run.
  - For faster iteration, further scope tests with --lib, --bin <name>, or --test=<name>.
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
  bash scripts/cargo_agent.sh test --locked -p nova-testing --lib

To override (rare): set `NOVA_CARGO_ALLOW_UNSCOPED_TEST=1` and re-run.
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

limit_as="${NOVA_CARGO_LIMIT_AS:-${LIMIT_AS:-4G}}"
limit_as_is_default=1
if [[ -n "${NOVA_CARGO_LIMIT_AS:-}" || -n "${LIMIT_AS:-}" ]]; then
  limit_as_is_default=0
fi

jobs="${NOVA_CARGO_JOBS:-}"
if [[ -z "${jobs}" ]]; then
  # Cargo defaults its job count to the detected CPU count. On large machines this can spawn a lot
  # of `rustc` processes (each with many helper threads), which can exceed container / CI process
  # limits even when system memory is plentiful.
  #
  # When we're running under an address-space cap (the default `scripts/cargo_agent.sh` mode), cap
  # `-j` to keep builds reliable in constrained environments. Consumers that want higher parallelism
  # can opt back in by setting `NOVA_CARGO_JOBS`.
  if [[ -n "${limit_as}" && "${limit_as}" != "0" && "${limit_as}" != "off" && "${limit_as}" != "unlimited" ]]; then
    jobs=$(( nproc < 32 ? nproc : 32 ))
  fi
fi

if [[ -n "${jobs}" ]]; then
  if ! [[ "${jobs}" =~ ^[0-9]+$ ]] || [[ "${jobs}" -lt 1 ]]; then
    echo "invalid NOVA_CARGO_JOBS: ${jobs}" >&2
    exit 2
  fi
fi

lock_dir="${NOVA_CARGO_LOCK_DIR:-${repo_root}/target/.cargo_agent_locks}"
mkdir -p "${lock_dir}"

run_cargo() {
  local cargo_cmd=(cargo)
  local toolchain_arg=""
  local subcommand=""
  local limit_as_effective="${limit_as}"

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

  # `cargo fuzz` builds and runs targets under ASAN by default. AddressSanitizer requires a very
  # large virtual address space to reserve its shadow memory, so applying a low RLIMIT_AS (our
  # default is 4G) causes the fuzzer to fail at startup with errors like:
  #
  #   AddressSanitizer failed to allocate ... ReserveShadowMemoryRange failed ...
  #
  # Prefer correctness here: if the user didn't explicitly set a tighter address-space limit,
  # disable RLIMIT_AS for fuzzing commands.
  if [[ "${subcommand}" == "fuzz" && "${limit_as_is_default}" -eq 1 ]]; then
    limit_as_effective="unlimited"
  fi

  # Cap RUST_TEST_THREADS for test runs
  if [[ "${subcommand}" == "test" && -z "${RUST_TEST_THREADS:-}" ]]; then
    local rust_test_threads="${NOVA_RUST_TEST_THREADS:-}"
    if [[ -z "${rust_test_threads}" ]]; then
      # Keep test parallelism conservative by default. In multi-agent sandboxes we can be constrained
      # by per-user thread limits (`EAGAIN`), and tests often spawn additional threads internally.
      rust_test_threads=$(( nproc < 8 ? nproc : 8 ))
    fi
    export RUST_TEST_THREADS="${rust_test_threads}"
  fi

  # `cargo fuzz run` defaults to AddressSanitizer, which reserves a huge virtual
  # address range for its shadow memory. Under the default RLIMIT_AS cap enforced
  # by this wrapper, ASAN cannot reserve that shadow memory and crashes before the
  # fuzz target even starts executing.
  #
  # To keep fuzzing usable in constrained multi-agent environments, default to
  # `-s none` when an address-space cap is active *unless* the caller explicitly
  # selected a sanitizer.
  if [[ "${subcommand}" == "fuzz" ]] \
    && [[ -n "${limit_as_effective}" && "${limit_as_effective}" != "0" && "${limit_as_effective}" != "off" && "${limit_as_effective}" != "unlimited" ]] \
    && [[ $# -ge 1 ]]
  then
    local fuzz_subcommand="$1"
    case "${fuzz_subcommand}" in
      run|r|cmin|tmin|coverage|cov)
        local has_sanitizer=""
        local arg
        for arg in "$@"; do
          if [[ "${arg}" == "-s" || "${arg}" == "--sanitizer" || "${arg}" == --sanitizer=* ]]; then
            has_sanitizer=1
            break
          fi
        done
        if [[ -z "${has_sanitizer}" ]]; then
          set -- "${fuzz_subcommand}" -s none "${@:2}"
        fi
        ;;
    esac
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
    # `-j/--jobs` is only valid for cargo's built-in build-like subcommands. Many
    # external `cargo-*` subcommands (e.g. `cargo fmt`, `cargo fuzz`) do NOT accept
    # `-j` and will fail with "unexpected argument '-j'".
    #
    # For `cargo fuzz`, we still want to cap the number of Rust compilation jobs
    # (it invokes `cargo build` internally). Cargo supports `build.jobs` via the
    # `CARGO_BUILD_JOBS` env var, and cargo-fuzz forwards the environment to those
    # nested cargo invocations.
    case "${subcommand}" in
      generate-lockfile|metadata|fmt)
        ;;
      fuzz)
        export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-${jobs}}"
        ;;
      *)
        cargo_cmd+=(-j "${jobs}")
        ;;
    esac
  fi

  cargo_cmd+=("$@")

  if [[ -z "${limit_as_effective}" || "${limit_as_effective}" == "0" || "${limit_as_effective}" == "off" || "${limit_as_effective}" == "unlimited" ]]; then
    "${cargo_cmd[@]}"
    return $?
  fi

  bash "${repo_root}/scripts/run_limited.sh" --as "${limit_as_effective}" -- "${cargo_cmd[@]}"
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

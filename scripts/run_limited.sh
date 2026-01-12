#!/usr/bin/env bash
set -euo pipefail

# Run any command under OS-enforced resource limits.
#
# Prefer `prlimit` when available (hard limits). Fall back to `ulimit` otherwise.
#
# Examples:
#   scripts/run_limited.sh --as 4G -- cargo build --locked --release
#   LIMIT_AS=8G scripts/run_limited.sh -- ./memory-hungry-task

usage() {
  cat <<'EOF'
usage: scripts/run_limited.sh [--as <size>] [--cpu <secs>] [--stack <size>] -- <command...>

Limits:
  --as <size>     Address-space (virtual memory) limit. Example: 4G, 8192M.
  --cpu <secs>    CPU time limit (seconds).
  --stack <size>  Stack size limit.

Environment defaults (optional):
  LIMIT_AS, LIMIT_CPU, LIMIT_STACK

Notes:
  - `--as` is the most reliable "hard memory ceiling" on Linux.
  - If `prlimit` is missing, we fall back to `ulimit`.
  - Size strings: 4G, 4096M, 4194304K, or raw bytes.
EOF
}

# Convert size string to KiB (for ulimit)
to_kib() {
  local raw="${1:-}"
  raw="${raw//[[:space:]]/}"
  raw="$(printf '%s' "${raw}" | tr '[:upper:]' '[:lower:]')"

  # Strip optional suffixes
  raw="${raw%ib}"
  raw="${raw%b}"

  if [[ "${raw}" =~ ^[0-9]+$ ]]; then
    # Bare number = MiB (human-friendly default)
    echo $((raw * 1024))
    return 0
  fi

  if [[ "${raw}" =~ ^([0-9]+)([kmgt])$ ]]; then
    local n="${BASH_REMATCH[1]}"
    local unit="${BASH_REMATCH[2]}"
    case "${unit}" in
      k) echo $((n)) ;;
      m) echo $((n * 1024)) ;;
      g) echo $((n * 1024 * 1024)) ;;
      t) echo $((n * 1024 * 1024 * 1024)) ;;
      *) return 1 ;;
    esac
    return 0
  fi

  return 1
}

to_bytes() {
  local kib
  kib="$(to_kib "${1:-}")" || return 1
  echo $((kib * 1024))
}

# Defaults
AS="${LIMIT_AS:-4G}"
CPU="${LIMIT_CPU:-}"
STACK="${LIMIT_STACK:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --as)
      AS="${2:-}"; shift 2 ;;
    --cpu)
      CPU="${2:-}"; shift 2 ;;
    --stack)
      STACK="${2:-}"; shift 2 ;;
    --no-as)
      AS=""; shift ;;
    --no-cpu)
      CPU=""; shift ;;
    --no-stack)
      STACK=""; shift ;;
    --)
      shift
      break
      ;;
    *)
      break
      ;;
  esac
done

if [[ $# -lt 1 ]]; then
  usage
  exit 2
fi

cmd=("$@")

# Check if any limits are set
any_limit=false
if [[ -n "${AS}" && "${AS}" != "0" && "${AS}" != "unlimited" ]]; then any_limit=true; fi
if [[ -n "${CPU}" && "${CPU}" != "0" && "${CPU}" != "unlimited" ]]; then any_limit=true; fi
if [[ -n "${STACK}" && "${STACK}" != "0" && "${STACK}" != "unlimited" ]]; then any_limit=true; fi

if [[ "${any_limit}" == "false" ]]; then
  exec "${cmd[@]}"
fi

# Resolve cargo shim to actual binary (rustup shims reserve lots of address space)
if [[ -n "${AS}" && "${AS}" != "0" && "${AS}" != "unlimited" ]] \
  && [[ "${cmd[0]}" == "cargo" ]] \
  && command -v rustup >/dev/null 2>&1
then
  cargo_shim="$(command -v cargo || true)"
  if [[ -n "${cargo_shim}" ]]; then
    cargo_target="${cargo_shim}"
    if [[ -L "${cargo_shim}" ]]; then
      cargo_target="$(readlink "${cargo_shim}" 2>/dev/null || echo "${cargo_shim}")"
    fi

    if [[ "${cargo_target}" == "rustup" || "${cargo_target}" == */rustup || "${cargo_shim}" == */.cargo/bin/cargo ]]; then
      toolchain=""
      if [[ ${#cmd[@]} -gt 1 && "${cmd[1]}" == +* ]]; then
        toolchain="${cmd[1]#+}"
        cmd=("${cmd[0]}" "${cmd[@]:2}")
      fi

      if [[ -n "${toolchain}" ]]; then
        resolved="$(rustup which --toolchain "${toolchain}" cargo 2>/dev/null || true)"
      else
        resolved="$(rustup which cargo 2>/dev/null || true)"
      fi

      if [[ -n "${resolved}" ]]; then
        cmd[0]="${resolved}"
        toolchain_bin="$(dirname "${resolved}")"
        export PATH="${toolchain_bin}:${PATH}"
      fi
    fi
  fi
fi

# Try prlimit first (more reliable)
prlimit_ok=0
if command -v prlimit >/dev/null 2>&1; then
  # Test that prlimit works (some builds are broken)
  if prlimit --as=67108864 --cpu=1 -- true >/dev/null 2>&1; then
    prlimit_ok=1
  fi
fi

if [[ "${prlimit_ok}" -eq 1 ]]; then
  pl=(prlimit --pid $$)
  
  if [[ -n "${AS}" && "${AS}" != "0" ]]; then
    if [[ "${AS}" == "unlimited" ]]; then
      pl+=(--as=unlimited)
    else
      as_bytes="$(to_bytes "${AS}")" || {
        echo "invalid --as size: ${AS}" >&2
        exit 2
      }
      pl+=(--as="${as_bytes}")
    fi
  fi
  
  if [[ -n "${CPU}" && "${CPU}" != "0" ]]; then
    if [[ "${CPU}" == "unlimited" ]]; then
      pl+=(--cpu=unlimited)
    else
      if ! [[ "${CPU}" =~ ^[0-9]+$ ]]; then
        echo "invalid --cpu seconds: ${CPU}" >&2
        exit 2
      fi
      pl+=(--cpu="${CPU}")
    fi
  fi
  
  if [[ -n "${STACK}" && "${STACK}" != "0" ]]; then
    if [[ "${STACK}" == "unlimited" ]]; then
      pl+=(--stack=unlimited)
    else
      stack_bytes="$(to_bytes "${STACK}")" || {
        echo "invalid --stack size: ${STACK}" >&2
        exit 2
      }
      pl+=(--stack="${stack_bytes}")
    fi
  fi

  if "${pl[@]}" >/dev/null 2>&1; then
    exec "${cmd[@]}"
  fi
fi

# Fallback: ulimit
if [[ -n "${AS}" && "${AS}" != "0" ]]; then
  if [[ "${AS}" == "unlimited" ]]; then
    ulimit -v unlimited
  else
    as_kib="$(to_kib "${AS}")" || {
      echo "invalid --as size: ${AS}" >&2
      exit 2
    }
    ulimit -v "${as_kib}"
  fi
fi

if [[ -n "${STACK}" && "${STACK}" != "0" ]]; then
  if [[ "${STACK}" == "unlimited" ]]; then
    ulimit -s unlimited
  else
    stack_kib="$(to_kib "${STACK}")" || {
      echo "invalid --stack size: ${STACK}" >&2
      exit 2
    }
    ulimit -s "${stack_kib}"
  fi
fi

if [[ -n "${CPU}" && "${CPU}" != "0" ]]; then
  if [[ "${CPU}" == "unlimited" ]]; then
    ulimit -t unlimited
  else
    if ! [[ "${CPU}" =~ ^[0-9]+$ ]]; then
      echo "invalid --cpu seconds: ${CPU}" >&2
      exit 2
    fi
    ulimit -t "${CPU}"
  fi
fi

exec "${cmd[@]}"

use crate::types::MemoryBreakdown;
use serde::{Deserialize, Serialize};

/// One mebibyte in bytes.
pub const MB: u64 = 1024 * 1024;

/// One gibibyte in bytes.
pub const GB: u64 = 1024 * MB;

/// Environment variable used to override the total memory budget.
pub const ENV_MEMORY_BUDGET_TOTAL: &str = "NOVA_MEMORY_BUDGET_TOTAL";
/// Environment variable used to override the query cache category budget.
pub const ENV_MEMORY_BUDGET_QUERY_CACHE: &str = "NOVA_MEMORY_BUDGET_QUERY_CACHE";
/// Environment variable used to override the syntax trees category budget.
pub const ENV_MEMORY_BUDGET_SYNTAX_TREES: &str = "NOVA_MEMORY_BUDGET_SYNTAX_TREES";
/// Environment variable used to override the indexes category budget.
pub const ENV_MEMORY_BUDGET_INDEXES: &str = "NOVA_MEMORY_BUDGET_INDEXES";
/// Environment variable used to override the type info category budget.
pub const ENV_MEMORY_BUDGET_TYPE_INFO: &str = "NOVA_MEMORY_BUDGET_TYPE_INFO";
/// Environment variable used to override the "other" category budget.
pub const ENV_MEMORY_BUDGET_OTHER: &str = "NOVA_MEMORY_BUDGET_OTHER";

/// A memory budget for Nova.
///
/// The budget is split into coarse categories; individual components register
/// their usage under one of them. Enforcement is best-effort and cooperative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudget {
    /// Total memory budget in bytes.
    pub total: u64,
    /// Per-category budgets in bytes.
    pub categories: MemoryBreakdown,
}

/// Optional overrides for [`MemoryBudget`], intended to be populated by a
/// configuration layer (`nova-config`) and/or environment variables.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBudgetOverrides {
    /// Override total budget. When set, category defaults are derived from this.
    pub total: Option<u64>,
    /// Override individual category budgets.
    pub categories: MemoryBreakdownOverrides,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryBreakdownOverrides {
    pub query_cache: Option<u64>,
    pub syntax_trees: Option<u64>,
    pub indexes: Option<u64>,
    pub type_info: Option<u64>,
    pub other: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseByteSizeError {
    Empty,
    InvalidNumber,
    UnknownUnit(String),
    Overflow,
}

impl std::fmt::Display for ParseByteSizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty size string"),
            Self::InvalidNumber => write!(f, "invalid number"),
            Self::UnknownUnit(unit) => write!(f, "unknown unit: {unit}"),
            Self::Overflow => write!(f, "size overflows u64"),
        }
    }
}

impl std::error::Error for ParseByteSizeError {}

/// Parse a byte size from either a raw byte count or a human-friendly suffix.
///
/// Supported formats:
/// - raw bytes: `"1048576"`
/// - binary suffixes (case-insensitive): `"512K"`, `"512M"`, `"2G"`, `"1T"`
/// - optional `"B"` or `"iB"` suffix: `"512MB"`, `"1GiB"`
///
/// Notes:
/// - This parser is intentionally strict: it only accepts integer values.
/// - The suffixes are interpreted as binary multiples (KiB/MiB/GiB/TiB).
pub fn parse_byte_size(input: &str) -> Result<u64, ParseByteSizeError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(ParseByteSizeError::Empty);
    }

    // TOML numbers can include `_` separators, and users often copy/paste those.
    let normalized: String = raw.chars().filter(|c| *c != '_').collect();
    let normalized = normalized.trim();
    if normalized.is_empty() {
        return Err(ParseByteSizeError::Empty);
    }

    let mut split_idx = None;
    for (idx, ch) in normalized.char_indices() {
        if !ch.is_ascii_digit() {
            split_idx = Some(idx);
            break;
        }
    }

    let (num_str, suffix) = match split_idx {
        Some(idx) => (&normalized[..idx], normalized[idx..].trim()),
        None => (normalized, ""),
    };

    let value: u64 = num_str
        .parse()
        .map_err(|_| ParseByteSizeError::InvalidNumber)?;
    let suffix = suffix.to_ascii_lowercase();
    let suffix = suffix.as_str();

    let (multiplier, consumed) = match suffix {
        "" | "b" => (1u64, true),
        "k" | "kb" | "kib" => (1024u64, true),
        "m" | "mb" | "mib" => (1024u64.pow(2), true),
        "g" | "gb" | "gib" => (1024u64.pow(3), true),
        "t" | "tb" | "tib" => (1024u64.pow(4), true),
        _ => (1u64, false),
    };

    if !consumed {
        return Err(ParseByteSizeError::UnknownUnit(suffix.to_string()));
    }

    value
        .checked_mul(multiplier)
        .ok_or(ParseByteSizeError::Overflow)
}

impl MemoryBudgetOverrides {
    /// Read memory budget overrides from environment variables.
    ///
    /// Any invalid values are ignored (treated as "unset").
    pub fn from_env() -> Self {
        fn read(name: &str) -> Option<u64> {
            let value = std::env::var_os(name)?;
            let value = value.to_string_lossy();
            let value = value.trim();
            if value.is_empty() {
                return None;
            }
            match parse_byte_size(value) {
                Ok(value) => Some(value),
                Err(err) => {
                    tracing::debug!(
                        target = "nova.memory",
                        name,
                        value,
                        error = %err,
                        "invalid memory budget env override; ignoring"
                    );
                    None
                }
            }
        }

        Self {
            total: read(ENV_MEMORY_BUDGET_TOTAL),
            categories: MemoryBreakdownOverrides {
                query_cache: read(ENV_MEMORY_BUDGET_QUERY_CACHE),
                syntax_trees: read(ENV_MEMORY_BUDGET_SYNTAX_TREES),
                indexes: read(ENV_MEMORY_BUDGET_INDEXES),
                type_info: read(ENV_MEMORY_BUDGET_TYPE_INFO),
                other: read(ENV_MEMORY_BUDGET_OTHER),
            },
        }
    }
}

impl MemoryBudget {
    /// Derive a default budget from the system's total RAM (best-effort).
    ///
    /// Strategy from `docs/10-performance-engineering.md`:
    /// - budget = min(total_ram / 4, 4GiB) clamped to at least 512MiB.
    pub fn default_for_system() -> Self {
        let total_ram = system_total_memory_bytes().unwrap_or(8 * GB);
        Self::default_for_system_memory_bytes(total_ram)
    }

    /// Derive the default budget from a caller-provided "system memory" value.
    ///
    /// This is a small pure helper used by [`MemoryBudget::default_for_system`]
    /// and exposed for integration tests.
    #[doc(hidden)]
    pub fn default_for_system_memory_bytes(system_memory_bytes: u64) -> Self {
        let budget = (system_memory_bytes / 4).clamp(512 * MB, 4 * GB);
        Self::from_total(budget)
    }

    /// Like [`Self::default_for_system`], but applies [`MemoryBudgetOverrides::from_env`].
    pub fn default_for_system_with_env_overrides() -> Self {
        Self::default_for_system().apply_overrides(MemoryBudgetOverrides::from_env())
    }

    /// Build a budget from an explicit total, using the default category split.
    pub fn from_total(total: u64) -> Self {
        // Percentages from docs/10-performance-engineering.md
        let query_cache = total * 40 / 100;
        let syntax_trees = total * 25 / 100;
        let indexes = total * 20 / 100;
        let type_info = total * 10 / 100;
        let assigned = query_cache + syntax_trees + indexes + type_info;
        let other = total.saturating_sub(assigned);

        Self {
            total,
            categories: MemoryBreakdown {
                query_cache,
                syntax_trees,
                indexes,
                type_info,
                other,
            },
        }
    }

    /// Apply overrides to this budget.
    ///
    /// If the override causes per-category budgets to exceed `total`, budgets are
    /// scaled down proportionally to preserve the `sum(categories) == total`
    /// invariant.
    pub fn apply_overrides(mut self, overrides: MemoryBudgetOverrides) -> Self {
        if let Some(total) = overrides.total {
            self = Self::from_total(total);
        }

        if let Some(bytes) = overrides.categories.query_cache {
            self.categories.query_cache = bytes;
        }
        if let Some(bytes) = overrides.categories.syntax_trees {
            self.categories.syntax_trees = bytes;
        }
        if let Some(bytes) = overrides.categories.indexes {
            self.categories.indexes = bytes;
        }
        if let Some(bytes) = overrides.categories.type_info {
            self.categories.type_info = bytes;
        }
        if let Some(bytes) = overrides.categories.other {
            self.categories.other = bytes;
        }

        // Re-normalize.
        let sum = self.categories.total();
        match sum.cmp(&self.total) {
            std::cmp::Ordering::Less => {
                // Give remaining headroom to "other" to keep accounting simple.
                self.categories.other = self.categories.other.saturating_add(self.total - sum);
            }
            std::cmp::Ordering::Equal => {}
            std::cmp::Ordering::Greater => {
                // Scale categories down proportionally; last category picks up remainder.
                // This is a best-effort "never exceed total" policy.
                let total = self.total.max(1);
                let query_cache = self.categories.query_cache.saturating_mul(total) / sum;
                let syntax_trees = self.categories.syntax_trees.saturating_mul(total) / sum;
                let indexes = self.categories.indexes.saturating_mul(total) / sum;
                let type_info = self.categories.type_info.saturating_mul(total) / sum;
                let assigned = query_cache + syntax_trees + indexes + type_info;
                let other = total.saturating_sub(assigned);
                self.categories = MemoryBreakdown {
                    query_cache,
                    syntax_trees,
                    indexes,
                    type_info,
                    other,
                };
            }
        }

        self
    }
}

const UNLIMITED_THRESHOLD_BYTES: u64 = 1 << 60; // 1 EiB; above this is treated as "unlimited".

/// Interpret a raw `RLIMIT_AS` value expressed in bytes.
///
/// Returns `None` for unlimited/unknown values.
///
/// Exposed for integration tests; callers should pass the platform-specific
/// `RLIM_INFINITY` value (e.g. `libc::RLIM_INFINITY`).
#[doc(hidden)]
pub fn interpret_rlimit_as_bytes(raw_bytes: u64, rlim_infinity: u64) -> Option<u64> {
    if raw_bytes == rlim_infinity || raw_bytes >= UNLIMITED_THRESHOLD_BYTES {
        return None;
    }

    Some(raw_bytes)
}

#[cfg(unix)]
fn process_rlimit_as_bytes() -> Option<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    let rc = unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut limit) };
    if rc != 0 {
        return None;
    }

    interpret_rlimit_as_bytes(limit.rlim_cur, libc::RLIM_INFINITY)
}

#[cfg(not(unix))]
fn process_rlimit_as_bytes() -> Option<u64> {
    None
}

/// Compute the effective system memory size for budgeting.
///
/// The returned value is the minimum of:
/// - host total RAM
/// - Linux cgroup memory limit (when available)
/// - process `RLIMIT_AS` (when set)
///
/// Exposed for integration tests; it does not query the OS.
#[doc(hidden)]
pub fn effective_system_total_memory_bytes(
    host_total_memory_bytes: u64,
    cgroup_limit_bytes: Option<u64>,
    rlimit_as_bytes: Option<u64>,
) -> u64 {
    let mut effective = host_total_memory_bytes;

    if let Some(limit) = cgroup_limit_bytes {
        effective = effective.min(limit);
    }

    if let Some(limit) = rlimit_as_bytes {
        effective = effective.min(limit);
    }

    effective
}

fn system_total_memory_bytes() -> Option<u64> {
    use sysinfo::System;

    let mut sys = System::new();
    sys.refresh_memory();
    // sysinfo reports KiB.
    let host_total = sys.total_memory().saturating_mul(1024);

    #[cfg(target_os = "linux")]
    let cgroup_limit = crate::cgroup::cgroup_memory_limit_bytes();
    #[cfg(not(target_os = "linux"))]
    let cgroup_limit: Option<u64> = None;

    let rlimit_as = process_rlimit_as_bytes();

    Some(effective_system_total_memory_bytes(
        host_total,
        cgroup_limit,
        rlimit_as,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::panic::Location;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[track_caller]
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        match ENV_LOCK.get_or_init(|| Mutex::new(())).lock() {
            Ok(guard) => guard,
            Err(err) => {
                let loc = Location::caller();
                tracing::error!(
                    target = "nova.memory.tests",
                    file = loc.file(),
                    line = loc.line(),
                    column = loc.column(),
                    error = %err,
                    "env lock poisoned; continuing with recovered guard"
                );
                err.into_inner()
            }
        }
    }

    fn set_env_var(name: &str, value: Option<&str>) -> Option<OsString> {
        let prev = std::env::var_os(name);
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
        prev
    }

    fn restore_env_var(name: &str, prev: Option<OsString>) {
        match prev {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    #[test]
    fn parse_byte_size_accepts_raw_bytes_and_suffixes() {
        assert_eq!(parse_byte_size("1").unwrap(), 1);
        assert_eq!(parse_byte_size("1024").unwrap(), 1024);
        assert_eq!(parse_byte_size("1_024").unwrap(), 1024);
        assert_eq!(parse_byte_size("1K").unwrap(), 1024);
        assert_eq!(parse_byte_size("1kb").unwrap(), 1024);
        assert_eq!(parse_byte_size("1KiB").unwrap(), 1024);
        assert_eq!(parse_byte_size("2M").unwrap(), 2 * MB);
        assert_eq!(parse_byte_size("2MiB").unwrap(), 2 * MB);
        assert_eq!(parse_byte_size("3G").unwrap(), 3 * GB);
        assert_eq!(parse_byte_size("3GiB").unwrap(), 3 * GB);
        assert_eq!(parse_byte_size("1T").unwrap(), 1024u64.pow(4));
    }

    #[test]
    fn default_for_system_with_env_overrides_honors_total() {
        let _guard = env_lock();
        let prev = set_env_var(ENV_MEMORY_BUDGET_TOTAL, Some("1G"));
        let budget = MemoryBudget::default_for_system_with_env_overrides();
        restore_env_var(ENV_MEMORY_BUDGET_TOTAL, prev);

        assert_eq!(budget.total, GB);
        assert_eq!(budget.categories.total(), GB);
    }

    #[test]
    fn env_overrides_win_over_config_overrides() {
        let _guard = env_lock();
        let prev = set_env_var(ENV_MEMORY_BUDGET_TOTAL, Some("1G"));

        let config_overrides = MemoryBudgetOverrides {
            total: Some(2 * GB),
            categories: MemoryBreakdownOverrides::default(),
        };
        let budget = MemoryBudget::default_for_system()
            .apply_overrides(config_overrides)
            .apply_overrides(MemoryBudgetOverrides::from_env());

        restore_env_var(ENV_MEMORY_BUDGET_TOTAL, prev);

        assert_eq!(budget.total, GB);
    }
}

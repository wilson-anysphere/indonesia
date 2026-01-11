use nova_memory::{
    cgroup::{parse_cgroup_memory_limit_bytes, parse_proc_self_cgroup},
    effective_system_total_memory_bytes,
    interpret_rlimit_as_bytes,
    MemoryBudget, GB, MB,
};

#[test]
fn parses_cgroup_v2_unified_path() {
    let fixture = "\
0::/user.slice/user-1000.slice/session-2.scope
1:name=systemd:/user.slice/user-1000.slice/session-2.scope
";

    let parsed = parse_proc_self_cgroup(fixture);
    assert_eq!(
        parsed.v2_path.as_deref(),
        Some("/user.slice/user-1000.slice/session-2.scope")
    );
}

#[test]
fn parses_cgroup_v1_memory_controller_path() {
    let fixture = "\
12:hugetlb:/
11:memory:/docker/0123456789abcdef
10:cpu,cpuacct:/docker/0123456789abcdef
";

    let parsed = parse_proc_self_cgroup(fixture);
    assert_eq!(
        parsed.v1_memory_path.as_deref(),
        Some("/docker/0123456789abcdef")
    );
}

#[test]
fn parses_cgroup_v1_memory_controller_among_multiple_controllers() {
    let fixture = "\
7:cpu,cpuacct:/user.slice
6:memory,blkio:/user.slice
";

    let parsed = parse_proc_self_cgroup(fixture);
    assert_eq!(parsed.v1_memory_path.as_deref(), Some("/user.slice"));
}

#[test]
fn interprets_memory_max_max_as_unlimited() {
    assert_eq!(parse_cgroup_memory_limit_bytes("max\n"), None);
}

#[test]
fn interprets_memory_max_numeric() {
    assert_eq!(
        parse_cgroup_memory_limit_bytes("1073741824\n"),
        Some(1_073_741_824)
    );
}

#[test]
fn interprets_cgroup_v1_unlimited_sentinel_as_unlimited() {
    // Common cgroup v1 "unlimited" value (0x7ffffffffffff000).
    assert_eq!(parse_cgroup_memory_limit_bytes("9223372036854771712"), None);
}

#[test]
fn interprets_empty_or_invalid_cgroup_limits_as_unlimited() {
    assert_eq!(parse_cgroup_memory_limit_bytes(""), None);
    assert_eq!(parse_cgroup_memory_limit_bytes(" \n\t"), None);
    assert_eq!(parse_cgroup_memory_limit_bytes("not-a-number"), None);
}

#[test]
fn interprets_very_large_cgroup_limits_as_unlimited() {
    let huge = (1u64 << 60).to_string();
    assert_eq!(parse_cgroup_memory_limit_bytes(&huge), None);

    let just_under = ((1u64 << 60) - 1).to_string();
    assert_eq!(
        parse_cgroup_memory_limit_bytes(&just_under),
        Some((1u64 << 60) - 1)
    );
}

#[test]
fn interprets_rlimit_as_infinity_or_extremely_large_as_unlimited() {
    // The helper is intentionally pure: we don't need to mutate the process rlimit in tests.
    assert_eq!(interpret_rlimit_as_bytes(123, 123), None);

    assert_eq!(
        interpret_rlimit_as_bytes(1u64 << 60, u64::MAX),
        None,
        "extremely large limits are treated as unlimited"
    );
    assert_eq!(
        interpret_rlimit_as_bytes((1u64 << 60) - 1, u64::MAX),
        Some((1u64 << 60) - 1)
    );
}

#[cfg(unix)]
#[test]
fn interprets_libc_rlim_infinity_as_unlimited() {
    assert_eq!(
        interpret_rlimit_as_bytes(libc::RLIM_INFINITY as u64, libc::RLIM_INFINITY as u64),
        None
    );
}

#[test]
fn budget_clamps_using_rlimit_when_smaller_than_host_and_cgroup() {
    let host_total = 32 * GB;
    let cgroup_limit = Some(16 * GB);
    let rlimit_as = Some(2 * GB);

    let effective_total = effective_system_total_memory_bytes(host_total, cgroup_limit, rlimit_as);
    assert_eq!(effective_total, 2 * GB);

    let budget = MemoryBudget::default_for_system_memory_bytes(effective_total);
    assert_eq!(budget.total, 512 * MB);
}

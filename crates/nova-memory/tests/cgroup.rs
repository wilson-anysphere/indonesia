use nova_memory::cgroup::{parse_cgroup_memory_limit_bytes, parse_proc_self_cgroup};

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

pub fn current_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = match std::fs::read_to_string("/proc/self/status") {
            Ok(status) => status,
            Err(err) => {
                // `/proc` may not be available in some sandboxed environments; treat it as
                // best-effort and only log unexpected filesystem errors.
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(
                        target = "nova.memory",
                        error = %err,
                        "failed to read /proc/self/status while sampling rss"
                    );
                }
                return None;
            }
        };
        for line in status.lines() {
            let line = line.trim_start();
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let Some(kb) = rest.split_whitespace().next() else {
                    return None;
                };
                let kb = match kb.parse::<u64>() {
                    Ok(kb) => kb,
                    Err(err) => {
                        // `VmRSS` is expected to be a numeric value in kB; log once if parsing
                        // fails to avoid spamming in hot call sites.
                        static REPORTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
                        if REPORTED.set(()).is_ok() {
                            tracing::debug!(
                                target = "nova.memory",
                                value = kb,
                                error = %err,
                                "failed to parse VmRSS from /proc/self/status"
                            );
                        }
                        return None;
                    }
                };
                return Some(kb.saturating_mul(1024));
            }
        }
        None
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

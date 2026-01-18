use nova_process::{run_command, CancellationToken, RunOptions};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn helper() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nova_process_test_helper"))
}

#[test]
fn truncates_large_stdout() {
    let opts = RunOptions {
        timeout: Some(Duration::from_secs(2)),
        max_bytes: 1024,
        ..RunOptions::default()
    };

    let result = run_command(
        Path::new("."),
        &helper(),
        &["--stdout-bytes".into(), "1048576".into()],
        opts,
    )
    .unwrap();

    assert!(result.status.success());
    assert!(!result.timed_out);
    assert!(result.output.truncated);
    assert_eq!(result.output.stdout.len(), 1024);
}

#[test]
fn timeout_kills_child() {
    let opts = RunOptions {
        timeout: Some(Duration::from_millis(50)),
        max_bytes: 1024,
        ..RunOptions::default()
    };

    let result = run_command(
        Path::new("."),
        &helper(),
        &["--sleep-ms".into(), "5000".into()],
        opts,
    )
    .unwrap();

    assert!(result.timed_out);
}

#[test]
fn timeout_kills_process_tree() {
    let opts = RunOptions {
        timeout: Some(Duration::from_millis(50)),
        max_bytes: 1024,
        ..RunOptions::default()
    };

    let start = Instant::now();
    let result = run_command(
        Path::new("."),
        &helper(),
        &[
            "--spawn-child-sleep-ms".into(),
            "5000".into(),
            "--sleep-ms".into(),
            "5000".into(),
        ],
        opts,
    )
    .unwrap();

    assert!(result.timed_out);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "expected timeout kill to return promptly, took {:?}",
        start.elapsed()
    );
}

#[test]
fn cancellation_kills_child() {
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        canceller.cancel();
    });

    let opts = RunOptions {
        timeout: None,
        max_bytes: 1024,
        cancellation: Some(cancel),
        ..RunOptions::default()
    };

    let start = Instant::now();
    let result = run_command(
        Path::new("."),
        &helper(),
        &["--sleep-ms".into(), "5000".into()],
        opts,
    )
    .unwrap();

    assert!(result.cancelled);
    assert!(!result.timed_out);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "expected cancellation kill to return promptly, took {:?}",
        start.elapsed()
    );
}

#[test]
fn cancellation_kills_process_tree() {
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        canceller.cancel();
    });

    let opts = RunOptions {
        timeout: None,
        max_bytes: 1024,
        cancellation: Some(cancel),
        ..RunOptions::default()
    };

    let start = Instant::now();
    let result = run_command(
        Path::new("."),
        &helper(),
        &[
            "--spawn-child-sleep-ms".into(),
            "5000".into(),
            "--sleep-ms".into(),
            "5000".into(),
        ],
        opts,
    )
    .unwrap();

    assert!(result.cancelled);
    assert!(!result.timed_out);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "expected cancellation kill to return promptly, took {:?}",
        start.elapsed()
    );
}

#[cfg(windows)]
#[test]
fn runs_cmd_scripts_via_comspec() {
    use std::time::SystemTime;

    let mut dir = std::env::temp_dir();
    let token = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_else(|err| err.duration())
        .as_nanos();
    // Include spaces to ensure the cmd.exe invocation handles quoted paths.
    dir.push(format!("nova process test {token}"));
    std::fs::create_dir_all(&dir).unwrap();

    let script = dir.join("hello.cmd");
    std::fs::write(&script, "@echo off\r\necho arg=%1\r\n").unwrap();

    let opts = RunOptions {
        timeout: Some(Duration::from_secs(2)),
        max_bytes: 1024,
        ..RunOptions::default()
    };
    let result = run_command(&dir, &script, &["foo bar".into()], opts).unwrap();
    assert!(result.status.success());
    assert!(result
        .output
        .stdout
        .to_ascii_lowercase()
        .contains("arg=foo bar"));

    if let Err(err) = std::fs::remove_dir_all(&dir) {
        if err.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "failed to remove process test directory {}: {err}",
                dir.display()
            );
        }
    }
}

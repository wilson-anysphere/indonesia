#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::FuzzRunner;

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    run_input(input);
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new_default("fuzz_archive_read", init, run_one))
}

fn run_input(input: &[u8]) {
    let mode = input.first().copied().unwrap_or(0);
    let payload = input.get(1..).unwrap_or(&[]);

    if mode & 1 == 0 {
        // In zip mode, feed the *entire* input to the archive reader so seeds
        // like `valid_metadata.jar` / `valid_metadata.jmod` remain valid
        // ZIP/JAR/JMOD bytes (rather than dropping the first byte).
        run_zip_mode(input);
    } else {
        run_dir_mode(payload);
    }
}

fn run_zip_mode(jar_bytes: &[u8]) {
    use std::io::Write;

    let mut tmp = tempfile::Builder::new()
        .prefix("fuzz_archive_read")
        .suffix(".jar")
        .tempfile()
        .expect("failed to create tempfile");

    // Write potentially-invalid JAR bytes. We're only asserting that we never
    // panic / hang / OOM when reading from third-party dependencies.
    tmp.write_all(jar_bytes).expect("failed to write jar bytes");
    tmp.flush().expect("failed to flush jar bytes");

    let archive = nova_archive::Archive::new(tmp.path());
    let _ = archive.read("META-INF/spring-configuration-metadata.json");
    let _ = archive.read("A.class");
}

fn run_dir_mode(file_bytes: &[u8]) {
    let dir = tempfile::tempdir().expect("failed to create tempdir");
    let archive = nova_archive::Archive::new(dir.path());

    // Pick one of two known entry names.
    let name = if file_bytes.first().copied().unwrap_or(0) & 1 == 0 {
        "META-INF/spring-configuration-metadata.json"
    } else {
        "A.class"
    };

    let path = dir.path().join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create parent dirs");
    }

    // Keep file sizes small and bounded regardless of the input.
    let cap = file_bytes.len().min(64 * 1024);
    std::fs::write(&path, &file_bytes[..cap]).expect("failed to write file bytes");

    let _ = archive.read(name);
    let _ = archive.read("does-not-exist");
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});

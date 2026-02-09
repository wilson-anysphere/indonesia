#![cfg(feature = "embeddings")]

use std::path::PathBuf;

use nova_ai::{EmbeddingSemanticSearch, HashEmbedder, SemanticSearch};

/// Regression test: constructing embedding-backed semantic search must not attempt to configure
/// Rayon's *global* thread pool (which is process-wide state).
///
/// This runs the assertion in a fresh child process to avoid interference from other tests that
/// may legitimately initialize Rayon's global pool.
#[test]
fn embedding_semantic_search_does_not_touch_rayon_global_pool() {
    const CHILD_ENV: &str = "NOVA_AI_RAYON_GLOBAL_POOL_REGRESSION_CHILD";
    const TEST_NAME: &str =
        "suite::rayon_global_pool_regression::embedding_semantic_search_does_not_touch_rayon_global_pool";

    if std::env::var_os(CHILD_ENV).is_none() {
        let exe = std::env::current_exe().expect("failed to resolve test binary path");

        let output = std::process::Command::new(exe)
            .env(CHILD_ENV, "1")
            // Ensure that if the global pool is initialized implicitly, it uses a deterministic
            // thread count. If semantic search incorrectly initializes Rayon's global pool to a
            // hard-coded thread count, this will not be observed.
            .env("RAYON_NUM_THREADS", "2")
            .arg("--exact")
            .arg(TEST_NAME)
            .arg("--test-threads=1")
            .output()
            .expect("failed to run child test process");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            output.status.success(),
            "child test process failed (status: {status})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            status = output.status,
            stdout = stdout.as_ref(),
            stderr = stderr.as_ref(),
        );

        assert!(
            stdout.contains(TEST_NAME) || stderr.contains(TEST_NAME),
            "child test process did not appear to execute the regression test.\nstdout:\n{stdout}\nstderr:\n{stderr}",
            stdout = stdout.as_ref(),
            stderr = stderr.as_ref(),
        );
        return;
    }

    // Create an independent Rayon pool inside the test to ensure it remains usable even after
    // constructing embedding semantic search.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .expect("failed to create local rayon pool");
    assert_eq!(pool.install(rayon::current_num_threads), 2);

    let mut search = EmbeddingSemanticSearch::new(HashEmbedder::default());
    search.index_file(
        PathBuf::from("src/Hello.java"),
        "public class Hello { public String helloWorld() { return \"hello world\"; } }".to_string(),
    );
    let _ = search.search("hello world");

    // The local pool should remain unchanged.
    assert_eq!(pool.install(rayon::current_num_threads), 2);

    // If embedding semantic search tried to configure the global pool to a hard-coded thread
    // count, then a *fresh* thread (which isn't "inside" any other Rayon pool) would observe a
    // different global thread count than `RAYON_NUM_THREADS`.
    let global_threads = std::thread::spawn(rayon::current_num_threads)
        .join()
        .expect("thread panicked while querying rayon global pool");
    assert_eq!(global_threads, 2);
}

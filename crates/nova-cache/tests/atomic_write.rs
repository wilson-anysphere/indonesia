use std::sync::{Arc, Barrier};

#[test]
fn atomic_write_is_safe_under_concurrent_writers() {
    let dir = tempfile::tempdir().unwrap();
    let dest = Arc::new(dir.path().join("concurrent.bin"));

    let payload_a = Arc::new(vec![0xA5u8; 64 * 1024]);
    let payload_b = Arc::new(vec![0x5Au8; 64 * 1024]);

    let threads = 8;
    let iterations = 64;
    let barrier = Arc::new(Barrier::new(threads));

    let mut handles = Vec::with_capacity(threads);
    for idx in 0..threads {
        let dest = dest.clone();
        let payload_a = payload_a.clone();
        let payload_b = payload_b.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(
            move || -> Result<(), nova_cache::CacheError> {
                let payload = if idx % 2 == 0 { payload_a } else { payload_b };
                let mut error: Option<nova_cache::CacheError> = None;
                for _ in 0..iterations {
                    barrier.wait();
                    if error.is_none() {
                        if let Err(err) = nova_cache::atomic_write(dest.as_path(), &payload) {
                            error = Some(err);
                        }
                    }
                }
                if let Some(err) = error {
                    Err(err)
                } else {
                    Ok(())
                }
            },
        ));
    }

    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    let bytes = std::fs::read(dest.as_path()).unwrap();
    assert!(
        bytes == *payload_a || bytes == *payload_b,
        "final file payload corrupted (len={})",
        bytes.len()
    );
}

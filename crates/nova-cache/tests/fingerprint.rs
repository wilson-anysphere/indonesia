use nova_cache::Fingerprint;

#[test]
fn from_file_matches_from_bytes() -> Result<(), nova_cache::CacheError> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("payload.bin");

    // Ensure we cover multiple read iterations (internal buffer is 64KiB).
    let mut bytes = Vec::with_capacity(256 * 1024 + 3);
    for i in 0..(256 * 1024 + 3) {
        bytes.push((i % 251) as u8);
    }

    std::fs::write(&path, &bytes)?;

    let from_file = Fingerprint::from_file(&path)?;
    let from_bytes = Fingerprint::from_bytes(&bytes);
    assert_eq!(from_file, from_bytes);

    Ok(())
}

#[test]
fn from_file_matches_from_bytes_for_empty_file() -> Result<(), nova_cache::CacheError> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("empty.bin");
    std::fs::write(&path, &[] as &[u8])?;

    let from_file = Fingerprint::from_file(&path)?;
    let from_bytes = Fingerprint::from_bytes([]);
    assert_eq!(from_file, from_bytes);

    Ok(())
}

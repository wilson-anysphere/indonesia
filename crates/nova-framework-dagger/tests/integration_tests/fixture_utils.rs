use std::path::{Path, PathBuf};

pub(super) fn load_fixture_sources(name: &str) -> Vec<(PathBuf, String)> {
    let root: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);

    let mut out = Vec::new();
    collect_java_sources(&root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn collect_java_sources(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    for entry in std::fs::read_dir(dir).expect("read fixture dir") {
        let entry = entry.expect("read entry");
        let path = entry.path();
        if path.is_dir() {
            collect_java_sources(&path, out);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("java") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read java file");
        out.push((path, text));
    }
}

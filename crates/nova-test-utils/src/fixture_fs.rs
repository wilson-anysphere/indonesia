use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Load a fixture directory into a `(relative_path -> text)` map.
pub fn load_fixture_dir(dir: &Path) -> BTreeMap<PathBuf, String> {
    fn visit_dir(
        root: &Path,
        dir: &Path,
        out: &mut BTreeMap<PathBuf, String>,
    ) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                visit_dir(root, &path, out)?;
            } else {
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                let text = fs::read_to_string(&path)?;
                out.insert(rel, text);
            }
        }
        Ok(())
    }

    let mut out = BTreeMap::new();
    visit_dir(dir, dir, &mut out).expect("fixture dir readable");
    out
}

pub fn assert_fixture_transformed(
    before: &Path,
    after: &Path,
    mut transform: impl FnMut(&mut BTreeMap<PathBuf, String>),
) {
    let mut files = load_fixture_dir(before);
    transform(&mut files);

    if !after.exists() {
        if bless_enabled() {
            bless_fixture_dir(after, &files);
            return;
        }
        panic!(
            "missing expected fixture dir {} (run with `BLESS=1` to write it)",
            after.display()
        );
    }

    let expected = load_fixture_dir(after);
    if files != expected {
        if bless_enabled() {
            bless_fixture_dir(after, &files);
            return;
        }
        assert_eq!(files, expected);
    }
}

fn bless_enabled() -> bool {
    let Ok(val) = env::var("BLESS") else {
        return false;
    };
    let val = val.trim().to_ascii_lowercase();
    !(val.is_empty() || val == "0" || val == "false")
}

fn bless_fixture_dir(dir: &Path, files: &BTreeMap<PathBuf, String>) {
    if dir.exists() {
        fs::remove_dir_all(dir).unwrap_or_else(|err| {
            panic!(
                "failed to remove existing fixture dir {}: {err}",
                dir.display()
            )
        });
    }
    fs::create_dir_all(dir)
        .unwrap_or_else(|err| panic!("failed to create fixture dir {}: {err}", dir.display()));

    for (rel, text) in files {
        assert!(
            rel.components()
                .all(|c| !matches!(c, std::path::Component::ParentDir)),
            "fixture paths must not contain '..': {}",
            rel.display()
        );
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap_or_else(|err| {
                panic!("failed to create fixture dir {}: {err}", parent.display())
            });
        }
        fs::write(&path, text)
            .unwrap_or_else(|err| panic!("failed to write fixture {}: {err}", path.display()));
    }
}

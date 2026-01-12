use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::{fs, io};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildFileFingerprint {
    pub digest: String,
}

impl BuildFileFingerprint {
    pub fn from_files(project_root: &Path, mut files: Vec<PathBuf>) -> io::Result<Self> {
        files.sort();
        files.dedup();

        let mut hasher = Sha256::new();
        for path in files {
            let rel = path.strip_prefix(project_root).unwrap_or(&path);
            hasher.update(rel.to_string_lossy().as_bytes());
            hasher.update([0]);

            let bytes = fs::read(&path)?;
            hasher.update(&bytes);
            hasher.update([0]);
        }

        Ok(Self {
            digest: hex::encode(hasher.finalize()),
        })
    }
}

pub fn collect_gradle_build_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_gradle_build_files_rec(root, root, &mut out)?;
    // Stable sort for hashing.
    out.sort_by(|a, b| {
        let ra = a.strip_prefix(root).unwrap_or(a);
        let rb = b.strip_prefix(root).unwrap_or(b);
        ra.cmp(rb)
    });
    out.dedup();
    Ok(out)
}

fn collect_gradle_build_files_rec(
    root: &Path,
    dir: &Path,
    out: &mut Vec<PathBuf>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        if path.is_dir() {
            // Avoid scanning huge non-source directories that commonly show up in mono-repos.
            // These trees can contain many files that look like build files but should not
            // influence Nova's build fingerprint (e.g. vendored JS dependencies).
            if file_name == "node_modules" {
                continue;
            }
            // Bazel output trees are typically created at the workspace root and can be enormous.
            // Skip any `bazel-*` entries (`bazel-out`, `bazel-bin`, `bazel-testlogs`,
            // `bazel-<workspace>`, etc).
            if file_name.starts_with("bazel-") {
                continue;
            }
            if file_name == ".git"
                || file_name == ".gradle"
                || file_name == "build"
                || file_name == "target"
                || file_name == ".nova"
                || file_name == ".idea"
            {
                continue;
            }
            collect_gradle_build_files_rec(root, &path, out)?;
            continue;
        }

        let name = file_name.as_ref();

        // Gradle dependency locking can change resolved classpaths without modifying any build
        // scripts, so include lockfiles in the fingerprint.
        //
        // Patterns:
        // - `gradle.lockfile` at any depth.
        // - `*.lockfile` under any `dependency-locks/` directory (covers Gradle's default
        //   `gradle/dependency-locks/` location).
        if name == "gradle.lockfile" {
            out.push(path);
            continue;
        }
        if name.ends_with(".lockfile")
            && path.parent().is_some_and(|parent| {
                parent.ancestors().any(|dir| {
                    dir.file_name()
                        .is_some_and(|name| name == "dependency-locks")
                })
            })
        {
            out.push(path);
            continue;
        }

        // Match `nova-build` build-file watcher semantics by including any
        // `build.gradle*` / `settings.gradle*` variants.
        if name.starts_with("build.gradle") || name.starts_with("settings.gradle") {
            out.push(path);
            continue;
        }

        // Gradle can emit per-configuration lockfiles under `gradle/dependency-locks/`.
        // Include them in the fingerprint so classpath caching stays correct when locks change.
        if path.extension().is_some_and(|ext| ext == "lockfile")
            && path
                .strip_prefix(root)
                .ok()
                .is_some_and(|rel| rel.starts_with(Path::new("gradle/dependency-locks")))
        {
            out.push(path);
            continue;
        }

        // Applied Gradle script plugins can influence dependencies and tasks
        // without being named `build.gradle*` / `settings.gradle*`.
        if name.ends_with(".gradle") || name.ends_with(".gradle.kts") {
            out.push(path);
            continue;
        }

        // Gradle version catalogs can define dependency versions and thus affect resolved
        // classpaths. In addition to the default `gradle/libs.versions.toml`, Gradle supports
        // custom catalogs referenced from `settings.gradle*` (e.g. `gradle/foo.versions.toml`).
        //
        // Only include catalogs that are direct children of a directory named `gradle` to avoid
        // accidentally picking up unrelated `*.toml` files elsewhere in the repo (including under
        // `node_modules/`).
        if name.ends_with(".versions.toml")
            && path
                .parent()
                .and_then(|parent| parent.file_name())
                .is_some_and(|dir| dir == "gradle")
        {
            out.push(path);
            continue;
        }

        match name {
            "gradle.properties" => out.push(path),
            "libs.versions.toml" => out.push(path),
            "gradlew" | "gradlew.bat" => {
                if path == root.join(name) {
                    out.push(path);
                }
            }
            "gradle-wrapper.properties" => {
                if path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.properties")) {
                    out.push(path);
                }
            }
            "gradle-wrapper.jar" => {
                if path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.jar")) {
                    out.push(path);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn collect_gradle_build_files_filters_ignored_dirs_and_matches_expected_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Included files.
        write_file(&root.join("build.gradle"), b"plugins {}");
        write_file(
            &root.join("settings.gradle.kts"),
            b"rootProject.name = \"demo\"",
        );
        write_file(&root.join("gradle.properties"), b"org.gradle.daemon=true");
        write_file(&root.join("gradle.lockfile"), b"# lockfile");
        write_file(
            &root.join("libs.versions.toml"),
            b"[versions]\nroot = \"1\"",
        );
        write_file(&root.join("random.lockfile"), b"should-not-be-included");

        write_file(&root.join("gradlew"), b"#!/bin/sh\necho gradlew");
        write_file(&root.join("gradlew.bat"), b"@echo off\r\necho gradlew");

        write_file(
            &root.join("gradle/libs.versions.toml"),
            b"[versions]\nfoo = \"1.0\"",
        );
        write_file(
            &root.join("gradle/custom.versions.toml"),
            b"[versions]\nbar = \"2.0\"",
        );
        write_file(
            &root.join("gradle/wrapper/gradle-wrapper.jar"),
            b"jar-bytes",
        );
        write_file(
            &root.join("gradle/wrapper/gradle-wrapper.properties"),
            b"distributionUrl=https\\://services.gradle.org/distributions/gradle-8.0-bin.zip",
        );

        write_file(&root.join("scripts/plugin.gradle"), b"println(\"hi\")");
        write_file(&root.join("sub/module.gradle.kts"), b"// module script");
        write_file(&root.join("sub/gradle.lockfile"), b"# sub lockfile");
        write_file(
            &root.join("gradle/dependency-locks/compileClasspath.lockfile"),
            b"# deps lockfile",
        );
        write_file(
            &root.join("other/dependency-locks/custom.lockfile"),
            b"# deps lockfile 2",
        );

        // Should *not* be included: non-gradle version catalogs outside `gradle/`.
        write_file(
            &root.join("config/other.versions.toml"),
            b"[versions]\nnope = \"1\"",
        );

        // Ignored directories.
        write_file(
            &root.join("node_modules/build.gradle"),
            b"should-be-ignored",
        );
        write_file(&root.join("build/settings.gradle"), b"should-be-ignored");
        write_file(&root.join("target/foo.gradle"), b"should-be-ignored");
        write_file(&root.join(".git/build.gradle"), b"should-be-ignored");
        write_file(&root.join(".gradle/build.gradle"), b"should-be-ignored");
        write_file(&root.join(".nova/build.gradle"), b"should-be-ignored");
        write_file(&root.join(".idea/build.gradle"), b"should-be-ignored");
        write_file(&root.join("bazel-out/build.gradle"), b"should-be-ignored");
        // Ignore Bazel output trees anywhere, not just at the root.
        write_file(
            &root.join("nested/bazel-out/build.gradle"),
            b"should-be-ignored",
        );
        write_file(
            &root.join("nested/bazel-myworkspace/build.gradle"),
            b"should-be-ignored",
        );

        let files = collect_gradle_build_files(root).unwrap();
        let rel: BTreeSet<PathBuf> = files
            .into_iter()
            .map(|p| p.strip_prefix(root).unwrap().to_path_buf())
            .collect();

        let expected: BTreeSet<PathBuf> = [
            PathBuf::from("build.gradle"),
            PathBuf::from("gradle.lockfile"),
            PathBuf::from("gradle/custom.versions.toml"),
            PathBuf::from("gradle/dependency-locks/compileClasspath.lockfile"),
            PathBuf::from("gradle/libs.versions.toml"),
            PathBuf::from("gradle/wrapper/gradle-wrapper.jar"),
            PathBuf::from("gradle/wrapper/gradle-wrapper.properties"),
            PathBuf::from("gradle.properties"),
            PathBuf::from("gradlew"),
            PathBuf::from("gradlew.bat"),
            PathBuf::from("libs.versions.toml"),
            PathBuf::from("other/dependency-locks/custom.lockfile"),
            PathBuf::from("scripts/plugin.gradle"),
            PathBuf::from("settings.gradle.kts"),
            PathBuf::from("sub/gradle.lockfile"),
            PathBuf::from("sub/module.gradle.kts"),
        ]
        .into_iter()
        .collect();

        assert_eq!(rel, expected);
    }

    #[test]
    fn fingerprint_is_deterministic_independent_of_input_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let a = root.join("build.gradle");
        let b = root.join("settings.gradle");
        write_file(&a, b"a");
        write_file(&b, b"b");

        let first = BuildFileFingerprint::from_files(root, vec![a.clone(), b.clone()]).unwrap();
        let second = BuildFileFingerprint::from_files(root, vec![b, a]).unwrap();

        assert_eq!(first, second);
    }
}

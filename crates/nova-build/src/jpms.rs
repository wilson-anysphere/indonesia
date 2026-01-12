use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

const MODULE_INFO_CLASS_CANDIDATES: [&str; 4] = [
    "module-info.class",
    "META-INF/versions/9/module-info.class",
    "classes/module-info.class",
    "classes/META-INF/versions/9/module-info.class",
];

const MANIFEST_CANDIDATES: [&str; 2] = ["META-INF/MANIFEST.MF", "classes/META-INF/MANIFEST.MF"];

const JMOD_HEADER_LEN: u64 = 4;

const JPMS_COMPILER_FLAG_NEEDLES: [&str; 12] = [
    "--module-path",
    "-p",
    "--add-modules",
    "--patch-module",
    "--add-reads",
    "--add-exports",
    "--add-opens",
    "--limit-modules",
    "--upgrade-module-path",
    "--module",
    "-m",
    "--module-source-path",
];

pub(crate) fn compiler_arg_looks_like_jpms(arg: &str) -> bool {
    let arg = arg.trim();
    JPMS_COMPILER_FLAG_NEEDLES.iter().any(|flag| {
        arg == *flag
            || arg
                .strip_prefix(flag)
                .is_some_and(|rest| rest.starts_with('='))
    })
}

pub(crate) fn compiler_args_looks_like_jpms(args: &[String]) -> bool {
    args.iter().any(|arg| compiler_arg_looks_like_jpms(arg))
}

pub(crate) fn main_source_roots_have_module_info(main_source_roots: &[PathBuf]) -> bool {
    main_source_roots
        .iter()
        .any(|root| root.join("module-info.java").is_file())
}

/// Best-effort JPMS module-path inference for a build-derived Java compile config.
///
/// This helper is used by build system integrations (Gradle/Maven) to derive a reasonable
/// `module_path` from build-provided classpath data.
///
/// - `resolved_compile_classpath` is the (absolute) list of classpath entries (jars/directories).
/// - `main_source_roots` is used as a heuristic to decide whether JPMS is in play (via
///   `module-info.java`).
/// - `main_output_dir` is excluded from the inferred module-path (output directories should live on
///   the compile classpath, not the module-path).
/// - `compiler_args_looks_like_jpms` forces module-path inference even if no `module-info.java` is
///   present (e.g. when the build tool is configured with explicit `--module-path` flags).
pub(crate) fn infer_module_path_for_compile_config(
    resolved_compile_classpath: &[PathBuf],
    main_source_roots: &[PathBuf],
    main_output_dir: Option<&PathBuf>,
    compiler_args_looks_like_jpms: bool,
) -> Vec<PathBuf> {
    let should_infer_module_path =
        compiler_args_looks_like_jpms || main_source_roots_have_module_info(main_source_roots);
    if !should_infer_module_path {
        return Vec::new();
    }

    let mut module_path: Vec<PathBuf> = resolved_compile_classpath
        .iter()
        .filter(|entry| {
            if main_output_dir.is_some_and(|out| out == *entry) {
                return false;
            }
            stable_module_path_entry(entry)
        })
        .cloned()
        .collect();

    dedupe_paths(&mut module_path);
    module_path
}

pub(crate) fn infer_module_path_entries(classpath: &[PathBuf]) -> Vec<PathBuf> {
    let mut module_path = Vec::new();
    for entry in classpath {
        if stable_module_path_entry(entry) {
            module_path.push(entry.clone());
        }
    }
    dedupe_paths(&mut module_path);
    module_path
}

/// Returns `true` if the path is a stable JPMS module:
/// - a jar/directory containing `module-info.class`, or
/// - a jar/directory whose manifest contains `Automatic-Module-Name`.
pub(crate) fn stable_module_path_entry(path: &Path) -> bool {
    if path.is_dir() {
        return directory_contains_module_info(path) || directory_has_automatic_module_name(path);
    }
    if !path.is_file() {
        return false;
    }

    archive_is_stable_module(path)
}

fn dedupe_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
}

fn directory_contains_module_info(dir: &Path) -> bool {
    MODULE_INFO_CLASS_CANDIDATES
        .iter()
        .any(|candidate| dir.join(candidate).is_file())
}

fn directory_has_automatic_module_name(dir: &Path) -> bool {
    for manifest_path in MANIFEST_CANDIDATES {
        let manifest_path = dir.join(manifest_path);
        let Ok(bytes) = std::fs::read(&manifest_path) else {
            continue;
        };
        let manifest = String::from_utf8_lossy(&bytes);
        if manifest_main_attribute(&manifest, "Automatic-Module-Name")
            .is_some_and(|name| !name.is_empty())
        {
            return true;
        }
    }

    false
}

fn archive_is_stable_module(path: &Path) -> bool {
    fn probe_archive<R: Read + Seek>(archive: &mut ZipArchive<R>) -> (bool, bool) {
        let mut had_error = false;

        for candidate in MODULE_INFO_CLASS_CANDIDATES {
            match archive.by_name(candidate) {
                Ok(_) => return (true, had_error),
                Err(zip::result::ZipError::FileNotFound) => continue,
                Err(_) => had_error = true,
            }
        }

        for manifest_path in MANIFEST_CANDIDATES {
            let mut file = match archive.by_name(manifest_path) {
                Ok(file) => file,
                Err(zip::result::ZipError::FileNotFound) => continue,
                Err(_) => {
                    had_error = true;
                    continue;
                }
            };

            let mut bytes = Vec::with_capacity(file.size() as usize);
            if file.read_to_end(&mut bytes).is_err() {
                had_error = true;
                continue;
            }
            let manifest = String::from_utf8_lossy(&bytes);
            if manifest_main_attribute(&manifest, "Automatic-Module-Name")
                .is_some_and(|name| !name.is_empty())
            {
                return (true, had_error);
            }
        }

        (false, had_error)
    }

    fn is_jmod_magic(file: &mut File) -> bool {
        let mut header = [0u8; 2];
        let ok = file.read_exact(&mut header).is_ok() && header == *b"JM";
        let _ = file.seek(SeekFrom::Start(0));
        ok
    }

    // First attempt: interpret the file as a normal zip (works for jars and for some jmod variants
    // where the central directory offsets are relative to the file start, even if the file has a
    // `JM<version>` preamble).
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let is_jmod_magic = is_jmod_magic(&mut file);

    if let Ok(mut archive) = ZipArchive::new(file) {
        let (found, had_error) = probe_archive(&mut archive);
        if found {
            return true;
        }
        // For `.jmod`-style archives with a `JM<version>` header, the zip offsets can also be
        // relative to the start of the embedded zip payload (after the 4-byte header). When the
        // initial probe fails due to archive parsing errors, retry with an offset reader.
        if !is_jmod_magic || !had_error {
            return false;
        }
    } else if !is_jmod_magic {
        return false;
    }

    // JMOD fallback: interpret zip offsets relative to the start of the embedded zip payload
    // (after the `JM<version>` header).
    let Ok(file) = File::open(path) else {
        return false;
    };
    let Ok(reader) = OffsetReader::new(file, JMOD_HEADER_LEN) else {
        return false;
    };
    let Ok(mut archive) = ZipArchive::new(reader) else {
        return false;
    };
    probe_archive(&mut archive).0
}

fn manifest_main_attribute(manifest: &str, key: &str) -> Option<String> {
    let mut current_key: Option<&str> = None;
    let mut current_value = String::new();

    for line in manifest.lines() {
        let line = line.trim_end_matches('\r');

        // The first empty line terminates the main attributes section.
        if line.is_empty() {
            break;
        }

        if let Some(rest) = line.strip_prefix(' ') {
            if current_key.is_some() {
                current_value.push_str(rest);
            }
            continue;
        }

        if let Some(k) = current_key.take() {
            if k.trim().eq_ignore_ascii_case(key) {
                return Some(current_value.trim().to_string());
            }
        }
        current_value.clear();

        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        current_key = Some(k);
        current_value.push_str(v.trim_start());
    }

    if let Some(k) = current_key {
        if k.trim().eq_ignore_ascii_case(key) {
            return Some(current_value.trim().to_string());
        }
    }

    None
}

struct OffsetReader<R> {
    inner: R,
    base: u64,
}

impl<R> OffsetReader<R>
where
    R: Seek,
{
    fn new(mut inner: R, base: u64) -> std::io::Result<Self> {
        inner.seek(SeekFrom::Start(base))?;
        Ok(Self { inner, base })
    }
}

impl<R> Read for OffsetReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R> Seek for OffsetReader<R>
where
    R: Seek,
{
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let base = self.base;
        let adjusted = match pos {
            SeekFrom::Start(offset) => SeekFrom::Start(offset.checked_add(base).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek overflow")
            })?),
            SeekFrom::End(offset) => SeekFrom::End(offset),
            SeekFrom::Current(offset) => SeekFrom::Current(offset),
        };

        let absolute = self.inner.seek(adjusted)?;
        absolute.checked_sub(base).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before archive start",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::io::Write;
    use std::path::Path;
    use zip::write::FileOptions;
    use zip::ZipWriter;

    #[test]
    fn stable_module_path_entry_detects_jars() {
        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jar");
        let automatic = testdata_dir.join("automatic-module-name-1.2.3.jar");
        let dep = testdata_dir.join("dep.jar");

        assert!(stable_module_path_entry(&named));
        assert!(stable_module_path_entry(&automatic));
        assert!(!stable_module_path_entry(&dep));

        let inferred =
            infer_module_path_entries(&[named.clone(), dep, automatic.clone(), named.clone()]);
        assert_eq!(inferred, vec![named, automatic]);
    }

    #[test]
    fn stable_module_path_entry_detects_jmods() {
        let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
        let named = testdata_dir.join("named-module.jmod");
        let dep = testdata_dir.join("dep.jar");

        assert!(stable_module_path_entry(&named));
        let inferred = infer_module_path_entries(&[dep, named.clone()]);
        assert_eq!(inferred, vec![named]);
    }

    #[test]
    fn stable_module_path_entry_detects_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let explicit = root.join("explicit");
        std::fs::create_dir_all(&explicit).unwrap();
        std::fs::write(explicit.join("module-info.class"), b"cafebabe").unwrap();
        assert!(stable_module_path_entry(&explicit));

        let automatic = root.join("automatic");
        std::fs::create_dir_all(automatic.join("META-INF")).unwrap();
        std::fs::write(
            automatic.join("META-INF").join("MANIFEST.MF"),
            "Manifest-Version: 1.0\nAutomatic-Module-Name: com.example.foo\n\n",
        )
        .unwrap();
        assert!(stable_module_path_entry(&automatic));

        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(!stable_module_path_entry(&plain));

        // Exploded JMOD-style layout (`classes/module-info.class`).
        let exploded = root.join("exploded");
        std::fs::create_dir_all(exploded.join("classes")).unwrap();
        std::fs::write(
            exploded.join("classes").join("module-info.class"),
            b"cafebabe",
        )
        .unwrap();
        assert!(stable_module_path_entry(&exploded));

        // Exploded JMOD-style manifest (`classes/META-INF/MANIFEST.MF`).
        let exploded_manifest = root.join("exploded-manifest");
        std::fs::create_dir_all(exploded_manifest.join("classes").join("META-INF")).unwrap();
        std::fs::write(
            exploded_manifest
                .join("classes")
                .join("META-INF")
                .join("MANIFEST.MF"),
            "Manifest-Version: 1.0\nAutomatic-Module-Name: com.example.exploded\n\n",
        )
        .unwrap();
        assert!(stable_module_path_entry(&exploded_manifest));

        // Multi-release module-info (`META-INF/versions/9/module-info.class`).
        let multirelease = root.join("multirelease");
        std::fs::create_dir_all(multirelease.join("META-INF").join("versions").join("9")).unwrap();
        std::fs::write(
            multirelease
                .join("META-INF")
                .join("versions")
                .join("9")
                .join("module-info.class"),
            b"cafebabe",
        )
        .unwrap();
        assert!(stable_module_path_entry(&multirelease));

        // Exploded JMOD + multi-release layout (`classes/META-INF/versions/9/module-info.class`).
        let exploded_multirelease = root.join("exploded-multirelease");
        std::fs::create_dir_all(
            exploded_multirelease
                .join("classes")
                .join("META-INF")
                .join("versions")
                .join("9"),
        )
        .unwrap();
        std::fs::write(
            exploded_multirelease
                .join("classes")
                .join("META-INF")
                .join("versions")
                .join("9")
                .join("module-info.class"),
            b"cafebabe",
        )
        .unwrap();
        assert!(stable_module_path_entry(&exploded_multirelease));
    }

    #[test]
    fn stable_module_path_entry_detects_archives_with_nonstandard_layouts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let options = FileOptions::default();

        // Multi-release module-info (`META-INF/versions/9/module-info.class`).
        let mr_jar = root.join("mr.jar");
        {
            let file = std::fs::File::create(&mr_jar).unwrap();
            let mut zip = ZipWriter::new(file);
            zip.start_file("META-INF/versions/9/module-info.class", options)
                .unwrap();
            zip.write_all(b"cafebabe").unwrap();
            zip.finish().unwrap();
        }
        assert!(stable_module_path_entry(&mr_jar));

        // Exploded JMOD + multi-release module-info (`classes/META-INF/versions/9/module-info.class`).
        let exploded_mr_jar = root.join("exploded-mr.jar");
        {
            let file = std::fs::File::create(&exploded_mr_jar).unwrap();
            let mut zip = ZipWriter::new(file);
            zip.start_file("classes/META-INF/versions/9/module-info.class", options)
                .unwrap();
            zip.write_all(b"cafebabe").unwrap();
            zip.finish().unwrap();
        }
        assert!(stable_module_path_entry(&exploded_mr_jar));

        // Exploded-jmod style manifest inside an archive (`classes/META-INF/MANIFEST.MF`).
        let classes_manifest_jar = root.join("classes-manifest.jar");
        {
            let file = std::fs::File::create(&classes_manifest_jar).unwrap();
            let mut zip = ZipWriter::new(file);
            zip.start_file("classes/META-INF/MANIFEST.MF", options)
                .unwrap();
            zip.write_all(
                "Manifest-Version: 1.0\nAutomatic-Module-Name: com.example.jar\n\n".as_bytes(),
            )
            .unwrap();
            zip.finish().unwrap();
        }
        assert!(stable_module_path_entry(&classes_manifest_jar));
    }

    #[test]
    fn stable_module_path_entry_detects_jmods_with_magic_header() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create a valid zip payload that contains a JPMS marker in the normal jmod location.
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut cursor);
            let options = FileOptions::default();
            zip.start_file("classes/module-info.class", options)
                .unwrap();
            zip.write_all(b"cafebabe").unwrap();
            zip.finish().unwrap();
        }
        let zip_bytes = cursor.into_inner();

        // Prefix with `JM<version>` header so the zip payload begins at offset 4.
        let path = root.join("demo.jmod");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"JM\x01\x00");
        bytes.extend_from_slice(&zip_bytes);
        std::fs::write(&path, bytes).unwrap();

        assert!(
            stable_module_path_entry(&path),
            "expected `JM<version>`-prefixed jmod archives to be detected as stable modules"
        );
    }

    #[test]
    fn compiler_args_looks_like_jpms_handles_short_and_long_flags() {
        assert!(compiler_args_looks_like_jpms(
            &["--module-path".to_string()]
        ));
        assert!(compiler_args_looks_like_jpms(&[
            "--module-path=/tmp".to_string()
        ]));
        assert!(compiler_args_looks_like_jpms(&["-p".to_string()]));

        assert!(compiler_args_looks_like_jpms(&["--module".to_string()]));
        assert!(compiler_args_looks_like_jpms(&["-m".to_string()]));

        assert!(!compiler_args_looks_like_jpms(&[
            "-processorpath".to_string()
        ]));
    }

    #[test]
    fn manifest_main_attribute_handles_continuation_lines_and_stops_at_end_of_main_section() {
        // Note: Don't use `\` line continuations in this string literal; Rust will strip the
        // leading space on continuation lines which would defeat the manifest continuation test.
        let manifest = "Manifest-Version: 1.0\n\
Automatic-Module-Name: com.example.\n foo\n\n\
Name: ignored\n\
X-Other: also-ignored\n";

        assert_eq!(
            manifest_main_attribute(manifest, "Automatic-Module-Name"),
            Some("com.example.foo".to_string())
        );

        // Main attributes parsing should stop at the first empty line.
        assert_eq!(manifest_main_attribute(manifest, "Name"), None);
    }

    #[test]
    fn main_source_roots_have_module_info_checks_for_source_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let src = root.join("src/main/java");
        std::fs::create_dir_all(&src).unwrap();
        assert!(!main_source_roots_have_module_info(std::slice::from_ref(
            &src
        )));
        std::fs::write(src.join("module-info.java"), "module demo {}".as_bytes()).unwrap();
        assert!(main_source_roots_have_module_info(std::slice::from_ref(
            &src
        )));
    }
}

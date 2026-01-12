use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use walkdir::WalkDir;

use crate::discover::{LoadOptions, ProjectError};
use crate::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, Dependency, JavaConfig, JavaLanguageLevel,
    JavaVersion, LanguageLevelProvenance, Module, ModuleLanguageLevel, OutputDir, OutputDirKind,
    ProjectConfig, SourceRoot, SourceRootKind, SourceRootOrigin, WorkspaceModuleBuildId,
    WorkspaceModuleConfig, WorkspaceProjectModel,
};

pub(crate) fn load_gradle_project(
    root: &Path,
    options: &LoadOptions,
) -> Result<ProjectConfig, ProjectError> {
    let settings_path = ["settings.gradle.kts", "settings.gradle"]
        .into_iter()
        .map(|name| root.join(name))
        .find(|p| p.is_file());

    let module_names = if let Some(settings_path) = settings_path {
        let contents =
            std::fs::read_to_string(&settings_path).map_err(|source| ProjectError::Io {
                path: settings_path.clone(),
                source,
            })?;
        parse_gradle_settings_modules(&contents)
    } else {
        vec![".".to_string()]
    };

    let mut modules = Vec::new();
    let mut source_roots = Vec::new();
    let mut output_dirs = Vec::new();
    let mut dependencies = Vec::new();
    let mut classpath = Vec::new();
    let mut dependency_entries = Vec::new();

    // Best-effort Gradle cache resolution. This does not execute Gradle; it only
    // adds jars that already exist in the local Gradle cache.
    let gradle_user_home = options
        .gradle_user_home
        .clone()
        .or_else(default_gradle_user_home);

    // Best-effort: parse Java level and deps from build scripts.
    let root_java = parse_gradle_java_config(root).unwrap_or_default();

    for module_name in module_names {
        let module_root = if module_name == "." {
            root.to_path_buf()
        } else {
            root.join(&module_name)
        };
        let module_display_name = if module_name == "." {
            root.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("root")
                .to_string()
        } else {
            module_name.clone()
        };

        modules.push(Module {
            name: module_display_name,
            root: module_root.clone(),
            annotation_processing: Default::default(),
        });

        let _module_java = parse_gradle_java_config(&module_root).unwrap_or(root_java);

        push_source_root(
            &mut source_roots,
            &module_root,
            SourceRootKind::Main,
            "src/main/java",
        );
        push_source_root(
            &mut source_roots,
            &module_root,
            SourceRootKind::Test,
            "src/test/java",
        );
        crate::generated::append_generated_source_roots(
            &mut source_roots,
            root,
            &module_root,
            BuildSystem::Gradle,
            &options.nova_config,
        );

        let main_output = module_root.join("build/classes/java/main");
        let test_output = module_root.join("build/classes/java/test");

        output_dirs.push(OutputDir {
            kind: OutputDirKind::Main,
            path: main_output.clone(),
        });
        output_dirs.push(OutputDir {
            kind: OutputDirKind::Test,
            path: test_output.clone(),
        });

        classpath.push(ClasspathEntry {
            kind: ClasspathEntryKind::Directory,
            path: main_output,
        });
        classpath.push(ClasspathEntry {
            kind: ClasspathEntryKind::Directory,
            path: test_output,
        });

        // Dependency extraction is best-effort; useful for later external jar resolution.
        dependencies.extend(parse_gradle_dependencies(&module_root));
    }

    // Sort/dedup before resolving jars so we don't scan the cache repeatedly for
    // the same coordinates.
    sort_dedup_dependencies(&mut dependencies);

    // Best-effort jar discovery for pinned Maven coordinates already present in
    // the Gradle cache (no transitive resolution / variants / etc).
    if let Some(gradle_user_home) = gradle_user_home.as_deref() {
        for dep in &dependencies {
            for jar_path in gradle_dependency_jar_paths(gradle_user_home, dep) {
                dependency_entries.push(ClasspathEntry {
                    kind: ClasspathEntryKind::Jar,
                    path: jar_path,
                });
            }
        }
    }

    // Add user-provided classpath entries for unresolved dependencies (Gradle).
    for entry in &options.classpath_overrides {
        dependency_entries.push(ClasspathEntry {
            kind: if entry.extension().is_some_and(|ext| ext == "jar") {
                ClasspathEntryKind::Jar
            } else {
                ClasspathEntryKind::Directory
            },
            path: entry.clone(),
        });
    }

    sort_dedup_source_roots(&mut source_roots);
    sort_dedup_output_dirs(&mut output_dirs);
    sort_dedup_classpath(&mut dependency_entries);
    sort_dedup_classpath(&mut classpath);
    // `dependencies` was already sorted/deduped above.

    let jpms_modules = crate::jpms::discover_jpms_modules(&modules);
    let (mut module_path, classpath_deps) =
        crate::jpms::classify_dependency_entries(&jpms_modules, dependency_entries);
    classpath.extend(classpath_deps);
    sort_dedup_classpath(&mut module_path);
    sort_dedup_classpath(&mut classpath);
    let jpms_workspace = crate::jpms::build_jpms_workspace(&jpms_modules, &module_path);

    Ok(ProjectConfig {
        workspace_root: root.to_path_buf(),
        build_system: BuildSystem::Gradle,
        java: root_java,
        modules,
        jpms_modules,
        jpms_workspace,
        source_roots,
        module_path,
        classpath,
        output_dirs,
        dependencies,
        workspace_model: None,
    })
}

pub(crate) fn load_gradle_workspace_model(
    root: &Path,
    options: &LoadOptions,
) -> Result<WorkspaceProjectModel, ProjectError> {
    let settings_path = ["settings.gradle.kts", "settings.gradle"]
        .into_iter()
        .map(|name| root.join(name))
        .find(|p| p.is_file());

    let module_refs = if let Some(settings_path) = settings_path {
        let contents =
            std::fs::read_to_string(&settings_path).map_err(|source| ProjectError::Io {
                path: settings_path.clone(),
                source,
            })?;
        parse_gradle_settings_projects(&contents)
    } else {
        vec![GradleModuleRef::root()]
    };

    let (root_java, root_java_provenance) = match parse_gradle_java_config_with_path(root) {
        Some((java, path)) => (java, LanguageLevelProvenance::BuildFile(path)),
        None => (JavaConfig::default(), LanguageLevelProvenance::Default),
    };

    // Best-effort Gradle cache resolution. This does not execute Gradle; it only
    // adds jars that already exist in the local Gradle cache.
    let gradle_user_home = options
        .gradle_user_home
        .clone()
        .or_else(default_gradle_user_home);

    let mut module_configs = Vec::new();
    for module_ref in module_refs {
        let module_root = if module_ref.dir_rel == "." {
            root.to_path_buf()
        } else {
            root.join(&module_ref.dir_rel)
        };

        let module_display_name = if module_ref.dir_rel == "." {
            root.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("root")
                .to_string()
        } else {
            module_ref
                .project_path
                .trim_start_matches(':')
                .rsplit(':')
                .next()
                .unwrap_or(&module_ref.project_path)
                .to_string()
        };

        let (module_java, provenance) = match parse_gradle_java_config_with_path(&module_root) {
            Some((java, path)) => (java, LanguageLevelProvenance::BuildFile(path)),
            None => (root_java, root_java_provenance.clone()),
        };

        let language_level = ModuleLanguageLevel {
            level: JavaLanguageLevel::from_java_config(module_java),
            provenance,
        };

        let mut source_roots = Vec::new();
        push_source_root(
            &mut source_roots,
            &module_root,
            SourceRootKind::Main,
            "src/main/java",
        );
        push_source_root(
            &mut source_roots,
            &module_root,
            SourceRootKind::Test,
            "src/test/java",
        );
        crate::generated::append_generated_source_roots(
            &mut source_roots,
            root,
            &module_root,
            BuildSystem::Gradle,
            &options.nova_config,
        );

        let main_output = module_root.join("build/classes/java/main");
        let test_output = module_root.join("build/classes/java/test");
        let mut output_dirs = vec![
            OutputDir {
                kind: OutputDirKind::Main,
                path: main_output.clone(),
            },
            OutputDir {
                kind: OutputDirKind::Test,
                path: test_output.clone(),
            },
        ];

        let mut classpath = vec![
            ClasspathEntry {
                kind: ClasspathEntryKind::Directory,
                path: main_output,
            },
            ClasspathEntry {
                kind: ClasspathEntryKind::Directory,
                path: test_output,
            },
        ];

        for entry in &options.classpath_overrides {
            classpath.push(ClasspathEntry {
                kind: if entry.extension().is_some_and(|ext| ext == "jar") {
                    ClasspathEntryKind::Jar
                } else {
                    ClasspathEntryKind::Directory
                },
                path: entry.clone(),
            });
        }

        let mut dependencies = parse_gradle_dependencies(&module_root);

        // Sort/dedup before resolving jars so we don't scan the cache repeatedly
        // for the same coordinates.
        sort_dedup_dependencies(&mut dependencies);

        // Best-effort jar discovery for pinned Maven coordinates already present
        // in the Gradle cache (no transitive resolution / variants / etc).
        if let Some(gradle_user_home) = gradle_user_home.as_deref() {
            for dep in &dependencies {
                for jar_path in gradle_dependency_jar_paths(gradle_user_home, dep) {
                    classpath.push(ClasspathEntry {
                        kind: ClasspathEntryKind::Jar,
                        path: jar_path,
                    });
                }
            }
        }

        sort_dedup_source_roots(&mut source_roots);
        sort_dedup_output_dirs(&mut output_dirs);
        sort_dedup_classpath(&mut classpath);
        // `dependencies` was already sorted/deduped above.

        module_configs.push(WorkspaceModuleConfig {
            id: format!("gradle:{}", module_ref.project_path),
            name: module_display_name,
            root: module_root,
            build_id: WorkspaceModuleBuildId::Gradle {
                project_path: module_ref.project_path,
            },
            language_level,
            source_roots,
            output_dirs,
            module_path: Vec::new(),
            classpath,
            dependencies,
        });
    }

    let modules_for_jpms = module_configs
        .iter()
        .map(|module| Module {
            name: module.name.clone(),
            root: module.root.clone(),
            annotation_processing: Default::default(),
        })
        .collect::<Vec<_>>();
    let jpms_modules = crate::jpms::discover_jpms_modules(&modules_for_jpms);

    Ok(WorkspaceProjectModel::new(
        root.to_path_buf(),
        BuildSystem::Gradle,
        root_java,
        module_configs,
        jpms_modules,
    ))
}

fn parse_gradle_settings_modules(contents: &str) -> Vec<String> {
    // `ProjectConfig` module roots are directory-relative; reuse the more robust project parser.
    parse_gradle_settings_projects(contents)
        .into_iter()
        .map(|m| m.dir_rel)
        .collect()
}

#[derive(Debug, Clone)]
struct GradleModuleRef {
    project_path: String,
    dir_rel: String,
}

impl GradleModuleRef {
    fn root() -> Self {
        Self {
            project_path: ":".to_string(),
            dir_rel: ".".to_string(),
        }
    }
}

fn parse_gradle_settings_projects(contents: &str) -> Vec<GradleModuleRef> {
    let contents = strip_gradle_comments(contents);

    let included = parse_gradle_settings_included_projects(&contents);
    if included.is_empty() {
        return vec![GradleModuleRef::root()];
    }

    let overrides = parse_gradle_settings_project_dir_overrides(&contents);

    // Deterministic + dedup: module refs are sorted by Gradle project path.
    let included: BTreeSet<_> = included.into_iter().collect();

    included
        .into_iter()
        .map(|project_path| {
            let dir_rel = overrides
                .get(&project_path)
                .cloned()
                .unwrap_or_else(|| heuristic_dir_rel_for_project_path(&project_path));

            GradleModuleRef {
                project_path,
                dir_rel,
            }
        })
        .collect()
}

fn extract_quoted_strings(text: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"['"]([^'"]+)['"]"#).expect("valid regex"));

    re.captures_iter(text)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

fn strip_gradle_comments(contents: &str) -> String {
    // Best-effort comment stripping to avoid parsing commented-out `include`/`projectDir` lines.
    // This is intentionally conservative and only strips:
    // - `// ...` to end-of-line
    // - `/* ... */` block comments
    // while preserving quoted strings (`'...'` / `"..."`).
    let bytes = contents.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());

    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
                out.push(b'\n');
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if in_single {
            out.push(b);
            if b == b'\\' {
                if let Some(next) = bytes.get(i + 1) {
                    out.push(*next);
                    i += 2;
                    continue;
                }
            } else if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        if in_double {
            out.push(b);
            if b == b'\\' {
                if let Some(next) = bytes.get(i + 1) {
                    out.push(*next);
                    i += 2;
                    continue;
                }
            } else if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            in_line_comment = true;
            i += 2;
            continue;
        }

        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            in_block_comment = true;
            i += 2;
            continue;
        }

        if b == b'\'' {
            in_single = true;
            out.push(b'\'');
            i += 1;
            continue;
        }

        if b == b'"' {
            in_double = true;
            out.push(b'"');
            i += 1;
            continue;
        }

        out.push(b);
        i += 1;
    }

    String::from_utf8(out).unwrap_or_else(|_| contents.to_string())
}

fn normalize_project_path(project_path: &str) -> String {
    let project_path = project_path.trim();
    if project_path.is_empty() || project_path == ":" {
        return ":".to_string();
    }
    if project_path.starts_with(':') {
        project_path.to_string()
    } else {
        format!(":{project_path}")
    }
}

fn heuristic_dir_rel_for_project_path(project_path: &str) -> String {
    let dir_rel = project_path.trim_start_matches(':').replace(':', "/");
    if dir_rel.trim().is_empty() {
        ".".to_string()
    } else {
        dir_rel
    }
}

fn normalize_dir_rel(dir_rel: &str) -> Option<String> {
    let mut dir_rel = dir_rel.trim().replace('\\', "/");
    while let Some(stripped) = dir_rel.strip_prefix("./") {
        dir_rel = stripped.to_string();
    }
    while dir_rel.ends_with('/') {
        dir_rel.pop();
    }

    if dir_rel.is_empty() {
        return Some(".".to_string());
    }

    // Avoid accidentally escaping the workspace root by joining with an absolute path.
    let is_absolute_unix = dir_rel.starts_with('/');
    let is_windows_drive = dir_rel.as_bytes().get(1).is_some_and(|b| *b == b':')
        && dir_rel
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic());
    if is_absolute_unix || is_windows_drive {
        return None;
    }

    Some(dir_rel)
}

fn parse_gradle_settings_included_projects(contents: &str) -> Vec<String> {
    static INCLUDE_RE: OnceLock<Regex> = OnceLock::new();
    let re = INCLUDE_RE.get_or_init(|| Regex::new(r"\binclude\b").expect("valid regex"));

    let mut projects = Vec::new();

    for m in re.find_iter(contents) {
        let mut idx = m.end();
        let bytes = contents.as_bytes();
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() {
            continue;
        }

        let args = if bytes[idx] == b'(' {
            extract_balanced_parens(contents, idx)
                .map(|(args, _end)| args)
                .unwrap_or_default()
        } else {
            extract_unparenthesized_args_until_eol_or_continuation(contents, idx)
        };

        projects.extend(
            extract_quoted_strings(&args)
                .into_iter()
                .map(|s| normalize_project_path(&s)),
        );
    }

    projects
}

fn extract_balanced_parens(contents: &str, open_paren_index: usize) -> Option<(String, usize)> {
    let bytes = contents.as_bytes();
    if bytes.get(open_paren_index) != Some(&b'(') {
        return None;
    }

    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    let mut i = open_paren_index;
    while i < bytes.len() {
        let b = bytes[i];

        if in_single {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        match b {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
                if depth == 0 {
                    let args = &contents[open_paren_index + 1..i - 1];
                    return Some((args.to_string(), i));
                }
            }
            _ => i += 1,
        }
    }

    None
}

fn extract_unparenthesized_args_until_eol_or_continuation(contents: &str, start: usize) -> String {
    // Groovy allows method calls without parentheses:
    //   include ':app', ':lib'
    // and can span lines after commas:
    //   include ':app',
    //           ':lib'
    let len = contents.len();
    let mut cursor = start;

    loop {
        let rest = &contents[cursor..];
        let line_break = rest.find('\n').map(|off| cursor + off).unwrap_or(len);
        let line = &contents[cursor..line_break];
        if line.trim_end().ends_with(',') && line_break < len {
            cursor = line_break + 1;
            continue;
        }
        return contents[start..line_break].to_string();
    }
}

fn parse_gradle_settings_project_dir_overrides(contents: &str) -> BTreeMap<String, String> {
    // Common overrides:
    //   project(':app').projectDir = file('modules/app')
    //   project(':lib').projectDir = new File(settingsDir, 'modules/lib')
    //   project(":app").projectDir = file("modules/app") (Kotlin DSL)
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"(?x)
                \bproject\s*\(\s*['"](?P<project>[^'"]+)['"]\s*\)
                \s*\.\s*projectDir\s*=\s*
                (?:
                    file\s*\(\s*['"](?P<file_dir>[^'"]+)['"]\s*\)
                  |
                    (?:new\s+)?(?:java\.io\.)?File\s*\(\s*settingsDir\s*,\s*['"](?P<settings_dir>[^'"]+)['"]\s*\)
                )
            "#,
        )
        .expect("valid regex")
    });

    let mut overrides = BTreeMap::new();
    for caps in re.captures_iter(contents) {
        let project_path = normalize_project_path(&caps["project"]);
        let dir_rel = caps
            .name("file_dir")
            .or_else(|| caps.name("settings_dir"))
            .map(|m| m.as_str())
            .and_then(normalize_dir_rel);
        let Some(dir_rel) = dir_rel else {
            continue;
        };
        overrides.insert(project_path, dir_rel);
    }
    overrides
}

fn parse_gradle_java_config(root: &Path) -> Option<JavaConfig> {
    let candidates = ["build.gradle.kts", "build.gradle"]
        .into_iter()
        .map(|name| root.join(name))
        .filter(|p| p.is_file())
        .collect::<Vec<_>>();

    for path in candidates {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Some(java) = extract_java_config_from_build_script(&contents) {
                return Some(java);
            }
        }
    }

    None
}

fn parse_gradle_java_config_with_path(root: &Path) -> Option<(JavaConfig, PathBuf)> {
    let candidates = ["build.gradle.kts", "build.gradle"]
        .into_iter()
        .map(|name| root.join(name))
        .filter(|p| p.is_file())
        .collect::<Vec<_>>();

    for path in candidates {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Some(java) = extract_java_config_from_build_script(&contents) {
                return Some((java, path));
            }
        }
    }

    None
}

fn extract_java_config_from_build_script(contents: &str) -> Option<JavaConfig> {
    let mut source = None;
    let mut target = None;

    for line in contents.lines() {
        if source.is_none() {
            source = parse_java_version_assignment(line, "sourceCompatibility");
        }
        if target.is_none() {
            target = parse_java_version_assignment(line, "targetCompatibility");
        }
        if source.is_some() && target.is_some() {
            break;
        }
    }

    match (source, target) {
        (Some(source), Some(target)) => Some(JavaConfig {
            source,
            target,
            enable_preview: false,
        }),
        (Some(v), None) | (None, Some(v)) => Some(JavaConfig {
            source: v,
            target: v,
            enable_preview: false,
        }),
        (None, None) => None,
    }
}

fn parse_java_version_assignment(line: &str, key: &str) -> Option<JavaVersion> {
    let line = line.trim();
    let rest = line.strip_prefix(key)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();

    if let Some(rest) = rest.strip_prefix("JavaVersion.VERSION_") {
        let normalized = rest.trim().replace('_', ".");
        return JavaVersion::parse(&normalized);
    }

    let rest = rest.trim();
    let rest = rest
        .strip_prefix('"')
        .and_then(|v| v.split_once('"').map(|(head, _)| head))
        .or_else(|| {
            rest.strip_prefix('\'')
                .and_then(|v| v.split_once('\'').map(|(head, _)| head))
        })
        .unwrap_or(rest);

    JavaVersion::parse(rest)
}

fn parse_gradle_dependencies(module_root: &Path) -> Vec<Dependency> {
    let candidates = ["build.gradle.kts", "build.gradle"]
        .into_iter()
        .map(|name| module_root.join(name))
        .filter(|p| p.is_file())
        .collect::<Vec<_>>();

    let mut out = Vec::new();
    for path in candidates {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };

        out.extend(parse_gradle_dependencies_from_text(&contents));
    }
    out
}

fn parse_gradle_dependencies_from_text(contents: &str) -> Vec<Dependency> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"(?i)\b(?:implementation|api|compileOnly|runtimeOnly|testImplementation)\s*\(?\s*['"]([^:'"]+):([^:'"]+):([^'"]+)['"]"#)
            .expect("valid regex")
    });

    let mut deps = Vec::new();
    for caps in re.captures_iter(contents) {
        deps.push(Dependency {
            group_id: caps[1].to_string(),
            artifact_id: caps[2].to_string(),
            version: Some(caps[3].to_string()),
            scope: None,
            classifier: None,
            type_: None,
        });
    }
    deps
}

fn default_gradle_user_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("GRADLE_USER_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home));
    }

    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)?;
    Some(home.join(".gradle"))
}

/// Best-effort jar discovery for Gradle dependencies.
///
/// This does **not** run Gradle and does **not** resolve transitive dependencies
/// or perform variant/attribute selection. It only attempts to locate jar files
/// for explicitly-versioned Maven coordinates that already exist in the local
/// Gradle cache.
fn gradle_dependency_jar_paths(gradle_user_home: &Path, dep: &Dependency) -> Vec<PathBuf> {
    let Some(version) = dep.version.as_deref() else {
        return Vec::new();
    };
    if dep.group_id.is_empty() || dep.artifact_id.is_empty() || version.is_empty() {
        return Vec::new();
    }

    let base = gradle_user_home
        .join("caches/modules-2/files-2.1")
        .join(&dep.group_id)
        .join(&dep.artifact_id)
        .join(version);
    if !base.is_dir() {
        return Vec::new();
    }

    let prefix = format!("{}-{}", dep.artifact_id, version);

    let mut preferred = Vec::new();
    let mut others = Vec::new();

    for entry in WalkDir::new(&base).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();

        if !is_jar_path(&path) || is_auxiliary_gradle_jar(&path) {
            continue;
        }

        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();

        if file_name.starts_with(&prefix) {
            preferred.push(path);
        } else {
            others.push(path);
        }
    }

    let mut out = if !preferred.is_empty() {
        preferred
    } else {
        others
    };
    out.sort();
    out.dedup();
    out
}

fn is_jar_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
}

fn is_auxiliary_gradle_jar(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    name.ends_with("-sources.jar") || name.ends_with("-javadoc.jar")
}

fn push_source_root(
    out: &mut Vec<SourceRoot>,
    module_root: &Path,
    kind: SourceRootKind,
    rel: &str,
) {
    let path = module_root.join(rel);
    if path.is_dir() {
        out.push(SourceRoot {
            kind,
            origin: SourceRootOrigin::Source,
            path,
        });
    }
}

fn sort_dedup_source_roots(roots: &mut Vec<SourceRoot>) {
    roots.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.kind.cmp(&b.kind))
            .then(a.origin.cmp(&b.origin))
    });
    roots.dedup_by(|a, b| a.kind == b.kind && a.origin == b.origin && a.path == b.path);
}

fn sort_dedup_output_dirs(dirs: &mut Vec<OutputDir>) {
    dirs.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    dirs.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
}

fn sort_dedup_classpath(entries: &mut Vec<ClasspathEntry>) {
    entries.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
    entries.dedup_by(|a, b| a.kind == b.kind && a.path == b.path);
}

fn sort_dedup_dependencies(deps: &mut Vec<Dependency>) {
    deps.sort_by(|a, b| {
        a.group_id
            .cmp(&b.group_id)
            .then(a.artifact_id.cmp(&b.artifact_id))
            .then(a.version.cmp(&b.version))
    });
    deps.dedup();
}

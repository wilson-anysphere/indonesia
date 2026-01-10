use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::discover::{LoadOptions, ProjectError};
use crate::{
    BuildSystem, ClasspathEntry, ClasspathEntryKind, Dependency, JavaConfig, JavaVersion, Module,
    OutputDir, OutputDirKind, ProjectConfig, SourceRoot, SourceRootKind, SourceRootOrigin,
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
        let contents = std::fs::read_to_string(&settings_path).map_err(|source| ProjectError::Io {
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
        });

        let _module_java = parse_gradle_java_config(&module_root).unwrap_or(root_java);

        push_source_root(&mut source_roots, &module_root, SourceRootKind::Main, "src/main/java");
        push_source_root(&mut source_roots, &module_root, SourceRootKind::Test, "src/test/java");
        crate::generated::append_generated_source_roots(
            &mut source_roots,
            &module_root,
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

    // Add user-provided classpath entries for unresolved dependencies (Gradle).
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

    sort_dedup_source_roots(&mut source_roots);
    sort_dedup_output_dirs(&mut output_dirs);
    sort_dedup_classpath(&mut classpath);
    sort_dedup_dependencies(&mut dependencies);

    Ok(ProjectConfig {
        workspace_root: root.to_path_buf(),
        build_system: BuildSystem::Gradle,
        java: root_java,
        modules,
        source_roots,
        classpath,
        output_dirs,
        dependencies,
    })
}

fn parse_gradle_settings_modules(contents: &str) -> Vec<String> {
    // Very conservative: look for quoted strings in lines containing `include`.
    let mut modules = Vec::new();
    for line in contents.lines() {
        if !line.contains("include") {
            continue;
        }
        modules.extend(extract_quoted_strings(line).into_iter().map(|s| {
            let s = s.trim();
            let s = s.strip_prefix(':').unwrap_or(s);
            s.replace(':', "/")
        }));
    }

    if modules.is_empty() {
        vec![".".to_string()]
    } else {
        modules
    }
}

fn extract_quoted_strings(text: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"['"]([^'"]+)['"]"#).expect("valid regex"));

    re.captures_iter(text)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .collect()
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
        (Some(source), Some(target)) => Some(JavaConfig { source, target }),
        (Some(v), None) | (None, Some(v)) => Some(JavaConfig {
            source: v,
            target: v,
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

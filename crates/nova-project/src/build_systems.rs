use std::path::{Path, PathBuf};

use nova_build_model::{
    BuildSystemBackend, BuildSystemError, Classpath, PathPattern, ProjectModel,
};

use crate::discover::LoadOptions;

fn common_watch_files() -> Vec<PathPattern> {
    vec![
        // JPMS module graph changes should trigger reload.
        PathPattern::ExactFileName("module-info.java"),
        // Nova configuration files.
        PathPattern::ExactFileName("nova.toml"),
        PathPattern::ExactFileName(".nova.toml"),
        PathPattern::ExactFileName("nova.config.toml"),
        PathPattern::Glob("**/.nova/config.toml"),
        // Generated source roots discovery snapshot.
        PathPattern::Glob("**/.nova/apt-cache/generated-roots.json"),
    ]
}

fn dedup_patterns(patterns: Vec<PathPattern>) -> Vec<PathPattern> {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    patterns
        .into_iter()
        .filter(|pattern| seen.insert(pattern.clone()))
        .collect()
}

#[derive(Debug, Clone)]
pub struct MavenBuildSystem {
    options: LoadOptions,
}

impl MavenBuildSystem {
    pub fn new(options: LoadOptions) -> Self {
        Self { options }
    }
}

impl BuildSystemBackend for MavenBuildSystem {
    fn detect(&self, root: &Path) -> bool {
        root.join("pom.xml").is_file()
    }

    fn parse_project(&self, root: &Path) -> Result<ProjectModel, BuildSystemError> {
        let root = canonicalize_root(root);
        crate::maven::load_maven_workspace_model(&root, &self.options)
            .map_err(BuildSystemError::other)
    }

    fn resolve_classpath(&self, project: &ProjectModel) -> Result<Classpath, BuildSystemError> {
        Ok(Classpath::from_workspace_model_union(project))
    }

    fn watch_files(&self) -> Vec<PathPattern> {
        dedup_patterns(
            vec![
                PathPattern::ExactFileName("pom.xml"),
                PathPattern::ExactFileName("mvnw"),
                PathPattern::ExactFileName("mvnw.cmd"),
                PathPattern::Glob("**/.mvn/wrapper/maven-wrapper.properties"),
                PathPattern::Glob("**/.mvn/wrapper/maven-wrapper.jar"),
                PathPattern::Glob("**/.mvn/extensions.xml"),
                PathPattern::Glob("**/.mvn/maven.config"),
                PathPattern::Glob("**/.mvn/jvm.config"),
            ]
            .into_iter()
            .chain(common_watch_files())
            .collect(),
        )
    }
}

#[derive(Debug, Clone)]
pub struct GradleBuildSystem {
    options: LoadOptions,
}

impl GradleBuildSystem {
    pub fn new(options: LoadOptions) -> Self {
        Self { options }
    }
}

impl BuildSystemBackend for GradleBuildSystem {
    fn detect(&self, root: &Path) -> bool {
        let gradle_markers = [
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ];
        gradle_markers
            .iter()
            .any(|marker| root.join(marker).is_file())
    }

    fn parse_project(&self, root: &Path) -> Result<ProjectModel, BuildSystemError> {
        let root = canonicalize_root(root);
        crate::gradle::load_gradle_workspace_model(&root, &self.options)
            .map_err(BuildSystemError::other)
    }

    fn resolve_classpath(&self, project: &ProjectModel) -> Result<Classpath, BuildSystemError> {
        Ok(Classpath::from_workspace_model_union(project))
    }

    fn watch_files(&self) -> Vec<PathPattern> {
        dedup_patterns(
            vec![
                PathPattern::ExactFileName("build.gradle"),
                PathPattern::ExactFileName("build.gradle.kts"),
                PathPattern::ExactFileName("settings.gradle"),
                PathPattern::ExactFileName("settings.gradle.kts"),
                // Gradle script plugins can influence the build and classpath.
                PathPattern::Glob("**/*.gradle"),
                PathPattern::Glob("**/*.gradle.kts"),
                // Version catalogs can influence dependency versions.
                PathPattern::ExactFileName("libs.versions.toml"),
                // Additional version catalogs can be custom-named but must be direct children of a
                // `gradle/` directory (e.g. `gradle/deps.versions.toml`).
                PathPattern::Glob("**/gradle/*.versions.toml"),
                PathPattern::ExactFileName("gradle.properties"),
                PathPattern::ExactFileName("gradlew"),
                PathPattern::ExactFileName("gradlew.bat"),
                // Dependency lockfiles can change resolved versions / transitive closure.
                PathPattern::ExactFileName("gradle.lockfile"),
                PathPattern::Glob("**/dependency-locks/**/*.lockfile"),
                PathPattern::Glob("**/gradle/wrapper/gradle-wrapper.properties"),
                PathPattern::Glob("**/gradle/wrapper/gradle-wrapper.jar"),
                // `nova-build` emits a file-based Gradle snapshot handoff here; treat it like a build
                // file so editors can trigger a reload when it changes.
                PathPattern::Glob(nova_build_model::GRADLE_SNAPSHOT_GLOB),
            ]
            .into_iter()
            .chain(common_watch_files())
            .collect(),
        )
    }
}

#[derive(Debug, Clone)]
pub struct BazelBuildSystem {
    options: LoadOptions,
}

impl BazelBuildSystem {
    pub fn new(options: LoadOptions) -> Self {
        Self { options }
    }
}

impl BuildSystemBackend for BazelBuildSystem {
    fn detect(&self, root: &Path) -> bool {
        crate::is_bazel_workspace(root)
    }

    fn parse_project(&self, root: &Path) -> Result<ProjectModel, BuildSystemError> {
        let root = canonicalize_root(root);
        crate::bazel::load_bazel_workspace_model(&root, &self.options)
            .map_err(BuildSystemError::other)
    }

    fn resolve_classpath(&self, project: &ProjectModel) -> Result<Classpath, BuildSystemError> {
        Ok(Classpath::from_workspace_model_union(project))
    }

    fn watch_files(&self) -> Vec<PathPattern> {
        dedup_patterns(
            vec![
                PathPattern::ExactFileName("WORKSPACE"),
                PathPattern::ExactFileName("WORKSPACE.bazel"),
                PathPattern::ExactFileName("MODULE.bazel"),
                PathPattern::ExactFileName("BUILD"),
                PathPattern::ExactFileName("BUILD.bazel"),
                PathPattern::ExactFileName(".bazelrc"),
                PathPattern::Glob("**/.bazelrc.*"),
                PathPattern::ExactFileName(".bazelversion"),
                PathPattern::ExactFileName("MODULE.bazel.lock"),
                PathPattern::ExactFileName("bazelisk.rc"),
                PathPattern::ExactFileName(".bazelignore"),
                // Bazel BSP server discovery uses `.bsp/*.json` connection files (optional).
                PathPattern::Glob("**/.bsp/*.json"),
                PathPattern::Glob("**/*.bzl"),
            ]
            .into_iter()
            .chain(common_watch_files())
            .collect(),
        )
    }
}

#[derive(Debug, Clone)]
pub struct SimpleBuildSystem {
    options: LoadOptions,
}

impl SimpleBuildSystem {
    pub fn new(options: LoadOptions) -> Self {
        Self { options }
    }
}

impl BuildSystemBackend for SimpleBuildSystem {
    fn detect(&self, root: &Path) -> bool {
        root.join("src").is_dir()
    }

    fn parse_project(&self, root: &Path) -> Result<ProjectModel, BuildSystemError> {
        let root = canonicalize_root(root);
        crate::simple::load_simple_workspace_model(&root, &self.options)
            .map_err(BuildSystemError::other)
    }

    fn resolve_classpath(&self, project: &ProjectModel) -> Result<Classpath, BuildSystemError> {
        Ok(Classpath::from_workspace_model_union(project))
    }

    fn watch_files(&self) -> Vec<PathPattern> {
        // Simple projects can "upgrade" to another build system as soon as a marker file appears
        // (e.g. creating a new `pom.xml`). Watch for the common build markers so callers can
        // re-detect the workspace model.
        dedup_patterns(
            vec![
                PathPattern::ExactFileName("pom.xml"),
                PathPattern::ExactFileName("build.gradle"),
                PathPattern::ExactFileName("build.gradle.kts"),
                PathPattern::ExactFileName("settings.gradle"),
                PathPattern::ExactFileName("settings.gradle.kts"),
                PathPattern::ExactFileName("gradle.properties"),
                // Simple projects can "upgrade" to Gradle when wrapper/build marker files appear.
                PathPattern::ExactFileName("gradlew"),
                PathPattern::ExactFileName("gradlew.bat"),
                PathPattern::Glob("**/gradle/wrapper/gradle-wrapper.properties"),
                PathPattern::Glob("**/gradle/wrapper/gradle-wrapper.jar"),
                PathPattern::ExactFileName("WORKSPACE"),
                PathPattern::ExactFileName("WORKSPACE.bazel"),
                PathPattern::ExactFileName("MODULE.bazel"),
                PathPattern::ExactFileName("BUILD"),
                PathPattern::ExactFileName("BUILD.bazel"),
                PathPattern::Glob("**/*.bzl"),
            ]
            .into_iter()
            .chain(common_watch_files())
            .collect(),
        )
    }
}

pub fn default_build_systems(options: LoadOptions) -> Vec<Box<dyn BuildSystemBackend>> {
    vec![
        Box::new(BazelBuildSystem::new(options.clone())),
        Box::new(MavenBuildSystem::new(options.clone())),
        Box::new(GradleBuildSystem::new(options.clone())),
        Box::new(SimpleBuildSystem::new(options)),
    ]
}

fn canonicalize_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

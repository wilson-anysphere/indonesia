/**
 * Glob patterns for workspace file watchers that should trigger
 * `workspace/didChangeWatchedFiles` notifications.
 *
 * These include source files and a small set of build/framework/config inputs
 * that can affect Nova's project model and diagnostics.
 */

const BUILD_SYSTEM_GLOB_PATTERNS = [
  // Maven build configuration / wrapper.
  '**/pom.xml',
  '**/mvnw',
  '**/mvnw.cmd',
  '**/.mvn/wrapper/maven-wrapper.properties',
  '**/.mvn/wrapper/maven-wrapper.jar',
  '**/.mvn/extensions.xml',
  '**/.mvn/maven.config',
  '**/.mvn/jvm.config',

  // Gradle build configuration (Groovy + Kotlin DSL) / wrapper.
  // Explicit top-level markers (required for basic Gradle project detection).
  '**/build.gradle',
  '**/build.gradle.kts',
  '**/settings.gradle',
  '**/settings.gradle.kts',
  // Script plugins / shared Gradle configuration can live anywhere.
  '**/*.gradle',
  '**/*.gradle.kts',
  '**/gradle.properties',
  '**/gradlew',
  '**/gradlew.bat',
  // Dependency lockfiles can change resolved versions / transitive closure.
  '**/gradle.lockfile',
  '**/dependency-locks/**/*.lockfile',
  '**/gradle/wrapper/gradle-wrapper.properties',
  '**/gradle/wrapper/gradle-wrapper.jar',
  // Gradle version catalogs can be named `*.versions.toml` and may live anywhere.
  '**/*.versions.toml',

  // Bazel build configuration.
  '**/WORKSPACE',
  '**/WORKSPACE.bazel',
  '**/MODULE.bazel',
  '**/MODULE.bazel.lock',
  '**/BUILD',
  '**/BUILD.bazel',
  '**/*.bzl',
  '**/.bazelrc',
  '**/.bazelrc.*',
  '**/.bazelversion',
  '**/bazelisk.rc',
  '**/.bazelignore',
  // Bazel BSP server discovery uses `.bsp/*.json` connection files (optional).
  '**/.bsp/*.json',

  // JPMS. `module-info.java` affects module-graph classification (classpath vs module-path).
  '**/module-info.java',
] as const;

const SPRING_CONFIG_GLOB_PATTERNS = [
  // Spring Boot config that can influence annotation processing / classpath
  // behavior / generated sources / diagnostics.
  '**/application*.properties',
  '**/application*.yml',
  '**/application*.yaml',
] as const;

const NOVA_CONFIG_GLOB_PATTERNS = [
  // Nova workspace config.
  '**/nova.toml',
  '**/.nova.toml',
  '**/nova.config.toml',
  '**/.nova/apt-cache/generated-roots.json',
  // `nova-build` -> `nova-project` file-based Gradle snapshot handoff.
  '**/.nova/queries/gradle.json',
  // Legacy workspace-local config (kept for backwards compatibility).
  '**/.nova/config.toml',
] as const;

const WATCHED_FILE_GLOB_PATTERNS = [
  // Java source files.
  '**/*.java',
  ...BUILD_SYSTEM_GLOB_PATTERNS,
  ...SPRING_CONFIG_GLOB_PATTERNS,
  ...NOVA_CONFIG_GLOB_PATTERNS,
] as const;

const BUILD_FILE_GLOB_PATTERNS = [...BUILD_SYSTEM_GLOB_PATTERNS, ...NOVA_CONFIG_GLOB_PATTERNS] as const;

export function getNovaBuildFileGlobPatterns(): string[] {
  return [...BUILD_FILE_GLOB_PATTERNS];
}

export function getNovaWatchedFileGlobPatterns(): string[] {
  // Return a copy so callers can't mutate our module-level constant.
  return [...WATCHED_FILE_GLOB_PATTERNS];
}

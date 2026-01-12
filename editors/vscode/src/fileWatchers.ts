/**
 * Glob patterns for workspace file watchers that should trigger
 * `workspace/didChangeWatchedFiles` notifications.
 *
 * These include source files and a small set of build/framework/config inputs
 * that can affect Nova's project model and diagnostics.
 */
const WATCHED_FILE_GLOB_PATTERNS = [
  // Java source files.
  '**/*.java',

  // Maven build configuration / wrapper.
  '**/pom.xml',
  '**/mvnw',
  '**/mvnw.cmd',
  '**/.mvn/wrapper/maven-wrapper.properties',
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
  '**/gradle/wrapper/gradle-wrapper.properties',
  '**/libs.versions.toml',

  // Bazel build configuration.
  '**/.bazelrc',
  '**/.bazelrc.*',
  '**/.bazelversion',
  '**/MODULE.bazel.lock',
  '**/bazelisk.rc',
  '**/.bazelignore',
  '**/WORKSPACE',
  '**/WORKSPACE.bazel',
  '**/MODULE.bazel',
  '**/BUILD',
  '**/BUILD.bazel',
  '**/*.bzl',

  // Spring Boot config that can influence annotation processing / classpath
  // behavior / generated sources / diagnostics.
  '**/application*.properties',
  '**/application*.yml',
  '**/application*.yaml',

  // Nova config.
  '**/nova.toml',
  '**/.nova.toml',
  '**/nova.config.toml',
  '**/.nova/apt-cache/generated-roots.json',
  '**/.nova/config.toml',
] as const;

export function getNovaWatchedFileGlobPatterns(): string[] {
  // Return a copy so callers can't mutate our module-level constant.
  return [...WATCHED_FILE_GLOB_PATTERNS];
}

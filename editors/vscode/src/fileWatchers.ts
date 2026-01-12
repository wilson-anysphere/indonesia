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
  '**/build.gradle',
  '**/build.gradle.kts',
  '**/settings.gradle',
  '**/settings.gradle.kts',
  '**/gradle.properties',
  '**/gradlew',
  '**/gradlew.bat',
  '**/gradle/wrapper/gradle-wrapper.properties',
  '**/gradle/libs.versions.toml',
  '**/gradle/*.gradle',
  '**/gradle/*.gradle.kts',

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

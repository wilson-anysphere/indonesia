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

  // Maven build configuration.
  '**/pom.xml',

  // Gradle build configuration (Groovy + Kotlin DSL).
  '**/build.gradle',
  '**/settings.gradle',
  '**/gradle.properties',
  '**/*.gradle.kts',

  // Spring Boot config that can influence annotation processing / classpath
  // behavior / generated sources / diagnostics.
  '**/application*.properties',
  '**/application*.yml',
  '**/application*.yaml',

  // Nova config.
  '**/nova.toml',
  '**/.nova/config.toml',
] as const;

export function getNovaWatchedFileGlobPatterns(): string[] {
  // Return a copy so callers can't mutate our module-level constant.
  return [...WATCHED_FILE_GLOB_PATTERNS];
}


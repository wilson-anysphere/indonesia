import test from 'node:test';
import assert from 'node:assert/strict';

import {
  getNovaBuildFileGlobPatterns,
  getNovaWatchedFileGlobPatterns,
  NOVA_APT_GENERATED_ROOTS_SNAPSHOT_GLOB,
  NOVA_CONFIG_GLOB,
  NOVA_GRADLE_SNAPSHOT_GLOB,
} from '../fileWatchers';

test('getNovaWatchedFileGlobPatterns returns the expected default glob list', () => {
  assert.deepEqual(getNovaWatchedFileGlobPatterns(), [
    '**/*.java',
    '**/pom.xml',
    '**/mvnw',
    '**/mvnw.cmd',
    '**/.mvn/wrapper/maven-wrapper.properties',
    '**/.mvn/wrapper/maven-wrapper.jar',
    '**/.mvn/extensions.xml',
    '**/.mvn/maven.config',
    '**/.mvn/jvm.config',
    '**/build.gradle',
    '**/build.gradle.kts',
    '**/settings.gradle',
    '**/settings.gradle.kts',
    '**/*.gradle',
    '**/*.gradle.kts',
    '**/gradle.lockfile',
    '**/dependency-locks/**/*.lockfile',
    '**/gradle.properties',
    '**/gradlew',
    '**/gradlew.bat',
    '**/gradle/wrapper/gradle-wrapper.properties',
    '**/gradle/wrapper/gradle-wrapper.jar',
    '**/*.versions.toml',
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
    '**/.bsp/*.json',
    '**/module-info.java',
    '**/application*.properties',
    '**/application*.yml',
    '**/application*.yaml',
    '**/microprofile-config.properties',
    '**/META-INF/spring-configuration-metadata.json',
    '**/META-INF/additional-spring-configuration-metadata.json',
    '**/nova.toml',
    '**/.nova.toml',
    '**/nova.config.toml',
    NOVA_APT_GENERATED_ROOTS_SNAPSHOT_GLOB,
    NOVA_GRADLE_SNAPSHOT_GLOB,
    NOVA_CONFIG_GLOB,
  ]);
});

test('getNovaBuildFileGlobPatterns returns the expected default glob list', () => {
  assert.deepEqual(getNovaBuildFileGlobPatterns(), [
    '**/pom.xml',
    '**/mvnw',
    '**/mvnw.cmd',
    '**/.mvn/wrapper/maven-wrapper.properties',
    '**/.mvn/wrapper/maven-wrapper.jar',
    '**/.mvn/extensions.xml',
    '**/.mvn/maven.config',
    '**/.mvn/jvm.config',
    '**/build.gradle',
    '**/build.gradle.kts',
    '**/settings.gradle',
    '**/settings.gradle.kts',
    '**/*.gradle',
    '**/*.gradle.kts',
    '**/gradle.lockfile',
    '**/dependency-locks/**/*.lockfile',
    '**/gradle.properties',
    '**/gradlew',
    '**/gradlew.bat',
    '**/gradle/wrapper/gradle-wrapper.properties',
    '**/gradle/wrapper/gradle-wrapper.jar',
    '**/*.versions.toml',
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
    '**/.bsp/*.json',
    '**/module-info.java',
    '**/nova.toml',
    '**/.nova.toml',
    '**/nova.config.toml',
    NOVA_APT_GENERATED_ROOTS_SNAPSHOT_GLOB,
    NOVA_GRADLE_SNAPSHOT_GLOB,
    NOVA_CONFIG_GLOB,
  ]);
});

test('getNovaWatchedFileGlobPatterns does not return duplicate globs', () => {
  const patterns = getNovaWatchedFileGlobPatterns();
  assert.equal(new Set(patterns).size, patterns.length);
});

test('getNovaBuildFileGlobPatterns does not return duplicate globs', () => {
  const patterns = getNovaBuildFileGlobPatterns();
  assert.equal(new Set(patterns).size, patterns.length);
});

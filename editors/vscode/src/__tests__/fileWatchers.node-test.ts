import test from 'node:test';
import assert from 'node:assert/strict';

import { getNovaBuildFileGlobPatterns, getNovaWatchedFileGlobPatterns } from '../fileWatchers';

test('getNovaWatchedFileGlobPatterns returns the expected default glob list', () => {
  assert.deepEqual(getNovaWatchedFileGlobPatterns(), [
    '**/*.java',
    '**/pom.xml',
    '**/mvnw',
    '**/mvnw.cmd',
    '**/.mvn/wrapper/maven-wrapper.properties',
    '**/.mvn/extensions.xml',
    '**/.mvn/maven.config',
    '**/.mvn/jvm.config',
    '**/build.gradle',
    '**/build.gradle.kts',
    '**/settings.gradle',
    '**/settings.gradle.kts',
    '**/*.gradle',
    '**/*.gradle.kts',
    '**/gradle.properties',
    '**/gradlew',
    '**/gradlew.bat',
    '**/gradle/wrapper/gradle-wrapper.properties',
    '**/libs.versions.toml',
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
    '**/module-info.java',
    '**/application*.properties',
    '**/application*.yml',
    '**/application*.yaml',
    '**/nova.toml',
    '**/.nova.toml',
    '**/nova.config.toml',
    '**/.nova/apt-cache/generated-roots.json',
    '**/.nova/**/*.toml',
  ]);
});

test('getNovaBuildFileGlobPatterns returns the expected default glob list', () => {
  assert.deepEqual(getNovaBuildFileGlobPatterns(), [
    '**/pom.xml',
    '**/mvnw',
    '**/mvnw.cmd',
    '**/.mvn/wrapper/maven-wrapper.properties',
    '**/.mvn/extensions.xml',
    '**/.mvn/maven.config',
    '**/.mvn/jvm.config',
    '**/build.gradle',
    '**/build.gradle.kts',
    '**/settings.gradle',
    '**/settings.gradle.kts',
    '**/*.gradle',
    '**/*.gradle.kts',
    '**/gradle.properties',
    '**/gradlew',
    '**/gradlew.bat',
    '**/gradle/wrapper/gradle-wrapper.properties',
    '**/libs.versions.toml',
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
    '**/module-info.java',
    '**/nova.toml',
    '**/.nova.toml',
    '**/nova.config.toml',
    '**/.nova/apt-cache/generated-roots.json',
    '**/.nova/**/*.toml',
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

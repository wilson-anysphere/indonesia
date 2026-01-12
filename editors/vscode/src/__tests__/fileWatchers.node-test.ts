import test from 'node:test';
import assert from 'node:assert/strict';

import { getNovaWatchedFileGlobPatterns } from '../fileWatchers';

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
    '**/gradle.properties',
    '**/gradlew',
    '**/gradlew.bat',
    '**/gradle/wrapper/gradle-wrapper.properties',
    '**/gradle/libs.versions.toml',
    '**/gradle/*.gradle',
    '**/gradle/*.gradle.kts',
    '**/.bazelrc',
    '**/.bazelrc.*',
    '**/.bazelversion',
    '**/MODULE.bazel.lock',
    '**/bazelisk.rc',
    '**/WORKSPACE',
    '**/WORKSPACE.bazel',
    '**/MODULE.bazel',
    '**/BUILD',
    '**/BUILD.bazel',
    '**/*.bzl',
    '**/application*.properties',
    '**/application*.yml',
    '**/application*.yaml',
    '**/nova.toml',
    '**/.nova.toml',
    '**/nova.config.toml',
    '**/.nova/config.toml',
  ]);
});

test('getNovaWatchedFileGlobPatterns does not return duplicate globs', () => {
  const patterns = getNovaWatchedFileGlobPatterns();
  assert.equal(new Set(patterns).size, patterns.length);
});

import test from 'node:test';
import assert from 'node:assert/strict';

import { getNovaWatchedFileGlobPatterns } from '../fileWatchers';

test('getNovaWatchedFileGlobPatterns returns the expected default glob list', () => {
  assert.deepEqual(getNovaWatchedFileGlobPatterns(), [
    '**/*.java',
    '**/pom.xml',
    '**/build.gradle',
    '**/settings.gradle',
    '**/gradle.properties',
    '**/*.gradle.kts',
    '**/application*.properties',
    '**/application*.yml',
    '**/application*.yaml',
    '**/nova.toml',
    '**/.nova/config.toml',
  ]);
});

test('getNovaWatchedFileGlobPatterns does not return duplicate globs', () => {
  const patterns = getNovaWatchedFileGlobPatterns();
  assert.equal(new Set(patterns).size, patterns.length);
});


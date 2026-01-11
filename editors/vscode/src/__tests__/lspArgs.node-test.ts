import test from 'node:test';
import assert from 'node:assert/strict';
import * as os from 'node:os';
import * as path from 'node:path';

import { buildNovaLspArgs, resolveNovaConfigPath } from '../lspArgs';

test('buildNovaLspArgs always includes --stdio', () => {
  assert.deepEqual(buildNovaLspArgs(), ['--stdio']);
});

test('buildNovaLspArgs appends --config when configPath is provided', () => {
  assert.deepEqual(buildNovaLspArgs({ configPath: '/tmp/nova.toml' }), ['--stdio', '--config', '/tmp/nova.toml']);
});

test('buildNovaLspArgs resolves relative configPath against workspaceRoot', () => {
  assert.deepEqual(buildNovaLspArgs({ configPath: 'nova.toml', workspaceRoot: '/workspace' }), [
    '--stdio',
    '--config',
    path.join('/workspace', 'nova.toml'),
  ]);
});

test('buildNovaLspArgs appends extraArgs after stdio/config flags', () => {
  assert.deepEqual(
    buildNovaLspArgs({ configPath: '/tmp/nova.toml', extraArgs: ['--log-level', 'debug'] }),
    ['--stdio', '--config', '/tmp/nova.toml', '--log-level', 'debug'],
  );
});

test('buildNovaLspArgs ignores blank configPath and empty extraArgs entries', () => {
  assert.deepEqual(buildNovaLspArgs({ configPath: '   ', extraArgs: [' ', '--foo', ''] }), ['--stdio', '--foo']);
});

test('resolveNovaConfigPath returns undefined when configPath is unset', () => {
  assert.equal(resolveNovaConfigPath({ configPath: null, workspaceRoot: '/workspace' }), undefined);
  assert.equal(resolveNovaConfigPath({ configPath: '   ', workspaceRoot: '/workspace' }), undefined);
});

test('resolveNovaConfigPath expands ~ to the user home dir', () => {
  assert.equal(resolveNovaConfigPath({ configPath: '~', workspaceRoot: '/workspace' }), os.homedir());
  assert.equal(resolveNovaConfigPath({ configPath: '~/nova.toml', workspaceRoot: '/workspace' }), path.join(os.homedir(), 'nova.toml'));
});

test('resolveNovaConfigPath expands ${workspaceFolder}', () => {
  assert.equal(
    resolveNovaConfigPath({ configPath: '${workspaceFolder}/nova.toml', workspaceRoot: '/workspace' }),
    '/workspace/nova.toml',
  );
});

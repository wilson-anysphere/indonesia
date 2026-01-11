import test from 'node:test';
import assert from 'node:assert/strict';
import * as os from 'node:os';
import * as path from 'node:path';

import { buildNovaLspArgs, buildNovaLspLaunchConfig, resolveNovaConfigPath } from '../lspArgs';

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

test('buildNovaLspLaunchConfig sets NOVA_CONFIG_PATH when configPath is provided', () => {
  const baseEnv: NodeJS.ProcessEnv = { FOO: 'bar' };
  const config = buildNovaLspLaunchConfig({ configPath: 'nova.toml', workspaceRoot: '/workspace', aiEnabled: true, baseEnv });

  assert.deepEqual(config.args, ['--stdio', '--config', '/workspace/nova.toml']);
  assert.equal(config.env.NOVA_CONFIG_PATH, '/workspace/nova.toml');
  assert.equal(config.env.FOO, 'bar');
  assert.equal(baseEnv.NOVA_CONFIG_PATH, undefined);
});

test('buildNovaLspLaunchConfig strips NOVA_AI_* env vars when aiEnabled is false', () => {
  const baseEnv: NodeJS.ProcessEnv = { NOVA_AI_PROVIDER: 'http', NOVA_AI_MODEL: 'default', OTHER: 'x' };
  const config = buildNovaLspLaunchConfig({ aiEnabled: false, baseEnv });

  assert.equal(config.env.NOVA_AI_PROVIDER, undefined);
  assert.equal(config.env.NOVA_AI_MODEL, undefined);
  assert.equal(config.env.OTHER, 'x');
  assert.equal(baseEnv.NOVA_AI_PROVIDER, 'http');
});

test('buildNovaLspLaunchConfig reuses baseEnv when no env changes are required', () => {
  const baseEnv: NodeJS.ProcessEnv = { FOO: 'bar' };
  const config = buildNovaLspLaunchConfig({ aiEnabled: true, baseEnv });

  assert.equal(config.env, baseEnv);
  assert.deepEqual(config.args, ['--stdio']);
});

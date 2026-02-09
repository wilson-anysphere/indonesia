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

test('buildNovaLspLaunchConfig strips NOVA_AI_* env vars and sets NOVA_DISABLE_AI when aiEnabled is false', () => {
  const baseEnv: NodeJS.ProcessEnv = { NOVA_AI_PROVIDER: 'http', NOVA_AI_MODEL: 'default', OTHER: 'x' };
  const config = buildNovaLspLaunchConfig({ aiEnabled: false, baseEnv });

  assert.equal(config.env.NOVA_DISABLE_AI, '1');
  assert.equal(config.env.NOVA_AI_PROVIDER, undefined);
  assert.equal(config.env.NOVA_AI_MODEL, undefined);
  assert.equal(config.env.OTHER, 'x');
  assert.equal(baseEnv.NOVA_DISABLE_AI, undefined);
  assert.equal(baseEnv.NOVA_AI_PROVIDER, 'http');
});

test('buildNovaLspLaunchConfig sets NOVA_DISABLE_AI_COMPLETIONS when AI completion features are disabled', () => {
  const baseEnv: NodeJS.ProcessEnv = { OTHER: 'x' };
  const config = buildNovaLspLaunchConfig({ aiEnabled: true, aiCompletionsEnabled: false, baseEnv });

  assert.equal(config.env.NOVA_DISABLE_AI, undefined);
  assert.equal(config.env.NOVA_DISABLE_AI_COMPLETIONS, '1');
  assert.equal(config.env.OTHER, 'x');
  assert.equal(baseEnv.NOVA_DISABLE_AI_COMPLETIONS, undefined);
});

test('buildNovaLspLaunchConfig sets NOVA_DISABLE_AI_CODE_ACTIONS when AI code actions are disabled', () => {
  const baseEnv: NodeJS.ProcessEnv = { OTHER: 'x' };
  const config = buildNovaLspLaunchConfig({ aiEnabled: true, aiCodeActionsEnabled: false, baseEnv });

  assert.equal(config.env.NOVA_DISABLE_AI, undefined);
  assert.equal(config.env.NOVA_DISABLE_AI_CODE_ACTIONS, '1');
  assert.equal(config.env.OTHER, 'x');
  assert.equal(baseEnv.NOVA_DISABLE_AI_CODE_ACTIONS, undefined);
});

test('buildNovaLspLaunchConfig sets NOVA_DISABLE_AI_CODE_REVIEW when AI code review is disabled', () => {
  const baseEnv: NodeJS.ProcessEnv = { OTHER: 'x' };
  const config = buildNovaLspLaunchConfig({ aiEnabled: true, aiCodeReviewEnabled: false, baseEnv });

  assert.equal(config.env.NOVA_DISABLE_AI, undefined);
  assert.equal(config.env.NOVA_DISABLE_AI_CODE_REVIEW, '1');
  assert.equal(config.env.OTHER, 'x');
  assert.equal(baseEnv.NOVA_DISABLE_AI_CODE_REVIEW, undefined);
});

test('buildNovaLspLaunchConfig removes NOVA_DISABLE_AI* hard-disable env vars when AI features are enabled', () => {
  const baseEnv: NodeJS.ProcessEnv = {
    NOVA_DISABLE_AI: '1',
    NOVA_DISABLE_AI_COMPLETIONS: '1',
    NOVA_DISABLE_AI_CODE_ACTIONS: '1',
    NOVA_DISABLE_AI_CODE_REVIEW: '1',
    OTHER: 'x',
  };
  const config = buildNovaLspLaunchConfig({ aiEnabled: true, aiCompletionsEnabled: true, baseEnv });

  assert.notEqual(config.env, baseEnv);
  assert.equal(config.env.NOVA_DISABLE_AI, undefined);
  assert.equal(config.env.NOVA_DISABLE_AI_COMPLETIONS, undefined);
  assert.equal(config.env.NOVA_DISABLE_AI_CODE_ACTIONS, undefined);
  assert.equal(config.env.NOVA_DISABLE_AI_CODE_REVIEW, undefined);
  assert.equal(config.env.OTHER, 'x');
  assert.equal(baseEnv.NOVA_DISABLE_AI, '1');
  assert.equal(baseEnv.NOVA_DISABLE_AI_COMPLETIONS, '1');
});

test('buildNovaLspLaunchConfig sets NOVA_AI_COMPLETIONS_MAX_ITEMS when aiCompletionsMaxItems is provided', () => {
  const baseEnv: NodeJS.ProcessEnv = { OTHER: 'x' };
  const config = buildNovaLspLaunchConfig({ aiEnabled: true, aiCompletionsMaxItems: 7, baseEnv });

  assert.equal(config.env.NOVA_AI_COMPLETIONS_MAX_ITEMS, '7');
  assert.equal(config.env.OTHER, 'x');
  assert.equal(baseEnv.NOVA_AI_COMPLETIONS_MAX_ITEMS, undefined);
});

test('buildNovaLspLaunchConfig reuses baseEnv when no env changes are required', () => {
  const baseEnv: NodeJS.ProcessEnv = { FOO: 'bar' };
  const config = buildNovaLspLaunchConfig({ aiEnabled: true, aiCompletionsEnabled: true, baseEnv });

  assert.equal(config.env, baseEnv);
  assert.deepEqual(config.args, ['--stdio']);

  const configWithMutations = buildNovaLspLaunchConfig({ aiEnabled: true, aiCompletionsEnabled: false, baseEnv });
  assert.notEqual(configWithMutations.env, baseEnv);
  assert.equal(configWithMutations.env.NOVA_DISABLE_AI_COMPLETIONS, '1');
  assert.equal(baseEnv.NOVA_DISABLE_AI_COMPLETIONS, undefined);
});

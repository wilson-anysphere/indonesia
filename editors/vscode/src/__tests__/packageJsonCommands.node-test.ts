import assert from 'node:assert/strict';
import test from 'node:test';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';

test('package.json contributes Nova request metrics commands', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { commands?: unknown };
  };

  const activationEvents = Array.isArray(pkg.activationEvents) ? pkg.activationEvents : [];
  const commands = Array.isArray(pkg.contributes?.commands) ? pkg.contributes.commands : [];

  const commandIds = new Set(
    commands
      .map((entry) => (entry && typeof entry === 'object' ? (entry as { command?: unknown }).command : undefined))
      .filter((id): id is string => typeof id === 'string'),
  );

  assert.ok(commandIds.has('nova.showRequestMetrics'));
  assert.ok(commandIds.has('nova.resetRequestMetrics'));

  assert.ok(activationEvents.includes('onCommand:nova.showRequestMetrics'));
  assert.ok(activationEvents.includes('onCommand:nova.resetRequestMetrics'));
});

test('package.json contributes Nova Frameworks view context-menu commands', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { commands?: unknown; menus?: unknown };
  };

  const activationEvents = Array.isArray(pkg.activationEvents) ? pkg.activationEvents : [];
  const commands = Array.isArray(pkg.contributes?.commands) ? pkg.contributes.commands : [];
  const menus = pkg.contributes?.menus;

  const commandIds = new Set(
    commands
      .map((entry) => (entry && typeof entry === 'object' ? (entry as { command?: unknown }).command : undefined))
      .filter((id): id is string => typeof id === 'string'),
  );

  const expectedCommands = [
    'nova.frameworks.copyEndpointPath',
    'nova.frameworks.copyEndpointMethodAndPath',
    'nova.frameworks.copyBeanId',
    'nova.frameworks.copyBeanType',
    'nova.frameworks.revealInExplorer',
  ];

  for (const id of expectedCommands) {
    assert.ok(commandIds.has(id));
    assert.ok(activationEvents.includes(`onCommand:${id}`));
  }

  assert.ok(menus && typeof menus === 'object');
  const viewItemContext = (menus as { 'view/item/context'?: unknown })['view/item/context'];
  assert.ok(Array.isArray(viewItemContext));

  const menuCommands = new Set(
    (viewItemContext as unknown[]).map((entry) => (entry && typeof entry === 'object' ? (entry as { command?: unknown }).command : undefined)),
  );

  for (const id of expectedCommands) {
    assert.ok(menuCommands.has(id));
  }
});

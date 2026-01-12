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


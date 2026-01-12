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

test('package.json contributes Nova framework search command', async () => {
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

  assert.ok(commandIds.has('nova.frameworks.search'));
  assert.ok(activationEvents.includes('onCommand:nova.frameworks.search'));
});

test('package.json contributes Nova Frameworks viewsWelcome empty-state guidance', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { views?: unknown; viewsWelcome?: unknown };
  };

  const activationEvents = Array.isArray(pkg.activationEvents) ? pkg.activationEvents : [];
  assert.ok(activationEvents.includes('onView:novaFrameworks'));

  const contributesViews = pkg.contributes?.views;
  assert.ok(contributesViews && typeof contributesViews === 'object');
  const explorerViews = (contributesViews as { explorer?: unknown }).explorer;
  assert.ok(Array.isArray(explorerViews));
  assert.ok((explorerViews as unknown[]).some((entry) => (entry as { id?: unknown })?.id === 'novaFrameworks'));

  const viewsWelcome = Array.isArray(pkg.contributes?.viewsWelcome) ? pkg.contributes.viewsWelcome : [];
  const frameworksWelcome = viewsWelcome.filter(
    (entry): entry is { view?: unknown; contents?: unknown; when?: unknown } =>
      entry && typeof entry === 'object' && (entry as { view?: unknown }).view === 'novaFrameworks',
  );

  assert.ok(frameworksWelcome.length >= 4);

  const hasNoWorkspaceHint = frameworksWelcome.some((entry) => {
    const when = typeof entry.when === 'string' ? entry.when : '';
    const contents = typeof entry.contents === 'string' ? entry.contents : '';
    return when.includes('workspaceFolderCount') && when.includes('0') && contents.toLowerCase().includes('open folder');
  });
  assert.ok(hasNoWorkspaceHint);

  const hasServerMissingHint = frameworksWelcome.some((entry) => {
    const contents = typeof entry.contents === 'string' ? entry.contents : '';
    return contents.includes('nova.installOrUpdateServer');
  });
  assert.ok(hasServerMissingHint);

  const hasMicronautEndpointsContext = frameworksWelcome.some((entry) => {
    const when = typeof entry.when === 'string' ? entry.when : '';
    return when.includes('nova.frameworks.micronautEndpointsSupported');
  });
  assert.ok(hasMicronautEndpointsContext);

  const hasMicronautBeansContext = frameworksWelcome.some((entry) => {
    const when = typeof entry.when === 'string' ? entry.when : '';
    return when.includes('nova.frameworks.micronautBeansSupported');
  });
  assert.ok(hasMicronautBeansContext);

  const hasUnsupportedHint = frameworksWelcome.some((entry) => {
    const contents = typeof entry.contents === 'string' ? entry.contents : '';
    return contents.toLowerCase().includes('upgrade') || contents.includes('nova.showServerVersion');
  });
  assert.ok(hasUnsupportedHint);
});

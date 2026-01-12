import assert from 'node:assert/strict';
import test from 'node:test';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';

function findDuplicates(values: readonly string[]): string[] {
  const seen = new Set<string>();
  const duplicates = new Set<string>();
  for (const value of values) {
    if (seen.has(value)) duplicates.add(value);
    else seen.add(value);
  }
  return [...duplicates].sort();
}

test('package.json has no duplicate activationEvents or contributes.commands entries', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { commands?: unknown };
  };

  const activationEventsRaw = Array.isArray(pkg.activationEvents) ? pkg.activationEvents : [];
  const activationEvents = activationEventsRaw.filter((value): value is string => typeof value === 'string');
  const activationEventDuplicates = findDuplicates(activationEvents);
  assert.equal(
    activationEventDuplicates.length,
    0,
    `Expected activationEvents to contain no duplicates, but found: ${activationEventDuplicates.join(', ')}`,
  );

  const commandsRaw = Array.isArray(pkg.contributes?.commands) ? pkg.contributes.commands : [];
  const commandIds = commandsRaw
    .map((entry) => (entry && typeof entry === 'object' ? (entry as { command?: unknown }).command : undefined))
    .filter((id): id is string => typeof id === 'string');

  const commandDuplicates = findDuplicates(commandIds);
  assert.equal(
    commandDuplicates.length,
    0,
    `Expected contributes.commands[].command to contain no duplicates, but found: ${commandDuplicates.join(', ')}`,
  );
});

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

test('package.json contributes Nova Frameworks view + refresh command', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { commands?: unknown; views?: unknown; menus?: unknown };
  };

  const activationEvents = Array.isArray(pkg.activationEvents) ? pkg.activationEvents : [];
  const commands = Array.isArray(pkg.contributes?.commands) ? pkg.contributes.commands : [];

  const refresh = commands.find((entry) => {
    if (!entry || typeof entry !== 'object') {
      return false;
    }
    return (entry as { command?: unknown }).command === 'nova.frameworks.refresh';
  }) as { command?: unknown; title?: unknown; icon?: unknown } | undefined;

  assert.ok(refresh, 'expected nova.frameworks.refresh command to be contributed');
  assert.equal(refresh.title, 'Nova: Refresh Frameworks');
  assert.ok(activationEvents.includes('onCommand:nova.frameworks.refresh'));

  const viewContainers = pkg.contributes?.views;
  assert.ok(viewContainers && typeof viewContainers === 'object');
  const explorerViews = (viewContainers as { explorer?: unknown }).explorer;
  assert.ok(Array.isArray(explorerViews));
  const frameworksView = (explorerViews as unknown[]).find((entry) => {
    if (!entry || typeof entry !== 'object') {
      return false;
    }
    return (entry as { id?: unknown }).id === 'novaFrameworks';
  }) as { id?: unknown; name?: unknown } | undefined;

  assert.ok(frameworksView, 'expected novaFrameworks view to be contributed under explorer');
  assert.equal(frameworksView.name, 'Nova Frameworks');

  const menus = pkg.contributes?.menus;
  assert.ok(menus && typeof menus === 'object');
  const viewTitle = (menus as { 'view/title'?: unknown })['view/title'];
  assert.ok(Array.isArray(viewTitle));
  assert.ok(
    (viewTitle as unknown[]).some((entry) => {
      if (!entry || typeof entry !== 'object') {
        return false;
      }
      const command = (entry as { command?: unknown }).command;
      const when = (entry as { when?: unknown }).when;
      return command === 'nova.frameworks.refresh' && typeof when === 'string' && when.includes('view == novaFrameworks');
    }),
    'expected nova.frameworks.refresh to be present in view/title menu for novaFrameworks',
  );

  const icon = refresh.icon;
  assert.ok(icon && typeof icon === 'object', 'expected refresh command to include light/dark icon paths');
  const light = (icon as { light?: unknown }).light;
  const dark = (icon as { dark?: unknown }).dark;
  assert.ok(typeof light === 'string' && light.length > 0);
  assert.ok(typeof dark === 'string' && dark.length > 0);
  await fs.stat(path.resolve(path.dirname(pkgPath), light));
  await fs.stat(path.resolve(path.dirname(pkgPath), dark));
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

test('package.json contributes Nova Project Explorer view + commands', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { commands?: unknown; views?: unknown; menus?: unknown };
  };

  const activationEvents = Array.isArray(pkg.activationEvents) ? pkg.activationEvents : [];
  assert.ok(activationEvents.includes('onView:novaProjectExplorer'));

  const commands = Array.isArray(pkg.contributes?.commands) ? pkg.contributes.commands : [];
  const commandIds = new Set(
    commands
      .map((entry) => (entry && typeof entry === 'object' ? (entry as { command?: unknown }).command : undefined))
      .filter((id): id is string => typeof id === 'string'),
  );

  const expectedCommands = ['nova.refreshProjectExplorer', 'nova.showProjectModel', 'nova.showProjectConfiguration'];
  for (const id of expectedCommands) {
    assert.ok(commandIds.has(id));
    assert.ok(activationEvents.includes(`onCommand:${id}`));
  }

  const views = pkg.contributes?.views;
  assert.ok(views && typeof views === 'object');
  const explorerViews = (views as { explorer?: unknown }).explorer;
  assert.ok(Array.isArray(explorerViews));
  assert.ok((explorerViews as unknown[]).some((entry) => (entry as { id?: unknown })?.id === 'novaProjectExplorer'));

  const menus = pkg.contributes?.menus;
  assert.ok(menus && typeof menus === 'object');

  const viewTitle = (menus as { 'view/title'?: unknown })['view/title'];
  assert.ok(Array.isArray(viewTitle));

  for (const id of expectedCommands) {
    const found = (viewTitle as unknown[]).some((entry) => {
      if (!entry || typeof entry !== 'object') {
        return false;
      }
      const cmd = (entry as { command?: unknown }).command;
      const when = (entry as { when?: unknown }).when;
      return cmd === id && typeof when === 'string' && when.includes('view == novaProjectExplorer');
    });
    assert.ok(found, `missing view/title menu entry for ${id}`);
  }

  const viewItemContext = (menus as { 'view/item/context'?: unknown })['view/item/context'];
  assert.ok(Array.isArray(viewItemContext));

  const itemCommands = new Set(
    (viewItemContext as unknown[])
      .map((entry) => (entry && typeof entry === 'object' ? (entry as { command?: unknown }).command : undefined))
      .filter((value): value is string => typeof value === 'string'),
  );

  assert.ok(itemCommands.has('nova.reloadProject'));
  assert.ok(itemCommands.has('nova.buildProject'));
});

test('package.json does not contain duplicate command or activation entries', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { commands?: unknown };
  };

  const activationEvents = Array.isArray(pkg.activationEvents)
    ? pkg.activationEvents.filter((entry): entry is string => typeof entry === 'string')
    : [];

  const activationSet = new Set(activationEvents);
  assert.equal(
    activationEvents.length,
    activationSet.size,
    `duplicate activationEvents: ${activationEvents.filter((e, i) => activationEvents.indexOf(e) !== i).join(', ')}`,
  );

  const commands = Array.isArray(pkg.contributes?.commands) ? pkg.contributes.commands : [];
  const commandIds = commands
    .map((entry) => (entry && typeof entry === 'object' ? (entry as { command?: unknown }).command : undefined))
    .filter((id): id is string => typeof id === 'string');

  const commandSet = new Set(commandIds);
  assert.equal(
    commandIds.length,
    commandSet.size,
    `duplicate contributes.commands entries: ${commandIds.filter((e, i) => commandIds.indexOf(e) !== i).join(', ')}`,
  );
});

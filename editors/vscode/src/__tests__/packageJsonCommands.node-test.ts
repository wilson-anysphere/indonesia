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

test('package.json contributes Run/Debug Test/Main as local interactive commands (avoids LSP executeCommand ID collisions)', async () => {
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

  const expectedLocalCommands = [
    'nova.runTestInteractive',
    'nova.debugTestInteractive',
    'nova.runMainInteractive',
    'nova.debugMainInteractive',
  ];
  for (const id of expectedLocalCommands) {
    assert.ok(commandIds.has(id));
    assert.ok(activationEvents.includes(`onCommand:${id}`));
  }

  const serverCommandIds = ['nova.runTest', 'nova.debugTest', 'nova.runMain', 'nova.debugMain'];
  for (const id of serverCommandIds) {
    assert.ok(!commandIds.has(id));
    assert.ok(!activationEvents.includes(`onCommand:${id}`));
  }
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

test('package.json contributes Run/Debug Test/Main command palette entries via local interactive IDs', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { commands?: unknown };
  };

  const activationEvents = Array.isArray(pkg.activationEvents) ? pkg.activationEvents : [];
  const commands = Array.isArray(pkg.contributes?.commands) ? pkg.contributes.commands : [];

  const byId = new Map<string, { title?: unknown }>();
  for (const entry of commands) {
    if (!entry || typeof entry !== 'object') {
      continue;
    }
    const id = (entry as { command?: unknown }).command;
    if (typeof id !== 'string') {
      continue;
    }
    byId.set(id, { title: (entry as { title?: unknown }).title });
  }

  // `nova.runTest`/`nova.debugTest`/`nova.runMain`/`nova.debugMain` are server-provided
  // `workspace/executeCommand` IDs.
  // We intentionally do NOT contribute command palette entries for them to avoid collisions with
  // vscode-languageclient's auto-registered commands.
  assert.equal(byId.get('nova.runTest')?.title, undefined);
  assert.equal(byId.get('nova.debugTest')?.title, undefined);
  assert.equal(byId.get('nova.runMain')?.title, undefined);
  assert.equal(byId.get('nova.debugMain')?.title, undefined);

  assert.equal(byId.get('nova.runTestInteractive')?.title, 'Nova: Run Test');
  assert.equal(byId.get('nova.debugTestInteractive')?.title, 'Nova: Debug Test');
  assert.equal(byId.get('nova.runMainInteractive')?.title, 'Nova: Run Main…');
  assert.equal(byId.get('nova.debugMainInteractive')?.title, 'Nova: Debug Main…');

  assert.ok(!activationEvents.includes('onCommand:nova.runTest'));
  assert.ok(!activationEvents.includes('onCommand:nova.debugTest'));
  assert.ok(!activationEvents.includes('onCommand:nova.runMain'));
  assert.ok(!activationEvents.includes('onCommand:nova.debugMain'));

  assert.ok(activationEvents.includes('onCommand:nova.runTestInteractive'));
  assert.ok(activationEvents.includes('onCommand:nova.debugTestInteractive'));
  assert.ok(activationEvents.includes('onCommand:nova.runMainInteractive'));
  assert.ok(activationEvents.includes('onCommand:nova.debugMainInteractive'));
});

test('package.json contributes Nova AI show commands (avoids LSP executeCommand ID collisions)', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { commands?: unknown };
  };

  const activationEvents = Array.isArray(pkg.activationEvents) ? pkg.activationEvents : [];
  const commands = Array.isArray(pkg.contributes?.commands) ? pkg.contributes.commands : [];

  const byId = new Map<string, { title?: unknown }>();
  for (const entry of commands) {
    if (!entry || typeof entry !== 'object') {
      continue;
    }
    const id = (entry as { command?: unknown }).command;
    if (typeof id !== 'string') {
      continue;
    }
    byId.set(id, { title: (entry as { title?: unknown }).title });
  }

  // Contributed commands are VS Code-side wrappers that run the underlying `workspace/executeCommand`
  // call and show the returned AI output.
  assert.equal(byId.get('nova.ai.showExplainError')?.title, 'Nova AI: Explain Error');
  assert.equal(byId.get('nova.ai.showGenerateMethodBody')?.title, 'Nova AI: Generate Method Body');
  assert.equal(byId.get('nova.ai.showGenerateTests')?.title, 'Nova AI: Generate Tests');

  assert.ok(activationEvents.includes('onCommand:nova.ai.showExplainError'));
  assert.ok(activationEvents.includes('onCommand:nova.ai.showGenerateMethodBody'));
  assert.ok(activationEvents.includes('onCommand:nova.ai.showGenerateTests'));

  // Server-provided `workspace/executeCommand` IDs must not be contributed to avoid collisions with
  // vscode-languageclient's auto-registered command handlers.
  assert.equal(byId.get('nova.ai.explainError')?.title, undefined);
  assert.equal(byId.get('nova.ai.generateMethodBody')?.title, undefined);
  assert.equal(byId.get('nova.ai.generateTests')?.title, undefined);

  assert.ok(!activationEvents.includes('onCommand:nova.ai.explainError'));
  assert.ok(!activationEvents.includes('onCommand:nova.ai.generateMethodBody'));
  assert.ok(!activationEvents.includes('onCommand:nova.ai.generateTests'));
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
    'nova.frameworks.open',
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

  const openCommand = commands.find((entry) => {
    if (!entry || typeof entry !== 'object') {
      return false;
    }
    return (entry as { command?: unknown }).command === 'nova.frameworks.open';
  }) as { title?: unknown } | undefined;
  assert.ok(openCommand);
  assert.equal(openCommand.title, 'Nova: Open Framework Item');

  const openMenuEntries = (viewItemContext as unknown[]).filter((entry) => {
    if (!entry || typeof entry !== 'object') {
      return false;
    }
    return (entry as { command?: unknown }).command === 'nova.frameworks.open';
  }) as Array<{ when?: unknown }>;
  assert.ok(openMenuEntries.length >= 2, 'expected nova.frameworks.open to appear for endpoints and beans');
  assert.ok(openMenuEntries.some((entry) => typeof entry.when === 'string' && entry.when.includes('viewItem == novaFrameworkEndpoint')));
  assert.ok(openMenuEntries.some((entry) => typeof entry.when === 'string' && entry.when.includes('viewItem == novaFrameworkBean')));
});

test('package.json path settings are resource-scoped and describe multi-root resolution', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    contributes?: { configuration?: { properties?: unknown } };
  };

  const properties = pkg.contributes?.configuration?.properties;
  assert.ok(properties && typeof properties === 'object');

  const props = properties as Record<string, unknown>;
  const expectedSettings = ['nova.server.path', 'nova.dap.path', 'nova.lsp.configPath'];

  for (const key of expectedSettings) {
    const setting = props[key];
    assert.ok(setting && typeof setting === 'object', `Missing ${key} configuration`);

    const obj = setting as { scope?: unknown; description?: unknown };
    assert.equal(obj.scope, 'resource', `${key} should be resource-scoped`);
    assert.ok(typeof obj.description === 'string', `${key} should have a description`);
    assert.ok(
      obj.description.includes('target workspace folder'),
      `${key} description should mention target workspace folder resolution`,
    );
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
  assert.ok(
    (viewTitle as unknown[]).some((entry) => {
      if (!entry || typeof entry !== 'object') {
        return false;
      }
      const command = (entry as { command?: unknown }).command;
      const when = (entry as { when?: unknown }).when;
      return command === 'nova.frameworks.search' && typeof when === 'string' && when.includes('view == novaFrameworks');
    }),
    'expected nova.frameworks.search to be present in view/title menu for novaFrameworks',
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

  const search = commands.find((entry) => {
    if (!entry || typeof entry !== 'object') {
      return false;
    }
    return (entry as { command?: unknown }).command === 'nova.frameworks.search';
  }) as { command?: unknown; title?: unknown; icon?: unknown } | undefined;

  assert.ok(search, 'expected nova.frameworks.search command to be contributed');
  assert.equal(search.title, 'Nova: Search Framework Items…');
  assert.ok(activationEvents.includes('onCommand:nova.frameworks.search'));

  const icon = search.icon;
  assert.ok(icon && typeof icon === 'object', 'expected search command to include light/dark icon paths');
  const light = (icon as { light?: unknown }).light;
  const dark = (icon as { dark?: unknown }).dark;
  assert.ok(typeof light === 'string' && light.length > 0);
  assert.ok(typeof dark === 'string' && dark.length > 0);
  await fs.stat(path.resolve(path.dirname(pkgPath), light));
  await fs.stat(path.resolve(path.dirname(pkgPath), dark));
});

test('package.json contributes Nova Project Explorer reveal path command', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { commands?: unknown };
  };

  const activationEvents = Array.isArray(pkg.activationEvents) ? pkg.activationEvents : [];
  const commands = Array.isArray(pkg.contributes?.commands) ? pkg.contributes.commands : [];

  const commandId = 'nova.projectExplorer.revealPath';

  const contributedCount = commands.filter(
    (entry) => entry && typeof entry === 'object' && (entry as { command?: unknown }).command === commandId,
  ).length;
  assert.equal(contributedCount, 1);

  const activationCount = activationEvents.filter((entry) => entry === `onCommand:${commandId}`).length;
  assert.equal(activationCount, 1);
});

test('package.json contributes Nova Project Explorer copy path command', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { commands?: unknown };
  };

  const activationEvents = Array.isArray(pkg.activationEvents) ? pkg.activationEvents : [];
  const commands = Array.isArray(pkg.contributes?.commands) ? pkg.contributes.commands : [];

  const commandId = 'nova.projectExplorer.copyPath';

  const contributedCount = commands.filter(
    (entry) => entry && typeof entry === 'object' && (entry as { command?: unknown }).command === commandId,
  ).length;
  assert.equal(contributedCount, 1);

  const activationCount = activationEvents.filter((entry) => entry === `onCommand:${commandId}`).length;
  assert.equal(activationCount, 1);
});

test('package.json contributes Nova Project Explorer copy path context menu entries', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    contributes?: { menus?: unknown };
  };

  const menus = pkg.contributes?.menus;
  assert.ok(menus && typeof menus === 'object');

  const viewItemContext = (menus as { 'view/item/context'?: unknown })['view/item/context'];
  assert.ok(Array.isArray(viewItemContext));

  const whenStrings = (viewItemContext as unknown[])
    .map((entry) => (entry && typeof entry === 'object' ? (entry as { command?: unknown; when?: unknown }) : undefined))
    .filter((entry): entry is { command?: unknown; when?: unknown } => Boolean(entry))
    .filter((entry) => entry.command === 'nova.projectExplorer.copyPath')
    .map((entry) => (typeof entry.when === 'string' ? entry.when : ''));

  const expected = ['novaProjectExplorerPath', 'novaProjectExplorerWorkspace', 'novaProjectExplorerUnit'];
  for (const ctx of expected) {
    assert.ok(
      whenStrings.some((when) => when.includes('view == novaProjectExplorer') && when.includes(`viewItem == ${ctx}`)),
      `missing copyPath menu entry for ${ctx}`,
    );
  }
});

test('package.json contributes Nova Frameworks + Project Explorer viewsWelcome empty-state guidance', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { views?: unknown; viewsWelcome?: unknown };
  };

  const activationEvents = Array.isArray(pkg.activationEvents) ? pkg.activationEvents : [];
  assert.ok(activationEvents.includes('onView:novaFrameworks'));
  assert.ok(activationEvents.includes('onView:novaProjectExplorer'));

  const contributesViews = pkg.contributes?.views;
  assert.ok(contributesViews && typeof contributesViews === 'object');
  const explorerViews = (contributesViews as { explorer?: unknown }).explorer;
  assert.ok(Array.isArray(explorerViews));
  assert.ok((explorerViews as unknown[]).some((entry) => (entry as { id?: unknown })?.id === 'novaFrameworks'));
  assert.ok((explorerViews as unknown[]).some((entry) => (entry as { id?: unknown })?.id === 'novaProjectExplorer'));

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

  const hasSafeModeHint = frameworksWelcome.some((entry) => {
    const when = typeof entry.when === 'string' ? entry.when : '';
    const contents = typeof entry.contents === 'string' ? entry.contents : '';
    return when.includes('nova.frameworks.safeMode') && (contents.includes('nova.bugReport') || contents.toLowerCase().includes('bug report'));
  });
  assert.ok(hasSafeModeHint);

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

  const projectExplorerWelcome = viewsWelcome.filter(
    (entry): entry is { view?: unknown; contents?: unknown; when?: unknown } =>
      entry && typeof entry === 'object' && (entry as { view?: unknown }).view === 'novaProjectExplorer',
  );

  assert.ok(projectExplorerWelcome.length >= 3);

  const hasProjectNoWorkspaceHint = projectExplorerWelcome.some((entry) => {
    const when = typeof entry.when === 'string' ? entry.when : '';
    const contents = typeof entry.contents === 'string' ? entry.contents : '';
    return when.includes('workspaceFolderCount') && when.includes('0') && contents.toLowerCase().includes('open folder');
  });
  assert.ok(hasProjectNoWorkspaceHint);

  const hasProjectServerMissingHint = projectExplorerWelcome.some((entry) => {
    const contents = typeof entry.contents === 'string' ? entry.contents : '';
    return contents.includes('nova.installOrUpdateServer');
  });
  assert.ok(hasProjectServerMissingHint);

  const hasProjectSafeModeHint = projectExplorerWelcome.some((entry) => {
    const when = typeof entry.when === 'string' ? entry.when : '';
    const contents = typeof entry.contents === 'string' ? entry.contents : '';
    return when.includes('nova.frameworks.safeMode') && (contents.includes('nova.bugReport') || contents.toLowerCase().includes('bug report'));
  });
  assert.ok(hasProjectSafeModeHint);

  const hasProjectUnsupportedHint = projectExplorerWelcome.some((entry) => {
    const contents = typeof entry.contents === 'string' ? entry.contents : '';
    return contents.toLowerCase().includes('upgrade') || contents.includes('nova.showServerVersion');
  });
  assert.ok(hasProjectUnsupportedHint);
});

test('package.json contributes Nova Project Explorer view + commands', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    activationEvents?: unknown;
    contributes?: { commands?: unknown; views?: unknown; menus?: unknown; viewsWelcome?: unknown };
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

  const viewsWelcome = Array.isArray(pkg.contributes?.viewsWelcome) ? pkg.contributes.viewsWelcome : [];
  const projectWelcome = viewsWelcome.filter(
    (entry): entry is { view?: unknown; contents?: unknown; when?: unknown } =>
      entry && typeof entry === 'object' && (entry as { view?: unknown }).view === 'novaProjectExplorer',
  );
  assert.ok(projectWelcome.length >= 1);
  assert.ok(projectWelcome.some((entry) => typeof entry.when === 'string' && entry.when.includes('workspaceFolderCount') && entry.when.includes('0')));

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

test('package.json contributes nova.build.buildTool setting', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    contributes?: { configuration?: { properties?: unknown } };
  };

  const properties = pkg.contributes?.configuration?.properties;
  assert.ok(properties && typeof properties === 'object');

  const setting = (properties as Record<string, unknown>)['nova.build.buildTool'];
  assert.ok(setting && typeof setting === 'object');

  const typed = setting as {
    type?: unknown;
    enum?: unknown;
    default?: unknown;
    scope?: unknown;
    description?: unknown;
  };

  assert.equal(typed.type, 'string');
  assert.deepEqual(typed.enum, ['auto', 'maven', 'gradle', 'prompt']);
  assert.equal(typed.default, 'auto');
  assert.equal(typed.scope, 'resource');
  assert.equal(
    typed.description,
    "Build tool to use for project builds/reloads. Use 'prompt' to choose each time.",
  );
});

test('package.json contributes nova.build.autoReloadOnBuildFileChange setting', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    contributes?: { configuration?: { properties?: unknown } };
  };

  const properties = pkg.contributes?.configuration?.properties;
  assert.ok(properties && typeof properties === 'object');

  const setting = (properties as Record<string, unknown>)['nova.build.autoReloadOnBuildFileChange'];
  assert.ok(setting && typeof setting === 'object');

  const typed = setting as {
    type?: unknown;
    default?: unknown;
    scope?: unknown;
    description?: unknown;
  };

  assert.equal(typed.type, 'boolean');
  assert.equal(typed.default, true);
  assert.equal(typed.scope, 'resource');
  assert.ok(typeof typed.description === 'string');
  assert.ok(typed.description.includes('workspace folder'));
});

test('package.json contributes nova.tests.buildTool setting', async () => {
  const pkgPath = path.resolve(__dirname, '../../package.json');
  const raw = await fs.readFile(pkgPath, 'utf8');
  const pkg = JSON.parse(raw) as {
    contributes?: { configuration?: { properties?: unknown } };
  };

  const properties = pkg.contributes?.configuration?.properties;
  assert.ok(properties && typeof properties === 'object');

  const setting = (properties as Record<string, unknown>)['nova.tests.buildTool'];
  assert.ok(setting && typeof setting === 'object');

  const typed = setting as {
    type?: unknown;
    enum?: unknown;
    default?: unknown;
    scope?: unknown;
    description?: unknown;
  };

  assert.equal(typed.type, 'string');
  assert.deepEqual(typed.enum, ['auto', 'maven', 'gradle', 'prompt']);
  assert.equal(typed.default, 'auto');
  assert.equal(typed.scope, 'resource');
  assert.ok(typeof typed.description === 'string');
  assert.ok(typed.description.includes('workspace folder'));
});

import { describe, expect, it } from 'vitest';

import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

describe('extension wiring', () => {
  it('routes Nova Project Explorer requests through sendNovaRequest({allowMethodFallback:true})', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const extensionPath = path.join(srcRoot, 'extension.ts');
    const contents = await fs.readFile(extensionPath, 'utf8');

    expect(contents).not.toMatch(/registerNovaProjectExplorer\(\s*context\s*,\s*sendNovaRequest\s*(,|\))/);

    // The Project Explorer's tree should not surface global unsupported-method popups. Instead,
    // route requests through the allowMethodFallback wrapper so the view can catch method-not-found
    // errors and render an "unsupported" placeholder node.
    expect(contents).toMatch(
      /const\s+requestWithFallback[\s\S]*?sendNovaRequest(?:<[^>]+>)?\(\s*method\s*,\s*params\s*,\s*\{\s*allowMethodFallback:\s*true\s*(?:,\s*token:\s*opts\?\.\s*token\s*)?\}\s*\)/s,
    );
    expect(contents).toMatch(/registerNovaProjectExplorer\(\s*context\s*,\s*requestWithFallback\s*,/);
  });

  it('routes Nova framework search requests through sendNovaRequest({allowMethodFallback:true})', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const extensionPath = path.join(srcRoot, 'extension.ts');
    const dashboardPath = path.join(srcRoot, 'frameworkDashboard.ts');
    const extensionContents = await fs.readFile(extensionPath, 'utf8');
    const dashboardContents = await fs.readFile(dashboardPath, 'utf8');

    // The framework search command is registered via the Frameworks dashboard module.
    expect(extensionContents).not.toMatch(/registerNovaFrameworkSearch\(/);
    expect(extensionContents).toMatch(/registerNovaFrameworkDashboard\(\s*context\s*,\s*sendNovaRequest/);

    // The dashboard should register framework search through an allowMethodFallback wrapper so
    // it can gracefully fall back (e.g. nova/web/endpoints -> nova/quarkus/endpoints) without
    // showing unsupported-method popups.
    expect(dashboardContents).toMatch(/registerNovaFrameworkSearch\([\s\S]*allowMethodFallback:\s*true/);
  });

  it('initializes nova.projectExplorer.projectModelSupported context to true on activation', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const extensionPath = path.join(srcRoot, 'extension.ts');
    const contents = await fs.readFile(extensionPath, 'utf8');

    expect(contents).toMatch(
      /executeCommand\(\s*['"]setContext['"]\s*,\s*['"]nova\.projectExplorer\.projectModelSupported['"]\s*,\s*true\s*\)/,
    );
  });

  it('treats the language server as running only when the client is starting or running', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const extensionPath = path.join(srcRoot, 'extension.ts');
    const contents = await fs.readFile(extensionPath, 'utf8');

    // Avoid treating a stale/stopped client instance as "running" for the Project Explorer view.
    expect(contents).toMatch(
      /registerNovaProjectExplorer\([\s\S]*isServerRunning:\s*\(\)\s*=>\s*client\?\.state\s*===\s*State\.Running[\s\S]*client\?\.state\s*===\s*State\.Starting/s,
    );
  });

  it('passes safe mode status to the Nova Project Explorer view', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const extensionPath = path.join(srcRoot, 'extension.ts');
    const contents = await fs.readFile(extensionPath, 'utf8');
 
    expect(contents).toMatch(/registerNovaProjectExplorer\([\s\S]*isSafeMode:\s*\(\)\s*=>\s*frameworksSafeMode/s);
  });

  it('makes nova.discoverTests cancellable and forwards the progress token to nova/test/discover', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const extensionPath = path.join(srcRoot, 'extension.ts');
    const contents = await fs.readFile(extensionPath, 'utf8');

    const discoverRegistrationIdx = contents.search(/registerCommand\(\s*['"]nova\.discoverTests['"]/);
    expect(discoverRegistrationIdx).toBeGreaterThanOrEqual(0);

    const afterDiscoverRegistration = contents.slice(discoverRegistrationIdx);

    expect(afterDiscoverRegistration).toMatch(/withProgress\(\s*\{[\s\S]*cancellable:\s*true/s);
    expect(afterDiscoverRegistration).toMatch(/discoverTestsForWorkspaces\(\s*workspaces\s*,\s*\{\s*token\s*\}\s*\)/);
    expect(afterDiscoverRegistration).toMatch(/refreshTests\(\s*discovered\s*,\s*\{\s*token\s*\}\s*\)/);
  });
});

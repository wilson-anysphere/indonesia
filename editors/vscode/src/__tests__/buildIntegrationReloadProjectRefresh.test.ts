import { describe, expect, it } from 'vitest';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

describe('nova.reloadProject', () => {
  it('refreshes Nova Project explorer after a successful reload', async () => {
    const testDir = path.dirname(fileURLToPath(import.meta.url));
    const buildIntegrationPath = path.resolve(testDir, '..', 'buildIntegration.ts');
    const contents = await fs.readFile(buildIntegrationPath, 'utf8');

    const reloadRegistrationIdx = contents.search(/registerCommand\(\s*['"]nova\.reloadProject['"]/);
    expect(reloadRegistrationIdx).toBeGreaterThanOrEqual(0);

    const afterReloadRegistration = contents.slice(reloadRegistrationIdx);

    const requestIdx = afterReloadRegistration.indexOf(`request('nova/reloadProject'`);
    const refreshExplorerIdx = afterReloadRegistration.indexOf(`executeCommand('nova.refreshProjectExplorer'`);

    expect(requestIdx).toBeGreaterThanOrEqual(0);
    expect(refreshExplorerIdx).toBeGreaterThanOrEqual(0);
    expect(refreshExplorerIdx).toBeGreaterThan(requestIdx);
  });
});


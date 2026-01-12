import { describe, expect, it } from 'vitest';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

describe('build integration polling', () => {
  it('triggers a silent diagnostics refresh when build status transitions to a terminal state', async () => {
    const testDir = path.dirname(fileURLToPath(import.meta.url));
    const buildIntegrationPath = path.resolve(testDir, '..', 'buildIntegration.ts');
    const contents = await fs.readFile(buildIntegrationPath, 'utf8');

    const pollOnceIdx = contents.indexOf('const pollBuildStatusOnce');
    expect(pollOnceIdx).toBeGreaterThanOrEqual(0);

    const afterPollOnce = contents.slice(pollOnceIdx);

    const statusRequestIdx = afterPollOnce.indexOf(`'nova/build/status'`);
    expect(statusRequestIdx).toBeGreaterThanOrEqual(0);

    const transitionHelperIdx = afterPollOnce.indexOf('shouldRefreshBuildDiagnosticsOnStatusTransition', statusRequestIdx);
    expect(transitionHelperIdx).toBeGreaterThan(statusRequestIdx);

    const refreshCallIdx = afterPollOnce.indexOf('refreshBuildDiagnostics(folder, { silent: true })', transitionHelperIdx);
    expect(refreshCallIdx).toBeGreaterThan(transitionHelperIdx);
  });
});


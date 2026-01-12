import { describe, expect, it } from 'vitest';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

describe('nova.build.buildTool setting', () => {
  it('build integration uses nova.build.buildTool for nova/buildProject and nova/reloadProject', async () => {
    const testDir = path.dirname(fileURLToPath(import.meta.url));
    const buildIntegrationPath = path.resolve(testDir, '..', 'buildIntegration.ts');
    const contents = await fs.readFile(buildIntegrationPath, 'utf8');

    expect(contents).toMatch(/getConfiguration\(\s*['"]nova['"]\s*,\s*workspace\.uri\s*\)/);
    expect(contents).toMatch(/get<string>\(\s*['"]build\.buildTool['"]\s*,\s*['"]auto['"]\s*\)/);

    // Ensure we forward the configured build tool to the server endpoints (rather than hard-coding "auto").
    expect(contents).toMatch(/request\(\s*['"]nova\/buildProject['"][\s\S]*?buildTool\s*,/);
    expect(contents).toMatch(/request\(\s*['"]nova\/reloadProject['"][\s\S]*?buildTool\s*,/);
  });

  it('skips the build tool picker when invoking build/reload for Bazel targets', async () => {
    const testDir = path.dirname(fileURLToPath(import.meta.url));
    const buildIntegrationPath = path.resolve(testDir, '..', 'buildIntegration.ts');
    const contents = await fs.readFile(buildIntegrationPath, 'utf8');

    const buildProjectIdx = contents.search(/registerCommand\(\s*['"]nova\.buildProject['"]/);
    expect(buildProjectIdx).toBeGreaterThanOrEqual(0);
    const afterBuildProject = contents.slice(buildProjectIdx);
    expect(afterBuildProject).toMatch(/target\s*\?\s*['"]auto['"]\s*:\s*await\s+getBuildBuildTool\(\s*folder\s*\)/);

    const reloadProjectIdx = contents.search(/registerCommand\(\s*['"]nova\.reloadProject['"]/);
    expect(reloadProjectIdx).toBeGreaterThanOrEqual(0);
    const afterReloadProject = contents.slice(reloadProjectIdx);
    expect(afterReloadProject).toMatch(/target\s*\?\s*['"]auto['"]\s*:\s*await\s+getBuildBuildTool\(\s*folder\s*\)/);
  });

  it('build-file auto reload uses nova.build.buildTool (without prompting)', async () => {
    const testDir = path.dirname(fileURLToPath(import.meta.url));
    const buildFileWatchPath = path.resolve(testDir, '..', 'buildFileWatch.ts');
    const contents = await fs.readFile(buildFileWatchPath, 'utf8');

    expect(contents).toMatch(/getConfiguration\(\s*['"]nova['"]\s*,\s*workspaceFolder\.uri\s*\)/);
    expect(contents).toMatch(/get<string>\(\s*['"]build\.buildTool['"]\s*,\s*['"]auto['"]\s*\)/);
    expect(contents).toMatch(/request\(\s*['"]nova\/reloadProject['"][\s\S]*?\{\s*projectRoot\s*,\s*buildTool\s*\}/);

    // Auto-reload should not open a picker UI.
    expect(contents).not.toMatch(/showQuickPick/);
  });
});

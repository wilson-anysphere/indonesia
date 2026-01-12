import { describe, expect, it } from 'vitest';

import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

describe('Nova Project Explorer commands', () => {
  it('shows an informational (not error) toast when project model/config endpoints are unsupported', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const filePath = path.join(srcRoot, 'projectExplorer.ts');
    const contents = await fs.readFile(filePath, 'utf8');

    // These commands are explicitly user-invoked via the view title menu / command palette, so
    // unsupported endpoints should still surface a user-facing message, but as information rather
    // than an error.
    expect(contents).toMatch(
      /showInformationMessage\(\s*formatUnsupportedNovaMethodMessage\(\s*['"]nova\/projectModel['"]\s*\)\s*\)/,
    );
    expect(contents).not.toMatch(
      /showErrorMessage\(\s*formatUnsupportedNovaMethodMessage\(\s*['"]nova\/projectModel['"]\s*\)\s*\)/,
    );

    expect(contents).toMatch(
      /showInformationMessage\(\s*formatUnsupportedNovaMethodMessage\(\s*['"]nova\/projectConfiguration['"]\s*\)\s*\)/,
    );
    expect(contents).not.toMatch(
      /showErrorMessage\(\s*formatUnsupportedNovaMethodMessage\(\s*['"]nova\/projectConfiguration['"]\s*\)\s*\)/,
    );
  });
});


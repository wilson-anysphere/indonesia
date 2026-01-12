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
      /const\s+requestWithFallback[\s\S]*?sendNovaRequest(?:<[^>]+>)?\(\s*method\s*,\s*params\s*,\s*\{\s*allowMethodFallback:\s*true\s*\}\s*\)/s,
    );
    expect(contents).toMatch(/registerNovaProjectExplorer\(\s*context\s*,\s*requestWithFallback\s*,/);
  });
});

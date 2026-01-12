import { describe, expect, it } from 'vitest';

import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

describe('extension file watcher wiring', () => {
  it('wires getNovaWatchedFileGlobPatterns into LanguageClientOptions.synchronize.fileEvents', async () => {
    const testsDir = path.dirname(fileURLToPath(import.meta.url));
    const extensionPath = path.join(testsDir, '..', 'extension.ts');
    const contents = await fs.readFile(extensionPath, 'utf8');

    // Ensure we didn't regress back to a single hard-coded Java watcher.
    expect(contents).not.toContain("createFileSystemWatcher('**/*.java')");

    // Ensure non-Java watcher globs are wired through the helper and used for fileEvents sync.
    expect(contents).toMatch(/const\s+fileWatchers\s*=\s*getNovaWatchedFileGlobPatterns\(\)\.map\(/);
    expect(contents).toMatch(/synchronize:\s*\{[\s\S]*?fileEvents:\s*fileWatchers/);
  });
});

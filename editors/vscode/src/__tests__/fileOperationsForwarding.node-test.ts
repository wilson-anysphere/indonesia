import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';

test('extension does not manually forward workspace/didRenameFiles', () => {
  const extensionPath = path.resolve(__dirname, '../../src/extension.ts');
  const source = fs.readFileSync(extensionPath, 'utf8');

  // When the server advertises `workspace.fileOperations.didRename`, the
  // `vscode-languageclient` fileOperations feature automatically wires up VS Code
  // `workspace.onDidRenameFiles` and forwards `workspace/didRenameFiles` to the
  // server. Manual forwarding would cause duplicate notifications.
  assert.doesNotMatch(source, /\bonDidRenameFiles\b/);
  assert.doesNotMatch(source, /workspace\/didRenameFiles/);
});


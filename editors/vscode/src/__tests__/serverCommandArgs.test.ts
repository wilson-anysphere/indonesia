import { describe, expect, it } from 'vitest';

import { readFileSync } from 'node:fs';

import { extractMainClassFromCommandArgs, extractTestIdFromCommandArgs } from '../serverCommandArgs';

describe('extractTestIdFromCommandArgs', () => {
  it('returns undefined when args are missing', () => {
    expect(extractTestIdFromCommandArgs([])).toBeUndefined();
    expect(extractTestIdFromCommandArgs([undefined])).toBeUndefined();
    expect(extractTestIdFromCommandArgs(['not-an-object'])).toBeUndefined();
  });

  it('supports camelCase', () => {
    expect(extractTestIdFromCommandArgs([{ testId: 'com.example.Test#method' }])).toBe('com.example.Test#method');
  });

  it('supports snake_case', () => {
    expect(extractTestIdFromCommandArgs([{ test_id: 'com.example.Test#method' }])).toBe('com.example.Test#method');
  });

  it('trims whitespace and rejects non-strings', () => {
    expect(extractTestIdFromCommandArgs([{ testId: '  abc  ' }])).toBe('abc');
    expect(extractTestIdFromCommandArgs([{ testId: 123 }])).toBeUndefined();
  });
});

describe('extractMainClassFromCommandArgs', () => {
  it('returns undefined when args are missing', () => {
    expect(extractMainClassFromCommandArgs([])).toBeUndefined();
    expect(extractMainClassFromCommandArgs([undefined])).toBeUndefined();
    expect(extractMainClassFromCommandArgs(['not-an-object'])).toBeUndefined();
  });

  it('supports camelCase', () => {
    expect(extractMainClassFromCommandArgs([{ mainClass: 'com.example.Main' }])).toBe('com.example.Main');
  });

  it('supports snake_case', () => {
    expect(extractMainClassFromCommandArgs([{ main_class: 'com.example.Main' }])).toBe('com.example.Main');
  });

  it('trims whitespace and rejects non-strings', () => {
    expect(extractMainClassFromCommandArgs([{ mainClass: '  com.example.Main  ' }])).toBe('com.example.Main');
    expect(extractMainClassFromCommandArgs([{ mainClass: 123 }])).toBeUndefined();
  });
});

describe('server command handler signatures', () => {
  it('does not locally register server-advertised executeCommandProvider command IDs', () => {
    const src = readFileSync('src/serverCommands.ts', 'utf8');
    // These command IDs are registered automatically by vscode-languageclient when the server
    // advertises them via `capabilities.executeCommandProvider.commands`. Registering them locally
    // would collide and can fail client initialization.
    const serverCommandIds = [
      'nova.runTest',
      'nova.debugTest',
      'nova.runMain',
      'nova.debugMain',
      'nova.extractMethod',
    ];

    for (const id of serverCommandIds) {
      // Direct registration.
      expect(src).not.toMatch(new RegExp(`registerCommand\\(\\s*['"]${id.replace('.', '\\\\.')}['"]`));
      // Registration via helper (historical).
      expect(src).not.toMatch(new RegExp(`registerCommandSafe\\([^\\n]*['"]${id.replace('.', '\\\\.')}['"]`));
    }
  });
});

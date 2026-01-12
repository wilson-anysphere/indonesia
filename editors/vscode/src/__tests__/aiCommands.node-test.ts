import assert from 'node:assert/strict';
import test from 'node:test';

import {
  isNovaAiCodeActionKind,
  isNovaAiCodeActionOrCommand,
  isNovaAiFileBackedCodeActionOrCommand,
  rewriteNovaAiCodeActionOrCommand,
  NOVA_AI_LSP_COMMAND_EXPLAIN_ERROR,
  NOVA_AI_LSP_COMMAND_GENERATE_METHOD_BODY,
  NOVA_AI_LSP_COMMAND_GENERATE_TESTS,
  NOVA_AI_SHOW_EXPLAIN_ERROR_COMMAND,
  NOVA_AI_SHOW_GENERATE_METHOD_BODY_COMMAND,
  NOVA_AI_SHOW_GENERATE_TESTS_COMMAND,
} from '../aiCommands';

test('isNovaAiCodeActionKind detects nova.explain and nova.ai.* kinds', () => {
  assert.equal(isNovaAiCodeActionKind({ value: 'nova.explain' }), true);
  assert.equal(isNovaAiCodeActionKind({ value: 'nova.ai.generate' }), true);
  assert.equal(isNovaAiCodeActionKind({ value: 'nova.ai.tests' }), true);
  assert.equal(isNovaAiCodeActionKind({ value: 'nova.ai.someFutureThing' }), true);

  assert.equal(isNovaAiCodeActionKind({ value: 'refactor.extract' }), false);
  assert.equal(isNovaAiCodeActionKind(undefined), false);
  assert.equal(isNovaAiCodeActionKind({}), false);
});

test('isNovaAiCodeActionOrCommand detects AI code actions and AI commands', () => {
  assert.equal(
    isNovaAiCodeActionOrCommand({
      kind: { value: 'nova.ai.generate' },
      title: 'Generate method body with AI',
      command: { command: 'nova.ai.generateMethodBody', title: 'Generate', arguments: [] },
    }),
    true,
  );

  assert.equal(
    isNovaAiCodeActionOrCommand({
      command: 'nova.ai.generateTests',
      title: 'Generate tests',
      arguments: [{ target: 'foo' }],
    }),
    true,
  );

  assert.equal(
    isNovaAiCodeActionOrCommand({
      command: 'editor.action.formatDocument',
      title: 'Format Document',
    }),
    false,
  );
});

test('isNovaAiFileBackedCodeActionOrCommand detects patch-based AI code actions/commands', () => {
  assert.equal(
    isNovaAiFileBackedCodeActionOrCommand({
      kind: { value: 'nova.ai.generate' },
      title: 'Generate method body with AI',
    }),
    true,
  );

  assert.equal(
    isNovaAiFileBackedCodeActionOrCommand({
      command: NOVA_AI_LSP_COMMAND_GENERATE_TESTS,
      title: 'Generate tests with AI',
      arguments: [],
    }),
    true,
  );

  assert.equal(
    isNovaAiFileBackedCodeActionOrCommand({
      kind: { value: 'nova.explain' },
      title: 'Explain this error',
      command: NOVA_AI_LSP_COMMAND_EXPLAIN_ERROR,
      arguments: [],
    }),
    false,
  );

  assert.equal(
    isNovaAiFileBackedCodeActionOrCommand({
      kind: { value: 'refactor.extract' },
      title: 'Extract method',
    }),
    false,
  );
});

test('rewriteNovaAiCodeActionOrCommand maps explainError to the VS Code-side command', () => {
  const rewritten = rewriteNovaAiCodeActionOrCommand({
    kind: { value: 'nova.explain' },
    title: 'Explain this error',
    command: {
      command: NOVA_AI_LSP_COMMAND_EXPLAIN_ERROR,
      title: 'Explain this error',
      arguments: [{ diagnostic_message: 'cannot find symbol' }],
    },
  });

  assert.deepEqual(rewritten, {
    command: NOVA_AI_SHOW_EXPLAIN_ERROR_COMMAND,
    args: [
      {
        lspCommand: NOVA_AI_LSP_COMMAND_EXPLAIN_ERROR,
        lspArguments: [{ diagnostic_message: 'cannot find symbol' }],
        kind: 'nova.explain',
        title: 'Explain this error',
      },
    ],
  });
});

test('rewriteNovaAiCodeActionOrCommand maps generateMethodBody to the VS Code-side command', () => {
  const rewritten = rewriteNovaAiCodeActionOrCommand({
    kind: { value: 'nova.ai.generate' },
    title: 'Generate method body with AI',
    command: {
      command: NOVA_AI_LSP_COMMAND_GENERATE_METHOD_BODY,
      title: 'Generate method body with AI',
      arguments: [{ method_signature: 'public int add(int a, int b)' }],
    },
  });

  assert.deepEqual(rewritten?.command, NOVA_AI_SHOW_GENERATE_METHOD_BODY_COMMAND);
  assert.deepEqual(rewritten?.args[0].lspCommand, NOVA_AI_LSP_COMMAND_GENERATE_METHOD_BODY);
});

test('rewriteNovaAiCodeActionOrCommand maps generateTests to the VS Code-side command', () => {
  const rewritten = rewriteNovaAiCodeActionOrCommand({
    command: NOVA_AI_LSP_COMMAND_GENERATE_TESTS,
    title: 'Generate tests with AI',
    arguments: [{ target: 'public int add(int a, int b)' }],
  });

  assert.deepEqual(rewritten, {
    command: NOVA_AI_SHOW_GENERATE_TESTS_COMMAND,
    args: [
      {
        lspCommand: NOVA_AI_LSP_COMMAND_GENERATE_TESTS,
        lspArguments: [{ target: 'public int add(int a, int b)' }],
        kind: undefined,
        title: 'Generate tests with AI',
      },
    ],
  });
});

test('rewriteNovaAiCodeActionOrCommand returns undefined when a command id is missing (cannot execute)', () => {
  const rewritten = rewriteNovaAiCodeActionOrCommand({
    kind: { value: 'nova.ai.tests' },
    title: 'Generate tests with AI',
  });

  assert.equal(rewritten, undefined);
});

test('rewriteNovaAiCodeActionOrCommand returns undefined for non-AI actions', () => {
  const rewritten = rewriteNovaAiCodeActionOrCommand({
    kind: { value: 'refactor.extract' },
    title: 'Extract method',
    command: { command: 'nova.refactor.extractMethod', title: 'Extract method', arguments: [] },
  });

  assert.equal(rewritten, undefined);
});

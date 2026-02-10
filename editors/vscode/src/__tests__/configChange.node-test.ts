import test from 'node:test';
import assert from 'node:assert/strict';

import { getNovaConfigChangeEffects } from '../configChange';

function eventFor(changed: readonly string[]) {
  const changedSet = new Set(changed);
  return {
    affectsConfiguration: (section: string) => changedSet.has(section),
  };
}

test('prompts restart when nova.aiCompletions.enabled changes', () => {
  const effects = getNovaConfigChangeEffects(eventFor(['nova.aiCompletions.enabled']));
  assert.equal(effects.shouldPromptRestartLanguageServer, true);
});

test('prompts restart when nova.aiCompletions.maxItems changes', () => {
  const effects = getNovaConfigChangeEffects(eventFor(['nova.aiCompletions.maxItems']));
  assert.equal(effects.shouldPromptRestartLanguageServer, true);
  assert.equal(effects.shouldClearAiCompletionCache, true);
});

test('preserves existing restart triggers (nova.ai.enabled / nova.lsp.* / nova.server.args)', () => {
  assert.equal(getNovaConfigChangeEffects(eventFor(['nova.ai.enabled'])).shouldPromptRestartLanguageServer, true);
  assert.equal(getNovaConfigChangeEffects(eventFor(['nova.lsp.configPath'])).shouldPromptRestartLanguageServer, true);
  assert.equal(getNovaConfigChangeEffects(eventFor(['nova.lsp.extraArgs'])).shouldPromptRestartLanguageServer, true);
  assert.equal(getNovaConfigChangeEffects(eventFor(['nova.server.args'])).shouldPromptRestartLanguageServer, true);
});

test('prompts restart when nova.aiCodeActions.enabled changes', () => {
  const effects = getNovaConfigChangeEffects(eventFor(['nova.aiCodeActions.enabled']));
  assert.equal(effects.shouldPromptRestartLanguageServer, true);
});

test('prompts restart when nova.aiCodeReview.enabled changes', () => {
  const effects = getNovaConfigChangeEffects(eventFor(['nova.aiCodeReview.enabled']));
  assert.equal(effects.shouldPromptRestartLanguageServer, true);
});

test('does not prompt restart when nova.server.path changes (auto-restart path)', () => {
  const effects = getNovaConfigChangeEffects(eventFor(['nova.server.path', 'nova.aiCompletions.enabled']));
  assert.equal(effects.serverPathChanged, true);
  assert.equal(effects.shouldPromptRestartLanguageServer, false);
  assert.equal(effects.shouldClearAiCompletionCache, true);
});

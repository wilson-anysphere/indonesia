import { describe, expect, it } from 'vitest';

import { getNovaConfigChangeEffects, type ConfigurationChangeEventLike } from './configChange';

function eventFor(changed: readonly string[]): ConfigurationChangeEventLike {
  const changedSet = new Set(changed);
  return {
    affectsConfiguration(section: string) {
      return changedSet.has(section);
    },
  };
}

describe('getNovaConfigChangeEffects', () => {
  it('prompts for language server restart when nova.aiCompletions.enabled changes', () => {
    const effects = getNovaConfigChangeEffects(eventFor(['nova.aiCompletions.enabled']));
    expect(effects.shouldPromptRestartLanguageServer).toBe(true);
    expect(effects.shouldClearAiCompletionCache).toBe(true);
  });

  it('prompts for language server restart when nova.aiCompletions.maxItems changes', () => {
    const effects = getNovaConfigChangeEffects(eventFor(['nova.aiCompletions.maxItems']));
    expect(effects.shouldPromptRestartLanguageServer).toBe(true);
    expect(effects.shouldClearAiCompletionCache).toBe(true);
  });

  it('prompts for language server restart when nova.aiCodeActions.enabled changes', () => {
    const effects = getNovaConfigChangeEffects(eventFor(['nova.aiCodeActions.enabled']));
    expect(effects.shouldPromptRestartLanguageServer).toBe(true);
  });

  it('prompts for language server restart when nova.aiCodeReview.enabled changes', () => {
    const effects = getNovaConfigChangeEffects(eventFor(['nova.aiCodeReview.enabled']));
    expect(effects.shouldPromptRestartLanguageServer).toBe(true);
  });

  it('does not prompt for restart when nova.server.path changes (server restarts automatically)', () => {
    const effects = getNovaConfigChangeEffects(eventFor(['nova.server.path', 'nova.aiCompletions.enabled']));
    expect(effects.serverPathChanged).toBe(true);
    expect(effects.shouldPromptRestartLanguageServer).toBe(false);
  });
});

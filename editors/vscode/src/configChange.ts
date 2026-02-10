export interface ConfigurationChangeEventLike {
  affectsConfiguration(section: string): boolean;
}

export interface NovaConfigChangeEffects {
  /**
   * `nova.server.path` changed (server binary override). The extension will attempt to
   * restart the language server automatically.
   */
  serverPathChanged: boolean;
  /**
   * Download settings changed. The extension will re-resolve the managed server binary
   * (and restart if needed) unless `serverPathChanged` is also true.
   */
  serverDownloadChanged: boolean;
  /**
   * Settings changed that require restarting nova-lsp to take effect (e.g. settings
   * that influence CLI args / environment variables).
   */
  shouldPromptRestartLanguageServer: boolean;
  /**
   * Settings changed that require clearing the in-memory AI completion cache.
   */
  shouldClearAiCompletionCache: boolean;
}

/**
 * Compute the side effects of a VS Code configuration change event for Nova.
 *
 * Kept in a separate module (no `vscode` imports) so it can be unit tested with plain Node.
 */
export function getNovaConfigChangeEffects(event: ConfigurationChangeEventLike): NovaConfigChangeEffects {
  const serverPathChanged = event.affectsConfiguration('nova.server.path');
  const serverDownloadChanged =
    event.affectsConfiguration('nova.download.mode') ||
    event.affectsConfiguration('nova.download.releaseTag') ||
    event.affectsConfiguration('nova.download.baseUrl') ||
    event.affectsConfiguration('nova.download.allowPrerelease') ||
    event.affectsConfiguration('nova.download.allowVersionMismatch');

  const shouldPromptRestartLanguageServer =
    !serverPathChanged &&
    (event.affectsConfiguration('nova.lsp.configPath') ||
      event.affectsConfiguration('nova.lsp.extraArgs') ||
      event.affectsConfiguration('nova.server.args') ||
      event.affectsConfiguration('nova.ai.enabled') ||
      // Server-side AI completions settings are controlled via env/CLI at nova-lsp startup, so
      // changing these settings requires a server restart to take effect.
      event.affectsConfiguration('nova.aiCompletions.enabled') ||
      event.affectsConfiguration('nova.aiCompletions.maxItems') ||
      event.affectsConfiguration('nova.aiCodeActions.enabled') ||
      event.affectsConfiguration('nova.aiCodeReview.enabled'));

  const shouldClearAiCompletionCache =
    event.affectsConfiguration('nova.ai.enabled') ||
    event.affectsConfiguration('nova.aiCompletions.enabled') ||
    event.affectsConfiguration('nova.aiCompletions.maxItems');

  return {
    serverPathChanged,
    serverDownloadChanged,
    shouldPromptRestartLanguageServer,
    shouldClearAiCompletionCache,
  };
}

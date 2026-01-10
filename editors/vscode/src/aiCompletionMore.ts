import * as vscode from 'vscode';
import type { LanguageClient } from 'vscode-languageclient/node';
import type { CompletionItem as ProtocolCompletionItem } from 'vscode-languageserver-protocol';

const MORE_COMPLETIONS_METHOD = 'nova/completion/more';

type NovaCompletionData = {
  nova?: {
    completion_context_id?: string;
  };
};

export function getCompletionContextId(items: readonly vscode.CompletionItem[]): string | undefined {
  for (const item of items) {
    const data = item.data as NovaCompletionData | undefined;
    const id = data?.nova?.completion_context_id;
    if (typeof id === 'string' && id.length > 0) {
      return id;
    }
  }
  return undefined;
}

export async function requestMoreCompletions(
  client: LanguageClient,
  completionItems: readonly vscode.CompletionItem[],
): Promise<vscode.CompletionItem[] | undefined> {
  const enabled = vscode.workspace.getConfiguration('nova').get<boolean>('aiCompletions.enabled', true);
  if (!enabled) {
    return undefined;
  }

  const contextId = getCompletionContextId(completionItems);
  if (!contextId) {
    return undefined;
  }

  const maxItems = vscode.workspace.getConfiguration('nova').get<number>('aiCompletions.maxItems', 5);
  if (maxItems <= 0) {
    return undefined;
  }

  try {
    const result = await client.sendRequest<{ items: ProtocolCompletionItem[]; is_incomplete: boolean }>(
      MORE_COMPLETIONS_METHOD,
      { context_id: contextId },
    );

    if (!result?.items?.length) {
      return undefined;
    }

    return result.items.slice(0, maxItems).map((item) => client.protocol2CodeConverter.asCompletionItem(item));
  } catch {
    // Graceful degradation: if the server doesn't support the custom request or AI is disabled.
    return undefined;
  }
}

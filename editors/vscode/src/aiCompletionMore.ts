import type { CompletionItem as ProtocolCompletionItem } from 'vscode-languageserver-protocol';

const MORE_COMPLETIONS_METHOD = 'nova/completion/more';

type NovaCompletionData = {
  nova?: {
    completion_context_id?: string;
  };
};

export type CompletionItemLike = {
  // Include `label` to avoid TypeScript "weak type" assignability issues when called with `vscode.CompletionItem`.
  label?: unknown;
  data?: unknown;
};

export interface CancellationTokenLike {
  readonly isCancellationRequested: boolean;
  onCancellationRequested(listener: () => void): { dispose(): void };
}

type MoreCompletionsResult = { items: ProtocolCompletionItem[]; is_incomplete: boolean };

export interface MoreCompletionsClient<TCompletionItem = unknown> {
  // Use `any` for compatibility with `vscode-languageclient`'s overloaded `sendRequest` signature.
  sendRequest: <R>(method: any, params?: any, token?: any) => Promise<R>;
  protocol2CodeConverter: { asCompletionItem(item: ProtocolCompletionItem): TCompletionItem };
}

export interface RequestMoreCompletionsOptions {
  token?: CancellationTokenLike;

  /**
   * Enable Nova AI multi-token completions via `nova/completion/more`.
   * Defaults to `nova.aiCompletions.enabled` (or `true` when vscode is unavailable).
   */
  enabled?: boolean;

  /**
   * Maximum number of AI completion items to return.
   * Defaults to `nova.aiCompletions.maxItems` (or `5` when vscode is unavailable).
   */
  maxItems?: number;

  /**
   * Maximum wall-clock time to spend polling for `nova/completion/more`.
   * Defaults to `nova.aiCompletions.requestTimeoutMs` (or `1000` when vscode is unavailable).
   */
  requestTimeoutMs?: number;

  /**
   * Base delay between poll requests. The polling loop uses an exponential backoff starting at
   * `pollIntervalMs / 2` and capping at `pollIntervalMs * 2`.
   *
   * Defaults to `nova.aiCompletions.pollIntervalMs` (or `50` when vscode is unavailable).
   */
  pollIntervalMs?: number;

  /**
   * Test hooks.
   */
  now?: () => number;
  sleep?: (ms: number, token?: CancellationTokenLike) => Promise<void>;
}

export function getCompletionContextId(items: readonly CompletionItemLike[]): string | undefined {
  for (const item of items) {
    const data = (item as unknown as { data?: unknown }).data as NovaCompletionData | undefined;
    const id = data?.nova?.completion_context_id;
    if (typeof id === 'string' && id.length > 0) {
      return id;
    }
  }
  return undefined;
}

type ResolvedRequestOptions = Required<
  Pick<RequestMoreCompletionsOptions, 'enabled' | 'maxItems' | 'requestTimeoutMs' | 'pollIntervalMs'>
> &
  Pick<RequestMoreCompletionsOptions, 'token' | 'now' | 'sleep'>;

function getConfigFromVscode(): Partial<ResolvedRequestOptions> {
  try {
    // Avoid a hard dependency on the `vscode` module so this file can be unit tested with plain Node.
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    const vscode = require('vscode') as typeof import('vscode');
    const config = vscode.workspace.getConfiguration('nova');

    return {
      enabled: config.get<boolean>('aiCompletions.enabled', true),
      maxItems: config.get<number>('aiCompletions.maxItems', 5),
      requestTimeoutMs: config.get<number>('aiCompletions.requestTimeoutMs', 1000),
      pollIntervalMs: config.get<number>('aiCompletions.pollIntervalMs', 50),
    };
  } catch {
    return {};
  }
}

function resolveRequestOptions(options: RequestMoreCompletionsOptions): ResolvedRequestOptions {
  const config = getConfigFromVscode();

  const enabled = options.enabled ?? config.enabled ?? true;
  const maxItems = options.maxItems ?? config.maxItems ?? 5;
  const requestTimeoutMs = options.requestTimeoutMs ?? config.requestTimeoutMs ?? 1000;
  const pollIntervalMs = options.pollIntervalMs ?? config.pollIntervalMs ?? 50;

  return {
    token: options.token,
    enabled,
    maxItems,
    requestTimeoutMs,
    pollIntervalMs,
    now: options.now,
    sleep: options.sleep,
  };
}

async function sleep(ms: number, token?: CancellationTokenLike): Promise<void> {
  if (ms <= 0) {
    return;
  }

  if (!token) {
    await new Promise<void>((resolve) => setTimeout(resolve, ms));
    return;
  }

  if (token.isCancellationRequested) {
    return;
  }

  await new Promise<void>((resolve) => {
    let disposable: { dispose(): void } | undefined;
    const timer = setTimeout(() => {
      disposable?.dispose();
      resolve();
    }, ms);

    disposable = token.onCancellationRequested(() => {
      clearTimeout(timer);
      disposable?.dispose();
      resolve();
    });
  });
}

export async function requestMoreCompletions<TCompletionItem>(
  client: MoreCompletionsClient<TCompletionItem>,
  completionItems: readonly CompletionItemLike[],
  options: RequestMoreCompletionsOptions = {},
): Promise<TCompletionItem[] | undefined> {
  const resolved = resolveRequestOptions(options);
  const now = resolved.now ?? Date.now;
  const sleepFn = resolved.sleep ?? sleep;

  const enabled = resolved.enabled;
  if (!enabled) {
    return undefined;
  }

  const contextId = getCompletionContextId(completionItems);
  if (!contextId) {
    return undefined;
  }

  const maxItems = resolved.maxItems;
  if (maxItems <= 0) {
    return undefined;
  }

  const requestTimeoutMs = resolved.requestTimeoutMs;
  if (requestTimeoutMs <= 0) {
    return undefined;
  }

  // `pollIntervalMs` is the base delay (see setting description). Use a tiny exponential backoff
  // (e.g. 25ms -> 50ms -> 100ms) to balance responsiveness with server load.
  const pollIntervalMs = Math.max(1, resolved.pollIntervalMs);
  const initialDelayMs = Math.max(1, Math.floor(pollIntervalMs / 2));
  const maxDelayMs = Math.max(initialDelayMs, pollIntervalMs * 2);

  const deadlineMs = now() + requestTimeoutMs;
  let delayMs = initialDelayMs;

  try {
    while (true) {
      if (resolved.token?.isCancellationRequested) {
        return undefined;
      }

      if (now() >= deadlineMs) {
        return undefined;
      }

      const result = await client.sendRequest<MoreCompletionsResult>(
        MORE_COMPLETIONS_METHOD,
        { context_id: contextId },
        resolved.token,
      );
      if (!result) {
        return undefined;
      }

      const items = result.items ?? [];
      if (items.length > 0) {
        return items.slice(0, maxItems).map((item) => client.protocol2CodeConverter.asCompletionItem(item));
      }

      if (!result.is_incomplete) {
        return undefined;
      }

      const remainingMs = deadlineMs - now();
      if (remainingMs <= 0) {
        return undefined;
      }

      // Delay before the next poll. Cap each sleep by the remaining time budget so we don't
      // overshoot the configured timeout by much.
      await sleepFn(Math.min(delayMs, remainingMs), resolved.token);
      delayMs = Math.min(delayMs * 2, maxDelayMs);
    }
  } catch {
    // Graceful degradation: if the server doesn't support the custom request or AI is disabled.
    return undefined;
  }
}

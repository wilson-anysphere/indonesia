import type { LanguageClient } from 'vscode-languageclient/node';

export type NovaExperimentalCapabilities = {
  requests: Set<string>;
  notifications: Set<string>;
};

let currentNovaExperimentalCapabilities: NovaExperimentalCapabilities | undefined;

function asObject(value: unknown): Record<string, unknown> | undefined {
  if (!value || typeof value !== 'object') {
    return undefined;
  }
  return value as Record<string, unknown>;
}

function parseStringArray(value: unknown): string[] | undefined {
  if (!Array.isArray(value)) {
    return undefined;
  }
  const out: string[] = [];
  for (const item of value) {
    if (typeof item === 'string') {
      out.push(item);
    }
  }
  return out;
}

export function parseNovaExperimentalCapabilities(initializeResult: unknown): NovaExperimentalCapabilities | undefined {
  const root = asObject(initializeResult);
  const capabilities = asObject(root?.capabilities);
  const experimental = asObject(capabilities?.experimental);
  const nova = asObject(experimental?.nova);
  if (!nova) {
    return undefined;
  }

  const requests = parseStringArray(nova.requests);
  const notifications = parseStringArray(nova.notifications);

  // Only treat the capability list as authoritative when both lists are present.
  if (!requests || !notifications) {
    return undefined;
  }

  return { requests: new Set(requests), notifications: new Set(notifications) };
}

/**
 * Returns the set of supported `nova/*` request methods as advertised by the server.
 *
 * This is sourced from `initializeResult.capabilities.experimental.nova.requests`.
 *
 * Note: Older Nova builds may omit these lists. In that case, `undefined` is returned so callers
 * can fall back to optimistic requests + graceful method-not-found handling.
 */
export function getSupportedNovaRequests(client: LanguageClient): Set<string> | undefined {
  const parsed = parseNovaExperimentalCapabilities(client.initializeResult);
  return parsed?.requests;
}

export function setNovaExperimentalCapabilities(initializeResult: unknown): void {
  currentNovaExperimentalCapabilities = parseNovaExperimentalCapabilities(initializeResult);
}

export function resetNovaExperimentalCapabilities(): void {
  currentNovaExperimentalCapabilities = undefined;
}

export function isNovaRequestSupported(method: string): boolean | 'unknown' {
  if (!method.startsWith('nova/')) {
    return 'unknown';
  }
  if (!currentNovaExperimentalCapabilities) {
    return 'unknown';
  }
  return currentNovaExperimentalCapabilities.requests.has(method);
}

export function isNovaNotificationSupported(method: string): boolean | 'unknown' {
  if (!method.startsWith('nova/')) {
    return 'unknown';
  }
  if (!currentNovaExperimentalCapabilities) {
    return 'unknown';
  }
  return currentNovaExperimentalCapabilities.notifications.has(method);
}

export function formatUnsupportedNovaMethodMessage(method: string): string {
  return `Nova: server does not support ${method}. You may be running an older nova-lsp; update or disable allowVersionMismatch.`;
}

/**
 * Detect "unsupported extension method" errors for `nova/*` requests when capability lists are not
 * available.
 *
 * Note: Nova routes most `nova/*` methods through a dispatcher which historically returns:
 * - `-32601` for "method not found"
 * - `-32602` with an "unknown (stateless) method" message for unknown `nova/*` methods
 */
export function isNovaMethodNotFoundError(err: unknown): boolean {
  const obj = asObject(err);
  if (!obj) {
    return false;
  }

  const code = obj.code;
  if (code === -32601) {
    return true;
  }

  const message = obj.message;
  if (code === -32602 && typeof message === 'string' && message.toLowerCase().includes('unknown (stateless) method')) {
    return true;
  }

  return typeof message === 'string' && message.toLowerCase().includes('method not found');
}

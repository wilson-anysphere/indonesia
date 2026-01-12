import type { LanguageClient } from 'vscode-languageclient/node';

export type NovaExperimentalCapabilities = {
  requests: Set<string>;
  notifications: Set<string>;
};

const DEFAULT_NOVA_CAPABILITIES_KEY = 'default';

const novaExperimentalCapabilitiesByKey = new Map<string, NovaExperimentalCapabilities>();

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

export function setNovaExperimentalCapabilities(key: string, initializeResult: unknown): void;
export function setNovaExperimentalCapabilities(initializeResult: unknown): void;
export function setNovaExperimentalCapabilities(keyOrInitializeResult: string | unknown, initializeResult?: unknown): void {
  if (arguments.length === 2) {
    const key = String(keyOrInitializeResult);
    const parsed = parseNovaExperimentalCapabilities(initializeResult);
    if (parsed) {
      novaExperimentalCapabilitiesByKey.set(key, parsed);
    } else {
      novaExperimentalCapabilitiesByKey.delete(key);
    }
    return;
  }

  const parsed = parseNovaExperimentalCapabilities(keyOrInitializeResult);
  if (parsed) {
    novaExperimentalCapabilitiesByKey.set(DEFAULT_NOVA_CAPABILITIES_KEY, parsed);
  } else {
    novaExperimentalCapabilitiesByKey.delete(DEFAULT_NOVA_CAPABILITIES_KEY);
  }
}

export function resetNovaExperimentalCapabilities(key: string): void;
export function resetNovaExperimentalCapabilities(): void;
export function resetNovaExperimentalCapabilities(key?: string): void {
  novaExperimentalCapabilitiesByKey.delete(typeof key === 'string' ? key : DEFAULT_NOVA_CAPABILITIES_KEY);
}

export function isNovaRequestSupported(key: string, method: string): boolean | 'unknown';
export function isNovaRequestSupported(method: string): boolean | 'unknown';
export function isNovaRequestSupported(keyOrMethod: string, methodArg?: string): boolean | 'unknown' {
  const key = typeof methodArg === 'string' ? keyOrMethod : DEFAULT_NOVA_CAPABILITIES_KEY;
  const method = typeof methodArg === 'string' ? methodArg : keyOrMethod;
  if (!method.startsWith('nova/')) {
    return 'unknown';
  }

  const capabilities = novaExperimentalCapabilitiesByKey.get(key);
  if (!capabilities) {
    return 'unknown';
  }

  return capabilities.requests.has(method);
}

export function isNovaNotificationSupported(key: string, method: string): boolean | 'unknown';
export function isNovaNotificationSupported(method: string): boolean | 'unknown';
export function isNovaNotificationSupported(keyOrMethod: string, methodArg?: string): boolean | 'unknown' {
  const key = typeof methodArg === 'string' ? keyOrMethod : DEFAULT_NOVA_CAPABILITIES_KEY;
  const method = typeof methodArg === 'string' ? methodArg : keyOrMethod;
  if (!method.startsWith('nova/')) {
    return 'unknown';
  }

  const capabilities = novaExperimentalCapabilitiesByKey.get(key);
  if (!capabilities) {
    return 'unknown';
  }

  return capabilities.notifications.has(method);
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

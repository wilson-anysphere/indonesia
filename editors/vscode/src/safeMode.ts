export const SAFE_MODE_EXEMPT_REQUESTS: ReadonlySet<string> = new Set<string>([
  'nova/bugReport',
  // These endpoints are intentionally available even while Nova is in safe mode, or have
  // historically bypassed the safe-mode guard in older server builds. A successful response
  // should not be treated as an indication that safe mode has exited.
  'nova/memoryStatus',
  'nova/metrics',
  'nova/resetMetrics',
  // Best-effort: safe mode status endpoints may exist in newer server builds.
  'nova/safeModeStatus',
  // `nova/java/organizeImports` has historically been handled outside the standard custom-request
  // dispatcher in some nova-lsp stdio server builds, so it may succeed even while safe mode is
  // active.
  'nova/java/organizeImports',
  // AI endpoints may be handled outside the standard custom-request dispatcher in some server
  // builds, so they should not clear the safe-mode indicator.
  'nova/completion/more',
  'nova/ai/explainError',
  'nova/ai/codeReview',
  'nova/ai/models',
  'nova/ai/status',
  'nova/ai/generateMethodBody',
  'nova/ai/generateTests',
  // Internal/experimental endpoints that may bypass safe-mode guard in some builds.
  'nova/semanticSearch/indexStatus',
  'nova/semanticSearch/reindex',
  'nova/semanticSearch/search',
]);

export function formatError(err: unknown): string {
  if (err instanceof Error) {
    return err.message;
  }
  if (typeof err === 'string') {
    return err;
  }
  if (err && typeof err === 'object' && 'message' in err && typeof (err as { message: unknown }).message === 'string') {
    return (err as { message: string }).message;
  }
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

export function isMethodNotFoundError(err: unknown): boolean {
  if (!err || typeof err !== 'object') {
    return false;
  }

  const code = (err as { code?: unknown }).code;
  if (code === -32601) {
    return true;
  }

  const message = (err as { message?: unknown }).message;
  // `nova-lsp` currently reports unknown `nova/*` custom methods as `-32602` with an
  // "unknown (stateless) method" message (because everything is routed through a single dispatcher).
  if (
    code === -32602 &&
    typeof message === 'string' &&
    message.toLowerCase().includes('unknown (stateless) method')
  ) {
    return true;
  }
  return typeof message === 'string' && message.toLowerCase().includes('method not found');
}

export function isSafeModeError(err: unknown): boolean {
  const message = formatError(err).toLowerCase();
  if (message.includes('safe-mode') || message.includes('safe mode')) {
    return true;
  }

  // Defensive: handle safe-mode guard messages that might not include the exact phrase.
  return message.includes('nova/bugreport') && message.includes('only') && message.includes('available');
}

export function isUnknownExecuteCommandError(err: unknown): boolean {
  if (!err || typeof err !== 'object') {
    return false;
  }

  const code = (err as { code?: unknown }).code;
  if (code !== -32602) {
    return false;
  }

  return formatError(err).toLowerCase().includes('unknown command');
}

export function parseSafeModeEnabled(payload: unknown): boolean | undefined {
  if (typeof payload === 'boolean') {
    return payload;
  }

  if (!payload || typeof payload !== 'object') {
    return undefined;
  }

  const obj = payload as Record<string, unknown>;
  const enabled = obj.enabled ?? obj.safeMode ?? obj.active;
  if (typeof enabled === 'boolean') {
    return enabled;
  }

  const status = obj.status;
  if (status && typeof status === 'object') {
    const statusObj = status as Record<string, unknown>;
    const nested = statusObj.enabled ?? statusObj.safeMode ?? statusObj.active;
    if (typeof nested === 'boolean') {
      return nested;
    }
  }

  return undefined;
}

export function parseSafeModeReason(payload: unknown): string | undefined {
  if (!payload || typeof payload !== 'object') {
    return undefined;
  }

  const obj = payload as Record<string, unknown>;
  const reason = obj.reason ?? obj.kind ?? obj.cause;
  if (typeof reason === 'string') {
    return reason;
  }

  const status = obj.status;
  if (status && typeof status === 'object') {
    const statusObj = status as Record<string, unknown>;
    const nested = statusObj.reason ?? statusObj.kind;
    if (typeof nested === 'string') {
      return nested;
    }
  }

  return undefined;
}

export function formatSafeModeReason(reason: string): string {
  const trimmed = reason.trim();
  if (!trimmed) {
    return trimmed;
  }

  const normalized = trimmed.replace(/[_-]+/g, ' ');
  return normalized.charAt(0).toUpperCase() + normalized.slice(1);
}

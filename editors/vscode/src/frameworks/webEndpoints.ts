export interface WebEndpoint {
  path: string;
  methods: string[];
  file?: string | null;
  /**
   * 1-based line number (matches nova-lsp schema).
   *
   * Note: some providers may send `0` when location is unknown.
   */
  line: number;
}

export type WebEndpointNavigationTarget = {
  file: string;
  /**
   * 1-based line number, clamped to >= 1.
   */
  line: number;
};

export function formatWebEndpointLabel(endpoint: Pick<WebEndpoint, 'path' | 'methods'>): string {
  const path = (endpoint.path ?? '').trim() || '<unknown path>';
  const methodLabel = formatWebEndpointMethodLabel(endpoint.methods);
  return `${methodLabel} ${path}`;
}

export function formatWebEndpointDescription(endpoint: Pick<WebEndpoint, 'file' | 'line'>): string {
  const file = normalizeWebEndpointFile(endpoint.file);
  if (!file) {
    return 'location unavailable';
  }

  return `${file}:${clampWebEndpointLine(endpoint.line)}`;
}

export function webEndpointNavigationTarget(
  endpoint: Pick<WebEndpoint, 'file' | 'line'>,
): WebEndpointNavigationTarget | undefined {
  const file = normalizeWebEndpointFile(endpoint.file);
  if (!file) {
    return undefined;
  }

  return { file, line: clampWebEndpointLine(endpoint.line) };
}

function formatWebEndpointMethodLabel(methods: readonly string[] | null | undefined): string {
  const normalized = Array.isArray(methods)
    ? methods
        .map((method) => (typeof method === 'string' ? method.trim() : ''))
        .filter(Boolean)
        .map((method) => method.toUpperCase())
    : [];

  if (normalized.length === 0) {
    return 'ANY';
  }

  const unique = Array.from(new Set(normalized));
  return unique.join(', ');
}

function normalizeWebEndpointFile(file: unknown): string | undefined {
  return typeof file === 'string' && file.trim().length > 0 ? file.trim() : undefined;
}

function clampWebEndpointLine(line: unknown): number {
  // Some endpoint sources report `line = 0` when location is unknown. Clamp to `1` so clients that
  // choose to navigate can reliably open the file.
  if (typeof line !== 'number' || !Number.isFinite(line)) {
    return 1;
  }

  const integer = Math.floor(line);
  return integer >= 1 ? integer : 1;
}

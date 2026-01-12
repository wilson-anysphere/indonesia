export type NovaRunTestArgs = { testId: string } | { test_id: string };
export type NovaRunMainArgs = { mainClass: string } | { main_class: string };

function isRecord(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === 'object';
}

function getNonEmptyString(value: unknown): string | undefined {
  if (typeof value !== 'string') {
    return undefined;
  }
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : undefined;
}

/**
 * Extracts a `testId` from the first argument passed to a VS Code command handler.
 *
 * Nova's LSP server can send either `{ testId: string }` (camelCase) or `{ test_id: string }`
 * (snake_case) depending on the originating feature/version.
 */
export function extractTestIdFromCommandArgs(args: readonly unknown[]): string | undefined {
  const first = args[0];
  if (!isRecord(first)) {
    return undefined;
  }
  return getNonEmptyString(first.testId) ?? getNonEmptyString(first.test_id);
}

/**
 * Extracts a `mainClass` from the first argument passed to a VS Code command handler.
 *
 * Nova's LSP server can send either `{ mainClass: string }` (camelCase) or `{ main_class: string }`
 * (snake_case) depending on the originating feature/version.
 */
export function extractMainClassFromCommandArgs(args: readonly unknown[]): string | undefined {
  const first = args[0];
  if (!isRecord(first)) {
    return undefined;
  }
  return getNonEmptyString(first.mainClass) ?? getNonEmptyString(first.main_class);
}


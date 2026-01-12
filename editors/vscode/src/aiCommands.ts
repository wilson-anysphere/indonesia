export const NOVA_AI_LSP_COMMAND_EXPLAIN_ERROR = 'nova.ai.explainError' as const;
export const NOVA_AI_LSP_COMMAND_GENERATE_METHOD_BODY = 'nova.ai.generateMethodBody' as const;
export const NOVA_AI_LSP_COMMAND_GENERATE_TESTS = 'nova.ai.generateTests' as const;

export const NOVA_AI_SHOW_EXPLAIN_ERROR_COMMAND = 'nova.ai.showExplainError' as const;
export const NOVA_AI_SHOW_GENERATE_METHOD_BODY_COMMAND = 'nova.ai.showGenerateMethodBody' as const;
export const NOVA_AI_SHOW_GENERATE_TESTS_COMMAND = 'nova.ai.showGenerateTests' as const;

export type NovaAiLspCommandId =
  | typeof NOVA_AI_LSP_COMMAND_EXPLAIN_ERROR
  | typeof NOVA_AI_LSP_COMMAND_GENERATE_METHOD_BODY
  | typeof NOVA_AI_LSP_COMMAND_GENERATE_TESTS;

export type NovaAiLocalCommandId =
  | typeof NOVA_AI_SHOW_EXPLAIN_ERROR_COMMAND
  | typeof NOVA_AI_SHOW_GENERATE_METHOD_BODY_COMMAND
  | typeof NOVA_AI_SHOW_GENERATE_TESTS_COMMAND;

export type NovaAiCodeActionKindValue = 'nova.explain' | 'nova.ai.generate' | 'nova.ai.tests';

export type NovaAiShowCommandArgs = {
  /**
   * The original LSP command name (e.g. `nova.ai.explainError`) that should be
   * invoked via `workspace/executeCommand`.
   */
  lspCommand: string;
  /**
   * Arguments forwarded to the underlying LSP `workspace/executeCommand` call.
   */
  lspArguments: unknown[];
  /**
   * Best-effort: used for debugging and telemetry.
   */
  kind?: string;
  /**
   * Best-effort: code action / command title provided by the server.
   */
  title?: string;
};

type CommandLike = {
  command?: unknown;
  title?: unknown;
  arguments?: unknown;
};

type CodeActionOrCommandLike = {
  kind?: unknown;
  title?: unknown;
  command?: unknown;
  arguments?: unknown;
};

function kindValue(kind: unknown): string | undefined {
  if (typeof kind === 'string') {
    return kind;
  }
  if (!kind || typeof kind !== 'object') {
    return undefined;
  }
  const value = (kind as { value?: unknown }).value;
  return typeof value === 'string' ? value : undefined;
}

function commandFromItem(item: unknown): { id: string; title?: string; args: unknown[] } | undefined {
  if (!item || typeof item !== 'object') {
    return undefined;
  }

  const rawCommand = (item as CodeActionOrCommandLike).command;

  // vscode.Command shape: { command: string, title: string, arguments?: unknown[] }
  if (typeof rawCommand === 'string') {
    const argsRaw = (item as CodeActionOrCommandLike).arguments;
    const args = Array.isArray(argsRaw) ? argsRaw : [];
    const rawTitle = (item as CodeActionOrCommandLike).title;
    const title = typeof rawTitle === 'string' ? rawTitle : undefined;
    return { id: rawCommand, title, args };
  }

  // vscode.CodeAction shape: { command?: vscode.Command }
  if (rawCommand && typeof rawCommand === 'object') {
    const id = (rawCommand as CommandLike).command;
    if (typeof id !== 'string') {
      return undefined;
    }
    const argsRaw = (rawCommand as CommandLike).arguments;
    const args = Array.isArray(argsRaw) ? argsRaw : [];
    const rawTitle = (rawCommand as CommandLike).title;
    const title = typeof rawTitle === 'string' ? rawTitle : undefined;
    return { id, title, args };
  }

  return undefined;
}

export function isNovaAiCodeActionKind(kind: unknown): boolean {
  const value = kindValue(kind);
  return typeof value === 'string' && (value === 'nova.explain' || value.startsWith('nova.ai'));
}

export function isNovaAiCommandId(commandId: unknown): boolean {
  return typeof commandId === 'string' && commandId.startsWith('nova.ai.');
}

export function isNovaAiCodeActionOrCommand(item: unknown): boolean {
  if (!item || typeof item !== 'object') {
    return false;
  }

  if (isNovaAiCodeActionKind((item as CodeActionOrCommandLike).kind)) {
    return true;
  }

  const command = commandFromItem(item);
  return typeof command?.id === 'string' && command.id.startsWith('nova.ai.');
}

/**
 * Detects AI code actions/commands that require a file-backed URI to execute.
 *
 * Today this primarily covers patch-based AI code edits (generate method body / tests).
 */
export function isNovaAiFileBackedCodeActionOrCommand(item: unknown): boolean {
  if (!item || typeof item !== 'object') {
    return false;
  }

  const kind = kindValue((item as CodeActionOrCommandLike).kind);
  if (kind === 'nova.ai.generate' || kind === 'nova.ai.tests') {
    return true;
  }

  const cmd = commandFromItem(item);
  switch (cmd?.id) {
    case NOVA_AI_LSP_COMMAND_GENERATE_METHOD_BODY:
    case NOVA_AI_LSP_COMMAND_GENERATE_TESTS:
      return true;
  }

  return false;
}

export function localCommandForNovaAiAction(action: {
  lspCommandId?: string | undefined;
  kind?: string | undefined;
}): NovaAiLocalCommandId | undefined {
  switch (action.lspCommandId) {
    case NOVA_AI_LSP_COMMAND_EXPLAIN_ERROR:
      return NOVA_AI_SHOW_EXPLAIN_ERROR_COMMAND;
    case NOVA_AI_LSP_COMMAND_GENERATE_METHOD_BODY:
      return NOVA_AI_SHOW_GENERATE_METHOD_BODY_COMMAND;
    case NOVA_AI_LSP_COMMAND_GENERATE_TESTS:
      return NOVA_AI_SHOW_GENERATE_TESTS_COMMAND;
  }

  // If we have an unknown `nova.ai.*` command, don't guess which UI to use based on
  // code action kind (future commands might share kinds). Only fall back to kind
  // mapping when no command id is available.
  if (typeof action.lspCommandId === 'string') {
    return undefined;
  }

  switch (action.kind) {
    case 'nova.explain':
      return NOVA_AI_SHOW_EXPLAIN_ERROR_COMMAND;
    case 'nova.ai.generate':
      return NOVA_AI_SHOW_GENERATE_METHOD_BODY_COMMAND;
    case 'nova.ai.tests':
      return NOVA_AI_SHOW_GENERATE_TESTS_COMMAND;
  }

  return undefined;
}

/**
 * Pure helper used by the VS Code extension middleware to replace nova-lsp AI
 * code actions/commands with VS Code-side command IDs.
 *
 * This is defined in a vscode-free module so it can be unit tested without a
 * VS Code runtime.
 */
export function rewriteNovaAiCodeActionOrCommand(
  item: unknown,
): { command: NovaAiLocalCommandId; args: [NovaAiShowCommandArgs] } | undefined {
  if (!item || typeof item !== 'object') {
    return undefined;
  }

  const kind = kindValue((item as CodeActionOrCommandLike).kind);
  const cmd = commandFromItem(item);
  const lspCommandId = cmd?.id;

  const isAi = isNovaAiCodeActionKind(kind) || isNovaAiCommandId(lspCommandId);
  if (!isAi) {
    return undefined;
  }

  const localCommand = localCommandForNovaAiAction({ lspCommandId, kind });
  const resolvedLspCommand = typeof lspCommandId === 'string' ? lspCommandId : undefined;

  // We can only execute server-side AI operations if the underlying `nova.ai.*`
  // command name is present. When it is missing, surface nothing (VS Code will
  // still render the code action, but invoking it won't do anything useful).
  if (!localCommand || !resolvedLspCommand || !isNovaAiCommandId(resolvedLspCommand)) {
    return undefined;
  }

  const cmdTitle = cmd?.title;
  const itemTitle = (item as CodeActionOrCommandLike).title;
  const title = typeof cmdTitle === 'string' ? cmdTitle : typeof itemTitle === 'string' ? itemTitle : undefined;

  return {
    command: localCommand,
    args: [
      {
        lspCommand: resolvedLspCommand,
        lspArguments: cmd?.args ?? [],
        kind,
        title,
      },
    ],
  };
}

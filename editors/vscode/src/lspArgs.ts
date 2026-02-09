import * as os from 'node:os';
import * as path from 'node:path';

export interface NovaLspArgsOptions {
  /**
   * Path to the Nova TOML config file passed via `nova-lsp --config <path>`.
   *
   * If the path is relative and `workspaceRoot` is provided, it will be resolved
   * relative to `workspaceRoot` so VS Code users can use workspace-local config
   * files (e.g. `nova.toml`).
   */
  configPath?: string | null;
  /**
   * Extra CLI args appended after `--stdio` (and optional `--config`).
   */
  extraArgs?: readonly string[] | null;
  workspaceRoot?: string | null;
}

export interface NovaLspLaunchConfigOptions {
  configPath?: string | null;
  extraArgs?: readonly string[] | null;
  workspaceRoot?: string | null;
  /**
   * Master toggle for AI features. When false, `NOVA_AI_*` environment variables
   * are stripped before launching `nova-lsp` so server-side AI stays disabled.
   */
  aiEnabled?: boolean;
  /**
   * Toggle for AI completion features.
   *
   * When false, the server is started with `NOVA_DISABLE_AI_COMPLETIONS=1` so
   * `nova.toml` cannot re-enable AI completion behavior (including async
   * multi-token completions via `nova/completion/more` and completion ranking
   * that re-orders standard `textDocument/completion` results).
   */
  aiCompletionsEnabled?: boolean;
  /**
   * Maximum number of AI completion items to request from the provider.
   *
   * When set, the server is started with `NOVA_AI_COMPLETIONS_MAX_ITEMS=<n>`.
   */
  aiCompletionsMaxItems?: number;
  /**
   * Base environment for the child process. Defaults to `process.env`.
   */
  baseEnv?: NodeJS.ProcessEnv;
}

export function resolveNovaConfigPath(
  options: Pick<NovaLspArgsOptions, 'configPath' | 'workspaceRoot'> = {},
): string | undefined {
  const trimmedConfigPath = options.configPath?.trim();
  if (!trimmedConfigPath) {
    return undefined;
  }

  const workspaceRoot = options.workspaceRoot?.trim();
  let candidate = trimmedConfigPath;
  if (workspaceRoot) {
    candidate = candidate.replace(/\$\{workspaceFolder\}/g, workspaceRoot);
  }

  if (candidate === '~') {
    candidate = os.homedir();
  } else if (candidate.startsWith('~/') || candidate.startsWith('~\\')) {
    candidate = path.join(os.homedir(), candidate.slice(2));
  }

  return workspaceRoot && !path.isAbsolute(candidate) ? path.join(workspaceRoot, candidate) : candidate;
}

export function buildNovaLspLaunchConfig(options: NovaLspLaunchConfigOptions = {}): { args: string[]; env: NodeJS.ProcessEnv } {
  const resolvedConfigPath = resolveNovaConfigPath(options);
  const args = buildNovaLspArgs({
    configPath: resolvedConfigPath ?? null,
    extraArgs: options.extraArgs,
    // `configPath` is already resolved, so we no longer need `workspaceRoot` for argument construction.
    workspaceRoot: null,
  });

  const aiEnabled = options.aiEnabled ?? true;
  const aiCompletionsEnabled = options.aiCompletionsEnabled ?? true;
  const aiCompletionsMaxItems = options.aiCompletionsMaxItems;
  const baseEnv = options.baseEnv ?? process.env;

  const disableAi = !aiEnabled;
  // If AI is disabled, AI completion features are also disabled.
  const disableAiCompletions = disableAi || !aiCompletionsEnabled;
  const aiCompletionsMaxItemsEnv =
    !disableAi && typeof aiCompletionsMaxItems === 'number' && Number.isFinite(aiCompletionsMaxItems)
      ? String(Math.max(0, Math.floor(aiCompletionsMaxItems)))
      : undefined;

  const baseHasNovaAiVars = disableAi && Object.keys(baseEnv).some((key) => key.startsWith('NOVA_AI_'));

  const needsConfigPathMutation = !!resolvedConfigPath && baseEnv.NOVA_CONFIG_PATH !== resolvedConfigPath;
  const needsDisableAiMutation = disableAi ? baseEnv.NOVA_DISABLE_AI !== '1' : typeof baseEnv.NOVA_DISABLE_AI !== 'undefined';
  const needsDisableAiCompletionsMutation = disableAiCompletions
    ? baseEnv.NOVA_DISABLE_AI_COMPLETIONS !== '1'
    : typeof baseEnv.NOVA_DISABLE_AI_COMPLETIONS !== 'undefined';
  const needsAiCompletionsMaxItemsMutation =
    typeof aiCompletionsMaxItemsEnv === 'string'
      ? baseEnv.NOVA_AI_COMPLETIONS_MAX_ITEMS !== aiCompletionsMaxItemsEnv
      : false;

  const needsEnvMutation =
    needsConfigPathMutation ||
    needsDisableAiMutation ||
    needsDisableAiCompletionsMutation ||
    needsAiCompletionsMaxItemsMutation ||
    baseHasNovaAiVars;

  let env: NodeJS.ProcessEnv = needsEnvMutation ? { ...baseEnv } : baseEnv;

  if (needsEnvMutation) {
    // `nova-config` supports `NOVA_CONFIG_PATH`; set it when a config path is
    // configured so users don't have to manually export env vars.
    if (resolvedConfigPath) {
      env.NOVA_CONFIG_PATH = resolvedConfigPath;
    }

    // These env vars are treated as deterministic overrides so VS Code settings
    // can always force-disable server-side AI features even if `nova.toml`
    // enables them (and can re-enable AI even if the parent process has global
    // disable flags set).
    if (disableAi) {
      env.NOVA_DISABLE_AI = '1';
    } else {
      delete env.NOVA_DISABLE_AI;
    }

    if (disableAiCompletions) {
      env.NOVA_DISABLE_AI_COMPLETIONS = '1';
    } else {
      delete env.NOVA_DISABLE_AI_COMPLETIONS;
    }

    if (typeof aiCompletionsMaxItemsEnv === 'string') {
      env.NOVA_AI_COMPLETIONS_MAX_ITEMS = aiCompletionsMaxItemsEnv;
    }

    // If AI is disabled in VS Code settings, ensure we don't leak any NOVA_AI_*
    // environment variables to the server process. This guarantees AI stays off
    // even if the user has set global env vars in their shell.
    if (disableAi) {
      for (const key of Object.keys(env)) {
        if (key.startsWith('NOVA_AI_')) {
          delete env[key];
        }
      }
    }
  }

  return { args, env };
}

export function buildNovaLspArgs(options: NovaLspArgsOptions = {}): string[] {
  const args: string[] = ['--stdio'];

  const resolvedConfigPath = resolveNovaConfigPath(options);
  if (resolvedConfigPath) {
    args.push('--config', resolvedConfigPath);
  }

  for (const arg of options.extraArgs ?? []) {
    const trimmed = arg.trim();
    if (trimmed) {
      args.push(trimmed);
    }
  }

  return args;
}

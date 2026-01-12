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
   * Toggle for multi-token completions only.
   *
   * When false, the server is started with `NOVA_DISABLE_AI_COMPLETIONS=1` so
   * `nova.toml` cannot re-enable completion provider traffic.
   */
  aiCompletionsEnabled?: boolean;
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
  const baseEnv = options.baseEnv ?? process.env;

  let env: NodeJS.ProcessEnv = baseEnv;
  // Only allocate a copy of the environment when we need to mutate it.
  if (resolvedConfigPath || !aiEnabled || !aiCompletionsEnabled) {
    env = { ...baseEnv };

    // `nova-config` supports `NOVA_CONFIG_PATH`; set it when a config path is
    // configured so users don't have to manually export env vars.
    if (resolvedConfigPath) {
      env.NOVA_CONFIG_PATH = resolvedConfigPath;
    }

    // If AI is disabled in VS Code settings, ensure we don't leak any NOVA_AI_*
    // environment variables to the server process. This guarantees AI stays off
    // even if the user has set global env vars in their shell.
    if (!aiEnabled) {
      env.NOVA_DISABLE_AI = '1';
      for (const key of Object.keys(env)) {
        if (key.startsWith('NOVA_AI_')) {
          delete env[key];
        }
      }
    }

    if (!aiCompletionsEnabled) {
      env.NOVA_DISABLE_AI_COMPLETIONS = '1';
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

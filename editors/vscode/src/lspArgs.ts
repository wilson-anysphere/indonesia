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

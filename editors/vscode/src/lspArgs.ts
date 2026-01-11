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

export function buildNovaLspArgs(options: NovaLspArgsOptions = {}): string[] {
  const args: string[] = ['--stdio'];

  const trimmedConfigPath = options.configPath?.trim();
  if (trimmedConfigPath) {
    const workspaceRoot = options.workspaceRoot?.trim();
    const resolvedConfigPath =
      workspaceRoot && !path.isAbsolute(trimmedConfigPath)
        ? path.join(workspaceRoot, trimmedConfigPath)
        : trimmedConfigPath;
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

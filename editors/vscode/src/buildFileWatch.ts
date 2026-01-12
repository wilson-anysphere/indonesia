import * as vscode from 'vscode';

export type NovaRequestOptions = {
  allowMethodFallback?: boolean;
};

export type NovaRequest = <R>(method: string, params?: unknown, opts?: NovaRequestOptions) => Promise<R | undefined>;

type FormatError = (err: unknown) => string;
type IsMethodNotFoundError = (err: unknown) => boolean;

const BUILD_FILE_GLOBS: readonly string[] = [
  // Maven
  '**/pom.xml',
  '**/mvnw',
  '**/mvnw.cmd',
  // Gradle
  '**/build.gradle',
  '**/build.gradle.kts',
  '**/settings.gradle',
  '**/settings.gradle.kts',
  '**/gradle.properties',
  '**/gradlew',
  '**/gradlew.bat',
  // Bazel
  '**/WORKSPACE',
  '**/WORKSPACE.bazel',
  '**/MODULE.bazel',
  '**/BUILD',
  '**/BUILD.bazel',
  '**/.bazelrc',
  // Nova workspace config (optional)
  '**/.nova/**/*.toml',
];

export function registerNovaBuildFileWatchers(
  context: vscode.ExtensionContext,
  request: NovaRequest,
  opts: { output: vscode.OutputChannel; formatError: FormatError; isMethodNotFoundError: IsMethodNotFoundError },
): void {
  const debounceMs = 1000;
  const reloadTimerByWorkspace = new Map<string, ReturnType<typeof setTimeout>>();
  const reloadInFlightByWorkspace = new Map<string, Promise<void>>();

  let reloadProjectSupported = true;
  let reloadProjectUnsupportedWarningLogged = false;

  const scheduleReload = (workspaceFolder: vscode.WorkspaceFolder) => {
    if (!reloadProjectSupported) {
      return;
    }

    const config = vscode.workspace.getConfiguration('nova', workspaceFolder.uri);
    const enabled = config.get<boolean>('build.autoReloadOnBuildFileChange', true);
    if (!enabled) {
      return;
    }

    const key = workspaceFolder.uri.toString();
    const existing = reloadTimerByWorkspace.get(key);
    if (existing) {
      clearTimeout(existing);
    }

    reloadTimerByWorkspace.set(
      key,
      setTimeout(() => {
        reloadTimerByWorkspace.delete(key);
        void queueReload(workspaceFolder);
      }, debounceMs),
    );
  };

  const queueReload = async (workspaceFolder: vscode.WorkspaceFolder): Promise<void> => {
    if (!reloadProjectSupported) {
      return;
    }

    const key = workspaceFolder.uri.toString();
    const prior = reloadInFlightByWorkspace.get(key);

    const task = (prior ?? Promise.resolve())
      .catch(() => undefined)
      .then(() => doReload(workspaceFolder));

    reloadInFlightByWorkspace.set(key, task);
    try {
      await task;
    } finally {
      if (reloadInFlightByWorkspace.get(key) === task) {
        reloadInFlightByWorkspace.delete(key);
      }
    }
  };

  const doReload = async (workspaceFolder: vscode.WorkspaceFolder): Promise<void> => {
    if (!reloadProjectSupported) {
      return;
    }

    const config = vscode.workspace.getConfiguration('nova', workspaceFolder.uri);
    const enabled = config.get<boolean>('build.autoReloadOnBuildFileChange', true);
    if (!enabled) {
      return;
    }

    const projectRoot = workspaceFolder.uri.fsPath;

    try {
      await request('nova/reloadProject', { projectRoot, buildTool: 'auto' }, { allowMethodFallback: true });
    } catch (err) {
      if (opts.isMethodNotFoundError(err)) {
        reloadProjectSupported = false;
        if (!reloadProjectUnsupportedWarningLogged) {
          reloadProjectUnsupportedWarningLogged = true;
          opts.output.appendLine(
            'Nova: nova/reloadProject is not supported by the connected server; auto-reload on build file changes is disabled for this session.',
          );
        }
      } else {
        opts.output.appendLine(`Nova: failed to auto-reload project for ${projectRoot}: ${opts.formatError(err)}`);
      }
      return;
    }

    // Best-effort refresh hooks. These may not exist in older servers / builds.
    try {
      await request('nova/build/diagnostics', { projectRoot, target: null }, { allowMethodFallback: true });
    } catch (err) {
      if (!opts.isMethodNotFoundError(err)) {
        opts.output.appendLine(`Nova: failed to refresh build diagnostics for ${projectRoot}: ${opts.formatError(err)}`);
      }
    }

    try {
      await vscode.commands.executeCommand('nova.refreshProjectExplorer');
    } catch {
      // Command is optional; ignore if not contributed.
    }
  };

  const handleUri = (uri: vscode.Uri) => {
    if (!reloadProjectSupported) {
      return;
    }

    const folder = vscode.workspace.getWorkspaceFolder(uri);
    if (!folder) {
      return;
    }
    scheduleReload(folder);
  };

  for (const glob of BUILD_FILE_GLOBS) {
    const watcher = vscode.workspace.createFileSystemWatcher(glob);
    context.subscriptions.push(watcher);
    watcher.onDidCreate(handleUri, undefined, context.subscriptions);
    watcher.onDidChange(handleUri, undefined, context.subscriptions);
    watcher.onDidDelete(handleUri, undefined, context.subscriptions);
  }

  context.subscriptions.push(
    new vscode.Disposable(() => {
      for (const timer of reloadTimerByWorkspace.values()) {
        clearTimeout(timer);
      }
      reloadTimerByWorkspace.clear();
    }),
  );
}

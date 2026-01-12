import * as vscode from 'vscode';

export type NovaRequestOptions = {
  allowMethodFallback?: boolean;
};

export type NovaRequest = <R>(method: string, params?: unknown, opts?: NovaRequestOptions) => Promise<R | undefined>;

type FormatError = (err: unknown) => string;
type IsMethodNotFoundError = (err: unknown) => boolean;

type BuildTool = 'auto' | 'maven' | 'gradle';

const BUILD_FILE_GLOBS: readonly string[] = [
  // Maven
  '**/pom.xml',
  '**/mvnw',
  '**/mvnw.cmd',
  '**/.mvn/wrapper/maven-wrapper.properties',
  '**/.mvn/extensions.xml',
  '**/.mvn/maven.config',
  '**/.mvn/jvm.config',
  // Gradle
  '**/*.gradle',
  '**/*.gradle.kts',
  '**/gradle.properties',
  '**/gradlew',
  '**/gradlew.bat',
  '**/gradle/wrapper/gradle-wrapper.properties',
  '**/libs.versions.toml',
  // Bazel
  '**/WORKSPACE',
  '**/WORKSPACE.bazel',
  '**/MODULE.bazel',
  '**/MODULE.bazel.lock',
  '**/BUILD',
  '**/BUILD.bazel',
  '**/*.bzl',
  '**/.bazelrc',
  '**/.bazelrc.*',
  '**/.bazelversion',
  '**/bazelisk.rc',
  '**/.bazelignore',
  // JPMS
  '**/module-info.java',
  // Nova workspace config (optional)
  '**/nova.toml',
  '**/.nova.toml',
  '**/nova.config.toml',
  '**/.nova/apt-cache/generated-roots.json',
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
    const rawBuildTool = config.get<string>('build.buildTool', 'auto');
    const buildTool: BuildTool =
      rawBuildTool === 'maven' || rawBuildTool === 'gradle' || rawBuildTool === 'auto' ? rawBuildTool : 'auto';

    try {
      // Auto-reload should never prompt for input. Treat the "prompt" setting as "auto".
      await request('nova/reloadProject', { projectRoot, buildTool }, { allowMethodFallback: true });
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
      await vscode.commands.executeCommand('nova.build.refreshDiagnostics', { projectRoot, silent: true });
    } catch (err) {
      opts.output.appendLine(`Nova: failed to refresh build diagnostics for ${projectRoot}: ${opts.formatError(err)}`);
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

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { NOVA_GRADLE_SNAPSHOT_REL_PATH } from '../fileWatchers';

beforeEach(() => {
  vi.resetModules();
  vi.restoreAllMocks();
});

class MockDisposable {
  private readonly fn: (() => void) | undefined;
  constructor(fn?: () => void) {
    this.fn = fn;
  }
  dispose(): void {
    this.fn?.();
  }
}

class MockFileSystemWatcher {
  private readonly createListeners: Array<(uri: unknown) => void> = [];
  private readonly changeListeners: Array<(uri: unknown) => void> = [];
  private readonly deleteListeners: Array<(uri: unknown) => void> = [];

  onDidCreate(listener: (uri: unknown) => void, _thisArgs?: unknown, disposables?: MockDisposable[]): MockDisposable {
    this.createListeners.push(listener);
    const d = new MockDisposable();
    disposables?.push(d);
    return d;
  }

  onDidChange(listener: (uri: unknown) => void, _thisArgs?: unknown, disposables?: MockDisposable[]): MockDisposable {
    this.changeListeners.push(listener);
    const d = new MockDisposable();
    disposables?.push(d);
    return d;
  }

  onDidDelete(listener: (uri: unknown) => void, _thisArgs?: unknown, disposables?: MockDisposable[]): MockDisposable {
    this.deleteListeners.push(listener);
    const d = new MockDisposable();
    disposables?.push(d);
    return d;
  }

  fireDidChange(uri: unknown): void {
    for (const listener of this.changeListeners) {
      listener(uri);
    }
  }

  dispose(): void {
    // noop
  }
}

describe('buildFileWatch', () => {
  it('auto-reload triggers a build diagnostics refresh (debounced) and forwards build.buildTool', async () => {
    vi.useFakeTimers();

    const watchers: MockFileSystemWatcher[] = [];

    const workspaceFolder = {
      uri: { fsPath: '/workspace', toString: () => 'file:///workspace' },
      name: 'workspace',
      index: 0,
    };

    const fileUri = { fsPath: '/workspace/pom.xml', toString: () => 'file:///workspace/pom.xml' };

    const output = { appendLine: vi.fn() };

    const request = vi.fn(async () => undefined);

    let resolveDiagnosticsRefresh: (() => void) | undefined;
    const diagnosticsRefresh = new Promise<void>((resolve) => {
      resolveDiagnosticsRefresh = resolve;
    });

    const executeCommand = vi.fn(async (command: string) => {
      if (command === 'nova.build.refreshDiagnostics') {
        resolveDiagnosticsRefresh?.();
      }
      return undefined;
    });

    vi.doMock(
      'vscode',
      () => ({
        workspace: {
          getConfiguration: () => ({
            get: (key: string, defaultValue: unknown) => {
              if (key === 'build.buildTool') {
                return 'maven';
              }
              return defaultValue;
            },
          }),
          getWorkspaceFolder: () => workspaceFolder,
          createFileSystemWatcher: () => {
            const watcher = new MockFileSystemWatcher();
            watchers.push(watcher);
            return watcher;
          },
        },
        commands: { executeCommand },
        Disposable: MockDisposable,
      }),
      { virtual: true },
    );

    const { registerNovaBuildFileWatchers } = await import('../buildFileWatch');

    const context = { subscriptions: [] as unknown[] };

    registerNovaBuildFileWatchers(context as never, request as never, {
      output: output as never,
      formatError: (err: unknown) => String(err),
      isMethodNotFoundError: () => false,
    });

    // Trigger multiple edits inside the debounce window; we should only reload once.
    watchers[0].fireDidChange(fileUri);
    watchers[0].fireDidChange(fileUri);

    await vi.advanceTimersByTimeAsync(1000);
    await diagnosticsRefresh;

    expect(request).toHaveBeenCalledTimes(1);
    expect(request).toHaveBeenCalledWith(
      'nova/reloadProject',
      { projectRoot: '/workspace', buildTool: 'maven' },
      { allowMethodFallback: true },
    );

    expect(executeCommand).toHaveBeenCalledWith('nova.build.refreshDiagnostics', { projectRoot: '/workspace', silent: true });

    vi.useRealTimers();
  });

  it('treats build.buildTool=prompt as auto (no prompting during file-watch reloads)', async () => {
    vi.useFakeTimers();

    const watchers: MockFileSystemWatcher[] = [];

    const workspaceFolder = {
      uri: { fsPath: '/workspace', toString: () => 'file:///workspace' },
      name: 'workspace',
      index: 0,
    };

    const fileUri = { fsPath: '/workspace/pom.xml', toString: () => 'file:///workspace/pom.xml' };

    const output = { appendLine: vi.fn() };

    const request = vi.fn(async () => undefined);

    let resolveDiagnosticsRefresh: (() => void) | undefined;
    const diagnosticsRefresh = new Promise<void>((resolve) => {
      resolveDiagnosticsRefresh = resolve;
    });

    const executeCommand = vi.fn(async (command: string) => {
      if (command === 'nova.build.refreshDiagnostics') {
        resolveDiagnosticsRefresh?.();
      }
      return undefined;
    });

    vi.doMock(
      'vscode',
      () => ({
        workspace: {
          getConfiguration: () => ({
            get: (key: string, defaultValue: unknown) => {
              if (key === 'build.buildTool') {
                return 'prompt';
              }
              return defaultValue;
            },
          }),
          getWorkspaceFolder: () => workspaceFolder,
          createFileSystemWatcher: () => {
            const watcher = new MockFileSystemWatcher();
            watchers.push(watcher);
            return watcher;
          },
        },
        commands: { executeCommand },
        Disposable: MockDisposable,
      }),
      { virtual: true },
    );

    const { registerNovaBuildFileWatchers } = await import('../buildFileWatch');

    const context = { subscriptions: [] as unknown[] };

    registerNovaBuildFileWatchers(context as never, request as never, {
      output: output as never,
      formatError: (err: unknown) => String(err),
      isMethodNotFoundError: () => false,
    });

    watchers[0].fireDidChange(fileUri);

    await vi.advanceTimersByTimeAsync(1000);
    await diagnosticsRefresh;

    expect(request).toHaveBeenCalledTimes(1);
    expect(request).toHaveBeenCalledWith(
      'nova/reloadProject',
      { projectRoot: '/workspace', buildTool: 'auto' },
      { allowMethodFallback: true },
    );

    vi.useRealTimers();
  });

  it('auto-reload triggers for .nova/queries/gradle.json changes (Gradle snapshot handoff)', async () => {
    vi.useFakeTimers();

    const watchers: MockFileSystemWatcher[] = [];

    const workspaceFolder = {
      uri: { fsPath: '/workspace', toString: () => 'file:///workspace' },
      name: 'workspace',
      index: 0,
    };

    const fileUri = {
      fsPath: `/workspace/${NOVA_GRADLE_SNAPSHOT_REL_PATH}`,
      toString: () => `file:///workspace/${NOVA_GRADLE_SNAPSHOT_REL_PATH}`,
    };

    const output = { appendLine: vi.fn() };

    const request = vi.fn(async () => undefined);

    let resolveDiagnosticsRefresh: (() => void) | undefined;
    const diagnosticsRefresh = new Promise<void>((resolve) => {
      resolveDiagnosticsRefresh = resolve;
    });

    const executeCommand = vi.fn(async (command: string) => {
      if (command === 'nova.build.refreshDiagnostics') {
        resolveDiagnosticsRefresh?.();
      }
      return undefined;
    });

    vi.doMock(
      'vscode',
      () => ({
        workspace: {
          getConfiguration: () => ({
            get: (key: string, defaultValue: unknown) => {
              if (key === 'build.buildTool') {
                return 'gradle';
              }
              return defaultValue;
            },
          }),
          getWorkspaceFolder: () => workspaceFolder,
          createFileSystemWatcher: () => {
            const watcher = new MockFileSystemWatcher();
            watchers.push(watcher);
            return watcher;
          },
        },
        commands: { executeCommand },
        Disposable: MockDisposable,
      }),
      { virtual: true },
    );

    const { registerNovaBuildFileWatchers } = await import('../buildFileWatch');

    const context = { subscriptions: [] as unknown[] };

    registerNovaBuildFileWatchers(context as never, request as never, {
      output: output as never,
      formatError: (err: unknown) => String(err),
      isMethodNotFoundError: () => false,
    });

    watchers[0].fireDidChange(fileUri);

    await vi.advanceTimersByTimeAsync(1000);
    await diagnosticsRefresh;

    expect(request).toHaveBeenCalledTimes(1);
    expect(request).toHaveBeenCalledWith(
      'nova/reloadProject',
      { projectRoot: '/workspace', buildTool: 'gradle' },
      { allowMethodFallback: true },
    );

    vi.useRealTimers();
  });

  it('does not auto-reload for changes under node_modules', async () => {
    vi.useFakeTimers();

    const watchers: MockFileSystemWatcher[] = [];

    const workspaceFolder = {
      uri: { fsPath: '/workspace', toString: () => 'file:///workspace' },
      name: 'workspace',
      index: 0,
    };

    const ignoredUri = {
      fsPath: '/workspace/node_modules/some/pkg/build.gradle',
      toString: () => 'file:///workspace/node_modules/some/pkg/build.gradle',
    };

    const output = { appendLine: vi.fn() };
    const request = vi.fn(async () => undefined);
    const executeCommand = vi.fn(async () => undefined);

    vi.doMock(
      'vscode',
      () => ({
        workspace: {
          getConfiguration: () => ({ get: (_key: string, defaultValue: unknown) => defaultValue }),
          getWorkspaceFolder: () => workspaceFolder,
          createFileSystemWatcher: () => {
            const watcher = new MockFileSystemWatcher();
            watchers.push(watcher);
            return watcher;
          },
        },
        commands: { executeCommand },
        Disposable: MockDisposable,
      }),
      { virtual: true },
    );

    const { registerNovaBuildFileWatchers } = await import('../buildFileWatch');

    const context = { subscriptions: [] as unknown[] };

    registerNovaBuildFileWatchers(context as never, request as never, {
      output: output as never,
      formatError: (err: unknown) => String(err),
      isMethodNotFoundError: () => false,
    });

    watchers[0].fireDidChange(ignoredUri);
    await vi.advanceTimersByTimeAsync(2000);

    expect(request).not.toHaveBeenCalled();
    expect(executeCommand).not.toHaveBeenCalled();

    vi.useRealTimers();
  });
});

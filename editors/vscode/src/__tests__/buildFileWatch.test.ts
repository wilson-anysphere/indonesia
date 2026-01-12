import { beforeEach, describe, expect, it, vi } from 'vitest';

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
});

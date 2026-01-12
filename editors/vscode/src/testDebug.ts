import * as vscode from 'vscode';
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import * as fs from 'node:fs';
import * as net from 'node:net';
import * as path from 'node:path';
import { NOVA_DEBUG_TYPE } from './debugAdapter';

export type NovaRequest = <R>(method: string, params?: unknown) => Promise<R>;

type BuildTool = 'auto' | 'maven' | 'gradle';

interface TestDebugConfiguration {
  schemaVersion: number;
  name: string;
  cwd: string;
  command: string;
  args: string[];
  env: Record<string, string>;
}

interface TestDebugResponse {
  schemaVersion: number;
  tool: BuildTool;
  configuration: TestDebugConfiguration;
}

interface SpawnedProcess {
  child: ChildProcess;
  dispose: (reason: string) => Promise<void>;
}

const NOVA_TEST_DEBUG_RUN_ID_KEY = '__novaTestDebugRunId';

export interface ResolvedTestTarget {
  item: vscode.TestItem | undefined;
  workspaceFolder: vscode.WorkspaceFolder;
  projectRoot: string;
  lspId: string;
}

export function registerNovaTestDebugRunProfile(
  context: vscode.ExtensionContext,
  controller: vscode.TestController,
  novaRequest: NovaRequest,
  ensureTestsDiscovered: () => Promise<void>,
  resolveTestTarget: (id: string) => ResolvedTestTarget | undefined,
): void {
  const output = vscode.window.createOutputChannel('Nova Test Debug');
  context.subscriptions.push(output);

  const processesByRunId = new Map<string, SpawnedProcess>();
  const processesByDebugSessionId = new Map<string, SpawnedProcess>();

  context.subscriptions.push(
    vscode.debug.onDidStartDebugSession((session) => {
      if (session.type !== NOVA_DEBUG_TYPE) {
        return;
      }
      const runId = (session.configuration as Record<string, unknown>)[NOVA_TEST_DEBUG_RUN_ID_KEY];
      if (typeof runId !== 'string') {
        return;
      }
      const proc = processesByRunId.get(runId);
      if (!proc) {
        return;
      }
      processesByRunId.delete(runId);
      processesByDebugSessionId.set(session.id, proc);
    }),
  );

  context.subscriptions.push(
    vscode.debug.onDidTerminateDebugSession((session) => {
      const proc = processesByDebugSessionId.get(session.id);
      if (!proc) {
        return;
      }
      processesByDebugSessionId.delete(session.id);
      void proc.dispose('debug session ended');
    }),
  );

  context.subscriptions.push(
    controller.createRunProfile(
      'Debug',
      vscode.TestRunProfileKind.Debug,
      async (request, token) => {
        await debugTestsFromTestExplorer(
          request,
          token,
          controller,
          output,
          novaRequest,
          ensureTestsDiscovered,
          resolveTestTarget,
          processesByRunId,
        );
      },
      true,
    ),
  );
}

async function debugTestsFromTestExplorer(
  request: vscode.TestRunRequest,
  token: vscode.CancellationToken,
  controller: vscode.TestController,
  output: vscode.OutputChannel,
  novaRequest: NovaRequest,
  ensureTestsDiscovered: () => Promise<void>,
  resolveTestTarget: (id: string) => ResolvedTestTarget | undefined,
  processesByRunId: Map<string, SpawnedProcess>,
): Promise<void> {
  await ensureTestsDiscovered();

  const exclude = request.exclude ?? [];
  const excludeIds = new Set(collectLeafIds(exclude));
  const explicitInclude = request.include;

  let ids: string[];
  if (explicitInclude && explicitInclude.length === 1) {
    const candidate = explicitInclude[0].id;
    const resolved = resolveTestTarget(candidate);
    if (resolved) {
      ids = [candidate];
    } else {
      const fallback = collectLeafIds([explicitInclude[0]]);
      ids = Array.from(new Set(fallback.filter((id) => !excludeIds.has(id))));
    }
  } else {
    const include = explicitInclude ?? getRootTestItems(controller);
    const includeIds = collectLeafIds(include);
    ids = Array.from(new Set(includeIds.filter((id) => !excludeIds.has(id))));
  }

  if (ids.length === 0) {
    return;
  }

  if (ids.length > 1) {
    void vscode.window.showWarningMessage('Nova: Debugging multiple tests at once is not supported yet. Debugging first.');
  }

  const run = controller.createTestRun(request);
  let cancellationSubscription: vscode.Disposable | undefined;
  try {
    const vsTestId = ids[0];
    const target = resolveTestTarget(vsTestId);
    if (!target) {
      const message = 'Nova: Select a specific test or test class to debug.';
      run.appendOutput(`${message}\n`);
      void vscode.window.showErrorMessage(message);
      return;
    }

    const testId = target.lspId;
    const item = target.item;

    if (item) {
      run.enqueued(item);
      run.started(item);
    }

    const workspaceFolder = target.workspaceFolder;
    const buildTool = await getBuildToolFromUser(workspaceFolder);

    const resp = (await novaRequest('nova/test/debugConfiguration', {
      projectRoot: target.projectRoot,
      buildTool,
      test: testId,
    })) as TestDebugResponse | undefined;
    if (!resp) {
      if (item) {
        run.skipped(item);
      }
      return;
    }

    const defaults = getDebugDefaults();
    const desiredHost = defaults.host;
    let desiredPort = defaults.port;

    const portFree = await isLocalPortFree(desiredHost, desiredPort);
    if (!portFree) {
      const choice = await vscode.window.showWarningMessage(
        `Nova: JDWP port ${desiredHost}:${desiredPort} appears to already be in use. ` +
          `The test JVM may fail to start, or the debugger may attach to an existing process.`,
        'Use Different Port',
        'Continue',
        'Cancel',
      );
      if (choice === 'Use Different Port') {
        desiredPort = await findFreeLocalPort(desiredHost);
      } else if (choice !== 'Continue') {
        if (item) {
          run.skipped(item);
        }
        return;
      }
    }

    const commandConfig = normalizeTestDebugCommand(resp, desiredPort);

    output.show(true);
    output.appendLine(`\n=== ${commandConfig.name} (${resp.tool}) ===`);
    output.appendLine(`cwd: ${commandConfig.cwd}`);
    output.appendLine(`command: ${commandConfig.command} ${commandConfig.args.join(' ')}`);

    const { child, ready } = spawnTestDebugProcess(commandConfig, output, run);
    const spawned: SpawnedProcess = {
      child,
      dispose: async (reason) => {
        output.appendLine(`\n[Nova] Stopping test debug process (${reason})...`);
        await terminateProcessTree(child);
      },
    };

    const runId = `${Date.now()}-${Math.random().toString(16).slice(2)}`;
    processesByRunId.set(runId, spawned);

    let startedSession: vscode.DebugSession | undefined;
    let cancelPromise: Promise<void> | undefined;
    const cancel = (reason: string): Promise<void> => {
      if (cancelPromise) {
        return cancelPromise;
      }
      cancelPromise = (async () => {
        await spawned.dispose(reason);
        processesByRunId.delete(runId);

        const session = startedSession ?? (await waitForDebugSession(runId));
        if (session) {
          await vscode.debug.stopDebugging(session);
        }
      })();
      return cancelPromise;
    };

    cancellationSubscription = token.onCancellationRequested(() => {
      void cancel('cancelled');
    });

    const portFromOutput = await ready;
    const attachPort = portFromOutput ?? desiredPort;

    if (child.exitCode !== null || child.signalCode !== null) {
      processesByRunId.delete(runId);
      throw new Error('Test debug process exited before the debugger could attach.');
    }

    if (token.isCancellationRequested) {
      processesByRunId.delete(runId);
      await spawned.dispose('cancelled before debugger attach');
      return;
    }

    const debugConfig: vscode.DebugConfiguration & Record<string, unknown> = {
      type: NOVA_DEBUG_TYPE,
      request: 'attach',
      name: `Nova: Debug Test (${testId})`,
      host: desiredHost,
      port: attachPort,
      projectRoot: target.projectRoot,
      [NOVA_TEST_DEBUG_RUN_ID_KEY]: runId,
    };

    const debugStarted = await vscode.debug.startDebugging(workspaceFolder, debugConfig);
    if (!debugStarted) {
      processesByRunId.delete(runId);
      await spawned.dispose('debug session failed to start');
      throw new Error('VS Code refused to start the Nova debug session. See Debug Console for details.');
    }

    const started = await waitForDebugSession(runId);
    startedSession = started ?? undefined;

    if (token.isCancellationRequested) {
      await cancel('cancelled');
      return;
    }

    const exit = await waitForExit(child);
    if (item) {
      if (exit.code === 0) {
        run.passed(item);
      } else if (exit.signal) {
        run.skipped(item);
      } else {
        run.failed(item, new vscode.TestMessage(`Exit code ${exit.code ?? 'unknown'}`));
      }
    }
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    run.appendOutput(`Nova: test debug failed: ${message}\n`);
    output.appendLine(`Nova: test debug failed: ${message}`);
  } finally {
    cancellationSubscription?.dispose();
    run.end();
  }
}

function collectLeafIds(items: Iterable<vscode.TestItem>): string[] {
  const out: string[] = [];
  for (const item of items) {
    collectLeafIdsFromItem(item, out);
  }
  return out;
}

function getRootTestItems(controller: vscode.TestController): vscode.TestItem[] {
  const out: vscode.TestItem[] = [];
  controller.items.forEach((item) => out.push(item));
  return out;
}

function collectLeafIdsFromItem(item: vscode.TestItem, out: string[]): void {
  if (item.children.size === 0) {
    out.push(item.id);
    return;
  }

  item.children.forEach((child) => collectLeafIdsFromItem(child, out));
}

function getDebugDefaults(): { host: string; port: number } {
  const config = vscode.workspace.getConfiguration('nova');
  const host = config.get<string>('debug.host', '127.0.0.1');
  const port = config.get<number>('debug.port', 5005);
  return { host, port };
}

async function getBuildToolFromUser(folder: vscode.WorkspaceFolder): Promise<BuildTool> {
  const config = vscode.workspace.getConfiguration('nova', folder.uri);
  const setting = config.get<string>('tests.buildTool', 'auto');
  if (setting === 'maven' || setting === 'gradle' || setting === 'auto') {
    return setting;
  }
  if (setting !== 'prompt') {
    return 'auto';
  }

  const picked = await vscode.window.showQuickPick(
    [
      { label: 'Auto', value: 'auto' as const },
      { label: 'Maven', value: 'maven' as const },
      { label: 'Gradle', value: 'gradle' as const },
    ],
    { placeHolder: 'Select build tool for debugging tests' },
  );
  return picked?.value ?? 'auto';
}

function normalizeTestDebugCommand(resp: TestDebugResponse, port: number): TestDebugConfiguration {
  const cfg = resp.configuration;
  const normalized: TestDebugConfiguration = {
    ...cfg,
    command: cfg.command,
    args: [...cfg.args],
  };

  if (process.platform === 'win32') {
    normalized.command = normalizeWrapperScriptForWindows(cfg.cwd, normalized.command);
  }

  if (port !== 5005) {
    switch (resp.tool) {
      case 'maven':
        normalized.args = normalized.args.map((arg) => {
          if (arg !== '-Dmaven.surefire.debug') {
            return arg;
          }
          return `-Dmaven.surefire.debug=-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=${port}`;
        });
        break;
      case 'gradle':
        if (!normalized.args.some((arg) => arg.startsWith('-Dorg.gradle.debug.port='))) {
          normalized.args.push(`-Dorg.gradle.debug.port=${port}`);
        }
        break;
      case 'auto':
        break;
    }
  }

  return normalized;
}

function normalizeWrapperScriptForWindows(cwd: string, command: string): string {
  const trimmed = command.replace(/^[.][\\/]/, '');

  const mvnw = path.join(cwd, 'mvnw.cmd');
  if (trimmed === 'mvnw' || trimmed === 'mvnw.cmd') {
    if (fileExists(mvnw)) {
      return mvnw;
    }
    return 'mvn';
  }

  const gradlew = path.join(cwd, 'gradlew.bat');
  if (trimmed === 'gradlew' || trimmed === 'gradlew.bat') {
    if (fileExists(gradlew)) {
      return gradlew;
    }
    return 'gradle';
  }

  return command;
}

function fileExists(filePath: string): boolean {
  return fs.existsSync(filePath);
}

function spawnTestDebugProcess(
  config: TestDebugConfiguration,
  output: vscode.OutputChannel,
  run: vscode.TestRun,
): { child: ChildProcess; ready: Promise<number | undefined> } {
  const mergedEnv: NodeJS.ProcessEnv = { ...process.env, ...config.env };
  const child = spawn(config.command, config.args, {
    cwd: config.cwd,
    env: mergedEnv,
    detached: process.platform !== 'win32',
    shell: process.platform === 'win32',
    windowsHide: true,
  });

  const ready = waitForJdwpListening(child, output, run);

  return { child, ready };
}

function waitForDebugSession(runId: string): Promise<vscode.DebugSession | undefined> {
  return new Promise((resolve) => {
    let sub: vscode.Disposable | undefined;
    const timeout = setTimeout(() => {
      sub?.dispose();
      resolve(undefined);
    }, 10_000);

    sub = vscode.debug.onDidStartDebugSession((session) => {
      if (session.type !== NOVA_DEBUG_TYPE) {
        return;
      }
      if ((session.configuration as Record<string, unknown>)[NOVA_TEST_DEBUG_RUN_ID_KEY] !== runId) {
        return;
      }
      clearTimeout(timeout);
      sub?.dispose();
      resolve(session);
    });
  });
}

async function waitForJdwpListening(
  child: ChildProcess,
  output: vscode.OutputChannel,
  run: vscode.TestRun,
): Promise<number | undefined> {
  const jdwpRegex = /Listening for transport dt_socket at address:\s*(\d+)/i;
  let resolved = false;
  let buffer = '';

  return await new Promise<number | undefined>((resolve) => {
    const timeout = setTimeout(() => {
      if (resolved) {
        return;
      }
      resolved = true;
      resolve(undefined);
    }, 10_000);

    const onData = (data: Buffer) => {
      const text = data.toString();
      output.append(text);
      run.appendOutput(text);
      buffer += text;
      if (buffer.length > 2048) {
        buffer = buffer.slice(-2048);
      }
      const match = jdwpRegex.exec(buffer);
      if (match && !resolved) {
        resolved = true;
        clearTimeout(timeout);
        const port = Number(match[1]);
        resolve(Number.isFinite(port) ? port : undefined);
      }
    };

    child.stdout?.on('data', onData);
    child.stderr?.on('data', onData);

    child.on('exit', () => {
      if (resolved) {
        return;
      }
      resolved = true;
      clearTimeout(timeout);
      resolve(undefined);
    });
  });
}

async function terminateProcessTree(child: ChildProcess): Promise<void> {
  if (!child.pid) {
    return;
  }
  if (child.exitCode !== null || child.signalCode !== null) {
    return;
  }

  const pid = child.pid;

  if (process.platform === 'win32') {
    spawnSync('taskkill', ['/PID', pid.toString(), '/T', '/F'], { windowsHide: true });
    return;
  }

  try {
    process.kill(-pid, 'SIGTERM');
  } catch {
    try {
      child.kill('SIGTERM');
    } catch {
    }
  }

  await new Promise((resolve) => setTimeout(resolve, 1500));

  if (child.killed) {
    return;
  }

  try {
    process.kill(-pid, 'SIGKILL');
  } catch {
    try {
      child.kill('SIGKILL');
    } catch {
    }
  }
}

function waitForExit(child: ChildProcess): Promise<{ code: number | null; signal: NodeJS.Signals | null }> {
  return new Promise((resolve) => {
    if (child.exitCode !== null || child.signalCode !== null) {
      resolve({ code: child.exitCode, signal: child.signalCode });
      return;
    }
    child.once('exit', (code, signal) => resolve({ code, signal }));
  });
}

async function isLocalPortFree(host: string, port: number): Promise<boolean> {
  const normalizedHost = host.trim().toLowerCase();
  const isLocal =
    normalizedHost === '127.0.0.1' ||
    normalizedHost === 'localhost' ||
    normalizedHost === '::1' ||
    normalizedHost === '[::1]';
  if (!isLocal) {
    return true;
  }

  const hostToListen =
    normalizedHost === 'localhost' ? undefined : normalizedHost === '[::1]' ? '::1' : normalizedHost;
  return await new Promise((resolve) => {
    const server = net.createServer();
    server.once('error', () => resolve(false));
    server.once('listening', () => {
      server.close(() => resolve(true));
    });
    server.listen(port, hostToListen);
  });
}

async function findFreeLocalPort(host: string): Promise<number> {
  const normalizedHost = host.trim().toLowerCase();
  const isLocal =
    normalizedHost === '127.0.0.1' ||
    normalizedHost === 'localhost' ||
    normalizedHost === '::1' ||
    normalizedHost === '[::1]';
  if (!isLocal) {
    throw new Error(`Unable to pick a free port for non-local host ${host}`);
  }

  const hostToListen =
    normalizedHost === 'localhost' ? undefined : normalizedHost === '[::1]' ? '::1' : normalizedHost;

  return await new Promise<number>((resolve, reject) => {
    const server = net.createServer();
    server.once('error', (err) => reject(err));
    server.listen(0, hostToListen, () => {
      const address = server.address();
      if (!address || typeof address === 'string') {
        server.close();
        reject(new Error('Failed to resolve ephemeral port'));
        return;
      }
      const port = address.port;
      server.close((err) => {
        if (err) {
          reject(err);
          return;
        }
        resolve(port);
      });
    });
  });
}

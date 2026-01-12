import * as vscode from 'vscode';
import { LanguageClient, State } from 'vscode-languageclient/node';

export type WorkspaceKey = string;

export interface WorkspaceClientEntry {
  workspaceKey: WorkspaceKey;
  workspaceFolder: vscode.WorkspaceFolder;
  client: LanguageClient;
  startPromise: Promise<void>;
  serverCommand: string;
  disposables: vscode.Disposable[];
}

export class MultiRootClientManager {
  private readonly clients = new Map<WorkspaceKey, WorkspaceClientEntry>();

  constructor(private readonly onDidDisposeClient?: (entry: WorkspaceClientEntry) => void) {}

  get(workspaceKey: WorkspaceKey): WorkspaceClientEntry | undefined {
    return this.clients.get(workspaceKey);
  }

  entries(): WorkspaceClientEntry[] {
    return Array.from(this.clients.values());
  }

  has(workspaceKey: WorkspaceKey): boolean {
    return this.clients.has(workspaceKey);
  }

  async ensureClient(
    folder: vscode.WorkspaceFolder,
    serverCommand: string,
    factory: (folder: vscode.WorkspaceFolder, workspaceKey: WorkspaceKey, serverCommand: string) => WorkspaceClientEntry,
  ): Promise<WorkspaceClientEntry> {
    const workspaceKey = folder.uri.toString();
    const existing = this.clients.get(workspaceKey);

    if (existing) {
      if (existing.serverCommand !== serverCommand) {
        await this.stopClient(workspaceKey);
      } else {
        if (existing.client.state === State.Running) {
          return existing;
        }

        try {
          await existing.startPromise;
          return existing;
        } catch {
          await this.stopClient(workspaceKey);
        }
      }
    }

    const created = factory(folder, workspaceKey, serverCommand);
    this.clients.set(workspaceKey, created);

    // Best-effort: ensure failed starts don't leave stale entries.
    created.startPromise
      .catch(async () => {
        if (this.clients.get(workspaceKey) !== created) {
          return;
        }
        await this.stopClient(workspaceKey);
      })
      .catch(() => {});

    return created;
  }

  async stopClient(workspaceKey: WorkspaceKey): Promise<void> {
    const entry = this.clients.get(workspaceKey);
    if (!entry) {
      return;
    }

    this.clients.delete(workspaceKey);

    try {
      await entry.client.stop();
    } catch {
      // Best-effort: stopping can fail if the server never started cleanly.
    } finally {
      for (const disposable of entry.disposables) {
        try {
          disposable.dispose();
        } catch {
          // Ignore dispose errors.
        }
      }
      this.onDidDisposeClient?.(entry);
    }
  }

  async stopAll(): Promise<void> {
    const keys = Array.from(this.clients.keys());
    for (const key of keys) {
      await this.stopClient(key);
    }
  }
}


import * as vscode from 'vscode';

export type NovaRequest = <R>(method: string, params?: unknown) => Promise<R | undefined>;

export interface JavaLanguageLevel {
  source: string;
  target: string;
  release: string | null;
}

export type ProjectModelUnit =
  | {
      kind: 'maven' | 'simple';
      module: string;
      compileClasspath: string[];
      modulePath: string[];
      sourceRoots: string[];
      languageLevel: JavaLanguageLevel;
    }
  | {
      kind: 'gradle';
      projectPath: string;
      compileClasspath: string[];
      modulePath: string[];
      sourceRoots: string[];
      languageLevel: JavaLanguageLevel;
    }
  | {
      kind: 'bazel';
      target: string;
      compileClasspath: string[];
      modulePath: string[];
      sourceRoots: string[];
      languageLevel: JavaLanguageLevel;
    };

export interface ProjectModelResult {
  projectRoot: string;
  units: ProjectModelUnit[];
}

export interface ProjectConfigurationResponse {
  schemaVersion: number;
  workspaceRoot: string;
  buildSystem: string;
  java?: { source?: number; target?: number } | null;
  modules?: Array<{ name?: string; root?: string }> | null;
  sourceRoots?: Array<{ kind?: string; origin?: string; path?: string }> | null;
  classpath?: Array<{ kind?: string; path?: string }> | null;
  modulePath?: Array<{ kind?: string; path?: string }> | null;
  outputDirs?: Array<{ kind?: string; path?: string }> | null;
  dependencies?: Array<{ groupId?: string; artifactId?: string; scope?: string }> | null;
}

export interface ProjectModelCacheOptions {
  /**
   * How long (ms) to treat a successful response as "fresh" before allowing a new request.
   */
  ttlMs?: number;
}

export interface CacheSnapshot<T> {
  value?: T;
  fetchedAt?: number;
  inFlight?: Promise<T>;
  lastError?: unknown;
  stale: boolean;
}

type CacheEntry<T> = {
  value?: T;
  fetchedAt?: number;
  inFlight?: Promise<T>;
  lastError?: unknown;
};

const DEFAULT_TTL_MS = 15_000;

export class ProjectModelCache {
  private readonly ttlMs: number;
  private readonly projectModelByWorkspace = new Map<string, CacheEntry<ProjectModelResult>>();
  private readonly projectConfigurationByWorkspace = new Map<string, CacheEntry<ProjectConfigurationResponse>>();

  // Session-level feature gating: once we know the server doesn't support a method, stop calling it.
  private projectModelUnsupported = false;
  private projectConfigurationUnsupported = false;

  constructor(
    private readonly novaRequest: NovaRequest,
    opts?: ProjectModelCacheOptions,
  ) {
    this.ttlMs = opts?.ttlMs ?? DEFAULT_TTL_MS;
  }

  isProjectModelUnsupported(): boolean {
    return this.projectModelUnsupported;
  }

  isProjectConfigurationUnsupported(): boolean {
    return this.projectConfigurationUnsupported;
  }

  clear(workspaceFolder?: vscode.WorkspaceFolder): void {
    if (!workspaceFolder) {
      this.projectModelByWorkspace.clear();
      this.projectConfigurationByWorkspace.clear();
      return;
    }
    const key = this.key(workspaceFolder);
    this.projectModelByWorkspace.delete(key);
    this.projectConfigurationByWorkspace.delete(key);
  }

  peekProjectModel(workspaceFolder: vscode.WorkspaceFolder): CacheSnapshot<ProjectModelResult> {
    return this.peekEntry(this.projectModelByWorkspace, this.key(workspaceFolder));
  }

  peekProjectConfiguration(workspaceFolder: vscode.WorkspaceFolder): CacheSnapshot<ProjectConfigurationResponse> {
    return this.peekEntry(this.projectConfigurationByWorkspace, this.key(workspaceFolder));
  }

  async getProjectModel(
    workspaceFolder: vscode.WorkspaceFolder,
    opts?: { forceRefresh?: boolean },
  ): Promise<ProjectModelResult> {
    if (this.projectModelUnsupported) {
      throw new Error('nova/projectModel is not supported by the active Nova server.');
    }

    const forceRefresh = opts?.forceRefresh === true;
    const key = this.key(workspaceFolder);
    const entry = this.getOrCreateEntry(this.projectModelByWorkspace, key);

    if (!forceRefresh && entry.value && !this.isStale(entry.fetchedAt)) {
      return entry.value;
    }

    if (entry.inFlight) {
      return entry.inFlight;
    }

    entry.lastError = undefined;

    entry.inFlight = this.novaRequest<ProjectModelResult>('nova/projectModel', { projectRoot: workspaceFolder.uri.fsPath })
      .then((result) => {
        if (!result) {
          this.projectModelUnsupported = true;
          throw new Error('nova/projectModel is not supported by the active Nova server.');
        }
        entry.value = result;
        entry.fetchedAt = Date.now();
        entry.lastError = undefined;
        return result;
      })
      .catch((err) => {
        entry.lastError = err;
        if (isMethodNotFoundError(err)) {
          this.projectModelUnsupported = true;
        }
        throw err;
      })
      .finally(() => {
        entry.inFlight = undefined;
      });

    return entry.inFlight;
  }

  async getProjectConfiguration(
    workspaceFolder: vscode.WorkspaceFolder,
    opts?: { forceRefresh?: boolean },
  ): Promise<ProjectConfigurationResponse> {
    if (this.projectConfigurationUnsupported) {
      throw new Error('nova/projectConfiguration is not supported by the active Nova server.');
    }

    const forceRefresh = opts?.forceRefresh === true;
    const key = this.key(workspaceFolder);
    const entry = this.getOrCreateEntry(this.projectConfigurationByWorkspace, key);

    if (!forceRefresh && entry.value && !this.isStale(entry.fetchedAt)) {
      return entry.value;
    }

    if (entry.inFlight) {
      return entry.inFlight;
    }

    entry.lastError = undefined;

    entry.inFlight = this.novaRequest<ProjectConfigurationResponse>('nova/projectConfiguration', {
      projectRoot: workspaceFolder.uri.fsPath,
    })
      .then((result) => {
        if (!result) {
          this.projectConfigurationUnsupported = true;
          throw new Error('nova/projectConfiguration is not supported by the active Nova server.');
        }
        entry.value = result;
        entry.fetchedAt = Date.now();
        entry.lastError = undefined;
        return result;
      })
      .catch((err) => {
        entry.lastError = err;
        if (isMethodNotFoundError(err)) {
          this.projectConfigurationUnsupported = true;
        }
        throw err;
      })
      .finally(() => {
        entry.inFlight = undefined;
      });

    return entry.inFlight;
  }

  private peekEntry<T>(cache: Map<string, CacheEntry<T>>, key: string): CacheSnapshot<T> {
    const entry = cache.get(key);
    return {
      value: entry?.value,
      fetchedAt: entry?.fetchedAt,
      inFlight: entry?.inFlight,
      lastError: entry?.lastError,
      stale: this.isStale(entry?.fetchedAt),
    };
  }

  private getOrCreateEntry<T>(cache: Map<string, CacheEntry<T>>, key: string): CacheEntry<T> {
    const existing = cache.get(key);
    if (existing) {
      return existing;
    }
    const created: CacheEntry<T> = {};
    cache.set(key, created);
    return created;
  }

  private isStale(fetchedAt: number | undefined): boolean {
    if (typeof fetchedAt !== 'number') {
      return true;
    }
    return Date.now() - fetchedAt > this.ttlMs;
  }

  private key(workspaceFolder: vscode.WorkspaceFolder): string {
    return workspaceFolder.uri.fsPath;
  }
}

function isMethodNotFoundError(err: unknown): boolean {
  if (!err || typeof err !== 'object') {
    return false;
  }

  const code = (err as { code?: unknown }).code;
  if (code === -32601) {
    return true;
  }

  const message = (err as { message?: unknown }).message;
  // `nova-lsp` currently reports unknown `nova/*` custom methods as `-32602` with an
  // "unknown (stateless) method" message (because everything is routed through a single dispatcher).
  if (code === -32602 && typeof message === 'string' && message.toLowerCase().includes('unknown (stateless) method')) {
    return true;
  }

  return typeof message === 'string' && message.toLowerCase().includes('method not found');
}

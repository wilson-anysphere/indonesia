import * as vscode from 'vscode';
import { isMethodNotFoundError } from './safeMode';

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

  // Session-level feature gating (per workspace key): once we know a workspace's server doesn't
  // support a method, stop calling it for that workspace. This will matter once Nova runs one
  // LanguageClient per workspace folder (multi-root).
  private readonly projectModelUnsupportedByWorkspace = new Set<string>();
  private readonly projectConfigurationUnsupportedByWorkspace = new Set<string>();

  constructor(
    private readonly novaRequest: NovaRequest,
    opts?: ProjectModelCacheOptions,
  ) {
    this.ttlMs = opts?.ttlMs ?? DEFAULT_TTL_MS;
  }

  isProjectModelUnsupported(workspaceFolder: vscode.WorkspaceFolder): boolean;
  isProjectModelUnsupported(): boolean;
  isProjectModelUnsupported(workspaceFolder?: vscode.WorkspaceFolder): boolean {
    if (!workspaceFolder) {
      // Backwards-compatible: treat "any unsupported" as unsupported.
      return this.projectModelUnsupportedByWorkspace.size > 0;
    }
    return this.projectModelUnsupportedByWorkspace.has(this.key(workspaceFolder));
  }

  isProjectConfigurationUnsupported(workspaceFolder: vscode.WorkspaceFolder): boolean;
  isProjectConfigurationUnsupported(): boolean;
  isProjectConfigurationUnsupported(workspaceFolder?: vscode.WorkspaceFolder): boolean {
    if (!workspaceFolder) {
      // Backwards-compatible: treat "any unsupported" as unsupported.
      return this.projectConfigurationUnsupportedByWorkspace.size > 0;
    }
    return this.projectConfigurationUnsupportedByWorkspace.has(this.key(workspaceFolder));
  }

  clear(workspaceFolder?: vscode.WorkspaceFolder): void {
    if (!workspaceFolder) {
      this.projectModelByWorkspace.clear();
      this.projectConfigurationByWorkspace.clear();
      this.projectModelUnsupportedByWorkspace.clear();
      this.projectConfigurationUnsupportedByWorkspace.clear();
      return;
    }
    const key = this.key(workspaceFolder);
    this.projectModelByWorkspace.delete(key);
    this.projectConfigurationByWorkspace.delete(key);
    this.projectModelUnsupportedByWorkspace.delete(key);
    this.projectConfigurationUnsupportedByWorkspace.delete(key);
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
    const key = this.key(workspaceFolder);
    if (this.projectModelUnsupportedByWorkspace.has(key)) {
      throw new Error('nova/projectModel is not supported by the active Nova server.');
    }

    const forceRefresh = opts?.forceRefresh === true;
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
          this.projectModelUnsupportedByWorkspace.add(key);
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
          this.projectModelUnsupportedByWorkspace.add(key);
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
    const key = this.key(workspaceFolder);
    if (this.projectConfigurationUnsupportedByWorkspace.has(key)) {
      throw new Error('nova/projectConfiguration is not supported by the active Nova server.');
    }

    const forceRefresh = opts?.forceRefresh === true;
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
          this.projectConfigurationUnsupportedByWorkspace.add(key);
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
          this.projectConfigurationUnsupportedByWorkspace.add(key);
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

import * as vscode from 'vscode';
import * as path from 'node:path';
import { resolvePossiblyRelativePath } from './pathUtils';
import { formatUnsupportedNovaMethodMessage, isNovaMethodNotFoundError, isNovaRequestSupported } from './novaCapabilities';
import { formatError, isSafeModeError } from './safeMode';
import {
  ProjectModelCache,
  type JavaLanguageLevel,
  type ProjectConfigurationResponse,
  type ProjectModelResult,
  type ProjectModelUnit,
} from './projectModelCache';

export type NovaRequest = <R>(
  method: string,
  params?: unknown,
  opts?: { token?: vscode.CancellationToken },
) => Promise<R | undefined>;
export type NovaProjectExplorerController = { refresh(): void };

type BuildSystemKind = ProjectModelUnit['kind'];

type ListKind = 'sourceRoots' | 'classpath' | 'modulePath';

type ConfigurationListKind = 'modules' | 'sourceRoots' | 'classpath' | 'modulePath' | 'outputDirs' | 'dependencies';

type ConfigurationModuleEntry = NonNullable<ProjectConfigurationResponse['modules']>[number];
type ConfigurationSourceRootEntry = NonNullable<ProjectConfigurationResponse['sourceRoots']>[number];
type ConfigurationClasspathEntry = NonNullable<ProjectConfigurationResponse['classpath']>[number];
type ConfigurationModulePathEntry = NonNullable<ProjectConfigurationResponse['modulePath']>[number];
type ConfigurationOutputDirEntry = NonNullable<ProjectConfigurationResponse['outputDirs']>[number];
type ConfigurationDependencyEntry = NonNullable<ProjectConfigurationResponse['dependencies']>[number];

type NovaProjectExplorerNode =
  | { type: 'message'; id: string; label: string; description?: string; icon?: vscode.ThemeIcon; command?: vscode.Command }
  | { type: 'workspace'; id: string; workspace: vscode.WorkspaceFolder }
  | { type: 'workspaceConfiguration'; id: string; workspace: vscode.WorkspaceFolder }
  | { type: 'workspaceInfo'; id: string; label: string; description?: string }
  | { type: 'unit'; id: string; workspace: vscode.WorkspaceFolder; projectRoot: string; unit: ProjectModelUnit }
  | { type: 'unitInfo'; id: string; label: string; description?: string }
  | {
      type: 'configGroup';
      id: string;
      label: string;
      workspace: vscode.WorkspaceFolder;
      baseDir: string;
      listKind: 'modules';
      entries: ConfigurationModuleEntry[];
    }
  | {
      type: 'configGroup';
      id: string;
      label: string;
      workspace: vscode.WorkspaceFolder;
      baseDir: string;
      listKind: 'sourceRoots';
      entries: ConfigurationSourceRootEntry[];
    }
  | {
      type: 'configGroup';
      id: string;
      label: string;
      workspace: vscode.WorkspaceFolder;
      baseDir: string;
      listKind: 'classpath';
      entries: ConfigurationClasspathEntry[];
    }
  | {
      type: 'configGroup';
      id: string;
      label: string;
      workspace: vscode.WorkspaceFolder;
      baseDir: string;
      listKind: 'modulePath';
      entries: ConfigurationModulePathEntry[];
    }
  | {
      type: 'configGroup';
      id: string;
      label: string;
      workspace: vscode.WorkspaceFolder;
      baseDir: string;
      listKind: 'outputDirs';
      entries: ConfigurationOutputDirEntry[];
    }
  | {
      type: 'configGroup';
      id: string;
      label: string;
      workspace: vscode.WorkspaceFolder;
      baseDir: string;
      listKind: 'dependencies';
      entries: ConfigurationDependencyEntry[];
    }
  | {
      type: 'configChunk';
      id: string;
      label: string;
      workspace: vscode.WorkspaceFolder;
      baseDir: string;
      listKind: ConfigurationListKind;
      entries:
        | ConfigurationModuleEntry[]
        | ConfigurationSourceRootEntry[]
        | ConfigurationClasspathEntry[]
        | ConfigurationModulePathEntry[]
        | ConfigurationOutputDirEntry[]
        | ConfigurationDependencyEntry[];
      start: number;
      end: number;
    }
  | {
      type: 'group';
      id: string;
      label: string;
      workspace: vscode.WorkspaceFolder;
      projectRoot: string;
      unit: ProjectModelUnit;
      listKind: ListKind;
      entries: string[];
    }
  | {
      type: 'chunk';
      id: string;
      label: string;
      workspace: vscode.WorkspaceFolder;
      projectRoot: string;
      unit: ProjectModelUnit;
      listKind: Exclude<ListKind, 'sourceRoots'>;
      entries: string[];
      start: number;
      end: number;
    }
  | {
      type: 'path';
      id: string;
      label: string;
      description?: string;
      uri?: vscode.Uri;
      contextValue?: string;
      command?: vscode.Command;
      icon?: vscode.ThemeIcon;
    };

const VIEW_ID = 'novaProjectExplorer';
const CONTEXT_WORKSPACE = 'novaProjectExplorerWorkspace';
const CONTEXT_UNIT = 'novaProjectExplorerUnit';
const CONTEXT_PATH = 'novaProjectExplorerPath';

const COMMAND_REFRESH = 'nova.refreshProjectExplorer';
const COMMAND_SHOW_MODEL = 'nova.showProjectModel';
const COMMAND_SHOW_CONFIG = 'nova.showProjectConfiguration';
const COMMAND_REVEAL_PATH = 'nova.projectExplorer.revealPath';
const COMMAND_COPY_PATH = 'nova.projectExplorer.copyPath';

const CLASS_PATH_PAGE_SIZE = 200;
const CONFIG_LIST_PAGE_SIZE = 200;

export function registerNovaProjectExplorer(
  context: vscode.ExtensionContext,
  request: NovaRequest,
  cache?: ProjectModelCache,
  opts?: { isServerRunning?: () => boolean; isSafeMode?: () => boolean },
): NovaProjectExplorerController {
  const projectModelCache = cache ?? new ProjectModelCache(request);
  const provider = new NovaProjectExplorerProvider(projectModelCache, opts);

  const view = vscode.window.createTreeView(VIEW_ID, {
    treeDataProvider: provider,
    showCollapseAll: true,
  });

  context.subscriptions.push(view);

  context.subscriptions.push(
    vscode.commands.registerCommand(COMMAND_REFRESH, (arg?: unknown) => {
      const workspace = getWorkspaceFolderFromCommandArg(arg ?? view.selection?.[0]);
      provider.refresh({ forceRefresh: true, workspace });
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(COMMAND_SHOW_MODEL, async (arg?: unknown) => {
      await showProjectModel(projectModelCache, request, arg ?? view.selection?.[0]);
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(COMMAND_SHOW_CONFIG, async (arg?: unknown) => {
      await showProjectConfiguration(projectModelCache, request, arg ?? view.selection?.[0]);
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(COMMAND_REVEAL_PATH, async (arg?: unknown) => {
      const uri = extractUri(arg ?? view.selection?.[0]);
      if (!uri) {
        void vscode.window.showErrorMessage('Nova: no path selected.');
        return;
      }
      await revealPath(uri);
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(COMMAND_COPY_PATH, async (arg?: unknown) => {
      await copyPath(arg ?? view.selection?.[0]);
    }),
  );

  context.subscriptions.push(
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      provider.refresh({ clearCache: true });
    }),
  );

  return provider;
}

class NovaProjectExplorerProvider implements vscode.TreeDataProvider<NovaProjectExplorerNode> {
  private readonly emitter = new vscode.EventEmitter<NovaProjectExplorerNode | undefined | null>();
  private lastContextServerRunning: boolean | undefined;
  private lastContextProjectModelSupported: boolean | undefined;

  private readonly isServerRunning: () => boolean;
  private readonly isSafeMode: () => boolean;

  constructor(
    private readonly cache: ProjectModelCache,
    opts?: { isServerRunning?: () => boolean; isSafeMode?: () => boolean },
  ) {
    this.isServerRunning = opts?.isServerRunning ?? (() => true);
    this.isSafeMode = opts?.isSafeMode ?? (() => false);
  }

  get onDidChangeTreeData(): vscode.Event<NovaProjectExplorerNode | undefined | null> {
    return this.emitter.event;
  }

  refresh(opts?: { clearCache?: boolean; forceRefresh?: boolean; workspace?: vscode.WorkspaceFolder }): void {
    if (opts?.clearCache) {
      this.cache.clear(opts.workspace);
    }

    // Avoid spamming `nova/*` requests while in safe mode. The view stays empty so `viewsWelcome`
    // can direct users to bug report generation.
    if (opts?.forceRefresh && this.isSafeMode()) {
      this.emitter.fire(undefined);
      return;
    }

    const folders = opts?.workspace ? [opts.workspace] : (vscode.workspace.workspaceFolders ?? []);
    if (opts?.forceRefresh) {
      for (const folder of folders) {
        const promises: Array<Promise<unknown>> = [this.cache.getProjectModel(folder, { forceRefresh: true })];

        // Also refresh project configuration if it's already been fetched (e.g. the node is expanded),
        // or when a specific workspace is being refreshed from an inline error banner.
        const shouldRefreshConfiguration = Boolean(opts.workspace) || Boolean(this.cache.peekProjectConfiguration(folder).value);
        if (shouldRefreshConfiguration) {
          promises.push(this.cache.getProjectConfiguration(folder, { forceRefresh: true }));
        }
        for (const promise of promises) {
          void promise
            .catch(() => {})
            .finally(() => {
              this.emitter.fire(undefined);
            });
        }
      }
    }

    // Fire after registering in-flight promises so getChildren can render loading placeholders.
    this.emitter.fire(undefined);
  }

  async getChildren(element?: NovaProjectExplorerNode): Promise<NovaProjectExplorerNode[]> {
    if (!element) {
      const workspaces = vscode.workspace.workspaceFolders ?? [];
      if (workspaces.length === 0) {
        await this.setContexts({ serverRunning: false, projectModelSupported: true });
        return [];
      }

      const serverRunning = this.isServerRunning();
      if (!serverRunning) {
        await this.setContexts({ serverRunning: false, projectModelSupported: true });
        return [];
      }

      if (this.isSafeMode()) {
        // Keep the view empty so `contributes.viewsWelcome` can direct users to bug report generation.
        await this.setContexts({ serverRunning: true, projectModelSupported: true });
        return [];
      }

      const projectModelSupported = this.isProjectModelSupported();
      await this.setContexts({ serverRunning: true, projectModelSupported });

      return workspaces.map((workspace) => ({
        type: 'workspace',
        id: `workspace:${workspace.uri.toString()}`,
        workspace,
      }));
    }

    switch (element.type) {
      case 'workspace': {
        const workspace = element.workspace;
        const configurationNode: NovaProjectExplorerNode = {
          type: 'workspaceConfiguration',
          id: `${element.id}:configuration`,
          workspace,
        };
        if (!this.isProjectModelSupportedForWorkspace(workspace)) {
          await this.triggerUnsupportedWelcome();
          return [
            {
              type: 'message',
              id: `${element.id}:unsupported`,
              label: 'Project model not supported by this server.',
              description: 'Update nova-lsp to a build that supports nova/projectModel.',
              icon: new vscode.ThemeIcon('info'),
              command: { title: 'Show Project Configuration', command: COMMAND_SHOW_CONFIG, arguments: [workspace] },
            },
            configurationNode,
          ];
        }

        const snapshot = this.cache.peekProjectModel(workspace);
        const model = snapshot.value;

        if (!model) {
          if (snapshot.inFlight) {
            return [
              {
                type: 'message',
                id: `${element.id}:loading`,
                label: 'Loading project model…',
                icon: new vscode.ThemeIcon('loading'),
              },
              configurationNode,
            ];
          }

          if (snapshot.lastError) {
            return [
              {
                type: 'message',
                id: `${element.id}:error`,
                label: 'Failed to load project model.',
                description: formatError(snapshot.lastError),
                icon: new vscode.ThemeIcon('error'),
                command: { title: 'Refresh', command: COMMAND_REFRESH, arguments: [workspace] },
              },
              configurationNode,
            ];
          }

          const promise = this.cache.getProjectModel(workspace);
          void promise
            .catch(() => {})
            .finally(() => {
              this.emitter.fire(element);
            });

          return [
            {
              type: 'message',
              id: `${element.id}:loading`,
              label: 'Loading project model…',
              icon: new vscode.ThemeIcon('loading'),
            },
            configurationNode,
          ];
        }

        // If the cached model is stale, refresh in the background but keep showing the previous model.
        let inFlight = snapshot.inFlight;
        if (!inFlight && snapshot.stale) {
          const promise = this.cache.getProjectModel(workspace);
          inFlight = promise;
          void promise
            .catch(() => {})
            .finally(() => {
              this.emitter.fire(element);
            });
        }

        const resolvedProjectRoot =
          typeof model.projectRoot === 'string' && model.projectRoot.trim().length > 0
            ? resolvePossiblyRelativePath(workspace.uri.fsPath, model.projectRoot)
            : '';
        const projectRoot = resolvedProjectRoot || workspace.uri.fsPath;

        const buildSystem = summarizeBuildSystemLabel(model.units);
        const javaLanguageLevel = summarizeJavaLanguageLevelLabel(model.units);

        const nodes: NovaProjectExplorerNode[] = [];

        if (inFlight) {
          nodes.push({
            type: 'message',
            id: `${element.id}:loading`,
            label: 'Loading project model…',
            icon: new vscode.ThemeIcon('loading'),
          });
        }

        if (snapshot.lastError && !inFlight) {
          nodes.push({
            type: 'message',
            id: `${element.id}:last-error`,
            label: 'Last refresh failed.',
            description: formatError(snapshot.lastError),
            icon: new vscode.ThemeIcon('error'),
            command: { title: 'Refresh', command: COMMAND_REFRESH, arguments: [workspace] },
          });
        }

        const infoNodes: NovaProjectExplorerNode[] = [];
        if (buildSystem) {
          infoNodes.push({
            type: 'workspaceInfo',
            id: `${element.id}:info:buildSystem`,
            label: `Build System: ${buildSystem}`,
          });
        }
        if (javaLanguageLevel) {
          infoNodes.push({
            type: 'workspaceInfo',
            id: `${element.id}:info:java`,
            label: 'Java Language Level',
            description: javaLanguageLevel,
          });
        }
        if (projectRoot && path.normalize(projectRoot) !== path.normalize(workspace.uri.fsPath)) {
          const uri = vscode.Uri.file(projectRoot);
          infoNodes.push({
            type: 'path',
            id: `${element.id}:projectRoot`,
            label: 'Project Root',
            description: projectRoot,
            uri,
            icon: vscode.ThemeIcon.Folder,
            command: {
              title: 'Reveal Project Root',
              command: COMMAND_REVEAL_PATH,
              arguments: [uri],
            },
          });
        }

        const unitNodes = model.units.map((unit, idx) => ({
          type: 'unit' as const,
          id: `${element.id}:unit:${idx}:${unitId(unit)}`,
          workspace,
          projectRoot,
          unit,
        }));

        if (unitNodes.length === 0) {
          nodes.push(...infoNodes);
          nodes.push({
            type: 'message',
            id: `${element.id}:no-units`,
            label: 'No project units reported.',
          });
          nodes.push(configurationNode);
          return nodes;
        }

        nodes.push(...infoNodes, ...unitNodes, configurationNode);
        return nodes;
      }

      case 'workspaceConfiguration': {
        const workspace = element.workspace;
        if (this.cache.isProjectConfigurationUnsupported(workspace)) {
          return [
            {
              type: 'message',
              id: `${element.id}:unsupported`,
              label: 'Project configuration not supported by this server.',
              description: 'Update nova-lsp to a build that supports nova/projectConfiguration.',
              icon: new vscode.ThemeIcon('info'),
            },
          ];
        }

        const snapshot = this.cache.peekProjectConfiguration(workspace);
        const config = snapshot.value;

        if (!config) {
          if (snapshot.inFlight) {
            return [
              {
                type: 'message',
                id: `${element.id}:loading`,
                label: 'Loading project configuration…',
                icon: new vscode.ThemeIcon('loading'),
              },
            ];
          }

          if (snapshot.lastError) {
            return [
              {
                type: 'message',
                id: `${element.id}:error`,
                label: 'Failed to load project configuration.',
                description: formatError(snapshot.lastError),
                icon: new vscode.ThemeIcon('error'),
                command: { title: 'Refresh', command: COMMAND_REFRESH, arguments: [workspace] },
              },
            ];
          }

          const promise = this.cache.getProjectConfiguration(workspace);
          void promise
            .catch(() => {})
            .finally(() => {
              this.emitter.fire(element);
            });

          return [
            {
              type: 'message',
              id: `${element.id}:loading`,
              label: 'Loading project configuration…',
              icon: new vscode.ThemeIcon('loading'),
            },
          ];
        }

        // If the cached configuration is stale, refresh in the background but keep showing the previous snapshot.
        let inFlight = snapshot.inFlight;
        if (!inFlight && snapshot.stale) {
          const promise = this.cache.getProjectConfiguration(workspace);
          inFlight = promise;
          void promise
            .catch(() => {})
            .finally(() => {
              this.emitter.fire(element);
            });
        }

        const nodes: NovaProjectExplorerNode[] = [];

        if (inFlight) {
          nodes.push({
            type: 'message',
            id: `${element.id}:loading`,
            label: 'Loading project configuration…',
            icon: new vscode.ThemeIcon('loading'),
          });
        }

        if (snapshot.lastError && !inFlight) {
          nodes.push({
            type: 'message',
            id: `${element.id}:last-error`,
            label: 'Last refresh failed.',
            description: formatError(snapshot.lastError),
            icon: new vscode.ThemeIcon('error'),
            command: { title: 'Refresh', command: COMMAND_REFRESH, arguments: [workspace] },
          });
        }

        const buildSystem = typeof config.buildSystem === 'string' && config.buildSystem.trim().length > 0 ? config.buildSystem : undefined;
        if (buildSystem) {
          nodes.push({
            type: 'workspaceInfo',
            id: `${element.id}:info:buildSystem`,
            label: 'Build System',
            description: buildSystem,
          });
        }

        const schemaVersion = typeof config.schemaVersion === 'number' ? config.schemaVersion : undefined;
        if (typeof schemaVersion === 'number') {
          nodes.push({
            type: 'workspaceInfo',
            id: `${element.id}:info:schemaVersion`,
            label: 'Schema Version',
            description: String(schemaVersion),
          });
        }

        const workspaceRootRaw =
          typeof config.workspaceRoot === 'string' && config.workspaceRoot.trim().length > 0 ? config.workspaceRoot.trim() : undefined;
        const resolvedWorkspaceRoot = workspaceRootRaw
          ? resolvePossiblyRelativePath(workspace.uri.fsPath, workspaceRootRaw)
          : workspace.uri.fsPath;
        const configBaseDir = resolvedWorkspaceRoot || workspace.uri.fsPath;
        if (workspaceRootRaw) {
          const uri = resolvedWorkspaceRoot ? vscode.Uri.file(resolvedWorkspaceRoot) : undefined;
          nodes.push({
            type: 'path',
            id: `${element.id}:workspaceRoot`,
            label: 'Workspace Root',
            description: resolvedWorkspaceRoot || workspaceRootRaw,
            uri,
            icon: vscode.ThemeIcon.Folder,
            command: uri
              ? {
                  title: 'Reveal Workspace Root',
                  command: COMMAND_REVEAL_PATH,
                  arguments: [uri],
                }
              : undefined,
          });
        }

        const javaSource = config.java?.source;
        const javaTarget = config.java?.target;
        if (typeof javaSource === 'number' || typeof javaTarget === 'number') {
          nodes.push({
            type: 'workspaceInfo',
            id: `${element.id}:info:java`,
            label: 'Java',
            description: `source=${typeof javaSource === 'number' ? javaSource : '—'}, target=${typeof javaTarget === 'number' ? javaTarget : '—'}`,
          });
        }

        const modules = Array.isArray(config.modules) ? config.modules.filter(Boolean) : [];
        nodes.push({
          type: 'configGroup',
          id: `${element.id}:modules`,
          label: `Modules (${modules.length})`,
          workspace,
          baseDir: configBaseDir,
          listKind: 'modules',
          entries: modules,
        });

        const sourceRoots = Array.isArray(config.sourceRoots) ? config.sourceRoots.filter(Boolean) : [];
        nodes.push({
          type: 'configGroup',
          id: `${element.id}:sourceRoots`,
          label: `Source Roots (${sourceRoots.length})`,
          workspace,
          baseDir: configBaseDir,
          listKind: 'sourceRoots',
          entries: sourceRoots,
        });

        const classpath = Array.isArray(config.classpath) ? config.classpath.filter(Boolean) : [];
        nodes.push({
          type: 'configGroup',
          id: `${element.id}:classpath`,
          label: `Classpath (${classpath.length})`,
          workspace,
          baseDir: configBaseDir,
          listKind: 'classpath',
          entries: classpath,
        });

        const modulePath = Array.isArray(config.modulePath) ? config.modulePath.filter(Boolean) : [];
        nodes.push({
          type: 'configGroup',
          id: `${element.id}:modulePath`,
          label: `Module Path (${modulePath.length})`,
          workspace,
          baseDir: configBaseDir,
          listKind: 'modulePath',
          entries: modulePath,
        });

        const outputDirs = Array.isArray(config.outputDirs) ? config.outputDirs.filter(Boolean) : [];
        nodes.push({
          type: 'configGroup',
          id: `${element.id}:outputDirs`,
          label: `Output Dirs (${outputDirs.length})`,
          workspace,
          baseDir: configBaseDir,
          listKind: 'outputDirs',
          entries: outputDirs,
        });

        const dependencies = Array.isArray(config.dependencies) ? config.dependencies.filter(Boolean) : [];
        nodes.push({
          type: 'configGroup',
          id: `${element.id}:dependencies`,
          label: `Dependencies (${dependencies.length})`,
          workspace,
          baseDir: configBaseDir,
          listKind: 'dependencies',
          entries: dependencies,
        });

        return nodes;
      }

      case 'unit': {
        const { unit, workspace, projectRoot } = element;

        const sourceRoots = Array.isArray(unit.sourceRoots) ? unit.sourceRoots : [];
        const classpath = Array.isArray(unit.compileClasspath) ? unit.compileClasspath : [];
        const modulePath = Array.isArray(unit.modulePath) ? unit.modulePath : [];

        const children: NovaProjectExplorerNode[] = [];

        children.push({
          type: 'group',
          id: `${element.id}:group:sourceRoots`,
          label: `Source Roots (${sourceRoots.length})`,
          workspace,
          projectRoot,
          unit,
          listKind: 'sourceRoots',
          entries: sourceRoots,
        });

        children.push({
          type: 'group',
          id: `${element.id}:group:classpath`,
          label: `Classpath (${classpath.length})`,
          workspace,
          projectRoot,
          unit,
          listKind: 'classpath',
          entries: classpath,
        });

        children.push({
          type: 'group',
          id: `${element.id}:group:modulePath`,
          label: `Module Path (${modulePath.length})`,
          workspace,
          projectRoot,
          unit,
          listKind: 'modulePath',
          entries: modulePath,
        });

        if (unit.languageLevel) {
          children.push({
            type: 'unitInfo',
            id: `${element.id}:info:languageLevel`,
            label: 'Language Level',
            description: formatJavaLanguageLevel(unit.languageLevel),
          });
        }

        return children;
      }

      case 'group': {
        if (element.listKind === 'sourceRoots') {
          return element.entries.map((entry, idx) =>
            createSourceRootNode(element, entry, idx, { projectRoot: element.projectRoot }),
          );
        }

        const entries = element.entries;
        if (entries.length <= CLASS_PATH_PAGE_SIZE) {
          return entries.map((entry, idx) => createClasspathEntryNode(element, entry, idx));
        }

        const chunks: NovaProjectExplorerNode[] = [];
        for (let start = 0; start < entries.length; start += CLASS_PATH_PAGE_SIZE) {
          const end = Math.min(entries.length, start + CLASS_PATH_PAGE_SIZE);
          chunks.push({
            type: 'chunk',
            id: `${element.id}:chunk:${start}-${end}`,
            label: `Entries ${start + 1}\u2013${end}`,
            workspace: element.workspace,
            projectRoot: element.projectRoot,
            unit: element.unit,
            listKind: element.listKind,
            entries,
            start,
            end,
          });
        }
        return chunks;
      }

      case 'chunk': {
        const slice = element.entries.slice(element.start, element.end);
        return slice.map((entry, idx) =>
          createClasspathEntryNode(element, entry, element.start + idx, { sliceLabel: element.label }),
        );
      }

      case 'configGroup': {
        if (element.entries.length <= CONFIG_LIST_PAGE_SIZE) {
          switch (element.listKind) {
            case 'modules':
              return element.entries.map((entry, idx) => createConfigModuleNode(element, entry, idx));
            case 'sourceRoots':
              return element.entries.map((entry, idx) => createConfigSourceRootNode(element, entry, idx));
            case 'classpath':
              return element.entries.map((entry, idx) => createConfigClasspathEntryNode(element, entry, idx));
            case 'modulePath':
              return element.entries.map((entry, idx) => createConfigModulePathEntryNode(element, entry, idx));
            case 'outputDirs':
              return element.entries.map((entry, idx) => createConfigOutputDirNode(element, entry, idx));
            case 'dependencies':
              return element.entries.map((entry, idx) => createConfigDependencyNode(element, entry, idx));
          }
        }

        const entries = element.entries;
        const chunks: NovaProjectExplorerNode[] = [];
        for (let start = 0; start < entries.length; start += CONFIG_LIST_PAGE_SIZE) {
          const end = Math.min(entries.length, start + CONFIG_LIST_PAGE_SIZE);
          chunks.push({
            type: 'configChunk',
            id: `${element.id}:chunk:${start}-${end}`,
            label: `Entries ${start + 1}\u2013${end}`,
            workspace: element.workspace,
            baseDir: element.baseDir,
            listKind: element.listKind,
            entries,
            start,
            end,
          });
        }
        return chunks;
      }

      case 'configChunk': {
        const slice = element.entries.slice(element.start, element.end);
        switch (element.listKind) {
          case 'modules':
            return slice.map((entry, idx) =>
              createConfigModuleNode(element, entry as ConfigurationModuleEntry, element.start + idx),
            );
          case 'sourceRoots':
            return slice.map((entry, idx) =>
              createConfigSourceRootNode(element, entry as ConfigurationSourceRootEntry, element.start + idx),
            );
          case 'classpath':
            return slice.map((entry, idx) =>
              createConfigClasspathEntryNode(element, entry as ConfigurationClasspathEntry, element.start + idx),
            );
          case 'modulePath':
            return slice.map((entry, idx) =>
              createConfigModulePathEntryNode(element, entry as ConfigurationModulePathEntry, element.start + idx),
            );
          case 'outputDirs':
            return slice.map((entry, idx) =>
              createConfigOutputDirNode(element, entry as ConfigurationOutputDirEntry, element.start + idx),
            );
          case 'dependencies':
            return slice.map((entry, idx) =>
              createConfigDependencyNode(element, entry as ConfigurationDependencyEntry, element.start + idx),
            );
        }
      }

      case 'workspaceInfo':
      case 'unitInfo':
      case 'path':
      case 'message':
        return [];
    }
  }

  getTreeItem(element: NovaProjectExplorerNode): vscode.TreeItem {
    switch (element.type) {
      case 'workspace': {
        const item = new vscode.TreeItem(element.workspace.name, vscode.TreeItemCollapsibleState.Collapsed);
        item.id = element.id;
        item.description = element.workspace.uri.fsPath;
        item.tooltip = element.workspace.uri.fsPath;
        item.contextValue = CONTEXT_WORKSPACE;
        item.iconPath = vscode.ThemeIcon.Folder;
        return item;
      }

      case 'workspaceConfiguration': {
        const item = new vscode.TreeItem('Project Configuration', vscode.TreeItemCollapsibleState.Collapsed);
        item.id = element.id;
        item.iconPath = new vscode.ThemeIcon('gear');
        return item;
      }

      case 'workspaceInfo': {
        const item = new vscode.TreeItem(element.label, vscode.TreeItemCollapsibleState.None);
        item.id = element.id;
        item.description = element.description;
        item.tooltip = element.description ? `${element.label}\n${element.description}` : element.label;
        item.iconPath = new vscode.ThemeIcon('info');
        return item;
      }

      case 'unit': {
        const item = new vscode.TreeItem(unitLabel(element.unit), vscode.TreeItemCollapsibleState.Collapsed);
        item.id = element.id;
        item.description = formatBuildSystemKind(element.unit.kind);
        item.contextValue = CONTEXT_UNIT;
        item.tooltip = `${formatBuildSystemKind(element.unit.kind)} unit\n${element.projectRoot}`;
        item.iconPath = new vscode.ThemeIcon('project');
        return item;
      }

      case 'unitInfo': {
        const item = new vscode.TreeItem(element.label, vscode.TreeItemCollapsibleState.None);
        item.id = element.id;
        item.description = element.description;
        item.tooltip = element.description ? `${element.label}\n${element.description}` : element.label;
        item.iconPath = new vscode.ThemeIcon('symbol-property');
        return item;
      }

      case 'configGroup': {
        const count = element.entries.length;
        const item = new vscode.TreeItem(
          element.label,
          count > 0 ? vscode.TreeItemCollapsibleState.Collapsed : vscode.TreeItemCollapsibleState.None,
        );
        item.id = element.id;
        switch (element.listKind) {
          case 'modules':
            item.iconPath = new vscode.ThemeIcon('project');
            break;
          case 'sourceRoots':
            item.iconPath = vscode.ThemeIcon.Folder;
            break;
          case 'classpath':
            item.iconPath = new vscode.ThemeIcon('library');
            break;
          case 'modulePath':
            item.iconPath = new vscode.ThemeIcon('folder-library');
            break;
          case 'outputDirs':
            item.iconPath = vscode.ThemeIcon.Folder;
            break;
          case 'dependencies':
            item.iconPath = new vscode.ThemeIcon('package');
            break;
        }
        return item;
      }

      case 'configChunk': {
        const item = new vscode.TreeItem(element.label, vscode.TreeItemCollapsibleState.Collapsed);
        item.id = element.id;
        item.iconPath = new vscode.ThemeIcon('list-unordered');
        return item;
      }

      case 'group': {
        const count = element.entries.length;
        const item = new vscode.TreeItem(
          element.label,
          count > 0 ? vscode.TreeItemCollapsibleState.Collapsed : vscode.TreeItemCollapsibleState.None,
        );
        item.id = element.id;
        item.iconPath =
          element.listKind === 'sourceRoots'
            ? vscode.ThemeIcon.Folder
            : element.listKind === 'classpath'
              ? new vscode.ThemeIcon('library')
              : new vscode.ThemeIcon('folder-library');
        return item;
      }

      case 'chunk': {
        const item = new vscode.TreeItem(element.label, vscode.TreeItemCollapsibleState.Collapsed);
        item.id = element.id;
        item.iconPath = new vscode.ThemeIcon('list-unordered');
        return item;
      }

      case 'path': {
        const item = new vscode.TreeItem(element.label, vscode.TreeItemCollapsibleState.None);
        item.id = element.id;
        item.description = element.description;
        item.tooltip = element.description ? `${element.label}\n${element.description}` : element.label;
        if (element.uri) {
          item.resourceUri = element.uri;
        }
        if (element.command) {
          item.command = element.command;
        }
        if (element.contextValue) {
          item.contextValue = element.contextValue;
        } else if (element.uri) {
          item.contextValue = CONTEXT_PATH;
        }
        item.iconPath = element.icon ?? item.iconPath;
        return item;
      }

      case 'message': {
        const item = new vscode.TreeItem(element.label, vscode.TreeItemCollapsibleState.None);
        item.id = element.id;
        item.description = element.description;
        item.tooltip = element.description ? `${element.label}\n${element.description}` : element.label;
        item.iconPath = element.icon ?? new vscode.ThemeIcon('warning');
        if (element.command) {
          item.command = element.command;
        }
        return item;
      }
    }
  }

  private isProjectModelSupported(): boolean {
    const workspaces = vscode.workspace.workspaceFolders ?? [];
    if (workspaces.length === 0) {
      return true;
    }

    return workspaces.some((workspace) => this.isProjectModelSupportedForWorkspace(workspace));
  }

  private isProjectModelSupportedForWorkspace(workspace: vscode.WorkspaceFolder): boolean {
    if (this.cache.isProjectModelUnsupported(workspace)) {
      return false;
    }

    const method = 'nova/projectModel';
    const supported = isNovaRequestSupported(workspace.uri.toString(), method);
    // Default to optimistic behaviour when capability lists are unavailable (unless we already
    // observed a method-not-found error for this workspace's server).
    return supported !== false;
  }

  private async triggerUnsupportedWelcome(): Promise<void> {
    const serverRunning = this.isServerRunning();
    const projectModelSupported = serverRunning ? this.isProjectModelSupported() : true;
    await this.setContexts({ serverRunning, projectModelSupported });

    // Re-render the root so VS Code can show the viewsWelcome unsupported-state guidance.
    this.emitter.fire(undefined);
  }

  private async setContexts(opts: { serverRunning: boolean; projectModelSupported: boolean }): Promise<void> {
    if (this.lastContextServerRunning !== opts.serverRunning) {
      this.lastContextServerRunning = opts.serverRunning;
      await vscode.commands.executeCommand('setContext', 'nova.frameworks.serverRunning', opts.serverRunning);
    }

    if (this.lastContextProjectModelSupported !== opts.projectModelSupported) {
      this.lastContextProjectModelSupported = opts.projectModelSupported;
      await vscode.commands.executeCommand(
        'setContext',
        'nova.projectExplorer.projectModelSupported',
        opts.projectModelSupported,
      );
    }
  }
}

function createSourceRootNode(
  parent: { id: string; workspace: vscode.WorkspaceFolder; projectRoot: string },
  root: string,
  idx: number,
  opts?: { projectRoot?: string },
): NovaProjectExplorerNode {
  const baseDir = opts?.projectRoot ?? parent.projectRoot;
  const resolved = resolvePossiblyRelativePath(baseDir, root);
  const uri = resolved ? vscode.Uri.file(resolved) : undefined;

  return {
    type: 'path',
    id: `${parent.id}:root:${idx}:${root}`,
    label: root,
    description: resolved && resolved !== root ? resolved : undefined,
    uri,
    icon: vscode.ThemeIcon.Folder,
    command: uri
      ? {
          title: 'Reveal Source Root',
          command: COMMAND_REVEAL_PATH,
          arguments: [uri],
        }
      : undefined,
  };
}

function createClasspathEntryNode(
  parent: { id: string; projectRoot: string },
  entry: string,
  idx: number,
  _opts?: { sliceLabel?: string },
): NovaProjectExplorerNode {
  const label = path.basename(entry) || entry;
  const resolved = resolvePossiblyRelativePath(parent.projectRoot, entry);
  const uri = resolved ? vscode.Uri.file(resolved) : undefined;

  return {
    type: 'path',
    id: `${parent.id}:entry:${idx}:${entry}`,
    label,
    description: resolved && resolved !== entry ? resolved : entry,
    uri,
    icon: new vscode.ThemeIcon('symbol-file'),
    command: uri
      ? {
          title: 'Reveal Path',
          command: COMMAND_REVEAL_PATH,
          arguments: [uri],
        }
      : undefined,
  };
}

function createConfigOutputDirNode(
  parent: { id: string; workspace: vscode.WorkspaceFolder; baseDir?: string },
  entry: ConfigurationOutputDirEntry,
  idx: number,
): NovaProjectExplorerNode {
  const rawPath = typeof entry.path === 'string' ? entry.path : '';
  const baseDir = parent.baseDir ?? parent.workspace.uri.fsPath;
  const resolved = rawPath ? resolvePossiblyRelativePath(baseDir, rawPath) : '';
  const uri = resolved ? vscode.Uri.file(resolved) : undefined;

  const kind = typeof entry.kind === 'string' && entry.kind.trim().length > 0 ? entry.kind.trim() : 'output';
  const baseName = resolved ? path.basename(resolved) : '';
  const label = baseName && baseName !== kind ? `${kind}: ${baseName}` : kind;

  return {
    type: 'path',
    id: `${parent.id}:output:${idx}:${rawPath}`,
    label,
    description: resolved || rawPath || undefined,
    uri,
    icon: vscode.ThemeIcon.Folder,
    command: uri
      ? {
          title: 'Reveal Output Dir',
          command: COMMAND_REVEAL_PATH,
          arguments: [uri],
        }
      : undefined,
  };
}

function createConfigModuleNode(
  parent: { id: string; workspace: vscode.WorkspaceFolder; baseDir?: string },
  entry: ConfigurationModuleEntry,
  idx: number,
): NovaProjectExplorerNode {
  const name = typeof entry.name === 'string' && entry.name.trim().length > 0 ? entry.name.trim() : `Module ${idx + 1}`;
  const rawRoot = typeof entry.root === 'string' ? entry.root : '';
  const baseDir = parent.baseDir ?? parent.workspace.uri.fsPath;
  const resolved = rawRoot ? resolvePossiblyRelativePath(baseDir, rawRoot) : '';
  const uri = resolved ? vscode.Uri.file(resolved) : undefined;

  return {
    type: 'path',
    id: `${parent.id}:module:${idx}:${rawRoot || name}`,
    label: name,
    description: resolved || rawRoot || undefined,
    uri,
    icon: new vscode.ThemeIcon('project'),
    command: uri
      ? {
          title: 'Reveal Module Root',
          command: COMMAND_REVEAL_PATH,
          arguments: [uri],
        }
      : undefined,
  };
}

function createConfigSourceRootNode(
  parent: { id: string; workspace: vscode.WorkspaceFolder; baseDir?: string },
  entry: ConfigurationSourceRootEntry,
  idx: number,
): NovaProjectExplorerNode {
  const rawPath = typeof entry.path === 'string' ? entry.path : '';
  const baseDir = parent.baseDir ?? parent.workspace.uri.fsPath;
  const resolved = rawPath ? resolvePossiblyRelativePath(baseDir, rawPath) : '';
  const uri = resolved ? vscode.Uri.file(resolved) : undefined;

  const kind = typeof entry.kind === 'string' && entry.kind.trim().length > 0 ? entry.kind.trim() : undefined;
  const origin = typeof entry.origin === 'string' && entry.origin.trim().length > 0 ? entry.origin.trim() : undefined;

  const metaParts: string[] = [];
  if (kind) {
    metaParts.push(`kind=${kind}`);
  }
  if (origin) {
    metaParts.push(`origin=${origin}`);
  }
  const meta = metaParts.join(', ');

  const descriptionParts: string[] = [];
  if (resolved && resolved !== rawPath) {
    descriptionParts.push(resolved);
  }
  if (meta) {
    descriptionParts.push(meta);
  }

  return {
    type: 'path',
    id: `${parent.id}:sourceRoot:${idx}:${rawPath}`,
    label: rawPath || (resolved ? path.basename(resolved) : `Source Root ${idx + 1}`),
    description: descriptionParts.join(' • ') || meta || (resolved || undefined),
    uri,
    icon: vscode.ThemeIcon.Folder,
    command: uri
      ? {
          title: 'Reveal Source Root',
          command: COMMAND_REVEAL_PATH,
          arguments: [uri],
        }
      : undefined,
  };
}

function createConfigClasspathEntryNode(
  parent: { id: string; workspace: vscode.WorkspaceFolder; baseDir?: string },
  entry: ConfigurationClasspathEntry,
  idx: number,
): NovaProjectExplorerNode {
  const rawPath = typeof entry.path === 'string' ? entry.path : '';
  const baseDir = parent.baseDir ?? parent.workspace.uri.fsPath;
  const resolved = rawPath ? resolvePossiblyRelativePath(baseDir, rawPath) : '';
  const uri = resolved ? vscode.Uri.file(resolved) : undefined;

  const kind = typeof entry.kind === 'string' && entry.kind.trim().length > 0 ? entry.kind.trim() : 'classpath';
  const baseName = resolved ? path.basename(resolved) : rawPath ? path.basename(rawPath) : '';
  const label = baseName && baseName !== kind ? `${kind}: ${baseName}` : kind;

  return {
    type: 'path',
    id: `${parent.id}:classpath:${idx}:${rawPath}`,
    label,
    description: resolved || rawPath || undefined,
    uri,
    icon: new vscode.ThemeIcon('symbol-file'),
    command: uri
      ? {
          title: 'Reveal Classpath Entry',
          command: COMMAND_REVEAL_PATH,
          arguments: [uri],
        }
      : undefined,
  };
}

function createConfigModulePathEntryNode(
  parent: { id: string; workspace: vscode.WorkspaceFolder; baseDir?: string },
  entry: ConfigurationModulePathEntry,
  idx: number,
): NovaProjectExplorerNode {
  const rawPath = typeof entry.path === 'string' ? entry.path : '';
  const baseDir = parent.baseDir ?? parent.workspace.uri.fsPath;
  const resolved = rawPath ? resolvePossiblyRelativePath(baseDir, rawPath) : '';
  const uri = resolved ? vscode.Uri.file(resolved) : undefined;

  const kind = typeof entry.kind === 'string' && entry.kind.trim().length > 0 ? entry.kind.trim() : 'modulePath';
  const baseName = resolved ? path.basename(resolved) : rawPath ? path.basename(rawPath) : '';
  const label = baseName && baseName !== kind ? `${kind}: ${baseName}` : kind;

  return {
    type: 'path',
    id: `${parent.id}:modulePath:${idx}:${rawPath}`,
    label,
    description: resolved || rawPath || undefined,
    uri,
    icon: new vscode.ThemeIcon('symbol-file'),
    command: uri
      ? {
          title: 'Reveal Module Path Entry',
          command: COMMAND_REVEAL_PATH,
          arguments: [uri],
        }
      : undefined,
  };
}

function createConfigDependencyNode(
  parent: { id: string },
  entry: ConfigurationDependencyEntry,
  idx: number,
): NovaProjectExplorerNode {
  const groupId = typeof entry.groupId === 'string' ? entry.groupId : '';
  const artifactId = typeof entry.artifactId === 'string' ? entry.artifactId : '';
  const scope = typeof entry.scope === 'string' ? entry.scope : '';

  const label =
    groupId && artifactId ? `${groupId}:${artifactId}` : artifactId || groupId || `Dependency ${idx + 1}`;

  return {
    type: 'path',
    id: `${parent.id}:dep:${idx}:${label}`,
    label,
    description: scope || undefined,
    icon: new vscode.ThemeIcon('package'),
  };
}

function unitLabel(unit: ProjectModelUnit): string {
  switch (unit.kind) {
    case 'maven':
      return unit.module;
    case 'gradle':
      return unit.projectPath;
    case 'bazel':
      return unit.target;
    case 'simple':
      return unit.module;
  }
}

function unitId(unit: ProjectModelUnit): string {
  return `${unit.kind}:${unitLabel(unit)}`;
}

function summarizeBuildSystemLabel(units: readonly ProjectModelUnit[]): string | null {
  if (units.length === 0) {
    return null;
  }

  const kinds = new Set(units.map((u) => u.kind));
  if (kinds.size === 1) {
    return formatBuildSystemKind(units[0].kind);
  }

  return 'Mixed';
}

function summarizeJavaLanguageLevelLabel(units: readonly ProjectModelUnit[]): string | null {
  const first = units[0]?.languageLevel;
  if (!first) {
    return null;
  }

  for (const unit of units) {
    const ll = unit.languageLevel;
    if (!ll) {
      return null;
    }
    if (ll.source !== first.source || ll.target !== first.target || ll.release !== first.release) {
      return 'varies by unit';
    }
  }

  return formatJavaLanguageLevel(first);
}

function formatBuildSystemKind(kind: BuildSystemKind): string {
  switch (kind) {
    case 'maven':
      return 'Maven';
    case 'gradle':
      return 'Gradle';
    case 'bazel':
      return 'Bazel';
    case 'simple':
      return 'Simple';
  }
}

function formatJavaLanguageLevel(level: JavaLanguageLevel): string {
  const source = level.source || '—';
  const target = level.target || '—';
  const release = level.release ?? '—';
  return `source=${source}, target=${target}, release=${release}`;
}

async function revealPath(uri: vscode.Uri): Promise<void> {
  try {
    await vscode.commands.executeCommand('revealInExplorer', uri);
  } catch {
    // Best-effort: fallback to OS file explorer.
    try {
      await vscode.commands.executeCommand('revealFileInOS', uri);
    } catch {
      // ignore
    }
  }
}

async function copyPath(arg: unknown): Promise<void> {
  const text = extractPathText(arg);
  if (!text) {
    void vscode.window.showErrorMessage('Nova: no path selected.');
    return;
  }
  await vscode.env.clipboard.writeText(text);
  void vscode.window.setStatusBarMessage('Nova: copied path to clipboard', 2000);
}

function extractPathText(arg: unknown): string | undefined {
  const asFolder = asWorkspaceFolder(arg);
  if (asFolder) {
    return asFolder.uri.fsPath;
  }

  if (arg instanceof vscode.Uri) {
    return arg.fsPath || arg.toString();
  }

  if (!arg || typeof arg !== 'object') {
    return undefined;
  }

  const projectRoot = (arg as { projectRoot?: unknown }).projectRoot;
  if (typeof projectRoot === 'string' && projectRoot.trim().length > 0) {
    return projectRoot.trim();
  }

  const workspace = (arg as { workspace?: unknown }).workspace;
  const asNestedFolder = asWorkspaceFolder(workspace);
  if (asNestedFolder) {
    return asNestedFolder.uri.fsPath;
  }

  const uri = (arg as { uri?: unknown }).uri;
  if (uri instanceof vscode.Uri) {
    return uri.fsPath || uri.toString();
  }

  const resourceUri = (arg as { resourceUri?: unknown }).resourceUri;
  if (resourceUri instanceof vscode.Uri) {
    return resourceUri.fsPath || resourceUri.toString();
  }

  const description = (arg as { description?: unknown }).description;
  if (typeof description === 'string' && description.trim().length > 0) {
    return description.trim();
  }

  const label = (arg as { label?: unknown }).label;
  if (typeof label === 'string' && label.trim().length > 0) {
    return label.trim();
  }

  return undefined;
}

function extractUri(arg: unknown): vscode.Uri | undefined {
  if (arg instanceof vscode.Uri) {
    return arg;
  }

  const asFolder = asWorkspaceFolder(arg);
  if (asFolder) {
    return asFolder.uri;
  }

  if (!arg || typeof arg !== 'object') {
    return undefined;
  }

  const uri = (arg as { uri?: unknown }).uri;
  if (uri instanceof vscode.Uri) {
    return uri;
  }

  const resourceUri = (arg as { resourceUri?: unknown }).resourceUri;
  if (resourceUri instanceof vscode.Uri) {
    return resourceUri;
  }

  const workspace = (arg as { workspace?: unknown }).workspace;
  const asNestedFolder = asWorkspaceFolder(workspace);
  if (asNestedFolder) {
    return asNestedFolder.uri;
  }

  const projectRoot = (arg as { projectRoot?: unknown }).projectRoot;
  if (typeof projectRoot === 'string' && projectRoot.trim().length > 0) {
    return vscode.Uri.file(projectRoot.trim());
  }

  return undefined;
}

async function showProjectModel(cache: ProjectModelCache, request: NovaRequest, arg?: unknown): Promise<void> {
  const workspace = getWorkspaceFolderFromCommandArg(arg) ?? (await pickWorkspaceFolder('Select workspace folder for project model'));
  if (!workspace) {
    return;
  }

  try {
    if (cache.isProjectModelUnsupported(workspace)) {
      void vscode.window.showInformationMessage(formatUnsupportedNovaMethodMessage('nova/projectModel'));
      return;
    }

    const model = await vscode.window.withProgress<ProjectModelResult | undefined>(
      {
        location: vscode.ProgressLocation.Notification,
        title: `Nova: Loading project model (${workspace.name})…`,
        cancellable: true,
      },
      async (_progress, token) => {
        return await request<ProjectModelResult>('nova/projectModel', { projectRoot: workspace.uri.fsPath }, { token });
      },
    );
    if (!model) {
      // Cancellation returns `undefined`. Unsupported methods are reported by `sendNovaRequest`.
      return;
    }
    await openJsonDocument(`Nova Project Model (${workspace.name}).json`, model);
  } catch (err) {
    if (isSafeModeError(err)) {
      const picked = await vscode.window.showWarningMessage(
        'Nova: nova-lsp is running in safe mode. Project model requests are unavailable.',
        'Generate Bug Report',
      );
      if (picked === 'Generate Bug Report') {
        await vscode.commands.executeCommand('nova.bugReport');
      }
      return;
    }
    if (cache.isProjectModelUnsupported(workspace) || isNovaMethodNotFoundError(err)) {
      void vscode.window.showInformationMessage(formatUnsupportedNovaMethodMessage('nova/projectModel'));
      return;
    }
    void vscode.window.showErrorMessage(`Nova: failed to fetch project model: ${formatError(err)}`);
  }
}

async function showProjectConfiguration(
  cache: ProjectModelCache,
  request: NovaRequest,
  arg?: unknown,
): Promise<void> {
  const workspace =
    getWorkspaceFolderFromCommandArg(arg) ?? (await pickWorkspaceFolder('Select workspace folder for project configuration'));
  if (!workspace) {
    return;
  }

  try {
    if (cache.isProjectConfigurationUnsupported(workspace)) {
      void vscode.window.showInformationMessage(formatUnsupportedNovaMethodMessage('nova/projectConfiguration'));
      return;
    }

    const config = await vscode.window.withProgress<ProjectConfigurationResponse | undefined>(
      {
        location: vscode.ProgressLocation.Notification,
        title: `Nova: Loading project configuration (${workspace.name})…`,
        cancellable: true,
      },
      async (_progress, token) => {
        return await request<ProjectConfigurationResponse>(
          'nova/projectConfiguration',
          { projectRoot: workspace.uri.fsPath },
          { token },
        );
      },
    );
    if (!config) {
      // Cancellation returns `undefined`. Unsupported methods are reported by `sendNovaRequest`.
      return;
    }
    await openJsonDocument(`Nova Project Configuration (${workspace.name}).json`, config);
  } catch (err) {
    if (isSafeModeError(err)) {
      const picked = await vscode.window.showWarningMessage(
        'Nova: nova-lsp is running in safe mode. Project configuration requests are unavailable.',
        'Generate Bug Report',
      );
      if (picked === 'Generate Bug Report') {
        await vscode.commands.executeCommand('nova.bugReport');
      }
      return;
    }
    if (cache.isProjectConfigurationUnsupported(workspace) || isNovaMethodNotFoundError(err)) {
      void vscode.window.showInformationMessage(formatUnsupportedNovaMethodMessage('nova/projectConfiguration'));
      return;
    }
    void vscode.window.showErrorMessage(`Nova: failed to fetch project configuration: ${formatError(err)}`);
  }
}

async function pickWorkspaceFolder(placeHolder: string): Promise<vscode.WorkspaceFolder | undefined> {
  const folders = vscode.workspace.workspaceFolders ?? [];
  if (folders.length === 0) {
    void vscode.window.showErrorMessage('Nova: Open a workspace folder first.');
    return undefined;
  }
  if (folders.length === 1) {
    return folders[0];
  }

  const picked = await vscode.window.showQuickPick(
    folders.map((folder) => ({
      label: folder.name,
      description: folder.uri.fsPath,
      folder,
    })),
    { placeHolder },
  );
  return picked?.folder;
}

function getWorkspaceFolderFromCommandArg(arg: unknown): vscode.WorkspaceFolder | undefined {
  const asFolder = asWorkspaceFolder(arg);
  if (asFolder) {
    return asFolder;
  }

  if (arg && typeof arg === 'object') {
    const workspace = (arg as { workspace?: unknown }).workspace;
    const asNested = asWorkspaceFolder(workspace);
    if (asNested) {
      return asNested;
    }
  }

  return undefined;
}

function asWorkspaceFolder(value: unknown): vscode.WorkspaceFolder | undefined {
  if (!value || typeof value !== 'object') {
    return undefined;
  }
  const uri = (value as { uri?: unknown }).uri;
  if (!uri || typeof uri !== 'object') {
    return undefined;
  }
  const fsPath = (uri as { fsPath?: unknown }).fsPath;
  if (typeof fsPath !== 'string' || fsPath.trim().length === 0) {
    return undefined;
  }
  return value as vscode.WorkspaceFolder;
}

async function openJsonDocument(name: string, payload: unknown): Promise<void> {
  const sanitizedName = sanitizeUntitledName(name);
  const uri = vscode.Uri.parse(`untitled:${sanitizedName}`);
  const doc = await vscode.workspace.openTextDocument(uri);
  const editor = await vscode.window.showTextDocument(doc, { preview: false });
  await vscode.languages.setTextDocumentLanguage(doc, 'json');

  const text = JSON.stringify(payload, null, 2);
  await editor.edit((builder) => {
    // Replace full document content (the untitled document may already be open from a prior run).
    const end = doc.positionAt(doc.getText().length);
    builder.replace(new vscode.Range(new vscode.Position(0, 0), end), text);
  });
}

function sanitizeUntitledName(value: string): string {
  // Untitled URIs treat the path segment as a file name; keep it readable.
  return value.replace(/[\\/]/g, '-');
}

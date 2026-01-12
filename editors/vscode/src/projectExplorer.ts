import * as vscode from 'vscode';
import * as path from 'node:path';
import { resolvePossiblyRelativePath } from './pathUtils';
import { formatUnsupportedNovaMethodMessage, isNovaMethodNotFoundError } from './novaCapabilities';
import { formatError } from './safeMode';
import { ProjectModelCache } from './projectModelCache';

export type NovaRequest = <R>(method: string, params?: unknown) => Promise<R | undefined>;

type BuildSystemKind = 'maven' | 'gradle' | 'bazel' | 'simple';

interface JavaLanguageLevel {
  source: string;
  target: string;
  release: string | null;
}

interface BaseProjectModelUnit {
  kind: BuildSystemKind;
  compileClasspath: string[];
  modulePath: string[];
  sourceRoots: string[];
  languageLevel: JavaLanguageLevel;
}

interface MavenProjectModelUnit extends BaseProjectModelUnit {
  kind: 'maven';
  module: string;
}

interface GradleProjectModelUnit extends BaseProjectModelUnit {
  kind: 'gradle';
  projectPath: string;
}

interface BazelProjectModelUnit extends BaseProjectModelUnit {
  kind: 'bazel';
  target: string;
}

interface SimpleProjectModelUnit extends BaseProjectModelUnit {
  kind: 'simple';
  module: string;
}

type ProjectModelUnit = MavenProjectModelUnit | GradleProjectModelUnit | BazelProjectModelUnit | SimpleProjectModelUnit;

interface ProjectModelResult {
  projectRoot: string;
  units: ProjectModelUnit[];
}

type ListKind = 'sourceRoots' | 'classpath' | 'modulePath';

type NovaProjectExplorerNode =
  | { type: 'message'; id: string; label: string; description?: string; icon?: vscode.ThemeIcon; command?: vscode.Command }
  | { type: 'workspace'; id: string; workspace: vscode.WorkspaceFolder }
  | { type: 'workspaceInfo'; id: string; label: string; description?: string }
  | { type: 'unit'; id: string; workspace: vscode.WorkspaceFolder; projectRoot: string; unit: ProjectModelUnit }
  | { type: 'unitInfo'; id: string; label: string; description?: string }
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

const COMMAND_REFRESH = 'nova.refreshProjectExplorer';
const COMMAND_SHOW_MODEL = 'nova.showProjectModel';
const COMMAND_SHOW_CONFIG = 'nova.showProjectConfiguration';
const COMMAND_REVEAL_PATH = 'nova.projectExplorer.revealPath';

const CLASS_PATH_PAGE_SIZE = 200;

export function registerNovaProjectExplorer(
  context: vscode.ExtensionContext,
  request: NovaRequest,
  cache?: ProjectModelCache,
): void {
  const projectModelCache = cache ?? new ProjectModelCache(request);
  const provider = new NovaProjectExplorerProvider(projectModelCache);

  const view = vscode.window.createTreeView(VIEW_ID, {
    treeDataProvider: provider,
    showCollapseAll: true,
  });

  context.subscriptions.push(view);

  context.subscriptions.push(
    vscode.commands.registerCommand(COMMAND_REFRESH, (workspace?: vscode.WorkspaceFolder) => {
      provider.refresh({ forceRefresh: true, workspace });
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(COMMAND_SHOW_MODEL, async () => {
      await showProjectModel(projectModelCache);
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(COMMAND_SHOW_CONFIG, async () => {
      await showProjectConfiguration(projectModelCache);
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(COMMAND_REVEAL_PATH, async (uri: vscode.Uri) => {
      await revealPath(uri);
    }),
  );

  context.subscriptions.push(
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      provider.refresh({ clearCache: true });
    }),
  );
}

class NovaProjectExplorerProvider implements vscode.TreeDataProvider<NovaProjectExplorerNode> {
  private readonly emitter = new vscode.EventEmitter<NovaProjectExplorerNode | undefined | null>();
  constructor(private readonly cache: ProjectModelCache) {}

  get onDidChangeTreeData(): vscode.Event<NovaProjectExplorerNode | undefined | null> {
    return this.emitter.event;
  }

  refresh(opts?: { clearCache?: boolean; forceRefresh?: boolean; workspace?: vscode.WorkspaceFolder }): void {
    if (opts?.clearCache) {
      this.cache.clear(opts.workspace);
    }

    const folders = opts?.workspace ? [opts.workspace] : (vscode.workspace.workspaceFolders ?? []);
    if (opts?.forceRefresh) {
      for (const folder of folders) {
        const promise = this.cache.getProjectModel(folder, { forceRefresh: true });
        void promise
          .catch(() => {})
          .finally(() => {
            this.emitter.fire(undefined);
          });
      }
    }

    // Fire after registering in-flight promises so getChildren can render loading placeholders.
    this.emitter.fire(undefined);
  }

  async getChildren(element?: NovaProjectExplorerNode): Promise<NovaProjectExplorerNode[]> {
    if (!element) {
      const workspaces = vscode.workspace.workspaceFolders ?? [];
      if (workspaces.length === 0) {
        return [
          {
            type: 'message',
            id: 'no-workspace',
            label: 'Open a workspace folder to view the Nova project model.',
          },
        ];
      }
      return workspaces.map((workspace) => ({
        type: 'workspace',
        id: `workspace:${workspace.uri.toString()}`,
        workspace,
      }));
    }

    switch (element.type) {
      case 'workspace': {
        const workspace = element.workspace;
        if (this.cache.isProjectModelUnsupported()) {
          return [
            {
              type: 'message',
              id: `${element.id}:unsupported`,
              label: 'Project model not supported by this server.',
              description: 'Update nova-lsp to a build that supports nova/projectModel.',
              icon: new vscode.ThemeIcon('info'),
            },
          ];
        }

        const snapshot = this.cache.peekProjectModel(workspace);
        const model = snapshot.value as unknown as ProjectModelResult | undefined;

        if (!model) {
          if (snapshot.inFlight) {
            return [
              {
                type: 'message',
                id: `${element.id}:loading`,
                label: 'Loading project model…',
                icon: new vscode.ThemeIcon('loading'),
              },
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

        const unitNodes = model.units.map((unit, idx) => ({
          type: 'unit' as const,
          id: `${element.id}:unit:${idx}:${unitId(unit)}`,
          workspace,
          projectRoot: model.projectRoot,
          unit,
        }));

        if (unitNodes.length === 0) {
          nodes.push(...infoNodes);
          nodes.push({
            type: 'message',
            id: `${element.id}:no-units`,
            label: 'No project units reported.',
          });
          return nodes;
        }

        nodes.push(...infoNodes, ...unitNodes);
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
        item.contextValue = CONTEXT_WORKSPACE;
        item.iconPath = vscode.ThemeIcon.Folder;
        return item;
      }

      case 'workspaceInfo': {
        const item = new vscode.TreeItem(element.label, vscode.TreeItemCollapsibleState.None);
        item.id = element.id;
        item.description = element.description;
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
        item.iconPath = new vscode.ThemeIcon('symbol-property');
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
        if (element.uri) {
          item.resourceUri = element.uri;
        }
        if (element.command) {
          item.command = element.command;
        }
        if (element.contextValue) {
          item.contextValue = element.contextValue;
        }
        item.iconPath = element.icon ?? item.iconPath;
        return item;
      }

      case 'message': {
        const item = new vscode.TreeItem(element.label, vscode.TreeItemCollapsibleState.None);
        item.id = element.id;
        item.description = element.description;
        item.iconPath = element.icon ?? new vscode.ThemeIcon('warning');
        if (element.command) {
          item.command = element.command;
        }
        return item;
      }
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

async function showProjectModel(cache: ProjectModelCache): Promise<void> {
  const workspace = await pickWorkspaceFolder('Select workspace folder for project model');
  if (!workspace) {
    return;
  }

  try {
    if (cache.isProjectModelUnsupported()) {
      void vscode.window.showInformationMessage(formatUnsupportedNovaMethodMessage('nova/projectModel'));
      return;
    }

    const model = await cache.getProjectModel(workspace);
    await openJsonDocument(`Nova Project Model (${workspace.name}).json`, model);
  } catch (err) {
    if (cache.isProjectModelUnsupported() || isNovaMethodNotFoundError(err)) {
      void vscode.window.showInformationMessage(formatUnsupportedNovaMethodMessage('nova/projectModel'));
      return;
    }
    void vscode.window.showErrorMessage(`Nova: failed to fetch project model: ${formatError(err)}`);
  }
}

async function showProjectConfiguration(cache: ProjectModelCache): Promise<void> {
  const workspace = await pickWorkspaceFolder('Select workspace folder for project configuration');
  if (!workspace) {
    return;
  }

  try {
    if (cache.isProjectConfigurationUnsupported()) {
      void vscode.window.showInformationMessage(formatUnsupportedNovaMethodMessage('nova/projectConfiguration'));
      return;
    }

    const config = await cache.getProjectConfiguration(workspace);
    await openJsonDocument(`Nova Project Configuration (${workspace.name}).json`, config);
  } catch (err) {
    if (cache.isProjectConfigurationUnsupported() || isNovaMethodNotFoundError(err)) {
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

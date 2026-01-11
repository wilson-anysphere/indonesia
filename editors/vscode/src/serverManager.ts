import { createHash } from 'node:crypto';
import { execFile } from 'node:child_process';
import * as fs from 'node:fs/promises';
import * as fsSync from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { Readable, Transform } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { promisify } from 'node:util';
import * as yauzl from 'yauzl';

export type ReleaseChannel = 'stable' | 'prerelease';

export type NovaServerSettings = {
  /** Absolute path to a user-managed nova-lsp binary. If set, downloads are skipped and this path is used. */
  path: string | null;
  /** Whether the extension should offer to download the server when missing. */
  autoDownload: boolean;
  /** Whether to use stable releases only, or allow prereleases when resolving `latest`. */
  releaseChannel: ReleaseChannel;
  /** Explicit version (e.g. `0.1.0` or `v0.1.0`) or `latest`. */
  version: string;
  /** GitHub repository URL or `owner/repo` to download releases from. */
  releaseUrl: string;
};

export type Logger = {
  appendLine(message: string): void;
};

export class ServerManager {
  private readonly fetchImpl: typeof fetch;
  private readonly platform: NodeJS.Platform;
  private readonly arch: string;
  private readonly extractor: ArchiveExtractor;

  constructor(
    private readonly storageRoot: string,
    private readonly logger?: Logger,
    opts?: {
      fetch?: typeof fetch;
      platform?: NodeJS.Platform;
      arch?: string;
      extractor?: ArchiveExtractor;
    },
  ) {
    this.fetchImpl = opts?.fetch ?? globalThis.fetch;
    this.platform = opts?.platform ?? process.platform;
    this.arch = opts?.arch ?? process.arch;
    this.extractor = opts?.extractor ?? new DefaultArchiveExtractor();
  }

  getManagedServerPath(): string {
    const binaryName = novaLspBinaryName(this.platform);
    return path.join(this.storageRoot, 'server', binaryName);
  }

  async isManagedServerInstalled(): Promise<boolean> {
    return fileExists(fs, this.getManagedServerPath());
  }

  async resolveServerPath(settings: Pick<NovaServerSettings, 'path'>): Promise<string | undefined> {
    if (settings.path) {
      if (await fileExists(fs, settings.path)) {
        return settings.path;
      }
      return undefined;
    }

    const managed = this.getManagedServerPath();
    if (await fileExists(fs, managed)) {
      return managed;
    }

    return undefined;
  }

  async installOrUpdate(settings: NovaServerSettings): Promise<{ path: string; version: string }> {
    const repo = parseGitHubRepo(settings.releaseUrl);
    if (!repo) {
      throw new Error(`Invalid nova.server.releaseUrl: "${settings.releaseUrl}"`);
    }

    const target = detectNovaTarget({ platform: this.platform, arch: this.arch });
    const serverDir = path.join(this.storageRoot, 'server');
    await fs.mkdir(serverDir, { recursive: true });

    const finalBinaryPath = this.getManagedServerPath();
    const metadataPath = path.join(serverDir, 'nova-lsp.json');

    const desiredTag = normalizeVersionTag(settings.version);
    if (desiredTag !== 'latest') {
      const installed = await readInstalledMetadata(metadataPath);
      if (
        installed &&
        installed.version === desiredTag &&
        installed.target === target &&
        (installed.releaseApiBaseUrl ?? parseGitHubRepo(installed.releaseUrl ?? '')?.apiBaseUrl) === repo.apiBaseUrl &&
        (await fileExists(fs, finalBinaryPath))
      ) {
        this.log(`nova-lsp ${desiredTag} is already installed`);
        return { path: finalBinaryPath, version: desiredTag };
      }
    }

    const tag = await resolveTag({
      fetchImpl: this.fetchImpl,
      repo,
      releaseChannel: settings.releaseChannel,
      version: settings.version,
    });

    {
      const installed = await readInstalledMetadata(metadataPath);
      if (
        installed &&
        installed.version === tag &&
        installed.target === target &&
        (installed.releaseApiBaseUrl ?? parseGitHubRepo(installed.releaseUrl ?? '')?.apiBaseUrl) === repo.apiBaseUrl &&
        (await fileExists(fs, finalBinaryPath))
      ) {
        this.log(`nova-lsp ${tag} is already installed`);
        return { path: finalBinaryPath, version: tag };
      }
    }

    const release = await fetchReleaseByTag({ fetchImpl: this.fetchImpl, repo, tag });
    const archiveNameCandidates = candidateArchiveNames(target, this.platform);
    const archive = findFirstAssetByName(release.assets, archiveNameCandidates);
    if (!archive) {
      const available = release.assets.map((asset) => asset.name).sort().join(', ');
      throw new Error(
        `Release ${release.tag_name} is missing ${archiveNameCandidates.join(' or ')}. Available assets: ${available}`,
      );
    }

    const { name: archiveName, asset: archiveAsset } = archive;

    this.log(`Selected release ${release.tag_name} (${target}, ${archiveName})`);

    const expectedSha256 = await fetchPublishedSha256({
      fetchImpl: this.fetchImpl,
      release,
      archiveName,
    });

    const tmpArchivePath = path.join(serverDir, `${archiveName}.tmp`);
    const tmpBinaryPath = path.join(serverDir, `${novaLspBinaryName(this.platform)}.tmp`);

    await safeRm(fs, tmpArchivePath);
    await safeRm(fs, tmpBinaryPath);

    try {
      const actualSha256 = await downloadToFileAndSha256(this.fetchImpl, archiveAsset.browser_download_url, tmpArchivePath);
      if (!sha256Equal(actualSha256, expectedSha256)) {
        throw new Error(
          `Checksum mismatch for ${archiveName}: expected ${expectedSha256}, got ${actualSha256}. Refusing to install.`,
        );
      }

      await this.extractor.extractBinaryFromArchive({
        archivePath: tmpArchivePath,
        binaryName: novaLspBinaryName(this.platform),
        outputPath: tmpBinaryPath,
      });

      if (this.platform !== 'win32') {
        await fs.chmod(tmpBinaryPath, 0o755);
      }

      await safeRm(fs, finalBinaryPath);
      await fs.rename(tmpBinaryPath, finalBinaryPath);

      await fs.writeFile(
        metadataPath,
        JSON.stringify(
          {
            installedAt: new Date().toISOString(),
            version: release.tag_name,
            target,
            releaseUrl: settings.releaseUrl,
            releaseApiBaseUrl: repo.apiBaseUrl,
          },
          null,
          2,
        ),
      );
    } finally {
      await safeRm(fs, tmpArchivePath);
      await safeRm(fs, tmpBinaryPath);
    }

    this.log(`Installed nova-lsp to ${finalBinaryPath}`);
    return { path: finalBinaryPath, version: release.tag_name };
  }

  async getServerVersion(serverPath: string): Promise<string> {
    const execFileAsync = promisify(execFile);
    const { stdout, stderr } = await execFileAsync(serverPath, ['--version'], { timeout: 10_000 });
    const output = `${stdout ?? ''}${stderr ?? ''}`.trim();
    return output.length > 0 ? output : '(no output)';
  }

  private log(message: string): void {
    this.logger?.appendLine(message);
  }
}

export type GitHubRepo = { owner: string; repo: string; apiBaseUrl: string };

type GitHubReleaseAsset = {
  name: string;
  browser_download_url: string;
};

type GitHubRelease = {
  tag_name: string;
  assets: GitHubReleaseAsset[];
};

type InstalledServerMetadata = {
  installedAt?: string;
  version?: string;
  target?: string;
  releaseUrl?: string;
  releaseApiBaseUrl?: string;
};

export function parseGitHubRepo(input: string): GitHubRepo | undefined {
  const trimmed = input.trim().replace(/\/+$/, '');
  if (!trimmed) {
    return undefined;
  }

  const repoMatch = /^(?<owner>[^/]+)\/(?<repo>[^/]+)$/.exec(trimmed);
  if (repoMatch?.groups?.owner && repoMatch.groups.repo) {
    const owner = repoMatch.groups.owner;
    const repo = stripDotGit(repoMatch.groups.repo);
    return { owner, repo, apiBaseUrl: `https://api.github.com/repos/${owner}/${repo}` };
  }

  try {
    const url = new URL(trimmed);
    const parts = url.pathname.replace(/^\//, '').split('/').filter(Boolean);
    if (url.hostname === 'github.com') {
      if (parts.length < 2) {
        return undefined;
      }
      const [owner, rawRepo] = parts;
      const repo = stripDotGit(rawRepo);
      return { owner, repo, apiBaseUrl: `https://api.github.com/repos/${owner}/${repo}` };
    }

    const reposIndex = parts.indexOf('repos');
    if (reposIndex >= 0 && parts.length >= reposIndex + 3) {
      const owner = parts[reposIndex + 1];
      const repo = stripDotGit(parts[reposIndex + 2]);
      const apiBase = `${url.origin}/${parts.slice(0, reposIndex + 3).join('/')}`;
      return { owner, repo, apiBaseUrl: apiBase };
    }

    return undefined;
  } catch {
    return undefined;
  }
}

function stripDotGit(repo: string): string {
  return repo.endsWith('.git') ? repo.slice(0, -4) : repo;
}

export function detectNovaTarget(info: { platform: NodeJS.Platform; arch: string }): string {
  const { platform, arch } = info;
  if (platform === 'darwin') {
    if (arch === 'arm64') {
      return 'aarch64-apple-darwin';
    }
    if (arch === 'x64') {
      return 'x86_64-apple-darwin';
    }
  }
  if (platform === 'linux') {
    if (arch === 'x64') {
      return 'x86_64-unknown-linux-gnu';
    }
  }
  if (platform === 'win32') {
    if (arch === 'x64') {
      return 'x86_64-pc-windows-msvc';
    }
  }

  throw new Error(
    `Unsupported platform/arch: ${platform}/${arch}. Nova does not currently ship a prebuilt nova-lsp for this platform; set nova.server.path to a local nova-lsp executable instead.`,
  );
}

export function novaLspBinaryName(platform: NodeJS.Platform): string {
  return platform === 'win32' ? 'nova-lsp.exe' : 'nova-lsp';
}

export function novaLspArchiveName(target: string, platform: NodeJS.Platform): string {
  const ext = platform === 'win32' ? 'zip' : 'tar.xz';
  return `nova-lsp-${target}.${ext}`;
}

function candidateArchiveNames(target: string, platform: NodeJS.Platform): string[] {
  const primary = novaLspArchiveName(target, platform);
  if (platform !== 'win32' && primary.endsWith('.tar.xz')) {
    return [primary, primary.replace(/\.tar\.xz$/, '.tar.gz')];
  }
  return [primary];
}

function findFirstAssetByName(
  assets: GitHubReleaseAsset[],
  names: readonly string[],
): { name: string; asset: GitHubReleaseAsset } | undefined {
  for (const name of names) {
    const asset = assets.find((candidate) => candidate.name === name);
    if (asset) {
      return { name, asset };
    }
  }
  return undefined;
}

async function resolveTag(opts: {
  fetchImpl: typeof fetch;
  repo: GitHubRepo;
  releaseChannel: ReleaseChannel;
  version: string;
}): Promise<string> {
  const normalized = normalizeVersionTag(opts.version);
  if (normalized !== 'latest') {
    return normalized;
  }

  if (opts.releaseChannel === 'stable') {
    const release = await fetchJson<GitHubRelease>(opts.fetchImpl, `${opts.repo.apiBaseUrl}/releases/latest`);
    return release.tag_name;
  }

  const releases = await fetchJson<Array<GitHubRelease & { draft?: boolean; prerelease?: boolean }>>(
    opts.fetchImpl,
    `${opts.repo.apiBaseUrl}/releases?per_page=20`,
  );
  const candidate =
    releases.find((r) => !r.draft && r.prerelease) ??
    releases.find((r) => !r.draft);
  if (!candidate) {
    throw new Error(`No releases found for ${opts.repo.owner}/${opts.repo.repo}`);
  }
  return candidate.tag_name;
}

function normalizeVersionTag(version: string): string {
  const trimmed = version.trim();
  if (trimmed === 'latest') {
    return 'latest';
  }
  if (!trimmed) {
    return 'latest';
  }
  if (trimmed.startsWith('v')) {
    return trimmed;
  }
  return `v${trimmed}`;
}

async function readInstalledMetadata(filePath: string): Promise<InstalledServerMetadata | undefined> {
  try {
    const raw = await fs.readFile(filePath, 'utf8');
    const parsed = JSON.parse(raw) as InstalledServerMetadata;
    if (!parsed || typeof parsed !== 'object') {
      return undefined;
    }
    const version = typeof parsed.version === 'string' ? parsed.version : undefined;
    const target = typeof parsed.target === 'string' ? parsed.target : undefined;
    const releaseUrl = typeof parsed.releaseUrl === 'string' ? parsed.releaseUrl : undefined;
    const releaseApiBaseUrl = typeof parsed.releaseApiBaseUrl === 'string' ? parsed.releaseApiBaseUrl : undefined;
    const installedAt = typeof parsed.installedAt === 'string' ? parsed.installedAt : undefined;
    return { installedAt, version, target, releaseUrl, releaseApiBaseUrl };
  } catch {
    return undefined;
  }
}

async function fetchReleaseByTag(opts: { fetchImpl: typeof fetch; repo: GitHubRepo; tag: string }): Promise<GitHubRelease> {
  return fetchJson<GitHubRelease>(opts.fetchImpl, `${opts.repo.apiBaseUrl}/releases/tags/${encodeURIComponent(opts.tag)}`);
}

async function fetchJson<T>(fetchImpl: typeof fetch, url: string): Promise<T> {
  let resp: Response;
  try {
    resp = await fetchImpl(url, {
      headers: {
        Accept: 'application/vnd.github+json',
        'User-Agent': 'nova-vscode',
        'X-GitHub-Api-Version': '2022-11-28',
        ...githubAuthHeaders(url),
      },
      signal: abortSignalTimeout(20_000),
    });
  } catch (err) {
    if (isAbortError(err)) {
      throw new Error(`GitHub API request timed out after 20s: ${url}`);
    }
    throw err;
  }

  if (!resp.ok) {
    const extra = await readErrorBody(resp);
    throw new Error(
      `GitHub API request failed (${resp.status}${resp.statusText ? ` ${resp.statusText}` : ''}): ${url}${extra ? `\n${extra}` : ''}`,
    );
  }

  return (await resp.json()) as T;
}

async function downloadBytes(fetchImpl: typeof fetch, url: string): Promise<ArrayBuffer> {
  let resp: Response;
  try {
    resp = await fetchImpl(url, {
      headers: {
        'User-Agent': 'nova-vscode',
        ...githubAuthHeaders(url),
      },
      signal: abortSignalTimeout(30_000),
    });
  } catch (err) {
    if (isAbortError(err)) {
      throw new Error(`Download timed out after 30s: ${url}`);
    }
    throw err;
  }
  if (!resp.ok) {
    const extra = await readErrorBody(resp);
    throw new Error(
      `Download failed (${resp.status}${resp.statusText ? ` ${resp.statusText}` : ''}): ${url}${extra ? `\n${extra}` : ''}`,
    );
  }
  return await resp.arrayBuffer();
}

function sha256Equal(a: string, b: string): boolean {
  return a.trim().toLowerCase() === b.trim().toLowerCase();
}

export function parseChecksumsFile(contents: string): Map<string, string> {
  const out = new Map<string, string>();
  for (const line of contents.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) {
      continue;
    }
    const shaSumMatch = /^(?<sha>[a-fA-F0-9]{64})\s+\*?(?<name>.+)$/.exec(trimmed);
    if (shaSumMatch?.groups?.sha && shaSumMatch.groups.name) {
      const sha = shaSumMatch.groups.sha.toLowerCase();
      const name = shaSumMatch.groups.name.trim();
      out.set(name, sha);
      out.set(path.basename(name), sha);
      continue;
    }

    // `shasum -a 256` on macOS can emit this format: `SHA256 (file) = <sha>`.
    const shasumMatch = /^SHA256 \((?<name>.+)\) = (?<sha>[a-fA-F0-9]{64})$/.exec(trimmed);
    if (shasumMatch?.groups?.sha && shasumMatch.groups.name) {
      const sha = shasumMatch.groups.sha.toLowerCase();
      const name = shasumMatch.groups.name.trim();
      out.set(name, sha);
      out.set(path.basename(name), sha);
    }
  }
  return out;
}

async function fetchPublishedSha256(opts: {
  fetchImpl: typeof fetch;
  release: GitHubRelease;
  archiveName: string;
}): Promise<string> {
  const sha256AssetNames = [`${opts.archiveName}.sha256`, `${opts.archiveName}.sha256.txt`];
  for (const candidate of sha256AssetNames) {
    const asset = opts.release.assets.find((a) => a.name === candidate);
    if (!asset) {
      continue;
    }
    const text = await fetchText(opts.fetchImpl, asset.browser_download_url);
    const sha = firstSha256(text);
    if (sha) {
      return sha;
    }
  }

  const checksumFiles = ['checksums.txt', 'SHA256SUMS', 'sha256sums.txt'];
  for (const name of checksumFiles) {
    const asset = opts.release.assets.find((a) => a.name === name);
    if (!asset) {
      continue;
    }
    const text = await fetchText(opts.fetchImpl, asset.browser_download_url);
    const map = parseChecksumsFile(text);
    const sha = map.get(opts.archiveName);
    if (sha) {
      return sha;
    }
  }

  throw new Error(`No published SHA-256 checksums found for ${opts.archiveName}`);
}

async function fetchText(fetchImpl: typeof fetch, url: string): Promise<string> {
  const bytes = await downloadBytes(fetchImpl, url);
  return Buffer.from(bytes).toString('utf8');
}

function firstSha256(text: string): string | undefined {
  const match = /\b[a-fA-F0-9]{64}\b/.exec(text);
  return match ? match[0].toLowerCase() : undefined;
}

async function fileExists(fsImpl: typeof fs, filePath: string): Promise<boolean> {
  try {
    await fsImpl.stat(filePath);
    return true;
  } catch {
    return false;
  }
}

async function safeRm(fsImpl: typeof fs, filePath: string): Promise<void> {
  try {
    await fsImpl.rm(filePath, { force: true, recursive: true });
  } catch {
    // ignore
  }
}

async function downloadToFileAndSha256(fetchImpl: typeof fetch, url: string, destPath: string): Promise<string> {
  let resp: Response;
  try {
    resp = await fetchImpl(url, {
      headers: {
        'User-Agent': 'nova-vscode',
        ...githubAuthHeaders(url),
      },
      signal: abortSignalTimeout(5 * 60_000),
    });
  } catch (err) {
    if (isAbortError(err)) {
      throw new Error(`Download timed out after 5m: ${url}`);
    }
    throw err;
  }
  if (!resp.ok) {
    const extra = await readErrorBody(resp);
    throw new Error(
      `Download failed (${resp.status}${resp.statusText ? ` ${resp.statusText}` : ''}): ${url}${extra ? `\n${extra}` : ''}`,
    );
  }

  const hash = createHash('sha256');

  if (resp.body) {
    const nodeReadable = Readable.fromWeb(resp.body as unknown as globalThis.ReadableStream<Uint8Array>);
    const hasher = new Transform({
      transform(chunk: Buffer, _encoding, callback) {
        hash.update(chunk);
        callback(null, chunk);
      },
    });

    try {
      await pipeline(nodeReadable, hasher, fsSync.createWriteStream(destPath));
    } catch (err) {
      if (isAbortError(err)) {
        throw new Error(`Download timed out after 5m: ${url}`);
      }
      throw err;
    }
    return hash.digest('hex');
  }

  const bytes = await resp.arrayBuffer();
  const buf = Buffer.from(bytes);
  hash.update(buf);
  await fs.writeFile(destPath, buf);
  return hash.digest('hex');
}

function abortSignalTimeout(ms: number): AbortSignal | undefined {
  if (!Number.isFinite(ms) || ms <= 0) {
    return undefined;
  }

  const anyAbortSignal = AbortSignal as unknown as { timeout?: (ms: number) => AbortSignal };
  if (typeof anyAbortSignal.timeout === 'function') {
    return anyAbortSignal.timeout(ms);
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), ms);
  timer.unref?.();
  return controller.signal;
}

function isAbortError(err: unknown): boolean {
  if (!err || typeof err !== 'object') {
    return false;
  }
  const candidate = err as { name?: unknown; code?: unknown };
  return candidate.name === 'AbortError' || candidate.code === 'ABORT_ERR';
}

function githubAuthHeaders(url: string): Record<string, string> {
  const explicitToken = process.env.NOVA_GITHUB_TOKEN;
  if (explicitToken) {
    return { Authorization: `Bearer ${explicitToken}` };
  }

  const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
  if (!token) {
    return {};
  }

  if (!isPublicGitHubHost(url)) {
    return {};
  }

  return { Authorization: `Bearer ${token}` };
}

function isPublicGitHubHost(url: string): boolean {
  try {
    const { hostname } = new URL(url);
    return hostname === 'github.com' || hostname.endsWith('.github.com') || hostname.endsWith('githubusercontent.com');
  } catch {
    return false;
  }
}

async function readErrorBody(resp: Response): Promise<string | undefined> {
  try {
    const text = (await resp.text()).trim();
    if (!text) {
      return undefined;
    }
    let message = text;
    try {
      const json = JSON.parse(text) as { message?: unknown };
      if (typeof json?.message === 'string' && json.message.trim().length > 0) {
        message = json.message.trim();
      }
    } catch {
      // ignore
    }
    return message.length > 400 ? `${message.slice(0, 400)}…` : message;
  } catch {
    return undefined;
  }
}

type ExtractBinaryOptions = {
  archivePath: string;
  binaryName: string;
  outputPath: string;
};

export type ArchiveExtractor = {
  extractBinaryFromArchive(opts: ExtractBinaryOptions): Promise<void>;
};

class DefaultArchiveExtractor implements ArchiveExtractor {
  async extractBinaryFromArchive(opts: ExtractBinaryOptions): Promise<void> {
    if (opts.archivePath.endsWith('.zip')) {
      await extractFromZip(opts);
      return;
    }
    await extractFromTar(opts);
  }
}

async function extractFromTar(opts: ExtractBinaryOptions): Promise<void> {
  const tarArgs = opts.archivePath.endsWith('.tar.gz')
    ? ['-xzf', opts.archivePath]
    : opts.archivePath.endsWith('.tar.xz')
      ? ['-xJf', opts.archivePath]
      : undefined;
  if (!tarArgs) {
    throw new Error(`Unsupported archive type: ${opts.archivePath}`);
  }

  const tmpRoot = await fs.mkdtemp(path.join(os.tmpdir(), 'nova-lsp-'));
  try {
    const execFileAsync = promisify(execFile);
    try {
      await execFileAsync('tar', [...tarArgs, '-C', tmpRoot]);
    } catch (err) {
      const code = (err as { code?: unknown }).code;
      if (code === 'ENOENT') {
        throw new Error(
          'Failed to extract nova-lsp: the `tar` command was not found. Install `tar` (required to unpack .tar.xz releases) or set nova.server.path to a local nova-lsp binary.',
        );
      }
      throw err;
    }

    const extracted = await findFileRecursive(tmpRoot, opts.binaryName);
    if (!extracted) {
      throw new Error(`Archive did not contain ${opts.binaryName}`);
    }

    await fs.copyFile(extracted, opts.outputPath);
  } finally {
    await safeRm(fs, tmpRoot);
  }
}

async function extractFromZip(opts: ExtractBinaryOptions): Promise<void> {
  const zipfile = await new Promise<yauzl.ZipFile>((resolve, reject) => {
    yauzl.open(opts.archivePath, { lazyEntries: true }, (err, opened) => {
      if (err || !opened) {
        reject(err ?? new Error('Failed to open zip'));
        return;
      }
      resolve(opened);
    });
  });

  try {
    await new Promise<void>((resolve, reject) => {
      let found = false;

      zipfile.readEntry();
      zipfile.on('entry', (entry: yauzl.Entry) => {
        if (/\/$/.test(entry.fileName)) {
          zipfile.readEntry();
          return;
        }

        if (path.basename(entry.fileName) !== opts.binaryName) {
          zipfile.readEntry();
          return;
        }

        found = true;
        zipfile.openReadStream(entry, (err, readStream) => {
          if (err || !readStream) {
            reject(err ?? new Error('Failed to open zip entry stream'));
            return;
          }
          const writeStream = fsSync.createWriteStream(opts.outputPath, { mode: 0o755 });
          pipeline(readStream, writeStream)
            .then(() => resolve())
            .catch(reject);
        });
      });

      zipfile.on('end', () => {
        if (!found) {
          reject(new Error(`Archive did not contain ${opts.binaryName}`));
        }
      });
      zipfile.on('error', reject);
    });
  } finally {
    zipfile.close();
  }
}

async function findFileRecursive(root: string, basename: string): Promise<string | undefined> {
  const entries = await fs.readdir(root, { withFileTypes: true });
  for (const entry of entries) {
    const full = path.join(root, entry.name);
    if (entry.isDirectory()) {
      const nested = await findFileRecursive(full, basename);
      if (nested) {
        return nested;
      }
      continue;
    }
    if (entry.isFile() && entry.name === basename) {
      return full;
    }
  }
  return undefined;
}

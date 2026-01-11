import { createHash } from 'node:crypto';
import { execFile } from 'node:child_process';
import * as fs from 'node:fs/promises';
import * as fsSync from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
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

    const tag = await resolveTag({
      fetchImpl: this.fetchImpl,
      repo,
      releaseChannel: settings.releaseChannel,
      version: settings.version,
    });

    const release = await fetchReleaseByTag({ fetchImpl: this.fetchImpl, repo, tag });
    const archiveName = novaLspArchiveName(target, this.platform);
    const archiveAsset = release.assets.find((asset) => asset.name === archiveName);
    if (!archiveAsset) {
      const available = release.assets.map((asset) => asset.name).sort().join(', ');
      throw new Error(`Release ${release.tag_name} is missing ${archiveName}. Available assets: ${available}`);
    }

    this.log(`Selected release ${release.tag_name} (${target})`);

    const expectedSha256 = await fetchPublishedSha256({
      fetchImpl: this.fetchImpl,
      release,
      archiveName,
    });

    const archiveBytes = await downloadBytes(this.fetchImpl, archiveAsset.browser_download_url);
    const actualSha256 = sha256Hex(Buffer.from(archiveBytes));

    if (!sha256Equal(actualSha256, expectedSha256)) {
      throw new Error(
        `Checksum mismatch for ${archiveName}: expected ${expectedSha256}, got ${actualSha256}. Refusing to install.`,
      );
    }

    const tmpArchivePath = path.join(serverDir, `${archiveName}.tmp`);
    const tmpBinaryPath = path.join(serverDir, `${novaLspBinaryName(this.platform)}.tmp`);
    const finalBinaryPath = this.getManagedServerPath();
    const metadataPath = path.join(serverDir, 'nova-lsp.json');

    await safeRm(fs, tmpArchivePath);
    await safeRm(fs, tmpBinaryPath);

    try {
      await fs.writeFile(tmpArchivePath, new Uint8Array(archiveBytes));
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

export function parseGitHubRepo(input: string): GitHubRepo | undefined {
  const trimmed = input.trim();
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

  throw new Error(`Unsupported platform/arch: ${platform}/${arch}`);
}

export function novaLspBinaryName(platform: NodeJS.Platform): string {
  return platform === 'win32' ? 'nova-lsp.exe' : 'nova-lsp';
}

export function novaLspArchiveName(target: string, platform: NodeJS.Platform): string {
  const ext = platform === 'win32' ? 'zip' : 'tar.xz';
  return `nova-lsp-${target}.${ext}`;
}

async function resolveTag(opts: {
  fetchImpl: typeof fetch;
  repo: GitHubRepo;
  releaseChannel: ReleaseChannel;
  version: string;
}): Promise<string> {
  const raw = opts.version.trim();
  if (raw !== 'latest') {
    return raw.startsWith('v') ? raw : `v${raw}`;
  }

  if (opts.releaseChannel === 'stable') {
    const release = await fetchJson<GitHubRelease>(opts.fetchImpl, `${opts.repo.apiBaseUrl}/releases/latest`);
    return release.tag_name;
  }

  const releases = await fetchJson<Array<GitHubRelease & { draft?: boolean }>>(
    opts.fetchImpl,
    `${opts.repo.apiBaseUrl}/releases?per_page=20`,
  );
  const candidate = releases.find((r) => !r.draft);
  if (!candidate) {
    throw new Error(`No releases found for ${opts.repo.owner}/${opts.repo.repo}`);
  }
  return candidate.tag_name;
}

async function fetchReleaseByTag(opts: { fetchImpl: typeof fetch; repo: GitHubRepo; tag: string }): Promise<GitHubRelease> {
  return fetchJson<GitHubRelease>(opts.fetchImpl, `${opts.repo.apiBaseUrl}/releases/tags/${encodeURIComponent(opts.tag)}`);
}

async function fetchJson<T>(fetchImpl: typeof fetch, url: string): Promise<T> {
  const resp = await fetchImpl(url, {
    headers: {
      Accept: 'application/vnd.github+json',
      'User-Agent': 'nova-vscode',
      'X-GitHub-Api-Version': '2022-11-28',
    },
  });

  if (!resp.ok) {
    throw new Error(`GitHub API request failed (${resp.status}): ${url}`);
  }

  return (await resp.json()) as T;
}

async function downloadBytes(fetchImpl: typeof fetch, url: string): Promise<ArrayBuffer> {
  const resp = await fetchImpl(url, {
    headers: {
      'User-Agent': 'nova-vscode',
    },
  });
  if (!resp.ok) {
    throw new Error(`Download failed (${resp.status}): ${url}`);
  }
  return await resp.arrayBuffer();
}

function sha256Hex(data: Buffer): string {
  return createHash('sha256').update(data).digest('hex');
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
    await execFileAsync('tar', [...tarArgs, '-C', tmpRoot]);

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

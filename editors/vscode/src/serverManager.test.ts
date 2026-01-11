import { beforeEach, describe, expect, it, vi } from 'vitest';

beforeEach(() => {
  vi.resetModules();
  vi.restoreAllMocks();
});

describe('serverManager helpers', () => {
  it('parses GitHub repo strings and URLs', async () => {
    const { parseGitHubRepo } = await import('./serverManager');

    expect(parseGitHubRepo('wilson-anysphere/indonesia')).toEqual({
      owner: 'wilson-anysphere',
      repo: 'indonesia',
      apiBaseUrl: 'https://api.github.com/repos/wilson-anysphere/indonesia',
    });

    expect(parseGitHubRepo('https://github.com/wilson-anysphere/indonesia')).toEqual({
      owner: 'wilson-anysphere',
      repo: 'indonesia',
      apiBaseUrl: 'https://api.github.com/repos/wilson-anysphere/indonesia',
    });

    expect(parseGitHubRepo('https://github.com/wilson-anysphere/indonesia.git')).toEqual({
      owner: 'wilson-anysphere',
      repo: 'indonesia',
      apiBaseUrl: 'https://api.github.com/repos/wilson-anysphere/indonesia',
    });

    expect(parseGitHubRepo('https://api.github.com/repos/wilson-anysphere/indonesia')).toEqual({
      owner: 'wilson-anysphere',
      repo: 'indonesia',
      apiBaseUrl: 'https://api.github.com/repos/wilson-anysphere/indonesia',
    });
  });

  it('detects supported targets', async () => {
    const { detectNovaTarget } = await import('./serverManager');

    expect(detectNovaTarget({ platform: 'darwin', arch: 'arm64' })).toBe('aarch64-apple-darwin');
    expect(detectNovaTarget({ platform: 'darwin', arch: 'x64' })).toBe('x86_64-apple-darwin');
    expect(detectNovaTarget({ platform: 'linux', arch: 'x64' })).toBe('x86_64-unknown-linux-gnu');
    expect(detectNovaTarget({ platform: 'win32', arch: 'x64' })).toBe('x86_64-pc-windows-msvc');
  });

  it('parses checksums files', async () => {
    const { parseChecksumsFile } = await import('./serverManager');

    const sha = 'a'.repeat(64);
    const map = parseChecksumsFile(
      `${sha}  file-one.tar.xz\n${sha} *file-two.zip\n${sha}  ./file-three.tar.xz\nSHA256 (file-four.zip) = ${sha}\n`,
    );
    expect(map.get('file-one.tar.xz')).toBe(sha);
    expect(map.get('file-two.zip')).toBe(sha);
    expect(map.get('./file-three.tar.xz')).toBe(sha);
    expect(map.get('file-three.tar.xz')).toBe(sha);
    expect(map.get('file-four.zip')).toBe(sha);
  });
});

describe('ServerManager install flow', () => {
  it('downloads, verifies, and installs nova-lsp', async () => {
    const { Volume, createFsFromVolume } = await import('memfs');
    const vol = new Volume();
    const memfs = createFsFromVolume(vol) as typeof import('node:fs');

    vi.doMock('node:fs/promises', () => memfs.promises as unknown as typeof import('node:fs/promises'));
    vi.doMock('node:fs', () => memfs);

    const { ServerManager } = await import('./serverManager');
    const archiveName = 'nova-lsp-x86_64-unknown-linux-gnu.tar.xz';
    const archive = Buffer.from('fake-archive-contents');
    const archiveBytes = archive.buffer.slice(archive.byteOffset, archive.byteOffset + archive.byteLength);
    const sha256 = await (async () => {
      const { createHash } = await import('node:crypto');
      return createHash('sha256').update(archive).digest('hex');
    })();

    const release = {
      tag_name: 'v0.1.0',
      assets: [
        { name: archiveName, browser_download_url: 'https://example.invalid/archive' },
        { name: `${archiveName}.sha256`, browser_download_url: 'https://example.invalid/archive.sha256' },
      ],
    };

    const fetchMock = vi.fn(async (url: string) => {
      if (url.endsWith(`/releases/tags/${encodeURIComponent('v0.1.0')}`)) {
        return { ok: true, status: 200, json: async () => release } as unknown as Response;
      }
      if (url === 'https://example.invalid/archive.sha256') {
        const body = Buffer.from(sha256);
        const ab = body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength);
        return { ok: true, status: 200, arrayBuffer: async () => ab } as unknown as Response;
      }
      if (url === 'https://example.invalid/archive') {
        return { ok: true, status: 200, arrayBuffer: async () => archiveBytes } as unknown as Response;
      }
      throw new Error(`Unexpected fetch url: ${url}`);
    });

    const extractor = {
      extractBinaryFromArchive: vi.fn(async ({ outputPath }: { outputPath: string }) => {
        await memfs.promises.writeFile(outputPath, 'binary');
      }),
    };

    const manager = new ServerManager('/storage', undefined, {
      fetch: fetchMock as unknown as typeof fetch,
      platform: 'linux',
      arch: 'x64',
      extractor,
    });

    const result = await manager.installOrUpdate({
      path: null,
      autoDownload: true,
      releaseChannel: 'stable',
      version: 'v0.1.0',
      releaseUrl: 'wilson-anysphere/indonesia',
    });

    expect(result.version).toBe('v0.1.0');
    expect(result.path).toBe('/storage/server/nova-lsp');
    expect(await memfs.promises.readFile('/storage/server/nova-lsp', 'utf8')).toBe('binary');

    const metadata = JSON.parse(await memfs.promises.readFile('/storage/server/nova-lsp.json', 'utf8'));
    expect(metadata.version).toBe('v0.1.0');
    expect(metadata.target).toBe('x86_64-unknown-linux-gnu');
  });

  it('refuses to install when the checksum mismatches', async () => {
    const { Volume, createFsFromVolume } = await import('memfs');
    const vol = new Volume();
    const memfs = createFsFromVolume(vol) as typeof import('node:fs');

    vi.doMock('node:fs/promises', () => memfs.promises as unknown as typeof import('node:fs/promises'));
    vi.doMock('node:fs', () => memfs);

    const { ServerManager } = await import('./serverManager');
    const archiveName = 'nova-lsp-x86_64-unknown-linux-gnu.tar.xz';

    const release = {
      tag_name: 'v0.1.0',
      assets: [
        { name: archiveName, browser_download_url: 'https://example.invalid/archive' },
        { name: `${archiveName}.sha256`, browser_download_url: 'https://example.invalid/archive.sha256' },
      ],
    };

    const fetchMock = vi.fn(async (url: string) => {
      if (url.endsWith(`/releases/tags/${encodeURIComponent('v0.1.0')}`)) {
        return { ok: true, status: 200, json: async () => release } as unknown as Response;
      }
      if (url === 'https://example.invalid/archive.sha256') {
        const body = Buffer.from('b'.repeat(64));
        const ab = body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength);
        return { ok: true, status: 200, arrayBuffer: async () => ab } as unknown as Response;
      }
      if (url === 'https://example.invalid/archive') {
        const archive = Buffer.from('different-contents');
        const ab = archive.buffer.slice(archive.byteOffset, archive.byteOffset + archive.byteLength);
        return { ok: true, status: 200, arrayBuffer: async () => ab } as unknown as Response;
      }
      throw new Error(`Unexpected fetch url: ${url}`);
    });

    const extractor = {
      extractBinaryFromArchive: vi.fn(async ({ outputPath }: { outputPath: string }) => {
        await memfs.promises.writeFile(outputPath, 'binary');
      }),
    };

    const manager = new ServerManager('/storage', undefined, {
      fetch: fetchMock as unknown as typeof fetch,
      platform: 'linux',
      arch: 'x64',
      extractor,
    });

    await expect(
      manager.installOrUpdate({
        path: null,
        autoDownload: true,
        releaseChannel: 'stable',
        version: 'v0.1.0',
        releaseUrl: 'wilson-anysphere/indonesia',
      }),
    ).rejects.toThrow(/Checksum mismatch/);
  });
});

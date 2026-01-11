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

    expect(parseGitHubRepo('wilson-anysphere/indonesia/')).toEqual({
      owner: 'wilson-anysphere',
      repo: 'indonesia',
      apiBaseUrl: 'https://api.github.com/repos/wilson-anysphere/indonesia',
    });

    expect(parseGitHubRepo('git@github.com:wilson-anysphere/indonesia.git')).toEqual({
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

    expect(parseGitHubRepo('https://github.example.com/wilson-anysphere/indonesia')).toEqual({
      owner: 'wilson-anysphere',
      repo: 'indonesia',
      apiBaseUrl: 'https://github.example.com/api/v3/repos/wilson-anysphere/indonesia',
    });

    expect(parseGitHubRepo('git@github.example.com:wilson-anysphere/indonesia.git')).toEqual({
      owner: 'wilson-anysphere',
      repo: 'indonesia',
      apiBaseUrl: 'https://github.example.com/api/v3/repos/wilson-anysphere/indonesia',
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
  it('skips download when the requested version is already installed', async () => {
    const { Volume, createFsFromVolume } = await import('memfs');
    const vol = new Volume();
    const memfs = createFsFromVolume(vol) as typeof import('node:fs');

    vi.doMock('node:fs/promises', () => memfs.promises as unknown as typeof import('node:fs/promises'));
    vi.doMock('node:fs', () => memfs);

    const { ServerManager } = await import('./serverManager');

    await memfs.promises.mkdir('/storage/server', { recursive: true });
    await memfs.promises.writeFile('/storage/server/nova-lsp', 'binary');
    await memfs.promises.writeFile(
      '/storage/server/nova-lsp.json',
      JSON.stringify(
        {
          version: 'v0.1.0',
          target: 'x86_64-unknown-linux-gnu',
          releaseApiBaseUrl: 'https://api.github.com/repos/wilson-anysphere/indonesia',
        },
        null,
        2,
      ),
    );

    const fetchMock = vi.fn();
    const extractor = {
      extractBinaryFromArchive: vi.fn(),
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

    expect(result).toEqual({ path: '/storage/server/nova-lsp', version: 'v0.1.0' });
    expect(fetchMock).not.toHaveBeenCalled();
    expect(extractor.extractBinaryFromArchive).not.toHaveBeenCalled();
  });

  it('selects the latest prerelease when version is latest and releaseChannel is prerelease', async () => {
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

    const prereleaseTag = 'v0.2.0-beta.1';

    const releases = [
      { tag_name: 'v0.1.0', draft: false, prerelease: false, assets: [] },
      { tag_name: prereleaseTag, draft: false, prerelease: true, assets: [] },
    ];

    const release = {
      tag_name: prereleaseTag,
      assets: [
        { name: archiveName, browser_download_url: 'https://example.invalid/archive' },
        { name: `${archiveName}.sha256`, browser_download_url: 'https://example.invalid/archive.sha256' },
      ],
    };

    const fetchMock = vi.fn(async (url: string) => {
      if (url.endsWith('/releases?per_page=20')) {
        return { ok: true, status: 200, json: async () => releases } as unknown as Response;
      }
      if (url.endsWith(`/releases/tags/${encodeURIComponent(prereleaseTag)}`)) {
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
      releaseChannel: 'prerelease',
      version: 'latest',
      releaseUrl: 'wilson-anysphere/indonesia',
    });

    expect(result.version).toBe(prereleaseTag);
  });

  it('suggests the prerelease channel when no stable releases exist', async () => {
    const { Volume, createFsFromVolume } = await import('memfs');
    const vol = new Volume();
    const memfs = createFsFromVolume(vol) as typeof import('node:fs');

    vi.doMock('node:fs/promises', () => memfs.promises as unknown as typeof import('node:fs/promises'));
    vi.doMock('node:fs', () => memfs);

    const { ServerManager } = await import('./serverManager');

    const fetchMock = vi.fn(async (url: string) => {
      if (url === 'https://api.github.com/repos/wilson-anysphere/indonesia/releases/latest') {
        return {
          ok: false,
          status: 404,
          statusText: 'Not Found',
          text: async () => JSON.stringify({ message: 'Not Found' }),
        } as unknown as Response;
      }
      throw new Error(`Unexpected fetch url: ${url}`);
    });

    const manager = new ServerManager('/storage', undefined, {
      fetch: fetchMock as unknown as typeof fetch,
      platform: 'linux',
      arch: 'x64',
      extractor: { extractBinaryFromArchive: vi.fn() },
    });

    await expect(
      manager.installOrUpdate({
        path: null,
        autoDownload: true,
        releaseChannel: 'stable',
        version: 'latest',
        releaseUrl: 'wilson-anysphere/indonesia',
      }),
    ).rejects.toThrow(/No stable releases found/);
  });

  it('adds Authorization headers for public GitHub URLs when GH_TOKEN is set', async () => {
    const oldGhToken = process.env.GH_TOKEN;
    process.env.GH_TOKEN = 'test-token';
    try {
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

      const archiveUrl = `https://github.com/wilson-anysphere/indonesia/releases/download/v0.1.0/${archiveName}`;
      const shaUrl = `${archiveUrl}.sha256`;

      const release = {
        tag_name: 'v0.1.0',
        assets: [
          { name: archiveName, browser_download_url: archiveUrl },
          { name: `${archiveName}.sha256`, browser_download_url: shaUrl },
        ],
      };

      const authByUrl = new Map<string, string | undefined>();
      const fetchMock = vi.fn(async (url: string, init?: { headers?: Record<string, string> }) => {
        const headers = init?.headers ?? {};
        authByUrl.set(url, headers.Authorization ?? headers.authorization);

        if (url.endsWith(`/releases/tags/${encodeURIComponent('v0.1.0')}`)) {
          return { ok: true, status: 200, json: async () => release } as unknown as Response;
        }
        if (url === shaUrl) {
          const body = Buffer.from(sha256);
          const ab = body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength);
          return { ok: true, status: 200, arrayBuffer: async () => ab } as unknown as Response;
        }
        if (url === archiveUrl) {
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

      await manager.installOrUpdate({
        path: null,
        autoDownload: true,
        releaseChannel: 'stable',
        version: 'v0.1.0',
        releaseUrl: 'wilson-anysphere/indonesia',
      });

      expect(authByUrl.get('https://api.github.com/repos/wilson-anysphere/indonesia/releases/tags/v0.1.0')).toBe(
        'Bearer test-token',
      );
      expect(authByUrl.get(shaUrl)).toBe('Bearer test-token');
      expect(authByUrl.get(archiveUrl)).toBe('Bearer test-token');
    } finally {
      if (oldGhToken === undefined) {
        delete process.env.GH_TOKEN;
      } else {
        process.env.GH_TOKEN = oldGhToken;
      }
    }
  });

  it('does not apply GH_TOKEN to non-GitHub hosts unless NOVA_GITHUB_TOKEN is set', async () => {
    const oldGhToken = process.env.GH_TOKEN;
    const oldNovaToken = process.env.NOVA_GITHUB_TOKEN;
    process.env.GH_TOKEN = 'test-token';
    delete process.env.NOVA_GITHUB_TOKEN;

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

    const archiveUrl = `https://ghe.example.com/wilson-anysphere/indonesia/releases/download/v0.1.0/${archiveName}`;
    const shaUrl = `${archiveUrl}.sha256`;

    const runInstall = async (storageRoot: string): Promise<Map<string, string | undefined>> => {
      const release = {
        tag_name: 'v0.1.0',
        assets: [
          { name: archiveName, browser_download_url: archiveUrl },
          { name: `${archiveName}.sha256`, browser_download_url: shaUrl },
        ],
      };

      const authByUrl = new Map<string, string | undefined>();
      const fetchMock = vi.fn(async (url: string, init?: { headers?: Record<string, string> }) => {
        const headers = init?.headers ?? {};
        authByUrl.set(url, headers.Authorization ?? headers.authorization);

        if (url.endsWith(`/releases/tags/${encodeURIComponent('v0.1.0')}`)) {
          return { ok: true, status: 200, json: async () => release } as unknown as Response;
        }
        if (url === shaUrl) {
          const body = Buffer.from(sha256);
          const ab = body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength);
          return { ok: true, status: 200, arrayBuffer: async () => ab } as unknown as Response;
        }
        if (url === archiveUrl) {
          return { ok: true, status: 200, arrayBuffer: async () => archiveBytes } as unknown as Response;
        }
        throw new Error(`Unexpected fetch url: ${url}`);
      });

      const extractor = {
        extractBinaryFromArchive: vi.fn(async ({ outputPath }: { outputPath: string }) => {
          await memfs.promises.writeFile(outputPath, 'binary');
        }),
      };

      const manager = new ServerManager(storageRoot, undefined, {
        fetch: fetchMock as unknown as typeof fetch,
        platform: 'linux',
        arch: 'x64',
        extractor,
      });

      await manager.installOrUpdate({
        path: null,
        autoDownload: true,
        releaseChannel: 'stable',
        version: 'v0.1.0',
        releaseUrl: 'https://ghe.example.com/api/v3/repos/wilson-anysphere/indonesia',
      });

      return authByUrl;
    };

    try {
      const authWithoutExplicit = await runInstall('/storage-no-token');
      expect(authWithoutExplicit.get('https://ghe.example.com/api/v3/repos/wilson-anysphere/indonesia/releases/tags/v0.1.0')).toBe(
        undefined,
      );

      process.env.NOVA_GITHUB_TOKEN = 'explicit-token';
      const authWithExplicit = await runInstall('/storage-explicit-token');
      expect(authWithExplicit.get('https://ghe.example.com/api/v3/repos/wilson-anysphere/indonesia/releases/tags/v0.1.0')).toBe(
        'Bearer explicit-token',
      );
    } finally {
      if (oldGhToken === undefined) {
        delete process.env.GH_TOKEN;
      } else {
        process.env.GH_TOKEN = oldGhToken;
      }
      if (oldNovaToken === undefined) {
        delete process.env.NOVA_GITHUB_TOKEN;
      } else {
        process.env.NOVA_GITHUB_TOKEN = oldNovaToken;
      }
    }
  });

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

  it('downloads, verifies, and installs nova-dap', async () => {
    const { Volume, createFsFromVolume } = await import('memfs');
    const vol = new Volume();
    const memfs = createFsFromVolume(vol) as typeof import('node:fs');

    vi.doMock('node:fs/promises', () => memfs.promises as unknown as typeof import('node:fs/promises'));
    vi.doMock('node:fs', () => memfs);

    const { ServerManager } = await import('./serverManager');
    const archiveName = 'nova-dap-x86_64-unknown-linux-gnu.tar.xz';
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

    const result = await manager.installOrUpdateDap({
      path: null,
      autoDownload: true,
      releaseChannel: 'stable',
      version: 'v0.1.0',
      releaseUrl: 'wilson-anysphere/indonesia',
    });

    expect(result.version).toBe('v0.1.0');
    expect(result.path).toBe('/storage/dap/nova-dap');
    expect(await memfs.promises.readFile('/storage/dap/nova-dap', 'utf8')).toBe('binary');

    const metadata = JSON.parse(await memfs.promises.readFile('/storage/dap/nova-dap.json', 'utf8'));
    expect(metadata.version).toBe('v0.1.0');
    expect(metadata.target).toBe('x86_64-unknown-linux-gnu');
  });

  it('falls back to .tar.gz archives when .tar.xz is not published', async () => {
    const { Volume, createFsFromVolume } = await import('memfs');
    const vol = new Volume();
    const memfs = createFsFromVolume(vol) as typeof import('node:fs');

    vi.doMock('node:fs/promises', () => memfs.promises as unknown as typeof import('node:fs/promises'));
    vi.doMock('node:fs', () => memfs);

    const { ServerManager } = await import('./serverManager');
    const archiveName = 'nova-lsp-x86_64-unknown-linux-gnu.tar.gz';
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

    expect(result.path).toBe('/storage/server/nova-lsp');
    expect(await memfs.promises.readFile('/storage/server/nova-lsp', 'utf8')).toBe('binary');
  });

  it('streams the archive download when the fetch response exposes a body', async () => {
    const { Volume, createFsFromVolume } = await import('memfs');
    const vol = new Volume();
    const memfs = createFsFromVolume(vol) as typeof import('node:fs');

    vi.doMock('node:fs/promises', () => memfs.promises as unknown as typeof import('node:fs/promises'));
    vi.doMock('node:fs', () => memfs);

    const { ServerManager } = await import('./serverManager');
    const archiveName = 'nova-lsp-x86_64-unknown-linux-gnu.tar.xz';
    const archive = Buffer.from('streamed-archive-contents');
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

    const archiveBody = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(new Uint8Array(archive));
        controller.close();
      },
    });

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
        return {
          ok: true,
          status: 200,
          body: archiveBody,
          arrayBuffer: async () => {
            throw new Error('arrayBuffer should not be called when body is available');
          },
        } as unknown as Response;
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
    expect(await memfs.promises.readFile('/storage/server/nova-lsp', 'utf8')).toBe('binary');
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

  it('refuses to install when no published SHA-256 checksum exists', async () => {
    const { Volume, createFsFromVolume } = await import('memfs');
    const vol = new Volume();
    const memfs = createFsFromVolume(vol) as typeof import('node:fs');

    vi.doMock('node:fs/promises', () => memfs.promises as unknown as typeof import('node:fs/promises'));
    vi.doMock('node:fs', () => memfs);

    const { ServerManager } = await import('./serverManager');
    const archiveName = 'nova-lsp-x86_64-unknown-linux-gnu.tar.xz';

    const release = {
      tag_name: 'v0.1.0',
      assets: [{ name: archiveName, browser_download_url: 'https://example.invalid/archive' }],
    };

    const fetchMock = vi.fn(async (url: string) => {
      if (url.endsWith(`/releases/tags/${encodeURIComponent('v0.1.0')}`)) {
        return { ok: true, status: 200, json: async () => release } as unknown as Response;
      }
      throw new Error(`Unexpected fetch url: ${url}`);
    });

    const manager = new ServerManager('/storage', undefined, {
      fetch: fetchMock as unknown as typeof fetch,
      platform: 'linux',
      arch: 'x64',
      extractor: { extractBinaryFromArchive: vi.fn() },
    });

    await expect(
      manager.installOrUpdate({
        path: null,
        autoDownload: true,
        releaseChannel: 'stable',
        version: 'v0.1.0',
        releaseUrl: 'wilson-anysphere/indonesia',
      }),
    ).rejects.toThrow(/No published SHA-256 checksums found/);
  });
});

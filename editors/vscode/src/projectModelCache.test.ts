import { beforeEach, describe, expect, it, vi } from 'vitest';

beforeEach(() => {
  vi.resetModules();
  vi.restoreAllMocks();
});

describe('ProjectModelCache', () => {
  it('marks nova/projectModel unsupported when request throws a method-not-found error', async () => {
    vi.doMock('vscode', () => ({}), { virtual: true });

    const { ProjectModelCache } = await import('./projectModelCache');

    const err = new Error('Method not found');
    (err as unknown as { code: number }).code = -32601;

    const request = vi.fn(async () => {
      throw err;
    });

    const cache = new ProjectModelCache(request as never);
    const folder = { uri: { fsPath: '/workspace' } } as never;

    await expect(cache.getProjectModel(folder)).rejects.toBe(err);
    expect(cache.isProjectModelUnsupported()).toBe(true);

    // Once we know a method is unsupported, we should stop calling it for the remainder of the session.
    await expect(cache.getProjectModel(folder)).rejects.toMatchObject({
      message: expect.stringContaining('nova/projectModel is not supported'),
    });
    expect(request).toHaveBeenCalledTimes(1);
  });

  it('marks nova/projectConfiguration unsupported when request returns undefined', async () => {
    vi.doMock('vscode', () => ({}), { virtual: true });

    const { ProjectModelCache } = await import('./projectModelCache');

    const request = vi.fn(async () => undefined);

    const cache = new ProjectModelCache(request as never);
    const folder = { uri: { fsPath: '/workspace' } } as never;

    await expect(cache.getProjectConfiguration(folder)).rejects.toMatchObject({
      message: expect.stringContaining('nova/projectConfiguration is not supported'),
    });
    expect(cache.isProjectConfigurationUnsupported()).toBe(true);

    // Should not retry the request after detecting unsupported.
    await expect(cache.getProjectConfiguration(folder)).rejects.toMatchObject({
      message: expect.stringContaining('nova/projectConfiguration is not supported'),
    });
    expect(request).toHaveBeenCalledTimes(1);
  });
});


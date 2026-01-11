import { describe, expect, it } from 'vitest';

import { deriveReleaseUrlFromBaseUrl } from './binaries';

describe('deriveReleaseUrlFromBaseUrl', () => {
  it('strips /releases/download suffix from github.com URLs', () => {
    expect(
      deriveReleaseUrlFromBaseUrl(
        'https://github.com/wilson-anysphere/indonesia/releases/download',
        'https://fallback.invalid',
      ),
    ).toBe('https://github.com/wilson-anysphere/indonesia');
  });

  it('strips /releases/download suffix from GitHub Enterprise URLs', () => {
    expect(
      deriveReleaseUrlFromBaseUrl(
        'https://github.example.com/wilson-anysphere/indonesia/releases/download',
        'https://fallback.invalid',
      ),
    ).toBe('https://github.example.com/wilson-anysphere/indonesia');
  });

  it('returns the fallback when baseUrl is empty', () => {
    expect(deriveReleaseUrlFromBaseUrl('   ', 'https://github.com/example/repo')).toBe('https://github.com/example/repo');
  });

  it('passes through non-download URLs', () => {
    expect(
      deriveReleaseUrlFromBaseUrl('https://github.com/wilson-anysphere/indonesia', 'https://fallback.invalid'),
    ).toBe('https://github.com/wilson-anysphere/indonesia');
  });
});


import { describe, expect, it } from 'vitest';

import { formatWebEndpointDescription, formatWebEndpointLabel, webEndpointNavigationTarget, type WebEndpoint } from './webEndpoints';

describe('web endpoint display formatting', () => {
  it('formats method + path labels', () => {
    const endpoint: WebEndpoint = { path: '/api/hello', methods: ['GET'], file: 'src/Hello.java', line: 42 };
    expect(formatWebEndpointLabel(endpoint)).toBe('GET /api/hello');
  });

  it('uses ANY when methods is empty', () => {
    const endpoint: WebEndpoint = { path: '/api/hello', methods: [], file: 'src/Hello.java', line: 42 };
    expect(formatWebEndpointLabel(endpoint)).toBe('ANY /api/hello');
  });

  it('shows location unavailable when file is missing', () => {
    const endpoint: WebEndpoint = { path: '/api/hello', methods: ['GET'], file: null, line: 42 };
    expect(formatWebEndpointDescription(endpoint)).toBe('location unavailable');
    expect(webEndpointNavigationTarget(endpoint)).toBeUndefined();
  });

  it('clamps invalid line numbers to 1', () => {
    const endpoint: WebEndpoint = { path: '/api/hello', methods: ['GET'], file: 'src/Hello.java', line: 0 };
    expect(formatWebEndpointDescription(endpoint)).toBe('src/Hello.java:1');
    expect(webEndpointNavigationTarget(endpoint)).toEqual({ file: 'src/Hello.java', line: 1 });
  });
});


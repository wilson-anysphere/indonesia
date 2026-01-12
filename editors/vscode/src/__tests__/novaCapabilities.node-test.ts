import assert from 'node:assert/strict';
import test from 'node:test';

import {
  formatUnsupportedNovaMethodMessage,
  isNovaMethodNotFoundError,
  isNovaRequestSupported,
  parseNovaExperimentalCapabilities,
  resetNovaExperimentalCapabilities,
  setNovaExperimentalCapabilities,
} from '../novaCapabilities';

test('parseNovaExperimentalCapabilities returns undefined for missing experimental.nova lists', () => {
  assert.equal(parseNovaExperimentalCapabilities(null), undefined);
  assert.equal(parseNovaExperimentalCapabilities({}), undefined);
  assert.equal(parseNovaExperimentalCapabilities({ capabilities: {} }), undefined);
  assert.equal(parseNovaExperimentalCapabilities({ capabilities: { experimental: {} } }), undefined);

  // Missing one of the lists -> treat as "unknown" (undefined), not "empty set".
  assert.equal(
    parseNovaExperimentalCapabilities({ capabilities: { experimental: { nova: { requests: ['nova/test/run'] } } } }),
    undefined,
  );
});

test('parseNovaExperimentalCapabilities parses requests/notifications into sets', () => {
  const parsed = parseNovaExperimentalCapabilities({
    capabilities: {
      experimental: {
        nova: {
          requests: ['nova/test/run', 123, 'nova/java/organizeImports'],
          notifications: ['nova/safeModeChanged', null],
        },
      },
    },
  });

  assert.ok(parsed);
  assert.equal(parsed.requests.has('nova/test/run'), true);
  assert.equal(parsed.requests.has('nova/java/organizeImports'), true);
  assert.equal(parsed.requests.has('nova/' + 'doesNotExist'), false);
  assert.equal(parsed.notifications.has('nova/safeModeChanged'), true);
});

test('isNovaRequestSupported returns unknown when no capability set is available', () => {
  resetNovaExperimentalCapabilities();

  assert.equal(isNovaRequestSupported('nova/test/run'), 'unknown');
  assert.equal(isNovaRequestSupported('textDocument/formatting'), 'unknown');
});

test('isNovaRequestSupported returns true/false when capability list is known', () => {
  resetNovaExperimentalCapabilities();
  setNovaExperimentalCapabilities({
    capabilities: { experimental: { nova: { requests: ['nova/test/run'], notifications: [] } } },
  });

  assert.equal(isNovaRequestSupported('nova/test/run'), true);
  assert.equal(isNovaRequestSupported('nova/test/discover'), false);
});

test('isNovaMethodNotFoundError detects nova-lsp method-not-found patterns', () => {
  assert.equal(isNovaMethodNotFoundError({ code: -32601, message: 'Method not found' }), true);
  assert.equal(
    isNovaMethodNotFoundError({ code: -32602, message: 'unknown (stateless) method: nova/refactor/moveMethod' }),
    true,
  );
  assert.equal(isNovaMethodNotFoundError({ code: -32602, message: 'invalid params' }), false);
  assert.equal(isNovaMethodNotFoundError({ message: 'METHOD NOT FOUND' }), true);
  assert.equal(isNovaMethodNotFoundError('nope'), false);
});

test('formatUnsupportedNovaMethodMessage includes method name', () => {
  assert.equal(
    formatUnsupportedNovaMethodMessage('nova/test/run'),
    'Nova: server does not support nova/test/run. You may be running an older nova-lsp; update or disable allowVersionMismatch.',
  );
});

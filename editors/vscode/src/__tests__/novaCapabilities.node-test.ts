import assert from 'node:assert/strict';
import test from 'node:test';

import {
  formatUnsupportedNovaMethodMessage,
  getSupportedNovaRequests,
  isNovaMethodNotFoundError,
  isNovaRequestSupported,
  parseNovaExperimentalCapabilities,
  resetNovaExperimentalCapabilities,
  setNovaExperimentalCapabilities,
} from '../novaCapabilities';
import type { LanguageClient } from 'vscode-languageclient/node';

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
  resetNovaExperimentalCapabilities('workspace-a');

  assert.equal(isNovaRequestSupported('workspace-a', 'nova/test/run'), 'unknown');
  assert.equal(isNovaRequestSupported('workspace-a', 'textDocument/formatting'), 'unknown');
});

test('isNovaRequestSupported returns true/false when capability list is known', () => {
  resetNovaExperimentalCapabilities('workspace-a');
  setNovaExperimentalCapabilities('workspace-a', {
    capabilities: { experimental: { nova: { requests: ['nova/test/run'], notifications: [] } } },
  });

  assert.equal(isNovaRequestSupported('workspace-a', 'nova/test/run'), true);
  assert.equal(isNovaRequestSupported('workspace-a', 'nova/test/discover'), false);
});

test('capability lists are tracked per workspace key', () => {
  resetNovaExperimentalCapabilities('workspace-a');
  resetNovaExperimentalCapabilities('workspace-b');

  setNovaExperimentalCapabilities('workspace-a', {
    capabilities: { experimental: { nova: { requests: ['nova/test/run'], notifications: [] } } },
  });
  setNovaExperimentalCapabilities('workspace-b', {
    capabilities: { experimental: { nova: { requests: ['nova/test/discover'], notifications: [] } } },
  });

  assert.equal(isNovaRequestSupported('workspace-a', 'nova/test/run'), true);
  assert.equal(isNovaRequestSupported('workspace-a', 'nova/test/discover'), false);
  assert.equal(isNovaRequestSupported('workspace-b', 'nova/test/run'), false);
  assert.equal(isNovaRequestSupported('workspace-b', 'nova/test/discover'), true);
});

test('resetNovaExperimentalCapabilities clears only the given workspace key', () => {
  resetNovaExperimentalCapabilities('workspace-a');
  resetNovaExperimentalCapabilities('workspace-b');

  setNovaExperimentalCapabilities('workspace-a', {
    capabilities: { experimental: { nova: { requests: ['nova/test/run'], notifications: [] } } },
  });
  setNovaExperimentalCapabilities('workspace-b', {
    capabilities: { experimental: { nova: { requests: ['nova/test/discover'], notifications: [] } } },
  });

  resetNovaExperimentalCapabilities('workspace-a');

  assert.equal(isNovaRequestSupported('workspace-a', 'nova/test/run'), 'unknown');
  assert.equal(isNovaRequestSupported('workspace-b', 'nova/test/discover'), true);
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

test('getSupportedNovaRequests reads initializeResult.capabilities.experimental.nova.requests', () => {
  const client = {
    initializeResult: {
      capabilities: {
        experimental: {
          nova: {
            requests: ['nova/test/run', 'initialize', 123],
          },
        },
      },
    },
  } as unknown as LanguageClient;

  const requests = getSupportedNovaRequests(client);
  assert.ok(requests);
  assert.equal(requests.has('nova/test/run'), true);
  assert.equal(requests.has('initialize'), false);
});

test('getSupportedNovaRequests returns undefined when requests list is missing', () => {
  const client = { initializeResult: { capabilities: { experimental: { nova: {} } } } } as unknown as LanguageClient;
  assert.equal(getSupportedNovaRequests(client), undefined);
});

import assert from 'node:assert/strict';
import test from 'node:test';

import {
  SAFE_MODE_EXEMPT_REQUESTS,
  formatSafeModeReason,
  isMethodNotFoundError,
  isSafeModeError,
  isUnknownExecuteCommandError,
  parseSafeModeEnabled,
  parseSafeModeReason,
} from '../safeMode';

test('isMethodNotFoundError detects JSON-RPC -32601', () => {
  assert.equal(isMethodNotFoundError({ code: -32601, message: 'Method not found' }), true);
});

test('isMethodNotFoundError detects Nova -32602 "unknown (stateless) method" variant', () => {
  assert.equal(isMethodNotFoundError({ code: -32602, message: 'Unknown (stateless) method: nova/ai/foo' }), true);
});

test('parseSafeModeEnabled handles boolean payloads', () => {
  assert.equal(parseSafeModeEnabled(true), true);
  assert.equal(parseSafeModeEnabled(false), false);
});

test('parseSafeModeEnabled handles object + nested payload variants', () => {
  assert.equal(parseSafeModeEnabled({ enabled: true }), true);
  assert.equal(parseSafeModeEnabled({ safeMode: false }), false);
  assert.equal(parseSafeModeEnabled({ status: { enabled: true } }), true);
  assert.equal(parseSafeModeEnabled({ status: { active: false } }), false);
  assert.equal(parseSafeModeEnabled({ status: { safeMode: true } }), true);
  assert.equal(parseSafeModeEnabled({}), undefined);
});

test('parseSafeModeReason reads reason from top-level and nested payloads', () => {
  assert.equal(parseSafeModeReason({ enabled: true, reason: 'panic' }), 'panic');
  assert.equal(parseSafeModeReason({ status: { enabled: true, reason: 'watchdog_timeout' } }), 'watchdog_timeout');
  assert.equal(parseSafeModeReason(true), undefined);
});

test('formatSafeModeReason normalizes underscores/dashes and title-cases', () => {
  assert.equal(formatSafeModeReason('watchdog_timeout'), 'Watchdog timeout');
  assert.equal(formatSafeModeReason('panic'), 'Panic');
  assert.equal(formatSafeModeReason('  '), '');
});

test('isSafeModeError matches canonical safe-mode guard messages', () => {
  assert.equal(
    isSafeModeError(
      new Error(
        'Nova is running in safe-mode. Only `nova/bugReport`, `nova/metrics`, `nova/resetMetrics`, and `nova/safeModeStatus` are available for now.',
      ),
    ),
    true,
  );

  assert.equal(
    isSafeModeError(
      new Error(
        'Nova is running in safe mode. Only `nova/bugReport`, `nova/metrics`, `nova/resetMetrics`, and `nova/safeModeStatus` are available for now.',
      ),
    ),
    true,
  );
});

test('isSafeModeError matches fallback guard messages without explicit "safe mode" phrasing', () => {
  assert.equal(
    isSafeModeError(
      new Error('Only `nova/bugReport`, `nova/metrics`, and `nova/resetMetrics` are available for now.'),
    ),
    true,
  );
});

test('isUnknownExecuteCommandError detects JSON-RPC -32602 unknown executeCommand variants', () => {
  assert.equal(isUnknownExecuteCommandError({ code: -32602, message: 'unknown command: nova.ai.explainError' }), true);
  assert.equal(isUnknownExecuteCommandError({ code: -32602, message: 'Unknown command: nova.ai.generateTests' }), true);

  const err = new Error('unknown command: nova.ai.generateMethodBody') as Error & { code?: number };
  err.code = -32602;
  assert.equal(isUnknownExecuteCommandError(err), true);
});

test('isUnknownExecuteCommandError ignores other errors', () => {
  assert.equal(isUnknownExecuteCommandError({ code: -32602, message: 'unsupported uri: untitled:foo.java' }), false);
  assert.equal(isUnknownExecuteCommandError({ code: -32601, message: 'unknown command: nova.ai.explainError' }), false);
  assert.equal(isUnknownExecuteCommandError('unknown command: nova.ai.explainError'), false);
});

test('SAFE_MODE_EXEMPT_REQUESTS includes nova/java/organizeImports', () => {
  assert.equal(SAFE_MODE_EXEMPT_REQUESTS.has('nova/java/organizeImports'), true);
});

test('SAFE_MODE_EXEMPT_REQUESTS includes nova/completion/more (AI polling)', () => {
  assert.equal(SAFE_MODE_EXEMPT_REQUESTS.has('nova/completion/more'), true);
});

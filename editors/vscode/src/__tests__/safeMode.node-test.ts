import assert from 'node:assert/strict';
import test from 'node:test';

import {
  SAFE_MODE_EXEMPT_REQUESTS,
  isMethodNotFoundError,
  isSafeModeError,
  parseSafeModeEnabled,
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

test('SAFE_MODE_EXEMPT_REQUESTS includes nova/java/organizeImports', () => {
  assert.equal(SAFE_MODE_EXEMPT_REQUESTS.has('nova/java/organizeImports'), true);
});


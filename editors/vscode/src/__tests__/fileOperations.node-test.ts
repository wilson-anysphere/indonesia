import test from 'node:test';
import assert from 'node:assert/strict';

import { toDidCreateFilesParams, toDidDeleteFilesParams, toDidRenameFilesParams } from '../fileOperations';

test('toDidRenameFilesParams returns the expected JSON shape', () => {
  assert.deepEqual(toDidRenameFilesParams([{ oldUri: 'file:///old.java', newUri: 'file:///new.java' }]), {
    files: [{ oldUri: 'file:///old.java', newUri: 'file:///new.java' }],
  });
});

test('toDidRenameFilesParams does not expose the input array by reference', () => {
  const input = [{ oldUri: 'file:///old.java', newUri: 'file:///new.java' }];
  const params = toDidRenameFilesParams(input);

  assert.deepEqual(input, [{ oldUri: 'file:///old.java', newUri: 'file:///new.java' }]);
  assert.notEqual(params.files, input);
  assert.notEqual(params.files[0], input[0]);
});

test('toDidCreateFilesParams returns the expected JSON shape', () => {
  assert.deepEqual(toDidCreateFilesParams(['file:///created.java']), {
    files: [{ uri: 'file:///created.java' }],
  });
});

test('toDidCreateFilesParams does not expose the input array by reference', () => {
  const input = [{ uri: 'file:///created.java' }];
  const params = toDidCreateFilesParams(input);

  assert.deepEqual(input, [{ uri: 'file:///created.java' }]);
  assert.notEqual(params.files, input);
  assert.notEqual(params.files[0], input[0]);
});

test('toDidDeleteFilesParams returns the expected JSON shape', () => {
  assert.deepEqual(toDidDeleteFilesParams(['file:///deleted.java']), {
    files: [{ uri: 'file:///deleted.java' }],
  });
});

test('toDidDeleteFilesParams does not expose the input array by reference', () => {
  const input = [{ uri: 'file:///deleted.java' }];
  const params = toDidDeleteFilesParams(input);

  assert.deepEqual(input, [{ uri: 'file:///deleted.java' }]);
  assert.notEqual(params.files, input);
  assert.notEqual(params.files[0], input[0]);
});

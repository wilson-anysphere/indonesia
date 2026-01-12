import test from 'node:test';
import assert from 'node:assert/strict';

import { toDidRenameFilesParams } from '../fileOperations';

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


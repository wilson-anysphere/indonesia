import * as path from 'node:path';
import { describe, expect, it } from 'vitest';

import { resolvePossiblyRelativePath } from '../pathUtils';

describe('resolvePossiblyRelativePath', () => {
  it('keeps absolute paths absolute', () => {
    const absolute = path.join(path.sep, 'tmp', 'foo.txt');
    const result = resolvePossiblyRelativePath(path.join(path.sep, 'ws'), absolute);
    expect(path.isAbsolute(result)).toBe(true);
    expect(result).toBe(path.normalize(absolute));
  });

  it('resolves relative paths against baseDir', () => {
    const baseDir = path.join(path.sep, 'ws');
    const result = resolvePossiblyRelativePath(baseDir, 'src/main/java');
    expect(result).toBe(path.normalize(path.join(baseDir, 'src/main/java')));
  });

  it('returns empty string for empty/blank candidates', () => {
    expect(resolvePossiblyRelativePath('/ws', '')).toBe('');
    expect(resolvePossiblyRelativePath('/ws', '   ')).toBe('');
  });
});


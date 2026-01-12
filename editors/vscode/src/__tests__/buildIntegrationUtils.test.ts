import { describe, expect, it } from 'vitest';

import { combineBuildStatuses, groupByFilePath, isBazelTargetRequiredMessage } from '../buildIntegrationUtils';

describe('buildIntegrationUtils', () => {
  it('detects Bazel target-required errors', () => {
    expect(isBazelTargetRequiredMessage('`target` must be provided for Bazel projects')).toBe(true);
    expect(isBazelTargetRequiredMessage('some other error')).toBe(false);
  });

  it('combines build statuses with building taking precedence', () => {
    expect(combineBuildStatuses([])).toBe('idle');
    expect(combineBuildStatuses(['idle'])).toBe('idle');
    expect(combineBuildStatuses(['failed'])).toBe('failed');
    expect(combineBuildStatuses(['idle', 'failed'])).toBe('failed');
    expect(combineBuildStatuses(['failed', 'building'])).toBe('building');
  });

  it('groups diagnostics by file path', () => {
    const grouped = groupByFilePath([
      { file: '/a/Foo.java', message: 'a' },
      { file: '/b/Bar.java', message: 'b' },
      { file: '/a/Foo.java', message: 'c' },
    ]);

    expect(Array.from(grouped.keys()).sort()).toEqual(['/a/Foo.java', '/b/Bar.java']);
    expect(grouped.get('/a/Foo.java')?.map((d) => d.message)).toEqual(['a', 'c']);
    expect(grouped.get('/b/Bar.java')?.map((d) => d.message)).toEqual(['b']);
  });
});


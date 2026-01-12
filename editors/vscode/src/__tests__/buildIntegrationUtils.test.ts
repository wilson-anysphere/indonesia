import { describe, expect, it } from 'vitest';

import {
  combineBuildStatuses,
  groupByFilePath,
  isBazelTargetRequiredMessage,
  shouldRefreshBuildDiagnosticsOnStatusTransition,
} from '../buildIntegrationUtils';

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

  it('refreshes build diagnostics when polling observes a build finishing', () => {
    expect(shouldRefreshBuildDiagnosticsOnStatusTransition({ prev: 'building', next: 'idle' })).toBe(true);
    expect(shouldRefreshBuildDiagnosticsOnStatusTransition({ prev: 'building', next: 'failed' })).toBe(true);

    expect(shouldRefreshBuildDiagnosticsOnStatusTransition({ prev: 'building', next: 'building' })).toBe(false);
    expect(shouldRefreshBuildDiagnosticsOnStatusTransition({ prev: 'idle', next: 'idle' })).toBe(false);
    expect(shouldRefreshBuildDiagnosticsOnStatusTransition({ prev: 'idle', next: 'failed' })).toBe(true);
    expect(shouldRefreshBuildDiagnosticsOnStatusTransition({ prev: 'failed', next: 'idle' })).toBe(true);
    expect(shouldRefreshBuildDiagnosticsOnStatusTransition({ prev: 'failed', next: 'failed' })).toBe(false);

    // Best-effort: if we missed earlier polling state, refresh once when we first see a failure.
    expect(shouldRefreshBuildDiagnosticsOnStatusTransition({ prev: undefined, next: 'failed' })).toBe(true);
    expect(shouldRefreshBuildDiagnosticsOnStatusTransition({ prev: undefined, next: 'idle' })).toBe(false);
  });
});

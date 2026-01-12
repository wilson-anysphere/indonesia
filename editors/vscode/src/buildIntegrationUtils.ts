export type NovaBuildStatus = 'idle' | 'building' | 'failed';

export const BAZEL_TARGET_REQUIRED_MESSAGE = '`target` must be provided for Bazel projects';

export function isBazelTargetRequiredMessage(message: string): boolean {
  return message.includes(BAZEL_TARGET_REQUIRED_MESSAGE);
}

export function combineBuildStatuses(statuses: readonly NovaBuildStatus[]): NovaBuildStatus {
  if (statuses.includes('building')) {
    return 'building';
  }
  if (statuses.includes('failed')) {
    return 'failed';
  }
  return 'idle';
}

export function groupByFilePath<T extends { file: string }>(diagnostics: readonly T[]): Map<string, T[]> {
  const grouped = new Map<string, T[]>();
  for (const diagnostic of diagnostics) {
    const file = diagnostic.file;
    const existing = grouped.get(file);
    if (existing) {
      existing.push(diagnostic);
    } else {
      grouped.set(file, [diagnostic]);
    }
  }
  return grouped;
}

export function shouldRefreshBuildDiagnosticsOnStatusTransition(opts: {
  prev?: NovaBuildStatus;
  next?: NovaBuildStatus;
}): boolean {
  const { prev, next } = opts;
  if (!next) {
    return false;
  }

  if (prev === next) {
    return false;
  }

  // Build diagnostics can become stale when builds are triggered outside of the explicit
  // `nova.buildProject` command. Refresh once when we observe the build reaching a terminal
  // state that could imply new diagnostics.
  //
  // Primary cases:
  // - building -> idle   (build succeeded)
  // - building -> failed (build failed)
  //
  // Best-effort cases:
  // - idle -> failed     (build finished between polls)
  // - failed -> idle     (build finished between polls, clearing old errors)
  // - undefined -> failed (extension started mid-failure, or status temporarily unavailable)
  if (next === 'failed') {
    return prev !== 'failed';
  }
  if (next === 'idle') {
    return prev === 'building' || prev === 'failed';
  }
  return false;
}

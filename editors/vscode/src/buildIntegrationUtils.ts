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

  // Build diagnostics can become stale when builds are triggered outside of the explicit
  // `nova.buildProject` command. Refresh once when we observe the build finishing.
  //
  // Primary cases:
  // - building -> idle   (build succeeded)
  // - building -> failed (build failed)
  //
  // Best-effort: also refresh when we see a terminal failed state without having observed
  // a prior status (e.g. extension started mid-build or missed a status update).
  if (prev === 'building') {
    return next === 'idle' || next === 'failed';
  }
  if (typeof prev === 'undefined') {
    return next === 'failed';
  }
  return false;
}

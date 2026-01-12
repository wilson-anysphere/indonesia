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


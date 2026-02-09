export type CompletionItemLabelLike = {
  label: string;
  detail?: string;
  description?: string;
};

type NovaCompletionItemData = {
  nova?: {
    source?: unknown;
    confidence?: unknown;
  };
};

export type CompletionItemPresentationLike = {
  // Include `label` to avoid TypeScript "weak type" assignability issues when called with `vscode.CompletionItem`.
  label?: unknown;
  data?: unknown;
};

export interface DecorateNovaAiCompletionItemsOptions {
  /**
   * Text to show in `CompletionItemLabel.detail` for AI items.
   * Defaults to `"AI"`.
   */
  aiLabelDetailText?: string;
}

function parseNovaAiConfidence(value: unknown): number | undefined {
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    return undefined;
  }
  return value;
}

function formatConfidence(value: number): string {
  // Keep the string short and stable. Clamp to [0, 1] and format as `0.87`.
  const clamped = Math.min(1, Math.max(0, value));
  return clamped.toFixed(2);
}

function isNovaAiCompletionItem(item: CompletionItemPresentationLike): boolean {
  const data = item.data as NovaCompletionItemData | undefined;
  const source = data?.nova?.source;
  return source === 'ai';
}

function getNovaItemConfidence(item: CompletionItemPresentationLike): number | undefined {
  const data = item.data as NovaCompletionItemData | undefined;
  return parseNovaAiConfidence(data?.nova?.confidence);
}

function extractLabelText(value: unknown): string | undefined {
  if (typeof value === 'string') {
    return value;
  }
  if (!value || typeof value !== 'object') {
    return undefined;
  }
  const label = (value as { label?: unknown }).label;
  return typeof label === 'string' ? label : undefined;
}

function normalizeCompletionItemLabel(
  value: unknown,
  labelText: string,
): CompletionItemLabelLike | undefined {
  if (typeof value === 'string') {
    return { label: labelText };
  }
  if (!value || typeof value !== 'object') {
    return undefined;
  }

  const existing = value as { label?: unknown; detail?: unknown; description?: unknown };
  const detail = typeof existing.detail === 'string' ? existing.detail : undefined;
  const description = typeof existing.description === 'string' ? existing.description : undefined;
  return { label: labelText, detail, description };
}

/**
 * Mutates completion items in-place to decorate Nova AI completion items with an "AI" indicator in
 * VS Code's suggest widget.
 *
 * This helper intentionally avoids importing `vscode` so it can be unit tested with plain Node.
 */
export function decorateNovaAiCompletionItems(
  items: readonly CompletionItemPresentationLike[],
  opts: DecorateNovaAiCompletionItemsOptions = {},
): void {
  const aiLabelDetailText = opts.aiLabelDetailText ?? 'AI';

  for (const item of items) {
    if (!item || typeof item !== 'object') {
      continue;
    }

    if (!isNovaAiCompletionItem(item)) {
      continue;
    }

    const labelText = extractLabelText(item.label);
    if (!labelText) {
      continue;
    }

    const normalized = normalizeCompletionItemLabel(item.label, labelText);
    if (!normalized) {
      continue;
    }

    // Preserve any server-provided label details/description; only fill missing fields.
    if (!normalized.detail || normalized.detail.trim().length === 0) {
      normalized.detail = aiLabelDetailText;
    } else if (
      (!normalized.description || normalized.description.trim().length === 0) &&
      !normalized.detail.includes(aiLabelDetailText)
    ) {
      // If `detail` is already in use (e.g. signature details), use `description` for the AI marker.
      normalized.description = aiLabelDetailText;
    }

    const confidence = getNovaItemConfidence(item);
    if (typeof confidence === 'number') {
      // Only attach confidence when the description slot is still available.
      if (!normalized.description || normalized.description.trim().length === 0 || normalized.description === aiLabelDetailText) {
        normalized.description = normalized.description === aiLabelDetailText ? `${aiLabelDetailText} ${formatConfidence(confidence)}` : formatConfidence(confidence);
      }
    }

    item.label = normalized;
  }
}


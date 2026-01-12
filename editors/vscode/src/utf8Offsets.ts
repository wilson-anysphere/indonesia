/**
 * Shared helper for converting UTF-8 byte offsets (as returned by Nova's Micronaut endpoints/beans APIs)
 * into UTF-16 code unit offsets (as used by VS Code's `TextDocument.positionAt`).
 *
 * NOTE: The Frameworks tree view should reuse this helper when implementing Micronaut span navigation
 * (Task 55) to avoid duplicating UTF-8/UTF-16 offset math.
 */

export interface Utf8Span {
  start: number;
  end: number;
}

/**
 * Convert a UTF-8 byte offset into a UTF-16 code unit offset for the same string.
 *
 * Behavior:
 * - Negative / NaN offsets clamp to 0.
 * - Offsets beyond the end clamp to `text.length`.
 * - If `byteOffset` points into the middle of a multi-byte code point, this returns the UTF-16 offset
 *   at the start of that code point (best-effort + resilient).
 */
export function utf8ByteOffsetToUtf16Offset(text: string, byteOffset: number): number {
  if (Number.isNaN(byteOffset) || byteOffset <= 0) {
    return 0;
  }

  let bytesSeen = 0;
  let utf16Offset = 0;

  for (const ch of text) {
    const codePoint = ch.codePointAt(0);
    if (typeof codePoint !== 'number') {
      break;
    }

    const utf8Bytes = utf8ByteLengthOfCodePoint(codePoint);

    // If the target offset falls within this character's UTF-8 encoding, clamp to the start.
    if (bytesSeen + utf8Bytes > byteOffset) {
      return utf16Offset;
    }

    bytesSeen += utf8Bytes;
    utf16Offset += ch.length;

    if (bytesSeen === byteOffset) {
      return utf16Offset;
    }
  }

  // If the byteOffset is past the end of the string, clamp to the end.
  return utf16Offset;
}

export function utf8SpanToUtf16Offsets(text: string, span: Utf8Span): Utf8Span {
  const startByte = typeof span.start === 'number' ? span.start : 0;
  const endByte = typeof span.end === 'number' ? span.end : startByte;

  return {
    start: utf8ByteOffsetToUtf16Offset(text, startByte),
    end: utf8ByteOffsetToUtf16Offset(text, Math.max(endByte, startByte)),
  };
}

function utf8ByteLengthOfCodePoint(codePoint: number): number {
  if (codePoint <= 0x7f) {
    return 1;
  }
  if (codePoint <= 0x7ff) {
    return 2;
  }
  if (codePoint <= 0xffff) {
    return 3;
  }
  return 4;
}


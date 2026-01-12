/**
 * Convert a UTF-8 byte offset into a UTF-16 code unit offset for the same string.
 *
 * VS Code's `TextDocument.positionAt` expects UTF-16 offsets, but Nova's Micronaut
 * introspection endpoints return spans as UTF-8 byte offsets.
 *
 * If `byteOffset` points into the middle of a multi-byte code point, this returns
 * the UTF-16 offset at the start of that code point (best-effort + resilient).
 */
export function utf8ByteOffsetToUtf16Offset(text: string, byteOffset: number): number {
  if (!Number.isFinite(byteOffset) || byteOffset <= 0) {
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


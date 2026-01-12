import { describe, expect, it } from 'vitest';

import { utf8ByteOffsetToUtf16Offset } from './utf8';

describe('utf8ByteOffsetToUtf16Offset', () => {
  it('maps ASCII byte offsets 1:1', () => {
    const text = 'hello';
    expect(utf8ByteOffsetToUtf16Offset(text, 0)).toBe(0);
    expect(utf8ByteOffsetToUtf16Offset(text, 1)).toBe(1);
    expect(utf8ByteOffsetToUtf16Offset(text, 5)).toBe(5);
    expect(utf8ByteOffsetToUtf16Offset(text, 999)).toBe(text.length);
  });

  it('maps multi-byte BMP characters', () => {
    // "é" is 2 bytes in UTF-8, 1 UTF-16 code unit.
    const text = 'aé';
    expect(utf8ByteOffsetToUtf16Offset(text, 0)).toBe(0);
    expect(utf8ByteOffsetToUtf16Offset(text, 1)).toBe(1); // after "a"
    expect(utf8ByteOffsetToUtf16Offset(text, 3)).toBe(2); // after "é"
  });

  it('maps surrogate pairs', () => {
    // "😀" is 4 bytes in UTF-8, 2 UTF-16 code units.
    const text = 'a😀b';
    expect(utf8ByteOffsetToUtf16Offset(text, 0)).toBe(0);
    expect(utf8ByteOffsetToUtf16Offset(text, 1)).toBe(1); // after "a"
    expect(utf8ByteOffsetToUtf16Offset(text, 5)).toBe(3); // after "😀"
    expect(utf8ByteOffsetToUtf16Offset(text, 6)).toBe(4); // after "b"
  });

  it('clamps offsets into the middle of a multi-byte character to the character start', () => {
    const text = 'é'; // 2 bytes
    expect(utf8ByteOffsetToUtf16Offset(text, 1)).toBe(0);
  });
});


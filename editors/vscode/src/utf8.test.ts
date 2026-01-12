import { describe, expect, it } from 'vitest';

import { utf8ByteOffsetToUtf16Offset, utf8SpanToUtf16Offsets } from './utf8';

describe('utf8ByteOffsetToUtf16Offset', () => {
  it('maps ASCII byte offsets 1:1', () => {
    const text = 'hello world';
    for (let i = 0; i <= text.length; i++) {
      expect(utf8ByteOffsetToUtf16Offset(text, i)).toBe(i);
    }
    expect(utf8ByteOffsetToUtf16Offset(text, 999)).toBe(text.length);
  });

  it('maps multi-byte BMP characters (é, €) and rounds down inside a code point', () => {
    // a   é   €   b
    // 1b  2b  3b  1b  => 7 bytes total, 4 UTF-16 code units total
    const text = 'aé€b';

    expect(utf8ByteOffsetToUtf16Offset(text, 0)).toBe(0);

    // After 'a'
    expect(utf8ByteOffsetToUtf16Offset(text, 1)).toBe(1);

    // Inside 'é' (2-byte code point) should round down to its start.
    expect(utf8ByteOffsetToUtf16Offset(text, 2)).toBe(1);

    // After 'é'
    expect(utf8ByteOffsetToUtf16Offset(text, 3)).toBe(2);

    // Inside '€' (3-byte code point) should round down to its start.
    expect(utf8ByteOffsetToUtf16Offset(text, 4)).toBe(2);
    expect(utf8ByteOffsetToUtf16Offset(text, 5)).toBe(2);

    // After '€'
    expect(utf8ByteOffsetToUtf16Offset(text, 6)).toBe(3);

    // End of string.
    expect(utf8ByteOffsetToUtf16Offset(text, 7)).toBe(4);
  });

  it('maps surrogate pairs (😀) and advances by 2 UTF-16 code units at the 4-byte boundary', () => {
    // "😀" is 4 bytes in UTF-8, 2 UTF-16 code units.
    const text = 'a😀b';
    expect(utf8ByteOffsetToUtf16Offset(text, 0)).toBe(0);
    expect(utf8ByteOffsetToUtf16Offset(text, 1)).toBe(1); // after "a"
    expect(utf8ByteOffsetToUtf16Offset(text, 2)).toBe(1); // inside 😀
    expect(utf8ByteOffsetToUtf16Offset(text, 4)).toBe(1); // inside 😀
    expect(utf8ByteOffsetToUtf16Offset(text, 5)).toBe(3); // after "😀"
    expect(utf8ByteOffsetToUtf16Offset(text, 6)).toBe(4); // after "b"
  });

  it('clamps negative/NaN to 0 and clamps beyond the end to text.length', () => {
    const text = 'aé';
    expect(utf8ByteOffsetToUtf16Offset(text, -1)).toBe(0);
    expect(utf8ByteOffsetToUtf16Offset(text, Number.NaN)).toBe(0);
    expect(utf8ByteOffsetToUtf16Offset(text, 999)).toBe(text.length);
  });
});

describe('utf8SpanToUtf16Offsets', () => {
  it('converts start/end together and clamps end >= start', () => {
    const text = 'a😀b';

    // Span that includes only the emoji bytes.
    expect(utf8SpanToUtf16Offsets(text, { start: 1, end: 5 })).toEqual({ start: 1, end: 3 });

    // End < start should still produce a non-negative range.
    expect(utf8SpanToUtf16Offsets(text, { start: 5, end: 1 })).toEqual({ start: 3, end: 3 });
  });
});

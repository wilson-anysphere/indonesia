// Backwards-compatible re-export.
//
// `utf8Offsets.ts` is the canonical module for converting UTF-8 byte offsets (Micronaut spans)
// to UTF-16 offsets (VS Code document offsets). Keep `utf8.ts` around so older imports continue
// to work.

export type { Utf8Span } from './utf8Offsets';
export { utf8ByteOffsetToUtf16Offset, utf8SpanToUtf16Offsets } from './utf8Offsets';


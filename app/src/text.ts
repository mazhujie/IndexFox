export interface CharRange {
  start: number;
  end: number;
  active?: boolean;
}

export interface HighlightSegment {
  text: string;
  active: boolean;
  highlighted: boolean;
}

export interface PreviewLine {
  text: string;
  /** Character offset of the line start inside the preview text. */
  start: number;
}

function utf8Length(codePoint: number): number {
  if (codePoint <= 0x7f) return 1;
  if (codePoint <= 0x7ff) return 2;
  if (codePoint <= 0xffff) return 3;
  return 4;
}

/**
 * Convert a UTF-8 byte offset (what the Rust query engine reports) into a
 * JavaScript UTF-16 string index. Offsets inside a multi-byte character are
 * snapped down to the character boundary.
 */
export function byteOffsetToCharIndex(text: string, byteOffset: number): number {
  if (!Number.isFinite(byteOffset) || byteOffset <= 0) return 0;
  let bytes = 0;
  let chars = 0;
  for (const character of text) {
    if (bytes >= byteOffset) return chars;
    const width = utf8Length(character.codePointAt(0) ?? 0);
    if (bytes + width > byteOffset) return chars;
    bytes += width;
    chars += character.length;
  }
  return chars;
}

export function charRangeForBytes(
  text: string,
  startByte: number,
  endByte: number,
): CharRange | null {
  const start = byteOffsetToCharIndex(text, startByte);
  const end = byteOffsetToCharIndex(text, endByte);
  if (end <= start) return null;
  return { start, end };
}

export function clampRange(text: string, range: CharRange): CharRange | null {
  const start = Math.max(0, Math.min(text.length, range.start));
  const end = Math.max(0, Math.min(text.length, range.end));
  if (end <= start) return null;
  return { start, end, active: range.active };
}

/** Merge overlapping ranges so every character is rendered at most once. */
export function buildSegments(text: string, ranges: CharRange[]): HighlightSegment[] {
  const valid = ranges
    .map((range) => clampRange(text, range))
    .filter((range): range is CharRange => range !== null)
    .sort((left, right) => left.start - right.start || right.end - left.end);

  const merged: CharRange[] = [];
  for (const range of valid) {
    const previous = merged[merged.length - 1];
    if (previous && range.start < previous.end) {
      previous.end = Math.max(previous.end, range.end);
      previous.active = Boolean(previous.active || range.active);
    } else if (previous && range.start === previous.end && range.active === previous.active) {
      previous.end = range.end;
    } else {
      merged.push({ ...range });
    }
  }

  const segments: HighlightSegment[] = [];
  let cursor = 0;
  for (const range of merged) {
    if (range.start > cursor) {
      segments.push({
        text: text.slice(cursor, range.start),
        active: false,
        highlighted: false,
      });
    }
    segments.push({
      text: text.slice(range.start, range.end),
      active: Boolean(range.active),
      highlighted: true,
    });
    cursor = range.end;
  }
  if (cursor < text.length) {
    segments.push({ text: text.slice(cursor), active: false, highlighted: false });
  }
  return segments;
}

export function splitPreviewLines(text: string): PreviewLine[] {
  const lines: PreviewLine[] = [];
  let start = 0;
  for (let index = 0; index < text.length; index += 1) {
    if (text[index] === "\n") {
      lines.push({ text: text.slice(start, index), start });
      start = index + 1;
    }
  }
  if (start < text.length || lines.length === 0 || !text.endsWith("\n")) {
    lines.push({ text: text.slice(start), start });
  }
  return lines;
}

export function rangesForLine(
  ranges: CharRange[],
  lineStart: number,
  lineEnd: number,
): CharRange[] {
  const clipped: CharRange[] = [];
  for (const range of ranges) {
    const start = Math.max(range.start, lineStart);
    const end = Math.min(range.end, lineEnd);
    if (end > start) {
      clipped.push({ start: start - lineStart, end: end - lineStart, active: range.active });
    }
  }
  return clipped;
}

export function lineHasActiveRange(
  ranges: CharRange[],
  lineStart: number,
  lineEnd: number,
): boolean {
  return ranges.some(
    (range) =>
      range.active && range.start < lineEnd && range.end > lineStart,
  );
}

export function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export function literalRanges(
  text: string,
  needle: string,
  caseSensitive: boolean,
): CharRange[] {
  if (!text || !needle) return [];
  const pattern = new RegExp(escapeRegExp(needle), caseSensitive ? "g" : "gi");
  const ranges: CharRange[] = [];
  let match: RegExpExecArray | null = null;
  let guard = 0;
  while ((match = pattern.exec(text)) !== null) {
    if (match[0].length === 0) break;
    ranges.push({ start: match.index, end: match.index + match[0].length });
    guard += 1;
    if (guard >= 200) break;
  }
  return ranges;
}

export function hitKey(fileId: number, hit: { chunk_id: number; kind: string; start: number; end: number }): string {
  return `${fileId}:${hit.chunk_id}:${hit.kind}:${hit.start}:${hit.end}`;
}

export function queryErrorContext(
  query: string,
  bytePosition: number,
): { before: string; at: string; after: string } {
  const index = byteOffsetToCharIndex(query, bytePosition);
  if (index >= query.length) {
    return { before: query, at: "", after: "" };
  }
  const codePoint = query.codePointAt(index);
  const width = codePoint !== undefined && codePoint > 0xffff ? 2 : 1;
  return {
    before: query.slice(0, index),
    at: query.slice(index, index + width),
    after: query.slice(index + width),
  };
}

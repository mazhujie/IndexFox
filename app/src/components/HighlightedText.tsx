import { buildSegments, type CharRange } from "../text";

interface HighlightedTextProps {
  text: string;
  ranges: CharRange[];
}

export function HighlightedText({ text, ranges }: HighlightedTextProps) {
  const segments = buildSegments(text, ranges);
  return (
    <>
      {segments.map((segment, index) =>
        segment.highlighted ? (
          <mark
            key={`${index}-${segment.text.length}`}
            className={segment.active ? "hit-mark active" : "hit-mark"}
            data-active-hit={segment.active ? "true" : undefined}
          >
            {segment.text}
          </mark>
        ) : (
          <span key={`${index}-${segment.text.length}`}>{segment.text}</span>
        ),
      )}
    </>
  );
}

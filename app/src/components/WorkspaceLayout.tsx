import { useRef, useState } from "react";
import type { CSSProperties, KeyboardEvent, PointerEvent, ReactNode } from "react";

type PaneWidths = [sources: number, results: number];
type Divider = 0 | 1;

interface SplitPosition {
  divider: Divider;
  widths: [number, number, number];
  minDelta: number;
  maxDelta: number;
}

const STORAGE_KEY = "indexfox.workspace-widths";

function readWidths(): PaneWidths | null {
  try {
    const widths: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null");
    if (Array.isArray(widths) && widths.length === 2 && widths.every(
      (width) => typeof width === "number" && Number.isFinite(width) && width > 0,
    )) {
      return [widths[0], widths[1]];
    }
  } catch {
    // The layout still works when desktop profile storage is unavailable.
  }
  return null;
}

function saveWidths(widths: PaneWidths) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(widths));
  } catch {
    // Keep the widths for the current window even if saving fails.
  }
}

function measure(splitter: HTMLDivElement, divider: Divider): SplitPosition | null {
  const workspace = splitter.parentElement;
  if (!workspace || !splitter.offsetWidth) return null;
  const widths: SplitPosition["widths"] = [0, 2, 4].map((index) =>
    workspace.children[index].getBoundingClientRect().width,
  ) as SplitPosition["widths"];
  const styles = getComputedStyle(workspace);
  const minima = ["--sources-min", "--results-min", "--preview-min"].map((property) =>
    Number.parseFloat(styles.getPropertyValue(property)),
  );
  return {
    divider,
    widths,
    minDelta: Math.min(0, minima[divider] - widths[divider]),
    maxDelta: Math.max(0, widths[divider + 1] - minima[divider + 1]),
  };
}

function resize(position: SplitPosition, delta: number): PaneWidths {
  const change = Math.max(position.minDelta, Math.min(position.maxDelta, delta));
  const [sources, results] = position.widths;
  return position.divider === 0 ? [sources + change, results - change] : [sources, results + change];
}

export function WorkspaceLayout({ children }: { children: [ReactNode, ReactNode, ReactNode] }) {
  const [widths, setWidths] = useState<PaneWidths | null>(readWidths);
  const [dragging, setDragging] = useState(false);
  const drag = useRef<{ pointer: number; start: number; position: SplitPosition; widths?: PaneWidths } | null>(null);
  const style = widths ? {
    "--sources-width": `${widths[0]}px`,
    "--results-width": `${widths[1]}px`,
  } as CSSProperties : undefined;

  function startDrag(event: PointerEvent<HTMLDivElement>, divider: Divider) {
    if (event.button !== 0 || !event.isPrimary || drag.current) return;
    const position = measure(event.currentTarget, divider);
    if (!position) return;
    event.preventDefault();
    event.currentTarget.focus();
    event.currentTarget.setPointerCapture(event.pointerId);
    drag.current = { pointer: event.pointerId, start: event.clientX, position };
    setDragging(true);
  }

  function moveDrag(event: PointerEvent<HTMLDivElement>) {
    const current = drag.current;
    if (current?.pointer !== event.pointerId) return;
    current.widths = resize(current.position, event.clientX - current.start);
    setWidths(current.widths);
  }

  function finishDrag(event: PointerEvent<HTMLDivElement>) {
    const current = drag.current;
    if (current?.pointer !== event.pointerId) return;
    if (current.widths) saveWidths(current.widths);
    drag.current = null;
    setDragging(false);
  }

  function resizeWithKey(event: KeyboardEvent<HTMLDivElement>, divider: Divider) {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
    const position = measure(event.currentTarget, divider);
    if (!position) return;
    event.preventDefault();
    const next = resize(position, event.key === "ArrowLeft" ? -16 : 16);
    setWidths(next);
    saveWidths(next);
  }

  function splitter(divider: Divider, label: string) {
    const defaultWidth = divider === 0 ? 250 : 400;
    const percent = Math.round(100 * (widths?.[divider] ?? defaultWidth) / Math.max(1, window.innerWidth));
    return (
      <div
        className="workspace-divider"
        role="separator"
        aria-label={label}
        aria-orientation="vertical"
        aria-valuenow={Math.min(100, percent)}
        tabIndex={0}
        onPointerDown={(event) => startDrag(event, divider)}
        onPointerMove={moveDrag}
        onPointerUp={finishDrag}
        onPointerCancel={finishDrag}
        onLostPointerCapture={finishDrag}
        onKeyDown={(event) => resizeWithKey(event, divider)}
      />
    );
  }

  return (
    <main className={`workspace${dragging ? " resizing" : ""}`} style={style}>
      {children[0]}
      {splitter(0, "调整索引目录和搜索结果栏宽")}
      {children[1]}
      {splitter(1, "调整搜索结果和预览栏宽")}
      {children[2]}
    </main>
  );
}

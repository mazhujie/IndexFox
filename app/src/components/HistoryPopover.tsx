import { useEffect, useRef } from "react";
import type { HistoryItem, HistorySettings } from "../types";
import { formatHistoryTime, parseHistoryFilters } from "../types";

interface HistoryPopoverProps {
  settings: HistorySettings;
  busy: boolean;
  error: string | null;
  onClose: () => void;
  onToggleEnabled: (enabled: boolean) => void;
  onClear: () => void;
  onApply: (item: HistoryItem) => void;
}

function filtersLabel(item: HistoryItem): string {
  const filters = parseHistoryFilters(item.filters_json);
  const parts: string[] = [];
  if (filters.caseSensitive) parts.push("Aa");
  if (filters.extensions && filters.extensions.length > 0) {
    parts.push(filters.extensions.slice(0, 4).join("/"));
  } else {
    parts.push("全部格式");
  }
  if (filters.sourceId !== null && filters.sourceId !== undefined) {
    parts.push(`来源 #${filters.sourceId}`);
  }
  if (filters.sort && filters.sort !== "relevance") parts.push(filters.sort);
  return parts.join(" · ");
}

export function HistoryPopover({
  settings,
  busy,
  error,
  onClose,
  onToggleEnabled,
  onClear,
  onApply,
}: HistoryPopoverProps) {
  const popoverRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    function handlePointerDown(event: PointerEvent) {
      if (!popoverRef.current) return;
      const target = event.target;
      if (
        target instanceof Element &&
        target.closest("[data-history-trigger]") !== null
      ) {
        return;
      }
      if (target instanceof Node && !popoverRef.current.contains(target)) {
        onClose();
      }
    }
    function handleKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape") onClose();
    }
    document.addEventListener("pointerdown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("pointerdown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [onClose]);

  return (
    <div className="history-popover" role="dialog" aria-label="搜索历史" ref={popoverRef}>
      <div className="history-header">
        <div>
          <strong>最近搜索</strong>
          <span className="muted">上限 100 条 · 自动去重</span>
        </div>
        <div className="history-actions">
          <label className="switch-label">
            <input
              type="checkbox"
              checked={settings.enabled}
              disabled={busy}
              onChange={(event) => onToggleEnabled(event.target.checked)}
            />
            <span>记录</span>
          </label>
          <button
            type="button"
            className="mini-button"
            onClick={onClear}
            disabled={busy || settings.items.length === 0}
          >
            清空
          </button>
        </div>
      </div>

      {!settings.enabled && (
        <p className="history-disabled" role="status">
          新搜索不会写入历史；已有条目仍可查看。
        </p>
      )}

      {error && (
        <p className="inline-error history-error" role="alert">
          {error}
        </p>
      )}

      {settings.items.length === 0 ? (
        <p className="history-empty">暂无搜索历史</p>
      ) : (
        <ul className="history-list">
          {settings.items.map((item) => (
            <li key={item.id}>
              <button type="button" className="history-item" onClick={() => onApply(item)}>
                <span className="history-query mono">{item.query_text}</span>
                <span className="history-meta">
                  <span className="tag">{item.search_mode}</span>
                  <span>{filtersLabel(item)}</span>
                  <span className="history-time">{formatHistoryTime(item.last_used_at)}</span>
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="history-footer">
        <span>点击条目回填查询条件，按 Enter 执行。</span>
      </div>
    </div>
  );
}

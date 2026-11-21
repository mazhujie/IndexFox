import { useEffect, useRef } from "react";
import type { KeyboardEvent as ReactKeyboardEvent } from "react";
import type { FileResult, SearchHit, SearchResponse } from "../types";
import {
  PAGE_SIZE,
  fileStateLabel,
  formatBytes,
  formatModified,
  locatorSummary,
} from "../types";
import { hitKey, literalRanges } from "../text";
import { HighlightedText } from "./HighlightedText";

interface ResultsPaneProps {
  response: SearchResponse | null;
  loading: boolean;
  searched: boolean;
  error: string | null;
  cancelled: boolean;
  selectedFileId: number | null;
  selectedHitKey: string | null;
  caseSensitive: boolean;
  pageIndex: number;
  onSelectFile: (file: FileResult, hit?: SearchHit) => void;
  onStepSelection: (delta: number) => void;
  onPageChange: (pageIndex: number) => void;
}

function hitKindLabel(hit: SearchHit): string {
  if (hit.kind === "filename") return "文件名";
  if (hit.kind === "ocr_body" || hit.origin === "ocr") return "OCR 正文";
  return "正文";
}

export function ResultsPane({
  response,
  loading,
  searched,
  error,
  cancelled,
  selectedFileId,
  selectedHitKey,
  caseSensitive,
  pageIndex,
  onSelectFile,
  onStepSelection,
  onPageChange,
}: ResultsPaneProps) {
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const list = listRef.current;
    if (!list || !list.contains(document.activeElement) || selectedFileId === null) return;
    const selected = list.querySelector<HTMLElement>(
      `[data-file-id="${selectedFileId}"] .result-select`,
    );
    selected?.focus();
  }, [selectedFileId]);

  const pageCount = response ? Math.max(1, Math.ceil(response.total_files / PAGE_SIZE)) : 1;
  const rangeStart = response && response.returned_files > 0 ? pageIndex * PAGE_SIZE + 1 : 0;
  const rangeEnd = response ? rangeStart + Math.max(0, response.returned_files - 1) : 0;

  function handleListKeyDown(event: ReactKeyboardEvent<HTMLElement>) {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      onStepSelection(event.key === "ArrowDown" ? 1 : -1);
    }
  }

  return (
    <section className="pane results-pane" aria-label="搜索结果">
      <div className="pane-header">
        <div>
          <h2>搜索结果</h2>
          <span className="pane-subtitle">
            {response
              ? `${response.total_files} 个文件 · ${response.total_hits} 处命中`
              : searched
                ? "等待结果"
                : "按 Enter 开始检索"}
          </span>
        </div>
        {response && (
          <span className="pill" title="查询耗时">
            {response.elapsed_ms} ms
          </span>
        )}
      </div>

      <div
        className="pane-body results-body"
        ref={listRef}
        onKeyDown={handleListKeyDown}
      >
        {!searched ? (
          <div className="pane-empty tall">
            <div className="empty-icon" aria-hidden="true">
              ⌕
            </div>
            <h3>本地全文检索</h3>
            <p>输入关键词或查询表达式，按 Enter 或点击“搜索”提交。</p>
            <ul className="hint-list">
              <li>
                <code>Linux 驱动</code> 隐式 AND
              </li>
              <li>
                <code>"Linux kernel"</code> 连续短语
              </li>
              <li>
                <code>Linux -Windows</code> 排除
              </li>
              <li>
                <code>(Linux OR Windows) 驱动</code> 分组
              </li>
            </ul>
          </div>
        ) : loading ? (
          <div className="loading-stack" role="status">
            <div className="loading-bar" />
            <div className="loading-bar short" />
            <div className="loading-bar" />
            <span className="muted">正在检索本地索引…</span>
          </div>
        ) : error ? (
          <div className="pane-empty" role="alert">
            <h3>检索失败</h3>
            <p className="error-text">{error}</p>
          </div>
        ) : cancelled ? (
          <div className="pane-empty" role="status">
            <h3>检索已取消</h3>
            <p>本地搜索任务已停止，可修改条件后重新提交。</p>
          </div>
        ) : !response ? (
          <div className="pane-empty">
            <h3>尚未获得结果</h3>
            <p>修改条件后按 Enter 或点击“搜索”重新提交。</p>
          </div>
        ) : response.results.length === 0 ? (
          <div className="pane-empty">
            <div className="empty-icon" aria-hidden="true">
              ∅
            </div>
            <h3>没有匹配文件</h3>
            <p>请检查表达式、大小写或格式过滤。</p>
            <p className="muted">超过大小上限或尚未成功索引的文件不会被标为全文命中。</p>
          </div>
        ) : (
          <>
            {response && (
              <div className="results-summary" aria-live="polite">
                <span>
                  第 {rangeStart}–{rangeEnd} 项，共 {response.total_files} 个文件
                </span>
                <span className="mono" title={response.query_plan}>
                  {response.query_plan.length > 56
                    ? `${response.query_plan.slice(0, 56)}…`
                    : response.query_plan}
                </span>
              </div>
            )}
            <ol className="result-list">
              {response?.results.map((file, index) => {
                const selected = file.file_id === selectedFileId;
                const stateLabel = fileStateLabel(file.state);
                return (
                  <li
                    key={file.file_id}
                    data-file-id={file.file_id}
                    className={`result-card ${selected ? "selected" : ""}`}
                  >
                    <button
                      type="button"
                      className="result-select"
                      onClick={() => onSelectFile(file)}
                      aria-current={selected ? "true" : undefined}
                    >
                      <span className="result-title-row">
                        <span className="result-title">
                          <span className="index-number">{index + 1 + pageIndex * PAGE_SIZE}</span>
                          <span className={`ext-badge ext-${file.ext.toLowerCase()}`}>
                            {file.ext || "file"}
                          </span>
                          <span className="file-title">{file.filename}</span>
                        </span>
                        <span className="pill accent">{file.hit_count} 命中</span>
                      </span>
                      <span className="file-path mono">{file.path}</span>
                      <span className="file-meta">
                        <span>{formatBytes(file.size_bytes)}</span>
                        <span>{formatModified(file.mtime_ns)}</span>
                        {stateLabel && <span className="tag tag-warn">{stateLabel}</span>}
                      </span>
                    </button>

                    <div className="snippet-list">
                      {file.hits.length === 0 ? (
                        <p className="snippet-empty">由排除表达式匹配，没有可定位的正文命中。</p>
                      ) : (
                        file.hits.map((hit) => {
                          const selectedHit = selected && hitKey(file.file_id, hit) === selectedHitKey;
                          const ranges = literalRanges(hit.snippet, hit.matched_text, caseSensitive);
                          return (
                            <button
                              key={hitKey(file.file_id, hit)}
                              type="button"
                              className={`snippet-item ${selectedHit ? "active" : ""}`}
                              onClick={() => onSelectFile(file, hit)}
                            >
                              <span className="snippet-locator">
                                <span className="snippet-locator-label">
                                  {hit.kind === "filename" ? "文件名" : locatorSummary(hit.locator)}
                                  {hit.crosses_chunk_end ? " · 跨分块" : ""}
                                </span>
                                <span className={`tag ${hit.origin === "ocr" ? "tag-ocr" : ""}`}>
                                  {hitKindLabel(hit)}
                                </span>
                                {hit.fuzzy && (
                                  <span className="tag" title="与查询词的编辑距离">
                                    容错 · 距离 {hit.edit_distance}
                                  </span>
                                )}
                              </span>
                              <span className="snippet-text mono">
                                <HighlightedText text={hit.snippet} ranges={ranges} />
                              </span>
                            </button>
                          );
                        })
                      )}
                    </div>
                  </li>
                );
              })}
            </ol>
          </>
        )}
      </div>

      {response && response.total_files > 0 && (
        <div className="pager" aria-label="结果分页">
          <button
            type="button"
            className="mini-button"
            onClick={() => onPageChange(pageIndex - 1)}
            disabled={loading || pageIndex <= 0}
          >
            ← 上一页
          </button>
          <span className="pager-status">
            第 {pageIndex + 1} / {pageCount} 页
          </span>
          <button
            type="button"
            className="mini-button"
            onClick={() => onPageChange(pageIndex + 1)}
            disabled={loading || pageIndex + 1 >= pageCount}
          >
            下一页 →
          </button>
        </div>
      )}
    </section>
  );
}

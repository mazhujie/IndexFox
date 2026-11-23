import { useEffect, useRef, useState } from "react";
import { api, errorMessage } from "../api";
import type {
  FileResult,
  PreviewState,
  SearchHit,
  SourceLocator,
} from "../types";
import {
  fileStateLabel,
  formatBytes,
  formatModified,
  locatorSummary,
  originLabel,
} from "../types";
import {
  charRangeForBytes,
  hitKey,
  lineHasActiveRange,
  rangesForLine,
  splitPreviewLines,
  type CharRange,
} from "../text";
import { HighlightedText } from "./HighlightedText";
import { PdfDocumentPreview } from "./PdfDocumentPreview";

interface PreviewPaneProps {
  selectedFile: FileResult | null;
  activeHit: SearchHit | null;
  selectedHitKey: string | null;
  preview: PreviewState;
  pdfView: "page" | "text";
  pdfNavigation: number;
  pdfFullscreen: boolean;
  fullscreenBusy: boolean;
  fullscreenError: string | null;
  onToggleFullscreen: () => void;
  onPdfViewChange: (view: "page" | "text") => void;
  onRetryPreview: () => void;
  onSelectHit: (hit: SearchHit) => void;
}

export function PreviewPane({
  selectedFile,
  activeHit,
  selectedHitKey,
  preview,
  pdfView,
  pdfNavigation,
  pdfFullscreen,
  fullscreenBusy,
  fullscreenError,
  onToggleFullscreen,
  onPdfViewChange,
  onRetryPreview,
  onSelectHit,
}: PreviewPaneProps) {
  const contentRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const container = contentRef.current;
    if (!container) return;
    const mark = container.querySelector<HTMLElement>('mark[data-active-hit="true"]');
    mark?.scrollIntoView({ block: "center", inline: "nearest" });
  }, [selectedHitKey, preview.data, preview.status, pdfView]);

  if (!selectedFile) {
    return (
      <section className="pane preview-pane" aria-label="文档预览">
        <div className="pane-header">
          <div>
            <h2>预览</h2>
            <span className="pane-subtitle">文本、代码与 PDF 原页</span>
          </div>
        </div>
        <div className="pane-body">
          <div className="pane-empty tall">
            <div className="empty-icon" aria-hidden="true">
              ◫
            </div>
            <h3>选择一条结果</h3>
            <p>右侧会显示缓存文本；PDF 同时支持真实页面渲染。</p>
          </div>
        </div>
      </section>
    );
  }

  const isPdf = selectedFile.ext.toLowerCase() === "pdf";
  const hits = selectedFile.hits;
  const hitIndex = Math.max(
    0,
    hits.findIndex((hit) => selectedHitKey === hitKey(selectedFile.file_id, hit)),
  );
  const stateLabel = fileStateLabel(selectedFile.state);
  const previewData = preview.status === "ready" ? preview.data : null;
  const cacheAvailable = Boolean(previewData?.text);
  const positionLabel = locatorSummary(activeHit?.locator ?? previewData?.locator ?? null);
  const ranges = buildPreviewRanges(
    selectedFile,
    previewData?.text ?? "",
    preview.status === "ready" ? preview.chunkId : null,
    selectedHitKey,
  );
  const activeMapped = ranges.some((range) => range.active);

  function stepHit(delta: number) {
    if (hits.length === 0) return;
    const next = (hitIndex + delta + hits.length) % hits.length;
    onSelectHit(hits[next]);
  }

  return (
    <section className="pane preview-pane" aria-label="文档预览">
      <div className="preview-toolbar">
        <div className="preview-title-wrap">
          <span className={`ext-badge ext-${selectedFile.ext.toLowerCase()}`}>
            {selectedFile.ext || "file"}
          </span>
          <div className="preview-title-text">
            <h2 title={selectedFile.filename}>{selectedFile.filename}</h2>
            <span className="mono muted">{selectedFile.path}</span>
          </div>
        </div>
        <div className="preview-actions">
          {navigator.userAgent.includes("Windows") && (
            <FileActions key={`${selectedFile.file_id}:${selectedFile.path}`} file={selectedFile} />
          )}
          {isPdf && (
            <div className="segmented" role="group" aria-label="PDF 预览方式">
              <button
                type="button"
                className={`seg-button ${pdfView === "page" ? "active" : ""}`}
                onClick={() => onPdfViewChange("page")}
                aria-pressed={pdfView === "page"}
              >
                PDF 原页
              </button>
              <button
                type="button"
                className={`seg-button ${pdfView === "text" ? "active" : ""}`}
                onClick={() => onPdfViewChange("text")}
                aria-pressed={pdfView === "text"}
              >
                文本命中
              </button>
            </div>
          )}
          {isPdf && pdfView === "page" && (
            <button
              type="button"
              className="mini-button"
              aria-pressed={pdfFullscreen}
              title={pdfFullscreen ? "按 Esc 退出全屏" : "将 PDF 显示在整个屏幕"}
              disabled={fullscreenBusy}
              onClick={onToggleFullscreen}
            >
              {pdfFullscreen ? "退出全屏" : "全屏"}
            </button>
          )}
          {hits.length > 0 && (
            <div className="hit-nav" role="group" aria-label="命中位置导航">
              <button
                type="button"
                className="mini-button"
                onClick={() => stepHit(-1)}
                aria-label="上一处命中"
              >
                ↑
              </button>
              <span className="hit-counter">
                {hitIndex + 1} / {hits.length}
              </span>
              <button
                type="button"
                className="mini-button"
                onClick={() => stepHit(1)}
                aria-label="下一处命中"
              >
                ↓
              </button>
            </div>
          )}
          {isPdf && fullscreenError && <p className="error-text" role="alert">{fullscreenError}</p>}
        </div>
      </div>

      <div className="preview-info">
        <span className="meta-item">
          <span className="meta-label">位置</span>
          <span>{positionLabel}</span>
        </span>
        <span className="meta-item">
          <span className="meta-label">文本来源</span>
          <span>{originLabel(previewData?.origin ?? activeHit?.origin ?? "native")}</span>
        </span>
        <span className="meta-item">
          <span className="meta-label">大小</span>
          <span>{formatBytes(selectedFile.size_bytes)}</span>
        </span>
        <span className="meta-item">
          <span className="meta-label">修改</span>
          <span>{formatModified(selectedFile.mtime_ns)}</span>
        </span>
        {stateLabel && <span className="tag tag-warn">{stateLabel}</span>}
      </div>

      {isPdf && pdfView === "page" ? (
        <PdfDocumentPreview
          key={`${selectedFile.file_id}:${selectedFile.path}:${selectedFile.mtime_ns}:${selectedFile.size_bytes}`}
          file={selectedFile}
          activeHit={activeHit}
          navigation={pdfNavigation}
          fullscreenTransition={fullscreenBusy}
          onOpenText={cacheAvailable ? () => onPdfViewChange("text") : undefined}
        />
      ) : (
        <div className="pane-body preview-body" ref={contentRef}>
          {preview.status === "loading" || preview.status === "idle" ? (
            <div className="preview-loading" role="status">
              <span className="spinner" aria-hidden="true" />
              正在读取本地缓存文本…
            </div>
          ) : preview.status === "error" ? (
            <div className="pane-empty" role="alert">
              <h3>预览加载失败</h3>
              <p className="error-text">{preview.error}</p>
              <button type="button" className="primary-button small" onClick={onRetryPreview}>
                重试
              </button>
            </div>
          ) : previewData?.text ? (
            <>
              {activeHit?.crosses_chunk_end && (
                <p className="cross-note">
                  命中跨越分块边界，仅高亮当前分块中可见的部分。
                </p>
              )}
              {!activeMapped &&
                activeHit &&
                activeHit.kind !== "filename" &&
                activeHit.chunk_id > 0 && (
                  <p className="cross-note warn">当前预览分块没有可高亮的可见范围。</p>
                )}
              {previewData.locator?.kind === "text" ? (
                <LinePreview
                  text={previewData.text}
                  startLine={previewData.locator.start_line}
                  endLine={previewData.locator.end_line}
                  ranges={ranges}
                />
              ) : (
                <TextSheet
                  text={previewData.text}
                  locator={previewData.locator}
                  ranges={ranges}
                  origin={previewData.origin}
                />
              )}
            </>
          ) : (
            <div className="pane-empty">
              <p>缓存文本为空。</p>
            </div>
          )}
        </div>
      )}
    </section>
  );
}

function FileActions({ file }: { file: FileResult }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function perform(action: "open" | "reveal") {
    setBusy(true);
    setError(null);
    try {
      await api.openIndexedFile(file.file_id, file.path, action);
    } catch (error) {
      setError(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="preview-file-actions">
      <button type="button" className="mini-button" disabled={busy} onClick={() => void perform("open")}>
        打开原文件
      </button>
      <button type="button" className="mini-button" disabled={busy} onClick={() => void perform("reveal")}>
        在资源管理器中显示
      </button>
      {error && <p className="error-text" role="alert">{error}</p>}
    </div>
  );
}

function buildPreviewRanges(
  file: FileResult,
  text: string,
  chunkId: number | null,
  selectedHitKey: string | null,
): CharRange[] {
  if (!text || chunkId === null) return [];
  const ranges: CharRange[] = [];
  for (const hit of file.hits) {
    if (hit.kind === "filename" || hit.chunk_id !== chunkId) continue;
    const range = charRangeForBytes(text, hit.start, hit.end);
    if (!range) continue;
    ranges.push({
      ...range,
      active: selectedHitKey === hitKey(file.file_id, hit),
    });
  }
  return ranges;
}

interface LinePreviewProps {
  text: string;
  startLine: number;
  endLine: number;
  ranges: CharRange[];
}

function LinePreview({ text, startLine, endLine, ranges }: LinePreviewProps) {
  const lines = splitPreviewLines(text);
  return (
    <div className="code-viewer" role="document" aria-label={`第 ${startLine} 至 ${endLine} 行文本预览`}>
      <div className="code-caption">
        <span>
          第 {startLine}–{endLine} 行
        </span>
      </div>
      <div className="code-lines">
        {lines.map((line, index) => {
          const lineNumber = startLine + index;
          const lineEnd = line.start + line.text.length;
          const lineRanges = rangesForLine(ranges, line.start, lineEnd);
          const activeLine = lineHasActiveRange(ranges, line.start, lineEnd);
          const hasHit = lineRanges.length > 0;
          return (
            <div
              key={`${lineNumber}-${line.start}`}
              className={`code-line ${hasHit ? "hit-line" : ""} ${activeLine ? "active-line" : ""}`}
            >
              <span className="line-number" aria-hidden="true">
                {lineNumber}
              </span>
              <span className="line-text">
                <HighlightedText text={line.text} ranges={lineRanges} />
                {line.text.length === 0 ? " " : null}
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}

interface TextSheetProps {
  text: string;
  locator: SourceLocator | null;
  ranges: CharRange[];
  origin: string;
}

function TextSheet({ text, locator, ranges, origin }: TextSheetProps) {
  const locatorLabel = locatorSummary(locator);
  return (
    <div className="text-sheet" role="document" aria-label={`${locatorLabel}文本预览`}>
      <div className="sheet-meta">
        <span>{locatorLabel}</span>
        <span>{originLabel(origin)}</span>
      </div>
      <div className="plain-text">
        <HighlightedText text={text} ranges={ranges} />
      </div>
    </div>
  );
}

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { FormEvent } from "react";
import { api, errorMessage } from "../api";
import type { FileResult, PdfPageSize, SearchHit } from "../types";

const PAGE_MARGIN = 12;
const PAGE_GAP = 34;
const EMPTY_VIEWPORT = { width: 0, height: 0, top: 0 };

// PDFium operations already run serially. Skip queued work for pages that
// have scrolled away before sending another request to the backend.
let pdfWork = Promise.resolve();

function queuePdfWork(work: () => Promise<void>) {
  pdfWork = pdfWork.then(work, work);
}

interface PagePosition {
  number: number;
  top: number;
  width: number;
  height: number;
}

function pagePositions(sizes: PdfPageSize[], viewportWidth: number): PagePosition[] {
  if (viewportWidth <= 0) return [];
  let top = 0;
  return sizes.map((size, index) => {
    const [pageWidth, pageHeight] = size ?? [595, 842];
    const ratio = pageHeight / pageWidth;
    const width = Math.min(Math.max(1, viewportWidth - 2 * PAGE_MARGIN), 1600, 16000 / ratio);
    const height = Math.max(1, Math.round(width * ratio));
    const bounds = { number: index + 1, top, width, height };
    top += height + PAGE_GAP;
    return bounds;
  });
}

function pageAtOffset(pages: PagePosition[], offset: number): number {
  let lo = 0;
  let hi = pages.length;
  while (lo < hi) {
    const mid = Math.floor((lo + hi) / 2);
    if (pages[mid].top <= offset) lo = mid + 1;
    else hi = mid;
  }
  return Math.max(0, lo - 1);
}

interface PdfDocumentPreviewProps {
  file: FileResult;
  activeHit: SearchHit | null;
  navigation: number;
  fullscreenTransition: boolean;
  onOpenText?: () => void;
}

export function PdfDocumentPreview({ file, activeHit, navigation, fullscreenTransition, onOpenText }: PdfDocumentPreviewProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [viewport, setViewport] = useState(EMPTY_VIEWPORT);
  const [sizes, setSizes] = useState<PdfPageSize[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const [pageDraft, setPageDraft] = useState<string | null>(null);
  const layout = useMemo(() => pagePositions(sizes, viewport.width), [sizes, viewport.width]);
  const previous = useRef<{ layout: PagePosition[]; height: number; navigation: number } | null>(null);
  const fullscreenAnchor = useRef<{ index: number; fraction: number } | null>(null);
  const hitPage = activeHit?.locator?.kind === "pdf_page" ? activeHit.locator.page : 1;
  const fileId = file.file_id;

  const hitsByPage = useMemo(() => {
    const hits = new Map<number, SearchHit>();
    for (const hit of file.hits) {
      if (hit.kind !== "filename" && hit.locator?.kind === "pdf_page" && !hits.has(hit.locator.page)) {
        hits.set(hit.locator.page, hit);
      }
    }
    if (activeHit && activeHit.locator?.kind === "pdf_page") hits.set(activeHit.locator.page, activeHit);
    return hits;
  }, [file.hits, activeHit]);

  useEffect(() => {
    let cancelled = false;
    setLoadError(null);
    queuePdfWork(async () => {
      if (cancelled) return;
      try {
        const pages = await api.pdfPreviewPages(fileId);
        if (!cancelled) setSizes(pages);
      } catch (error) {
        if (!cancelled) setLoadError(errorMessage(error));
      }
    });
    return () => { cancelled = true; };
  }, [fileId, retry]);

  const syncViewport = useCallback((preserveReadingPosition = true) => {
    const element = scrollRef.current;
    if (!element) return;
    const { clientWidth: width, clientHeight: height, scrollTop } = element;
    setViewport((current) => {
      if (current.width !== width || current.height !== height) {
        // The browser may clamp scrollTop during a resize. Re-anchor from
        // the old reading position once the new page layout is available.
        return { width, height, top: preserveReadingPosition ? current.top : scrollTop };
      }
      return current.top === scrollTop ? current : { width, height, top: scrollTop };
    });
  }, []);

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (!element) return;
    syncViewport();
    const observer = new ResizeObserver(() => syncViewport());
    observer.observe(element);
    return () => observer.disconnect();
  }, [syncViewport]);

  useLayoutEffect(() => {
    if (fullscreenTransition) {
      const pages = previous.current?.layout;
      const element = scrollRef.current;
      if (element && pages?.length) {
        const index = pageAtOffset(pages, element.scrollTop);
        const page = pages[index];
        fullscreenAnchor.current = {
          index,
          fraction: (element.scrollTop - page.top) / (page.height + PAGE_GAP),
        };
      }
      return;
    }
    // WebView2 can deliver the final resize after the native command returns.
    const timer = window.setTimeout(() => { fullscreenAnchor.current = null; }, 300);
    return () => window.clearTimeout(timer);
  }, [fullscreenTransition]);

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (!element || !layout.length) return;
    const old = previous.current;
    if (!old || old.navigation !== navigation) {
      fullscreenAnchor.current = null;
      element.scrollTop = layout[Math.max(1, Math.min(hitPage, layout.length)) - 1].top;
      setPageDraft(null);
    } else if (old.layout !== layout || old.height !== viewport.height) {
      const anchor = fullscreenAnchor.current;
      const index = Math.min(anchor?.index ?? pageAtOffset(old.layout, viewport.top), layout.length - 1);
      const page = old.layout[index];
      const offset = anchor?.fraction ?? (viewport.top - page.top) / (page.height + PAGE_GAP);
      element.scrollTop = layout[index].top + offset * (layout[index].height + PAGE_GAP);
    } else {
      return;
    }
    previous.current = { layout, height: viewport.height, navigation };
    syncViewport(false);
  }, [hitPage, layout, navigation, syncViewport, viewport.height, viewport.top]);

  function submitPage(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const page = Number.parseInt(pageDraft ?? "", 10);
    if (scrollRef.current && Number.isFinite(page) && layout.length > 0) {
      fullscreenAnchor.current = null;
      scrollRef.current.scrollTop = layout[Math.max(1, Math.min(page, layout.length)) - 1].top;
      syncViewport(false);
    }
    setPageDraft(null);
  }

  const current = pageAtOffset(layout, viewport.top + 1);
  const lastVisible = pageAtOffset(layout, viewport.top + viewport.height);
  const nearby = layout.slice(current, lastVisible + 2);
  if (current > 0) nearby.push(layout[current - 1]);
  const lastPage = layout[layout.length - 1];
  const scrollHeight = lastPage
    ? lastPage.top + Math.max(lastPage.height + PAGE_GAP, viewport.height)
    : 0;

  return (
    <>
      {sizes.length > 0 && (
        <div className="pdf-toolbar">
          <form className="pdf-page-form" onSubmit={submitPage}>
            <label htmlFor="pdf-page-number">页码</label>
            <input
              id="pdf-page-number"
              className="text-input page-input mono"
              inputMode="numeric"
              value={pageDraft ?? String(current + 1)}
              onChange={(event) => setPageDraft(event.target.value.replace(/[^\d]/g, ""))}
              aria-label="PDF 页码"
            />
            <button type="submit" className="mini-button">跳转</button>
          </form>
          <span className="muted">第 {current + 1} / {sizes.length} 页</span>
          {hitsByPage.get(current + 1)?.kind === "ocr_body" && (
            <span className="form-hint">扫描版原页暂不支持高亮，可切换到「文本命中」。</span>
          )}
        </div>
      )}
      <div
        className="pane-body preview-body pdf-body"
        ref={scrollRef}
        onScroll={() => syncViewport()}
        onWheel={() => { fullscreenAnchor.current = null; }}
        tabIndex={0}
        role="region"
        aria-label="PDF 原页连续预览"
      >
        {loadError ? (
          <PdfPreviewError message={loadError} onRetry={() => setRetry((value) => value + 1)} onOpenText={onOpenText} />
        ) : !sizes.length ? (
          <div className="pdf-loading" role="status">
            <span className="spinner" aria-hidden="true" />
            正在读取 PDF 页面…
          </div>
        ) : (
          <div className="pdf-pages" style={{ height: scrollHeight }}>
            {nearby.map((page) => {
              const hit = hitsByPage.get(page.number);
              const highlight = hit?.kind === "body" ? hit.matched_text : null;
              return (
                <PdfPage
                  key={JSON.stringify([page.number, highlight])}
                  fileId={fileId}
                  filename={file.filename}
                  page={page}
                  highlight={highlight}
                  onOpenText={onOpenText}
                />
              );
            })}
          </div>
        )}
      </div>
    </>
  );
}

function PdfPage({ fileId, filename, page, highlight, onOpenText }: {
  fileId: number;
  filename: string;
  page: PagePosition;
  highlight: string | null;
  onOpenText?: () => void;
}) {
  const [result, setResult] = useState<{ url: string } | { error: string } | null>(null);
  const [retry, setRetry] = useState(0);
  const pageNumber = page.number;

  useEffect(() => {
    let cancelled = false;
    let url: string | null = null;
    setResult(null);
    queuePdfWork(async () => {
      if (cancelled) return;
      try {
        const bytes = await api.renderPdfPage(fileId, pageNumber, highlight);
        if (cancelled) return;
        url = URL.createObjectURL(new Blob([new Uint8Array(bytes).buffer], { type: "image/png" }));
        setResult({ url });
      } catch (error) {
        if (!cancelled) setResult({ error: errorMessage(error) });
      }
    });
    return () => {
      cancelled = true;
      if (url) URL.revokeObjectURL(url);
    };
  }, [fileId, highlight, pageNumber, retry]);

  return (
    <figure className="pdf-figure" data-page={pageNumber} style={{ top: page.top, width: page.width }}>
      <div className="pdf-page-frame" style={{ height: page.height }}>
        {result === null ? (
          <div className="pdf-loading" role="status">
            <span className="spinner" aria-hidden="true" />
            正在渲染第 {pageNumber} 页…
          </div>
        ) : "error" in result ? (
          <PdfPreviewError message={result.error} onRetry={() => setRetry((value) => value + 1)} onOpenText={onOpenText} />
        ) : (
          <img className="pdf-page-image" src={result.url} alt={`${filename} 第 ${pageNumber} 页`} />
        )}
      </div>
      <figcaption>第 {pageNumber} 页</figcaption>
    </figure>
  );
}

function PdfPreviewError({ message, onRetry, onOpenText }: {
  message: string;
  onRetry: () => void;
  onOpenText?: () => void;
}) {
  return (
    <div className="pdf-error" role="alert">
      <h3>PDF 预览失败</h3>
      <p className="error-text">{message}</p>
      <div className="pdf-error-actions">
        <button type="button" className="primary-button small" onClick={onRetry}>重试</button>
        {onOpenText && <button type="button" className="mini-button" onClick={onOpenText}>查看缓存文本</button>}
      </div>
    </div>
  );
}

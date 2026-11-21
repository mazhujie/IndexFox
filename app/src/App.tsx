import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { FormEvent, KeyboardEvent as ReactKeyboardEvent } from "react";
import { api, errorMessage, parseErrorPosition } from "./api";
import indexFoxIcon from "./assets/indexfox-icon.png";
import { HistoryPopover } from "./components/HistoryPopover";
import { PreviewPane } from "./components/PreviewPane";
import { ResultsPane } from "./components/ResultsPane";
import { SettingsDialog } from "./components/SettingsDialog";
import { SourcesPane } from "./components/SourcesPane";
import { loadAndApplyDesktopSettings, settingsApi } from "./settings";
import type { IndexRuntime } from "./settings";
import { hitKey, queryErrorContext } from "./text";
import type {
  DraftSearch,
  FileResult,
  HistoryItem,
  HistorySettings,
  PdfPageState,
  PreviewState,
  SearchHit,
  SearchRequest,
  SearchResponse,
  SearchSnapshot,
  SortKey,
  SourceInfo,
} from "./types";
import {
  DEFAULT_SORT,
  EXTENSION_PRESETS,
  PAGE_SIZE,
  SORT_OPTIONS,
  arraysEqual,
  normalizeMode,
  parseHistoryFilters,
  sameSnapshot,
} from "./types";

const INITIAL_DRAFT: DraftSearch = {
  query: "",
  mode: "exact",
  caseSensitive: false,
  sort: DEFAULT_SORT,
  sourceId: null,
  extensions: [],
};

const INITIAL_SNAPSHOT: SearchSnapshot = {
  query: "",
  mode: "exact",
  caseSensitive: false,
  sort: DEFAULT_SORT,
  sourceId: null,
  extensions: [],
};

const IDLE_PREVIEW: PreviewState = {
  status: "idle",
  data: null,
  chunkId: null,
  error: null,
};

const IDLE_PDF: PdfPageState = {
  status: "idle",
  url: null,
  page: null,
  error: null,
};

function snapshotOf(draft: DraftSearch): SearchSnapshot {
  return {
    query: draft.query,
    mode: draft.mode,
    caseSensitive: draft.caseSensitive,
    sort: draft.sort,
    sourceId: draft.sourceId,
    extensions: [...draft.extensions],
  };
}

function firstHit(file: FileResult): SearchHit | null {
  return file.hits.find((hit) => hit.kind !== "filename" && hit.chunk_id > 0) ?? file.hits[0] ?? null;
}

function pdfLocatorPage(hit: SearchHit | null, preview: PreviewState): number {
  if (hit?.locator?.kind === "pdf_page") return hit.locator.page;
  if (preview.data?.locator?.kind === "pdf_page") return preview.data.locator.page;
  return 1;
}

function copyBytes(bytes: Uint8Array): ArrayBuffer {
  const copy = new Uint8Array(bytes.byteLength);
  copy.set(bytes);
  return copy.buffer;
}

export default function App() {
  const [draft, setDraft] = useState<DraftSearch>(INITIAL_DRAFT);
  const [committed, setCommitted] = useState<SearchSnapshot | null>(null);
  const [response, setResponse] = useState<SearchResponse | null>(null);
  const [searched, setSearched] = useState(false);
  const [searchLoading, setSearchLoading] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);
  const [queryError, setQueryError] = useState<{
    message: string;
    position: number | null;
    query: string;
  } | null>(null);
  const [cancelled, setCancelled] = useState(false);
  const [pageIndex, setPageIndex] = useState(0);

  const [sources, setSources] = useState<SourceInfo[]>([]);
  const [sourceError, setSourceError] = useState<string | null>(null);
  const [addOpen, setAddOpen] = useState(false);
  const [sourcePath, setSourcePath] = useState("");
  const [sourceBusy, setSourceBusy] = useState(false);
  const [indexError, setIndexError] = useState<string | null>(null);
  const [runtime, setRuntime] = useState<IndexRuntime | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);

  const [history, setHistory] = useState<HistorySettings>({ enabled: true, items: [] });
  const [historyOpen, setHistoryOpen] = useState(false);
  const [historyBusy, setHistoryBusy] = useState(false);
  const [historyError, setHistoryError] = useState<string | null>(null);

  const [selectedFileId, setSelectedFileId] = useState<number | null>(null);
  const [selectedHitKey, setSelectedHitKey] = useState<string | null>(null);
  const [preview, setPreview] = useState<PreviewState>(IDLE_PREVIEW);
  const [previewRetry, setPreviewRetry] = useState(0);
  const [pdf, setPdf] = useState<PdfPageState>(IDLE_PDF);
  const [pdfRetry, setPdfRetry] = useState(0);
  const [pdfView, setPdfView] = useState<"page" | "text">("page");
  const [pdfPageOverride, setPdfPageOverride] = useState<number | null>(null);

  const sourceListTokenRef = useRef(0);
  const requestCounterRef = useRef(1);
  const activeRequestRef = useRef<{
    id: number;
    pending: boolean;
    cancelled: boolean;
  } | null>(null);
  const previewTokenRef = useRef(0);
  const pdfTokenRef = useRef(0);
  const composingRef = useRef(false);
  const compositionEndedAtRef = useRef(0);
  const inputRef = useRef<HTMLInputElement>(null);

  const loadSources = useCallback(async () => {
    const token = sourceListTokenRef.current + 1;
    sourceListTokenRef.current = token;
    try {
      const list = await api.listSources();
      if (sourceListTokenRef.current !== token) return;
      setSources(list);
      setSourceError(null);
    } catch (error) {
      if (sourceListTokenRef.current !== token) return;
      setSourceError(`来源列表加载失败：${errorMessage(error)}`);
    }
  }, []);

  const loadHistory = useCallback(async () => {
    try {
      const settings = await api.historySettings();
      setHistory(settings);
      setHistoryError(null);
    } catch (error) {
      setHistoryError(`搜索历史加载失败：${errorMessage(error)}`);
    }
  }, []);

  useEffect(() => {
    let refreshing = false;
    async function refresh() {
      if (refreshing) return;
      refreshing = true;
      try {
        await Promise.all([loadSources(), settingsApi.getIndexRuntime().then(setRuntime)]);
      } catch {
        // The local desktop backend may still be starting.
      } finally {
        refreshing = false;
      }
    }
    void refresh();
    void loadHistory();
    void loadAndApplyDesktopSettings();
    const timer = window.setInterval(() => void refresh(), 1500);
    return () => window.clearInterval(timer);
  }, [loadSources, loadHistory]);

  const selectedFile = useMemo(
    () => response?.results.find((file) => file.file_id === selectedFileId) ?? null,
    [response, selectedFileId],
  );

  const activeHit = useMemo(() => {
    if (!selectedFile || selectedHitKey === null) return null;
    return (
      selectedFile.hits.find((hit) => hitKey(selectedFile.file_id, hit) === selectedHitKey) ?? null
    );
  }, [selectedFile, selectedHitKey]);

  const previewChunkId =
    activeHit && activeHit.kind !== "filename" && activeHit.chunk_id > 0
      ? activeHit.chunk_id
      : null;
  const selectedFileIdValue = selectedFile?.file_id ?? null;
  const isPdf = selectedFile?.ext.toLowerCase() === "pdf";
  const pdfPage = pdfPageOverride ?? pdfLocatorPage(activeHit, preview);

  useEffect(() => {
    const token = previewTokenRef.current + 1;
    previewTokenRef.current = token;
    if (selectedFileIdValue === null) {
      setPreview(IDLE_PREVIEW);
      return;
    }
    let cancelledRequest = false;
    setPreview({ status: "loading", data: null, chunkId: previewChunkId, error: null });
    api
      .previewText(selectedFileIdValue, previewChunkId)
      .then((data) => {
        if (cancelledRequest || previewTokenRef.current !== token) return;
        setPreview({ status: "ready", data, chunkId: previewChunkId, error: null });
      })
      .catch((error) => {
        if (cancelledRequest || previewTokenRef.current !== token) return;
        setPreview({
          status: "error",
          data: null,
          chunkId: previewChunkId,
          error: errorMessage(error),
        });
      });
    return () => {
      cancelledRequest = true;
    };
  }, [selectedFileIdValue, previewChunkId, previewRetry]);

  useEffect(() => {
    const token = pdfTokenRef.current + 1;
    pdfTokenRef.current = token;
    if (selectedFileIdValue === null || !isPdf || pdfView !== "page") {
      setPdf(IDLE_PDF);
      return;
    }
    let cancelledRequest = false;
    let objectUrl: string | null = null;
    setPdf({ status: "loading", url: null, page: pdfPage, error: null });
    api
      .renderPdfPage(selectedFileIdValue, pdfPage)
      .then((bytes) => {
        if (cancelledRequest || pdfTokenRef.current !== token) return;
        const blob = new Blob([copyBytes(bytes)], { type: "image/png" });
        objectUrl = URL.createObjectURL(blob);
        setPdf({ status: "ready", url: objectUrl, page: pdfPage, error: null });
      })
      .catch((error) => {
        if (cancelledRequest || pdfTokenRef.current !== token) return;
        setPdf({
          status: "error",
          url: null,
          page: pdfPage,
          error: errorMessage(error),
        });
      });
    return () => {
      cancelledRequest = true;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [selectedFileIdValue, isPdf, pdfView, pdfPage, pdfRetry]);

  function patchDraft(patch: Partial<DraftSearch>) {
    setDraft((previous) => ({ ...previous, ...patch }));
    setQueryError(null);
    setSearchError(null);
    setCancelled(false);
  }

  function applySelection(file: FileResult | null, preferredHit?: SearchHit) {
    if (!file) {
      setSelectedFileId(null);
      setSelectedHitKey(null);
      return;
    }
    const changedFile = file.file_id !== selectedFileId;
    const hit = preferredHit ?? firstHit(file);
    setSelectedFileId(file.file_id);
    setSelectedHitKey(hit ? hitKey(file.file_id, hit) : null);
    setPdfPageOverride(null);
    if (changedFile) setPdfView("page");
  }

  function cancelActiveRequest() {
    const active = activeRequestRef.current;
    if (!active?.pending) return;
    active.cancelled = true;
    active.pending = false;
    void api.cancelSearch(active.id).catch(() => undefined);
  }

  async function runSearch(
    snapshot: SearchSnapshot,
    offset: number,
    recordHistory: boolean,
    nextPage: number,
  ) {
    cancelActiveRequest();
    const requestId = requestCounterRef.current;
    requestCounterRef.current += 1;
    activeRequestRef.current = { id: requestId, pending: true, cancelled: false };
    setCommitted(snapshot);
    setPageIndex(nextPage);
    setSearched(true);
    setSearchLoading(true);
    setSearchError(null);
    setQueryError(null);
    setCancelled(false);
    setResponse(null);
    setSelectedFileId(null);
    setSelectedHitKey(null);
    setPreview(IDLE_PREVIEW);
    setPdf(IDLE_PDF);
    setPdfPageOverride(null);

    const request: SearchRequest = {
      requestId,
      query: snapshot.query,
      mode: snapshot.mode,
      caseSensitive: snapshot.caseSensitive,
      sort: snapshot.sort,
      limit: PAGE_SIZE,
      offset,
      sourceId: snapshot.sourceId,
      extensions: snapshot.extensions,
      recordHistory,
    };

    try {
      const result = await api.searchDocuments(request);
      const active = activeRequestRef.current;
      if (!active || active.id !== requestId || active.cancelled) return;
      active.pending = false;
      setResponse(result);
      applySelection(result.results[0] ?? null);
      setSearchLoading(false);
      if (recordHistory) void loadHistory();
    } catch (error) {
      const active = activeRequestRef.current;
      if (!active || active.id !== requestId || active.cancelled) return;
      active.pending = false;
      const message = errorMessage(error);
      setSearchError(message);
      const position = parseErrorPosition(message);
      if (position !== null) {
        setQueryError({ message, position, query: snapshot.query });
      }
      setSearchLoading(false);
    }
  }

  function submitDraft() {
    if (composingRef.current || Date.now() - compositionEndedAtRef.current < 40) return;
    if (!draft.query.trim()) {
      setSearchError("请输入检索内容后再提交。");
      setQueryError(null);
      inputRef.current?.focus();
      return;
    }
    void runSearch(snapshotOf(draft), 0, history.enabled, 0);
  }

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    submitDraft();
  }

  function handleCancel() {
    const active = activeRequestRef.current;
    if (!active?.pending) return;
    active.cancelled = true;
    active.pending = false;
    void api.cancelSearch(active.id).catch(() => undefined);
    setSearchLoading(false);
    setCancelled(true);
  }

  function handleInputKeyDown(event: ReactKeyboardEvent<HTMLInputElement>) {
    const native = event.nativeEvent;
    if (native.isComposing || native.keyCode === 229) {
      if (event.key === "Enter") event.preventDefault();
      return;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      if (!response || response.results.length === 0) return;
      event.preventDefault();
      stepSelection(event.key === "ArrowDown" ? 1 : -1);
    }
  }

  function stepSelection(delta: number) {
    if (!response || response.results.length === 0) return;
    const currentIndex = response.results.findIndex((file) => file.file_id === selectedFileId);
    const base = currentIndex === -1 ? (delta > 0 ? -1 : 0) : currentIndex;
    const nextIndex = Math.max(0, Math.min(response.results.length - 1, base + delta));
    applySelection(response.results[nextIndex]);
  }

  function handlePageChange(nextPage: number) {
    if (!committed || !response) return;
    const pageCount = Math.max(1, Math.ceil(response.total_files / PAGE_SIZE));
    if (nextPage < 0 || nextPage >= pageCount) return;
    void runSearch(committed, nextPage * PAGE_SIZE, false, nextPage);
  }

  function selectSource(sourceId: number | null) {
    patchDraft({ sourceId });
  }

  async function addSource(manualPath?: string) {
    if (sourceBusy) return;
    setSourceBusy(true);
    setIndexError(null);
    try {
      const path = manualPath ?? (await api.pickIndexFolder());
      if (path === null) return;
      await api.indexFolder(path);
      setSourcePath("");
      setAddOpen(false);
      await loadSources();
    } catch (error) {
      setIndexError(`目录索引失败：${errorMessage(error)}`);
    } finally {
      setSourceBusy(false);
    }
  }

  async function handleRemoveSource(source: SourceInfo) {
    if (sourceBusy) return;
    if (
      !window.confirm(
        `移除索引目录“${source.root_path}”？\n将清理该目录的索引和任务，保留原始文件。`,
      )
    ) {
      return;
    }
    setSourceBusy(true);
    setIndexError(null);
    try {
      await api.removeSource(source.id);
      setSources((previous) => previous.filter((item) => item.id !== source.id));
      setDraft((previous) => previous.sourceId === source.id ? { ...previous, sourceId: null } : previous);
      cancelActiveRequest();
      setCommitted(null);
      setResponse(null);
      setSearched(false);
      setSearchLoading(false);
      setSearchError(null);
      setQueryError(null);
      setCancelled(false);
      applySelection(null);
      await loadSources();
    } catch (error) {
      setIndexError(`移除目录失败：${errorMessage(error)}`);
    } finally {
      setSourceBusy(false);
    }
  }

  async function handleHistoryToggle(enabled: boolean) {
    setHistoryBusy(true);
    setHistoryError(null);
    try {
      await api.setHistoryEnabled(enabled);
      setHistory((previous) => ({ ...previous, enabled }));
    } catch (error) {
      setHistoryError(`历史设置失败：${errorMessage(error)}`);
    } finally {
      setHistoryBusy(false);
    }
  }

  async function handleHistoryClear() {
    setHistoryBusy(true);
    setHistoryError(null);
    try {
      await api.clearHistory();
      setHistory((previous) => ({ ...previous, items: [] }));
    } catch (error) {
      setHistoryError(`清空历史失败：${errorMessage(error)}`);
    } finally {
      setHistoryBusy(false);
    }
  }

  function handleHistoryApply(item: HistoryItem) {
    const filters = parseHistoryFilters(item.filters_json);
    const requestedSourceId = filters.sourceId ?? null;
    setDraft({
      query: item.query_text,
      mode: normalizeMode(item.search_mode),
      caseSensitive: filters.caseSensitive ?? false,
      sort: (filters.sort ?? DEFAULT_SORT) as SortKey,
      sourceId:
        requestedSourceId !== null && sources.some((source) => source.id === requestedSourceId)
          ? requestedSourceId
          : null,
      extensions: filters.extensions ?? [],
    });
    setHistoryOpen(false);
    setQueryError(null);
    setSearchError(null);
    window.setTimeout(() => inputRef.current?.focus(), 0);
  }

  const dirty = !sameSnapshot(draft, committed ?? INITIAL_SNAPSHOT);
  const pageCount = response ? Math.max(1, Math.ceil(response.total_files / PAGE_SIZE)) : 1;
  const activePreset = EXTENSION_PRESETS.find((preset) =>
    arraysEqual(preset.extensions, draft.extensions),
  );
  const errorContext =
    queryError && queryError.position !== null
      ? queryErrorContext(queryError.query, queryError.position)
      : null;

  return (
    <div className="app-shell">
      <section className="search-header" aria-label="检索条件">
        <form className="search-form" onSubmit={handleSubmit} role="search">
          <img className="search-brand-mark" src={indexFoxIcon} alt="" aria-hidden="true" />
          <div className="search-input-wrap">
            <span className="search-icon" aria-hidden="true">
              ⌕
            </span>
            <input
              ref={inputRef}
              className={`search-input ${dirty ? "dirty" : ""} ${queryError ? "has-error" : ""}`}
              type="text"
              value={draft.query}
              onChange={(event) => patchDraft({ query: event.target.value })}
              onKeyDown={handleInputKeyDown}
              onCompositionStart={() => {
                composingRef.current = true;
              }}
              onCompositionEnd={() => {
                composingRef.current = false;
                compositionEndedAtRef.current = Date.now();
              }}
              placeholder={'输入关键词或表达式：AND / OR / -排除 / "短语" / (分组)，按 Enter 提交'}
              aria-label="搜索查询"
              autoComplete="off"
              spellCheck={false}
            />
            <div className="search-inline">
              {dirty && <span className="dirty-hint">未提交</span>}
              <button
                type="button"
                className="inline-button"
                data-history-trigger="true"
                aria-expanded={historyOpen}
                onClick={() => {
                  setHistoryOpen((open) => !open);
                  setHistoryError(null);
                }}
              >
                历史 {history.items.length}
              </button>
              {draft.query && (
                <button
                  type="button"
                  className="inline-button icon"
                  aria-label="清空查询"
                  onClick={() => {
                    patchDraft({ query: "" });
                    inputRef.current?.focus();
                  }}
                >
                  ✕
                </button>
              )}
            </div>
            {historyOpen && (
              <HistoryPopover
                settings={history}
                busy={historyBusy}
                error={historyError}
                onClose={() => setHistoryOpen(false)}
                onToggleEnabled={(enabled) => void handleHistoryToggle(enabled)}
                onClear={() => void handleHistoryClear()}
                onApply={handleHistoryApply}
              />
            )}
          </div>
          <button type="submit" className="primary-button search-submit">
            搜索 <kbd>Enter</kbd>
          </button>
          {searchLoading && (
            <button type="button" className="secondary-button" onClick={handleCancel}>
              取消
            </button>
          )}
          <button
            type="button"
            className="mini-button search-settings"
            onClick={() => setSettingsOpen(true)}
          >
            设置
          </button>
        </form>

        <div className="search-controls">
          <div className="control-group">
            <span className="control-label">模式</span>
            <div className="segmented" role="group" aria-label="检索模式">
              <button
                type="button"
                className={`seg-button ${draft.mode === "exact" ? "active" : ""}`}
                aria-pressed={draft.mode === "exact"}
                onClick={() => patchDraft({ mode: "exact" })}
              >
                精确
              </button>
              <button
                type="button"
                className={`seg-button ${draft.mode === "substring" ? "active" : ""}`}
                aria-pressed={draft.mode === "substring"}
                onClick={() => patchDraft({ mode: "substring" })}
              >
                子串
              </button>
              <button
                type="button"
                className={`seg-button ${draft.mode === "fuzzy" ? "active" : ""}`}
                aria-pressed={draft.mode === "fuzzy"}
                onClick={() => patchDraft({ mode: "fuzzy" })}
              >
                拼写容错
              </button>
            </div>
            <label className="check-label">
              <input
                type="checkbox"
                checked={draft.caseSensitive}
                onChange={(event) => patchDraft({ caseSensitive: event.target.checked })}
              />
              区分大小写
            </label>
          </div>

          <div className="control-group">
            <span className="control-label">格式</span>
            <div className="pill-group" role="group" aria-label="格式过滤">
              {EXTENSION_PRESETS.map((preset) => {
                const selected = activePreset?.key === preset.key;
                return (
                  <button
                    key={preset.key}
                    type="button"
                    className={`filter-pill ${selected ? "active" : ""}`}
                    aria-pressed={selected}
                    onClick={() => patchDraft({ extensions: [...preset.extensions] })}
                  >
                    {preset.label}
                  </button>
                );
              })}
            </div>
          </div>

          <div className="control-group">
            <label className="control-label" htmlFor="sort-select">
              排序
            </label>
            <select
              id="sort-select"
              className="select-box"
              value={draft.sort}
              onChange={(event) => patchDraft({ sort: event.target.value as SortKey })}
            >
              {SORT_OPTIONS.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
          </div>
        </div>

        {queryError ? (
          <div className="query-error" role="alert">
            <div className="query-error-title">查询语法错误</div>
            <div className="query-error-message">{queryError.message}</div>
            {errorContext && (
              <code className="query-error-code">
                {errorContext.before}
                {errorContext.at ? (
                  <span className="error-char">{errorContext.at}</span>
                ) : (
                  <span className="error-char">␄</span>
                )}
                {errorContext.after}
              </code>
            )}
            <span className="muted">
              位置按 UTF-8 字节报告
              {queryError.position !== null ? `（第 ${queryError.position} 字节）` : ""}。
            </span>
          </div>
        ) : searchError ? (
          <div className="search-banner error" role="alert">
            {searchError}
          </div>
        ) : response ? (
          <div className="search-banner">
            <span className="plan-label">查询计划</span>
            <code className="plan-code">{response.query_plan}</code>
            <span className="muted">
              {response.mode === "fuzzy"
                ? "拼写容错，严格命中优先"
                : response.mode === "exact"
                  ? "精确连续匹配"
                  : "任意位置子串"} ·{" "}
              {committed?.caseSensitive ? "区分大小写" : "ASCII 大小写不敏感"} · 结果按已提交条件分页
            </span>
          </div>
        ) : cancelled ? (
          <div className="search-banner" role="status">
            检索已取消，本地任务已停止。
          </div>
        ) : (
          <div className="search-banner muted">
            只在按 Enter 或点击“搜索”时提交；输入与筛选变更不会触发实时检索。
          </div>
        )}
      </section>

      <main className="workspace">
        <SourcesPane
          sources={sources}
          sourceError={sourceError}
          selectedSourceId={draft.sourceId}
          onSelectSource={selectSource}
          addOpen={addOpen}
          onToggleAdd={() => {
            setAddOpen((open) => !open);
            setIndexError(null);
          }}
          sourcePath={sourcePath}
          onSourcePathChange={setSourcePath}
          onSubmitPath={(event) => {
            event.preventDefault();
            const path = sourcePath.trim();
            if (!path) {
              setIndexError("请输入目录绝对路径。");
              return;
            }
            void addSource(path);
          }}
          onPickFolder={() => void addSource()}
          onRemoveSource={(source) => void handleRemoveSource(source)}
          busy={sourceBusy}
          indexError={indexError}
          runtime={runtime}
          onReload={() => void loadSources()}
        />
        <ResultsPane
          response={response}
          loading={searchLoading}
          searched={searched}
          error={searchError}
          cancelled={cancelled}
          selectedFileId={selectedFileId}
          selectedHitKey={selectedHitKey}
          caseSensitive={committed?.caseSensitive ?? draft.caseSensitive}
          pageIndex={pageIndex}
          onSelectFile={(file, hit) => applySelection(file, hit)}
          onStepSelection={stepSelection}
          onPageChange={handlePageChange}
        />
        <PreviewPane
          selectedFile={selectedFile}
          activeHit={activeHit}
          selectedHitKey={selectedHitKey}
          preview={preview}
          pdf={pdf}
          pdfView={pdfView}
          pdfPage={pdfPage}
          onPdfViewChange={setPdfView}
          onPdfPageChange={(page) => setPdfPageOverride(page)}
          onRetryPreview={() => setPreviewRetry((value) => value + 1)}
          onRetryPdf={() => setPdfRetry((value) => value + 1)}
          onSelectHit={(hit) => applySelection(selectedFile, hit)}
        />
      </main>

      <footer className="status-bar">
        <div className="status-group">
          {searchLoading ? (
            <>
              <span className="spinner small" aria-hidden="true" />
              <span>本地检索中…</span>
            </>
          ) : response ? (
            <>
              <span>
                找到 <strong>{response.total_files}</strong> 个文件
              </span>
              <span className="status-sep">|</span>
              <span>
                本页 {response.returned_files} 个 · 共 {response.total_hits} 处命中
              </span>
              <span className="status-sep">|</span>
              <span>
                第 {pageIndex + 1} / {pageCount} 页
              </span>
              <span className="status-sep">|</span>
              <span>检索耗时 {response.elapsed_ms} ms</span>
            </>
          ) : (
            <span>{searched ? "就绪" : "等待检索"}</span>
          )}
        </div>
        <div className="status-group">
          <span>SQLite 本地索引</span>
          <span className="status-sep">|</span>
          <span>不发起网络请求</span>
        </div>
      </footer>
      {settingsOpen && (
        <SettingsDialog
          sources={sources}
          onClose={() => setSettingsOpen(false)}
          onChanged={() => void loadSources()}
        />
      )}
    </div>
  );
}

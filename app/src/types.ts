export type SearchMode = "exact" | "substring" | "fuzzy";

export type SortKey =
  | "relevance"
  | "modified_time"
  | "size"
  | "path"
  | "filename"
  | "hit_count";

export type HitKind = "filename" | "body" | "ocr_body" | string;

export type SourceLocator =
  | {
      kind: "text";
      start_line: number;
      end_line: number;
      start_offset: number;
      end_offset: number;
    }
  | { kind: "pdf_page"; page: number; bbox?: number[] | null }
  | { kind: "docx_paragraph"; paragraph: number }
  | {
      kind: "docx_table_cell";
      table: number;
      row: number;
      column: number;
      paragraph: number;
    };

export interface SearchRequest {
  requestId: number;
  query: string;
  mode: SearchMode;
  caseSensitive: boolean;
  sort: SortKey;
  limit: number;
  offset: number;
  sourceId: number | null;
  extensions: string[];
  recordHistory: boolean;
}

export interface SearchHit {
  chunk_id: number;
  ordinal: number;
  locator: SourceLocator | null;
  origin: string;
  start: number;
  end: number;
  matched_text: string;
  snippet: string;
  kind: HitKind;
  crosses_chunk_end: boolean;
  fuzzy?: boolean;
  edit_distance?: number | null;
}

export interface FileResult {
  file_id: number;
  path: string;
  filename: string;
  ext: string;
  size_bytes: number;
  mtime_ns: number | null;
  state: string;
  score: number;
  hit_count: number;
  hits: SearchHit[];
  strict?: boolean;
}

export interface SearchResponse {
  query_plan: string;
  mode: string;
  total_files: number;
  returned_files: number;
  total_hits: number;
  elapsed_ms: number;
  results: FileResult[];
}

export interface SourceInfo {
  id: number;
  root_path: string;
  online_state: string;
  enabled: boolean;
}

export interface PreviewText {
  text: string;
  locator: SourceLocator | null;
  origin: string;
}

export interface HistoryFilters {
  caseSensitive: boolean;
  sort: SortKey;
  sourceId: number | null;
  extensions: string[];
}

export interface HistoryItem {
  id: number;
  query_text: string;
  search_mode: string;
  filters_json: string;
  last_used_at: string;
}

export interface HistorySettings {
  enabled: boolean;
  items: HistoryItem[];
}

export interface PreviewState {
  status: "idle" | "loading" | "ready" | "error";
  data: PreviewText | null;
  chunkId: number | null;
  error: string | null;
}

export type PdfPageSize = [width: number, height: number] | null;

export interface SearchSnapshot {
  query: string;
  mode: SearchMode;
  caseSensitive: boolean;
  sort: SortKey;
  sourceId: number | null;
  extensions: string[];
}

export interface DraftSearch {
  query: string;
  mode: SearchMode;
  caseSensitive: boolean;
  sort: SortKey;
  sourceId: number | null;
  extensions: string[];
}

export const PAGE_SIZE = 50;

export const SORT_OPTIONS: { value: SortKey; label: string }[] = [
  { value: "relevance", label: "相关度" },
  { value: "modified_time", label: "修改时间（最新）" },
  { value: "size", label: "文件大小（最大）" },
  { value: "path", label: "路径（A → Z）" },
  { value: "filename", label: "文件名（A → Z）" },
  { value: "hit_count", label: "命中数（最多）" },
];

export const CODE_EXTENSIONS = [
  "c",
  "h",
  "cc",
  "cpp",
  "cxx",
  "hpp",
  "rs",
  "go",
  "java",
  "py",
  "js",
  "jsx",
  "ts",
  "tsx",
  "json",
  "toml",
  "yaml",
  "yml",
  "ini",
  "cfg",
  "conf",
  "log",
  "sh",
  "ps1",
  "bat",
  "sql",
  "xml",
  "html",
  "css",
];

export const EXTENSION_PRESETS: { key: string; label: string; extensions: string[] }[] = [
  { key: "all", label: "全部", extensions: [] },
  { key: "pdf", label: "PDF", extensions: ["pdf"] },
  { key: "docx", label: "DOCX", extensions: ["docx"] },
  { key: "md", label: "MD", extensions: ["md"] },
  { key: "txt", label: "TXT", extensions: ["txt"] },
  { key: "code", label: "代码/配置", extensions: CODE_EXTENSIONS },
];

export const DEFAULT_SORT: SortKey = "relevance";

export function arraysEqual(left: string[], right: string[]): boolean {
  return left.length === right.length && left.every((value, index) => value === right[index]);
}

export function sameSnapshot(draft: DraftSearch, committed: SearchSnapshot | null): boolean {
  return (
    committed !== null &&
    draft.query === committed.query &&
    draft.mode === committed.mode &&
    draft.caseSensitive === committed.caseSensitive &&
    draft.sort === committed.sort &&
    draft.sourceId === committed.sourceId &&
    arraysEqual(draft.extensions, committed.extensions)
  );
}

export function parseHistoryFilters(raw: string): Partial<HistoryFilters> {
  try {
    const value: unknown = JSON.parse(raw);
    if (!value || typeof value !== "object") return {};
    const record = value as Record<string, unknown>;
    const parsed: Partial<HistoryFilters> = {};
    if (typeof record.caseSensitive === "boolean") {
      parsed.caseSensitive = record.caseSensitive;
    }
    if (typeof record.sort === "string" && SORT_OPTIONS.some((item) => item.value === record.sort)) {
      parsed.sort = record.sort as SortKey;
    }
    if (typeof record.sourceId === "number" || record.sourceId === null) {
      parsed.sourceId = record.sourceId as number | null;
    }
    if (Array.isArray(record.extensions)) {
      parsed.extensions = record.extensions.filter(
        (item): item is string => typeof item === "string",
      );
    }
    return parsed;
  } catch {
    return {};
  }
}

export function normalizeMode(value: string): SearchMode {
  return value === "exact" || value === "fuzzy" ? value : "substring";
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 10 ? value.toFixed(1) : value.toFixed(2)} ${units[unit]}`;
}

export function formatModified(mtimeNs: number | null): string {
  if (mtimeNs === null || !Number.isFinite(mtimeNs)) return "时间未知";
  const date = new Date(mtimeNs / 1_000_000);
  if (Number.isNaN(date.getTime())) return "时间未知";
  return new Intl.DateTimeFormat("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).format(date);
}

export function formatHistoryTime(value: string): string {
  if (!value) return "";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).format(date);
}

export function fileStateLabel(state: string): string | null {
  switch (state) {
    case "indexed":
      return null;
    case "skipped_size":
      return "超过大小上限 · 仅文件名";
    case "failed":
      return "解析失败 · 未覆盖全文";
    case "stale":
      return "索引过期 · 使用上次结果";
    case "pending":
      return "等待索引 · 未覆盖全文";
    case "offline":
      return "来源离线 · 缓存结果";
    default:
      return state;
  }
}

export function locatorSummary(locator: SourceLocator | null): string {
  if (!locator) return "文件名命中";
  switch (locator.kind) {
    case "text": {
      const start = locator.start_line;
      const end = locator.end_line;
      return start === end ? `第 ${start} 行` : `第 ${start}–${end} 行`;
    }
    case "pdf_page":
      return `第 ${locator.page} 页`;
    case "docx_paragraph":
      return `段落 ${locator.paragraph + 1}`;
    case "docx_table_cell":
      return `表格 ${locator.table + 1} · ${locator.row + 1} 行 ${locator.column + 1} 列`;
    default:
      return "文本位置";
  }
}

export function originLabel(origin: string): string {
  return origin === "ocr" ? "OCR 文本" : "原生文本";
}

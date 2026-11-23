import { invoke } from "@tauri-apps/api/core";
import type {
  HistorySettings,
  PdfPageSize,
  PreviewText,
  SearchRequest,
  SearchResponse,
  SourceInfo,
} from "./types";

export function errorMessage(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  if (error && typeof error === "object") {
    const record = error as Record<string, unknown>;
    if (typeof record.message === "string") return record.message;
    if (typeof record.error === "string") return record.error;
  }
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

/** The Rust parser reports a UTF-8 byte position in its error string. */
export function parseErrorPosition(message: string): number | null {
  const match = message.match(/position\s*[:=]?\s*(\d+)/i);
  if (match) {
    const value = Number.parseInt(match[1], 10);
    return Number.isFinite(value) ? value : null;
  }
  try {
    const parsed: unknown = JSON.parse(message);
    if (parsed && typeof parsed === "object") {
      const position = (parsed as Record<string, unknown>).position;
      if (typeof position === "number" && Number.isFinite(position)) return position;
    }
  } catch {
    // Not JSON; there is no structured position to recover.
  }
  return null;
}

function toBytes(raw: unknown): Uint8Array {
  if (raw instanceof ArrayBuffer) return new Uint8Array(raw);
  if (ArrayBuffer.isView(raw)) {
    return new Uint8Array(raw.buffer, raw.byteOffset, raw.byteLength);
  }
  if (Array.isArray(raw)) {
    const bytes = new Uint8Array(raw.length);
    raw.forEach((value, index) => {
      bytes[index] = Number(value) & 0xff;
    });
    return bytes;
  }
  throw new Error("PDF 渲染响应不是二进制 PNG 数据");
}

export const api = {
  searchDocuments(request: SearchRequest): Promise<SearchResponse> {
    return invoke<SearchResponse>("search_documents", { request });
  },
  cancelSearch(requestId: number): Promise<void> {
    return invoke<void>("cancel_search", { requestId });
  },
  listSources(): Promise<SourceInfo[]> {
    return invoke<SourceInfo[]>("list_sources");
  },
  pickIndexFolder(): Promise<string | null> {
    return invoke<string | null>("pick_index_folder");
  },
  indexFolder(path: string): Promise<number> {
    return invoke<number>("index_folder", { path });
  },
  removeSource(sourceId: number): Promise<void> {
    return invoke<void>("remove_source", { sourceId });
  },
  previewText(fileId: number, chunkId: number | null): Promise<PreviewText> {
    return invoke<PreviewText>("preview_text", { fileId, chunkId });
  },
  openIndexedFile(fileId: number, expectedPath: string, action: "open" | "reveal"): Promise<void> {
    return invoke<void>("open_indexed_file", { fileId, expectedPath, action });
  },
  pdfPreviewPages(fileId: number): Promise<PdfPageSize[]> {
    return invoke<PdfPageSize[]>("pdf_preview_pages", { fileId });
  },
  async renderPdfPage(fileId: number, page: number, highlight: string | null): Promise<Uint8Array> {
    const raw = await invoke<unknown>("render_pdf_page", { fileId, page, highlight });
    return toBytes(raw);
  },
  historySettings(): Promise<HistorySettings> {
    return invoke<HistorySettings>("history_settings");
  },
  setHistoryEnabled(enabled: boolean): Promise<void> {
    return invoke<void>("set_history_enabled", { enabled });
  },
  clearHistory(): Promise<void> {
    return invoke<void>("clear_history");
  },
};

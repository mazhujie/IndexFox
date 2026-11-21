import { invoke } from "@tauri-apps/api/core";

export type ResourceMode = "saver" | "balanced" | "fast";
export type ScanMode = "auto" | "poll" | "manual";

export interface DesktopSettings {
  closeToTray: boolean;
  launchOnStartup: boolean;
  uiFont: string | null;
  previewFont: string | null;
  previewFontSize: number;
}

export interface ActiveIndexJob {
  jobId: number;
  sourceId: number;
  rootPath: string;
  processed: number;
  total: number;
  indexed: number;
  failed: number;
}

export interface IndexRuntime {
  paused: boolean;
  resourceMode: ResourceMode;
  active: ActiveIndexJob[];
  queued: number;
  lastError: string | null;
}

export interface OcrSettings {
  enabled: boolean;
  languages: string;
  maxConcurrency: number;
  dpi: number;
  pageTimeoutSecs: number;
}

export interface SourceConfig {
  enabled: boolean;
  scanMode: ScanMode;
  recursive: boolean;
  extensions: string[];
  excludedDirs: string[];
  excludeGlobs: string[];
  maxFileSizeBytes: number;
  pollIntervalSecs: number;
}

export const DEFAULT_DESKTOP_SETTINGS: DesktopSettings = {
  closeToTray: false,
  launchOnStartup: false,
  uiFont: null,
  previewFont: null,
  previewFontSize: 14,
};

export const DEFAULT_OCR_SETTINGS: OcrSettings = {
  enabled: true,
  languages: "chi_sim+eng",
  maxConcurrency: 2,
  dpi: 250,
  pageTimeoutSecs: 60,
};

export const MIN_PREVIEW_FONT_SIZE = 8;
export const MAX_PREVIEW_FONT_SIZE = 72;

export const DEFAULT_UI_FONT_STACK =
  '"Segoe UI", "PingFang SC", "Microsoft YaHei", system-ui, sans-serif';
export const DEFAULT_PREVIEW_FONT_STACK =
  '"Cascadia Code", "JetBrains Mono", Consolas, "SFMono-Regular", monospace';

export const settingsApi = {
  getDesktopSettings(): Promise<DesktopSettings> {
    return invoke<DesktopSettings>("get_desktop_settings");
  },
  systemFonts(): Promise<string[]> {
    return invoke<string[]>("system_fonts");
  },
  setDesktopSettings(settings: DesktopSettings): Promise<void> {
    return invoke<void>("set_desktop_settings", {
      settings: {
        ...settings,
        uiFont: settings.uiFont ?? "",
        previewFont: settings.previewFont ?? "",
      },
    });
  },
  getOcrSettings(): Promise<OcrSettings> {
    return invoke<OcrSettings>("get_ocr_settings");
  },
  setOcrSettings(settings: OcrSettings): Promise<void> {
    return invoke<void>("set_ocr_settings", { settings });
  },
  getIndexRuntime(): Promise<IndexRuntime> {
    return invoke<IndexRuntime>("get_index_runtime");
  },
  setIndexPaused(paused: boolean): Promise<void> {
    return invoke<void>("set_index_paused", { paused });
  },
  setResourceMode(mode: ResourceMode): Promise<void> {
    return invoke<void>("set_resource_mode", { mode });
  },
  cancelIndexJob(jobId: number): Promise<void> {
    return invoke<void>("cancel_index_job", { jobId });
  },
  scanSource(sourceId: number, force: boolean): Promise<void> {
    return invoke<void>("scan_source", { sourceId, force });
  },
  rebuildIndexes(): Promise<number> {
    return invoke<number>("rebuild_indexes");
  },
  getSourceConfig(sourceId: number): Promise<SourceConfig> {
    return invoke<SourceConfig>("get_source_config", { sourceId });
  },
  setSourceConfig(sourceId: number, config: SourceConfig): Promise<void> {
    return invoke<void>("set_source_config", { sourceId, config });
  },
  getDatabasePath(): Promise<string> {
    return invoke<string>("get_database_path");
  },
  migrateDatabase(path: string): Promise<void> {
    return invoke<void>("migrate_database", { path });
  },
  clearIndexCache(): Promise<void> {
    return invoke<void>("clear_index_cache");
  },
};

export interface ResolvedDesktopFonts {
  uiFont: string | null;
  previewFont: string | null;
  uiStack: string;
  previewStack: string;
  previewFontSize: number;
  fallbacks: string[];
}

export function clampPreviewFontSize(size: number): number {
  if (!Number.isFinite(size)) return DEFAULT_DESKTOP_SETTINGS.previewFontSize;
  return Math.min(MAX_PREVIEW_FONT_SIZE, Math.max(MIN_PREVIEW_FONT_SIZE, Math.round(size)));
}

function cssFontFamily(family: string): string {
  return `"${family.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

interface ResolvedFont {
  family: string | null;
  stack: string;
}

function resolveFont(
  configured: string | null,
  label: string,
  fallbackStack: string,
  installed: Set<string>,
  fallbacks: string[],
): ResolvedFont {
  const family = configured?.trim() ?? "";
  if (!family) return { family: null, stack: fallbackStack };
  // An empty list means enumeration failed; the browser still gets the full
  // fallback stack, so a missing family degrades gracefully.
  if (installed.size > 0 && !installed.has(family.toLocaleLowerCase())) {
    fallbacks.push(`${label}“${family}”未在本机字体列表中找到，已回退默认字体。`);
    return { family: null, stack: fallbackStack };
  }
  return { family, stack: `${cssFontFamily(family)}, ${fallbackStack}` };
}

export function resolveDesktopFonts(
  settings: DesktopSettings,
  fonts: string[],
): ResolvedDesktopFonts {
  const installed = new Set(
    fonts.map((font) => font.trim().toLocaleLowerCase()).filter(Boolean),
  );
  const fallbacks: string[] = [];
  const ui = resolveFont(settings.uiFont, "界面字体", DEFAULT_UI_FONT_STACK, installed, fallbacks);
  const preview = resolveFont(
    settings.previewFont,
    "预览字体",
    DEFAULT_PREVIEW_FONT_STACK,
    installed,
    fallbacks,
  );
  return {
    uiFont: ui.family,
    previewFont: preview.family,
    uiStack: ui.stack,
    previewStack: preview.stack,
    previewFontSize: clampPreviewFontSize(settings.previewFontSize),
    fallbacks,
  };
}

/** Applies fonts through the CSS variables already used by styles.css. */
export function applyDesktopSettings(
  settings: DesktopSettings,
  fonts: string[],
  root: HTMLElement = document.documentElement,
): ResolvedDesktopFonts {
  const resolved = resolveDesktopFonts(settings, fonts);
  root.style.setProperty("--font-sans", resolved.uiStack);
  root.style.setProperty("--preview-font-family", resolved.previewStack);
  root.style.setProperty("--preview-font-size", `${resolved.previewFontSize}px`);
  return resolved;
}

/** Startup helper: never blocks the window on a missing or failing backend. */
export async function loadAndApplyDesktopSettings(): Promise<DesktopSettings> {
  try {
    const settings = await settingsApi.getDesktopSettings();
    let fonts: string[] = [];
    try {
      fonts = await settingsApi.systemFonts();
    } catch (error) {
      console.warn("IndexFox: failed to enumerate system fonts", error);
    }
    applyDesktopSettings(settings, fonts);
    return settings;
  } catch (error) {
    console.warn("IndexFox: failed to load desktop settings; using defaults", error);
    applyDesktopSettings(DEFAULT_DESKTOP_SETTINGS, []);
    return DEFAULT_DESKTOP_SETTINGS;
  }
}

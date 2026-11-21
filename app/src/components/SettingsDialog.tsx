import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { FormEvent } from "react";
import { errorMessage } from "../api";
import type { SourceInfo } from "../types";
import {
  DEFAULT_DESKTOP_SETTINGS,
  DEFAULT_OCR_SETTINGS,
  MAX_PREVIEW_FONT_SIZE,
  MIN_PREVIEW_FONT_SIZE,
  applyDesktopSettings,
  clampPreviewFontSize,
  resolveDesktopFonts,
  settingsApi,
} from "../settings";
import type {
  DesktopSettings,
  IndexRuntime,
  OcrSettings,
  ResourceMode,
  ScanMode,
  SourceConfig,
} from "../settings";
import "./SettingsDialog.css";

export { applyDesktopSettings, loadAndApplyDesktopSettings } from "../settings";
export type { DesktopSettings } from "../settings";

interface SettingsDialogProps {
  sources: SourceInfo[];
  onClose: () => void;
  onChanged: () => void;
}

type SettingsTab = "runtime" | "sources" | "ocr" | "desktop" | "maintenance";

const TABS: { id: SettingsTab; label: string }[] = [
  { id: "runtime", label: "索引调度" },
  { id: "sources", label: "来源配置" },
  { id: "ocr", label: "OCR 识别" },
  { id: "desktop", label: "桌面与字体" },
  { id: "maintenance", label: "数据库维护" },
];

const OCR_LANGUAGES: { value: string; label: string }[] = [
  { value: "chi_sim+eng", label: "中文 + 英文（chi_sim+eng）" },
  { value: "eng", label: "英文（eng）" },
  { value: "chi_sim", label: "中文（chi_sim）" },
];

const RESOURCE_MODES: { mode: ResourceMode; label: string }[] = [
  { mode: "saver", label: "节能" },
  { mode: "balanced", label: "均衡" },
  { mode: "fast", label: "高速" },
];

const SCAN_MODES: { mode: ScanMode; label: string }[] = [
  { mode: "auto", label: "自动（文件变更通知）" },
  { mode: "poll", label: "定时轮询" },
  { mode: "manual", label: "仅手动" },
];

const MB = 1024 * 1024;

interface OcrDraft {
  enabled: boolean;
  languages: string;
  maxConcurrency: string;
  dpi: string;
  pageTimeoutSecs: string;
}

interface SourceConfigDraft {
  enabled: boolean;
  scanMode: ScanMode;
  recursive: boolean;
  extensions: string;
  excludedDirs: string;
  excludeGlobs: string;
  maxFileSizeMb: string;
  pollIntervalSecs: string;
}

function onlineLabel(state: string): string {
  if (state === "online") return "在线";
  if (state === "offline") return "离线";
  if (state === "scanning") return "扫描中";
  if (state === "failed") return "连接失败";
  return state || "状态未知";
}

function splitLines(value: string): string[] {
  return value
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
}

function splitExtensions(value: string): string[] {
  const seen = new Set<string>();
  for (const token of value.split(/[\s,;]+/)) {
    const normalized = token.trim().replace(/^\./, "").toLowerCase();
    if (normalized) seen.add(normalized);
  }
  return [...seen];
}

function megabytesText(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0";
  return String(Math.round((bytes / MB) * 100) / 100);
}

function toDraft(config: SourceConfig): SourceConfigDraft {
  return {
    enabled: config.enabled,
    scanMode: config.scanMode,
    recursive: config.recursive,
    extensions: config.extensions.join(", "),
    excludedDirs: config.excludedDirs.join("\n"),
    excludeGlobs: config.excludeGlobs.join("\n"),
    maxFileSizeMb: megabytesText(config.maxFileSizeBytes),
    pollIntervalSecs: String(config.pollIntervalSecs),
  };
}

function toOcrDraft(settings: OcrSettings): OcrDraft {
  return {
    enabled: settings.enabled,
    languages: settings.languages,
    maxConcurrency: String(settings.maxConcurrency),
    dpi: String(settings.dpi),
    pageTimeoutSecs: String(settings.pageTimeoutSecs),
  };
}

function ocrInteger(value: string, label: string, min: number, max: number): number {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < min || parsed > max) {
    throw new Error(`${label}必须是 ${min}–${max} 之间的整数。`);
  }
  return parsed;
}

function ocrFromDraft(draft: OcrDraft): OcrSettings {
  return {
    enabled: draft.enabled,
    languages: draft.languages,
    maxConcurrency: ocrInteger(draft.maxConcurrency, "并发页数", 1, 4),
    dpi: ocrInteger(draft.dpi, "渲染 DPI", 100, 400),
    pageTimeoutSecs: ocrInteger(draft.pageTimeoutSecs, "单页超时", 1, 180),
  };
}

function configFromDraft(draft: SourceConfigDraft): SourceConfig {
  const maxFileSizeMb = Number.parseFloat(draft.maxFileSizeMb);
  if (!Number.isFinite(maxFileSizeMb) || maxFileSizeMb <= 0) {
    throw new Error("单文件上限必须是大于 0 的 MB 数值。");
  }
  const maxFileSizeBytes = Math.round(maxFileSizeMb * MB);
  if (!Number.isSafeInteger(maxFileSizeBytes) || maxFileSizeBytes <= 0) {
    throw new Error("单文件上限超出可表示范围。");
  }
  const pollIntervalSecs = Number.parseInt(draft.pollIntervalSecs, 10);
  if (!Number.isFinite(pollIntervalSecs) || pollIntervalSecs < 1) {
    throw new Error("轮询间隔必须是大于等于 1 的整数秒。");
  }
  return {
    enabled: draft.enabled,
    scanMode: draft.scanMode,
    recursive: draft.recursive,
    extensions: splitExtensions(draft.extensions),
    excludedDirs: splitLines(draft.excludedDirs),
    excludeGlobs: splitLines(draft.excludeGlobs),
    maxFileSizeBytes,
    pollIntervalSecs,
  };
}

function progressWidth(job: IndexRuntime["active"][number]): number | null {
  if (!Number.isFinite(job.total) || job.total <= 0) return null;
  return Math.min(100, Math.max(0, Math.round((job.processed / job.total) * 100)));
}

export function SettingsDialog({ sources, onClose, onChanged }: SettingsDialogProps) {
  const [tab, setTab] = useState<SettingsTab>("runtime");

  const [runtime, setRuntime] = useState<IndexRuntime | null>(null);
  const [runtimeLoading, setRuntimeLoading] = useState(true);
  const [runtimeBusy, setRuntimeBusy] = useState(false);
  const [runtimeError, setRuntimeError] = useState<string | null>(null);
  const [runtimeNotice, setRuntimeNotice] = useState<string | null>(null);

  const [selectedSourceId, setSelectedSourceId] = useState<number | null>(
    sources[0]?.id ?? null,
  );
  const [configDraft, setConfigDraft] = useState<SourceConfigDraft | null>(null);
  const [configLoading, setConfigLoading] = useState(false);
  const [configSaving, setConfigSaving] = useState(false);
  const [configError, setConfigError] = useState<string | null>(null);
  const [configReload, setConfigReload] = useState(0);
  const [scanBusy, setScanBusy] = useState(false);
  const [sourceActionError, setSourceActionError] = useState<string | null>(null);
  const [sourceMessage, setSourceMessage] = useState<string | null>(null);

  const [desktop, setDesktop] = useState<DesktopSettings>(DEFAULT_DESKTOP_SETTINGS);
  const [fonts, setFonts] = useState<string[]>([]);
  const [desktopLoading, setDesktopLoading] = useState(true);
  const [desktopSaving, setDesktopSaving] = useState(false);
  const [desktopError, setDesktopError] = useState<string | null>(null);
  const [desktopNotice, setDesktopNotice] = useState<string | null>(null);
  const [fontsLoading, setFontsLoading] = useState(true);
  const [fontsError, setFontsError] = useState<string | null>(null);

  const [ocr, setOcr] = useState<OcrDraft>(toOcrDraft(DEFAULT_OCR_SETTINGS));
  const [ocrLoading, setOcrLoading] = useState(true);
  const [ocrSaving, setOcrSaving] = useState(false);
  const [ocrError, setOcrError] = useState<string | null>(null);
  const [ocrNotice, setOcrNotice] = useState<string | null>(null);

  const [dbPath, setDbPath] = useState("");
  const [dbPathLoading, setDbPathLoading] = useState(true);
  const [dbPathError, setDbPathError] = useState<string | null>(null);
  const [migratePath, setMigratePath] = useState("");
  const [migrateBusy, setMigrateBusy] = useState(false);
  const [migrateError, setMigrateError] = useState<string | null>(null);
  const [migrateNotice, setMigrateNotice] = useState<string | null>(null);
  const [cacheBusy, setCacheBusy] = useState(false);
  const [cacheError, setCacheError] = useState<string | null>(null);
  const [cacheNotice, setCacheNotice] = useState<string | null>(null);

  const runtimeRequestRef = useRef<Promise<void> | null>(null);
  const savedDesktopRef = useRef<DesktopSettings>(DEFAULT_DESKTOP_SETTINGS);
  const fontsRef = useRef<string[]>([]);
  const fontPreviewRef = useRef(false);
  fontsRef.current = fonts;

  const refreshRuntime = useCallback((quiet = false): Promise<void> => {
    const pending = runtimeRequestRef.current;
    if (pending) return pending;
    if (!quiet) setRuntimeLoading(true);
    const request = settingsApi
      .getIndexRuntime()
      .then((data) => {
        setRuntime(data);
        setRuntimeError(null);
      })
      .catch((error) => {
        setRuntimeError(`索引状态读取失败：${errorMessage(error)}`);
      })
      .finally(() => {
        if (!quiet) setRuntimeLoading(false);
        runtimeRequestRef.current = null;
      });
    runtimeRequestRef.current = request;
    return request;
  }, []);

  useEffect(() => {
    void refreshRuntime();
    const timer = window.setInterval(() => {
      void refreshRuntime(true);
    }, 2000);
    return () => window.clearInterval(timer);
  }, [refreshRuntime]);

  useEffect(() => {
    let cancelled = false;
    async function loadInitial() {
      const [desktopResult, fontsResult, dbResult, ocrResult] = await Promise.allSettled([
        settingsApi.getDesktopSettings(),
        settingsApi.systemFonts(),
        settingsApi.getDatabasePath(),
        settingsApi.getOcrSettings(),
      ]);
      if (cancelled) return;

      const fontList = fontsResult.status === "fulfilled" ? fontsResult.value : [];
      fontsRef.current = fontList;
      setFonts(fontList);
      setFontsLoading(false);
      if (fontsResult.status === "rejected") {
        setFontsError(`系统字体读取失败：${errorMessage(fontsResult.reason)}；将使用默认字体栈。`);
      }

      if (desktopResult.status === "fulfilled") {
        const settings = desktopResult.value;
        setDesktop(settings);
        savedDesktopRef.current = settings;
        applyDesktopSettings(settings, fontList);
      } else {
        setDesktopError(
          `桌面设置读取失败：${errorMessage(desktopResult.reason)}；保存前不会写入新配置。`,
        );
      }
      setDesktopLoading(false);

      if (ocrResult.status === "fulfilled") {
        setOcr(toOcrDraft(ocrResult.value));
      } else {
        setOcrError(
          `OCR 设置读取失败：${errorMessage(ocrResult.reason)}；保存前不会写入新配置。`,
        );
      }
      setOcrLoading(false);

      if (dbResult.status === "fulfilled") {
        setDbPath(dbResult.value);
      } else {
        setDbPathError(`数据库路径读取失败：${errorMessage(dbResult.reason)}`);
      }
      setDbPathLoading(false);
    }
    void loadInitial();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    return () => {
      if (fontPreviewRef.current) {
        applyDesktopSettings(savedDesktopRef.current, fontsRef.current);
      }
    };
  }, []);

  useEffect(() => {
    function handleKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape") closeDialog();
    }
    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, []);

  useEffect(() => {
    if (sources.length === 0) {
      setSelectedSourceId(null);
      return;
    }
    if (selectedSourceId === null || !sources.some((source) => source.id === selectedSourceId)) {
      setSelectedSourceId(sources[0].id);
    }
  }, [sources, selectedSourceId]);

  useEffect(() => {
    if (selectedSourceId === null) {
      setConfigDraft(null);
      setConfigError(null);
      return;
    }
    let cancelled = false;
    setConfigDraft(null);
    setConfigLoading(true);
    setConfigError(null);
    setSourceActionError(null);
    setSourceMessage(null);
    settingsApi
      .getSourceConfig(selectedSourceId)
      .then((config) => {
        if (!cancelled) setConfigDraft(toDraft(config));
      })
      .catch((error) => {
        if (!cancelled) setConfigError(`来源配置读取失败：${errorMessage(error)}`);
      })
      .finally(() => {
        if (!cancelled) setConfigLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [selectedSourceId, configReload]);

  const selectedSource = useMemo(
    () => sources.find((source) => source.id === selectedSourceId) ?? null,
    [sources, selectedSourceId],
  );

  const fontOptions = useMemo(() => {
    const unique = new Map<string, string>();
    for (const font of fonts) {
      const trimmed = font.trim();
      if (trimmed) unique.set(trimmed.toLocaleLowerCase(), trimmed);
    }
    return [...unique.values()].sort((left, right) => left.localeCompare(right, "zh-CN"));
  }, [fonts]);

  const resolvedFonts = resolveDesktopFonts(desktop, fonts);

  function closeDialog() {
    if (fontPreviewRef.current) {
      applyDesktopSettings(savedDesktopRef.current, fontsRef.current);
      fontPreviewRef.current = false;
    }
    onClose();
  }

  function patchConfig(patch: Partial<SourceConfigDraft>) {
    setConfigDraft((previous) => (previous ? { ...previous, ...patch } : previous));
    setConfigError(null);
    setSourceMessage(null);
  }

  async function runRuntimeAction(action: () => Promise<unknown>, notice: string) {
    setRuntimeBusy(true);
    setRuntimeError(null);
    setRuntimeNotice(null);
    try {
      await action();
      setRuntimeNotice(notice);
      await refreshRuntime(true);
      onChanged();
    } catch (error) {
      setRuntimeError(`索引操作失败：${errorMessage(error)}`);
    } finally {
      setRuntimeBusy(false);
    }
  }

  function handlePauseToggle() {
    if (!runtime) return;
    const next = !runtime.paused;
    void runRuntimeAction(
      () => settingsApi.setIndexPaused(next),
      next ? "索引调度已暂停。" : "索引调度已恢复。",
    );
  }

  function handleResourceMode(mode: ResourceMode) {
    if (!runtime || runtime.resourceMode === mode) return;
    const label = RESOURCE_MODES.find((item) => item.mode === mode)?.label ?? mode;
    void runRuntimeAction(
      () => settingsApi.setResourceMode(mode),
      `资源模式已切换为${label}。`,
    );
  }

  function handleCancelJob(jobId: number) {
    void runRuntimeAction(
      () => settingsApi.cancelIndexJob(jobId),
      `已请求取消任务 #${jobId}。`,
    );
  }

  async function handleRebuild() {
    if (!window.confirm("重建索引会重新扫描所有来源，并替换现有索引。原始文件不会被删除。继续？")) {
      return;
    }
    setRuntimeBusy(true);
    setRuntimeError(null);
    setRuntimeNotice(null);
    try {
      const count = await settingsApi.rebuildIndexes();
      setRuntimeNotice(`索引重建完成，返回计数 ${count}。`);
      await refreshRuntime(true);
      onChanged();
    } catch (error) {
      setRuntimeError(`重建索引失败：${errorMessage(error)}`);
    } finally {
      setRuntimeBusy(false);
    }
  }

  async function handleSaveSourceConfig(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (selectedSourceId === null || !configDraft) return;
    let config: SourceConfig;
    try {
      config = configFromDraft(configDraft);
    } catch (error) {
      setConfigError(errorMessage(error));
      return;
    }
    setConfigSaving(true);
    setConfigError(null);
    setSourceMessage(null);
    try {
      await settingsApi.setSourceConfig(selectedSourceId, config);
      setConfigDraft(toDraft(config));
      setSourceMessage("来源配置已保存并生效。");
      await refreshRuntime(true);
      onChanged();
    } catch (error) {
      setConfigError(`来源配置保存失败：${errorMessage(error)}`);
    } finally {
      setConfigSaving(false);
    }
  }

  async function handleScan(force: boolean) {
    if (selectedSourceId === null) return;
    setScanBusy(true);
    setSourceActionError(null);
    setSourceMessage(null);
    try {
      await settingsApi.scanSource(selectedSourceId, force);
      setSourceMessage(force ? "已请求强制重扫该来源。" : "已请求增量扫描该来源。");
      await refreshRuntime(true);
      onChanged();
    } catch (error) {
      setSourceActionError(`扫描请求失败：${errorMessage(error)}`);
    } finally {
      setScanBusy(false);
    }
  }

  function patchFontSettings(patch: Partial<DesktopSettings>) {
    const next: DesktopSettings = {
      ...desktop,
      ...patch,
      previewFontSize: clampPreviewFontSize(
        patch.previewFontSize ?? desktop.previewFontSize,
      ),
    };
    setDesktop(next);
    setDesktopError(null);
    setDesktopNotice(null);
    applyDesktopSettings(next, fonts);
    fontPreviewRef.current = true;
  }

  function patchDesktopPreference(patch: Partial<DesktopSettings>) {
    setDesktop((previous) => ({ ...previous, ...patch }));
    setDesktopError(null);
    setDesktopNotice(null);
  }

  async function handleSaveDesktop() {
    const next: DesktopSettings = {
      ...desktop,
      previewFontSize: clampPreviewFontSize(desktop.previewFontSize),
    };
    setDesktopSaving(true);
    setDesktopError(null);
    setDesktopNotice(null);
    try {
      await settingsApi.setDesktopSettings(next);
      setDesktop(next);
      savedDesktopRef.current = next;
      fontPreviewRef.current = false;
      applyDesktopSettings(next, fonts);
      setDesktopNotice("桌面设置已保存。");
      onChanged();
    } catch (error) {
      setDesktopError(`桌面设置保存失败：${errorMessage(error)}`);
    } finally {
      setDesktopSaving(false);
    }
  }

  function patchOcr(patch: Partial<OcrDraft>) {
    setOcr((previous) => ({ ...previous, ...patch }));
    setOcrError(null);
    setOcrNotice(null);
  }

  async function handleSaveOcr() {
    let next: OcrSettings;
    try {
      next = ocrFromDraft(ocr);
    } catch (error) {
      setOcrError(errorMessage(error));
      return;
    }
    setOcrSaving(true);
    setOcrError(null);
    setOcrNotice(null);
    try {
      await settingsApi.setOcrSettings(next);
      setOcr(toOcrDraft(next));
      setOcrNotice("OCR 设置已保存；将在下一次扫描时生效。");
    } catch (error) {
      setOcrError(`OCR 设置保存失败：${errorMessage(error)}`);
    } finally {
      setOcrSaving(false);
    }
  }

  async function handleReloadFonts() {
    setFontsLoading(true);
    setFontsError(null);
    try {
      const fontList = await settingsApi.systemFonts();
      setFonts(fontList);
      fontsRef.current = fontList;
      applyDesktopSettings(desktop, fontList);
    } catch (error) {
      setFontsError(`系统字体读取失败：${errorMessage(error)}；继续使用默认字体栈。`);
    } finally {
      setFontsLoading(false);
    }
  }

  function handleResetFonts() {
    patchFontSettings({
      uiFont: null,
      previewFont: null,
      previewFontSize: DEFAULT_DESKTOP_SETTINGS.previewFontSize,
    });
    setDesktopNotice("已恢复默认字体；点击“保存桌面设置”写入配置。");
  }

  async function refreshDatabasePath() {
    setDbPathLoading(true);
    setDbPathError(null);
    try {
      setDbPath(await settingsApi.getDatabasePath());
    } catch (error) {
      setDbPathError(`数据库路径读取失败：${errorMessage(error)}`);
    } finally {
      setDbPathLoading(false);
    }
  }

  async function handleMigrate() {
    const path = migratePath.trim();
    if (!path) {
      setMigrateError("请输入新的数据库目标路径。");
      return;
    }
    if (
      !window.confirm(
        "迁移会校验并切换到新的数据库路径，过程中请勿结束进程。原始文件不受影响；建议先暂停索引。继续？",
      )
    ) {
      return;
    }
    setMigrateBusy(true);
    setMigrateError(null);
    setMigrateNotice(null);
    try {
      await settingsApi.migrateDatabase(path);
      setMigrateNotice("数据库已迁移并通过校验，当前路径已更新。");
      await refreshDatabasePath();
      await refreshRuntime(true);
      onChanged();
    } catch (error) {
      setMigrateError(`数据库迁移失败：${errorMessage(error)}`);
    } finally {
      setMigrateBusy(false);
    }
  }

  async function handleClearCache() {
    if (
      !window.confirm(
        "将清空数据库中的正文与分块缓存，不删除任何原始文件。清理后全文检索需重建索引才能恢复。继续？",
      )
    ) {
      return;
    }
    setCacheBusy(true);
    setCacheError(null);
    setCacheNotice(null);
    try {
      await settingsApi.clearIndexCache();
      setCacheNotice("正文缓存已清理；原始文件未改动。请重建索引以恢复全文结果。");
      await refreshRuntime(true);
      onChanged();
    } catch (error) {
      setCacheError(`清理正文缓存失败：${errorMessage(error)}`);
    } finally {
      setCacheBusy(false);
    }
  }

  function renderFontSelect(
    label: string,
    value: string | null,
    onChange: (font: string | null) => void,
  ) {
    const missingFont =
      value && !fontOptions.some((font) => font.toLocaleLowerCase() === value.toLocaleLowerCase())
        ? value
        : null;
    return (
      <label className="settings-field">
        <span>{label}</span>
        <select
          className="select-box"
          value={value ?? ""}
          disabled={fontsLoading || desktopLoading}
          onChange={(event) => onChange(event.target.value || null)}
        >
          <option value="">系统默认字体栈</option>
          {fontOptions.map((font) => (
            <option key={font} value={font}>
              {font}
            </option>
          ))}
          {missingFont !== null && (
            <option value={missingFont}>{missingFont}（未检测到，回退默认）</option>
          )}
        </select>
      </label>
    );
  }

  function renderRuntime() {
    return (
      <section className="settings-section">
        <div className="settings-section-head">
          <div>
            <h3>索引调度</h3>
            <p className="settings-subtitle">打开设置时每 2 秒刷新一次运行状态。</p>
          </div>
          <button
            type="button"
            className="mini-button"
            disabled={runtimeLoading}
            onClick={() => void refreshRuntime()}
          >
            {runtimeLoading ? "刷新中…" : "刷新"}
          </button>
        </div>

        {runtimeError && (
          <p className="inline-error" role="alert">
            {runtimeError}
          </p>
        )}
        {runtimeNotice && (
          <p className="settings-notice" role="status">
            {runtimeNotice}
          </p>
        )}

        {!runtime ? (
          <p className="form-hint">{runtimeLoading ? "正在读取索引状态…" : "暂无索引状态。"}</p>
        ) : (
          <>
            <div className="settings-card">
              <div className="settings-row">
                <span className="settings-label">状态</span>
                <strong className={runtime.paused ? "settings-paused" : "settings-running"}>
                  {runtime.paused ? "已暂停" : "调度中"}
                </strong>
                <span className="form-hint">
                  活跃 {runtime.active.length} · 排队 {runtime.queued}
                </span>
                <div className="settings-row-actions">
                  <button
                    type="button"
                    className={runtime.paused ? "primary-button small" : "secondary-button"}
                    disabled={runtimeBusy}
                    onClick={handlePauseToggle}
                  >
                    {runtime.paused ? "恢复索引" : "暂停索引"}
                  </button>
                  <button
                    type="button"
                    className="secondary-button"
                    disabled={runtimeBusy}
                    onClick={() => void handleRebuild()}
                  >
                    重建索引
                  </button>
                </div>
              </div>

              <div className="settings-row">
                <span className="settings-label">资源模式</span>
                <div className="segmented" role="group" aria-label="资源模式">
                  {RESOURCE_MODES.map((item) => (
                    <button
                      key={item.mode}
                      type="button"
                      className={`seg-button ${runtime.resourceMode === item.mode ? "active" : ""}`}
                      aria-pressed={runtime.resourceMode === item.mode}
                      disabled={runtimeBusy}
                      onClick={() => handleResourceMode(item.mode)}
                    >
                      {item.label}
                    </button>
                  ))}
                </div>
                <span className="form-hint">节能档降低并发，高速档优先吞吐。</span>
              </div>

              {runtime.lastError && (
                <p className="inline-error" role="alert">
                  最近任务错误：{runtime.lastError}
                </p>
              )}
            </div>

            {runtime.paused && (
              <p className="settings-warning" role="status">
                索引调度已暂停；已在执行的任务可能继续收尾。恢复后继续排队任务。
              </p>
            )}

            <h4 className="settings-list-title">活动任务</h4>
            {runtime.active.length === 0 ? (
              <p className="form-hint">当前没有正在执行的任务。</p>
            ) : (
              <ul className="settings-job-list">
                {runtime.active.map((job) => {
                  const percent = progressWidth(job);
                  return (
                    <li className="settings-job" key={job.jobId}>
                      <div className="settings-job-head">
                        <strong>
                          任务 #{job.jobId} · 来源 #{job.sourceId}
                        </strong>
                        <button
                          type="button"
                          className="mini-button"
                          disabled={runtimeBusy}
                          onClick={() => handleCancelJob(job.jobId)}
                        >
                          取消
                        </button>
                      </div>
                      <span className="mono settings-job-path">{job.rootPath}</span>
                      <div className="settings-progress" aria-hidden="true">
                        <span
                          className={percent === null ? "indeterminate" : ""}
                          style={percent === null ? undefined : { width: `${percent}%` }}
                        />
                      </div>
                      <span className="settings-job-meta">
                        {percent === null ? "扫描中…" : `${percent}%`} · 已处理 {job.processed}/
                        {job.total} · 已索引 {job.indexed} · 失败 {job.failed}
                      </span>
                    </li>
                  );
                })}
              </ul>
            )}
          </>
        )}
      </section>
    );
  }

  function renderSources() {
    return (
      <section className="settings-section">
        <div className="settings-section-head">
          <div>
            <h3>来源配置</h3>
            <p className="settings-subtitle">逐来源设置启用、扫描方式、过滤规则与大小上限。</p>
          </div>
        </div>

        {sources.length === 0 ? (
          <p className="form-hint">尚未添加索引来源，请先在主界面添加目录。</p>
        ) : (
          <>
            <div className="source-list">
              {sources.map((source) => (
                <button
                  key={source.id}
                  type="button"
                  className={`source-card ${source.id === selectedSourceId ? "active" : ""}`}
                  onClick={() => setSelectedSourceId(source.id)}
                >
                  <span className="source-title">
                    <span className="source-name">
                      #{source.id} · {source.root_path}
                    </span>
                    <span className="tag">{source.enabled ? "已启用" : "已停用"}</span>
                  </span>
                  <span className="source-meta">
                    <span className={`status-dot ${source.online_state}`} aria-hidden="true" />
                    <span>{onlineLabel(source.online_state)}</span>
                  </span>
                </button>
              ))}
            </div>

            {selectedSource && (
              <div className="settings-card">
                <div className="settings-section-head compact">
                  <div>
                    <h4 className="settings-list-title">#{selectedSource.id} 配置</h4>
                    <p className="mono settings-source-path">{selectedSource.root_path}</p>
                  </div>
                  <div className="settings-row-actions">
                    <button
                      type="button"
                      className="mini-button"
                      disabled={scanBusy}
                      onClick={() => void handleScan(false)}
                    >
                      {scanBusy ? "请求中…" : "扫描"}
                    </button>
                    <button
                      type="button"
                      className="mini-button"
                      disabled={scanBusy}
                      onClick={() => void handleScan(true)}
                    >
                      强制重扫
                    </button>
                  </div>
                </div>

                {sourceActionError && (
                  <p className="inline-error" role="alert">
                    {sourceActionError}
                  </p>
                )}
                {sourceMessage && (
                  <p className="settings-notice" role="status">
                    {sourceMessage}
                  </p>
                )}

                {configLoading ? (
                  <p className="form-hint">正在读取来源配置…</p>
                ) : configError ? (
                  <div className="settings-error-actions">
                    <p className="inline-error" role="alert">
                      {configError}
                    </p>
                    <button
                      type="button"
                      className="mini-button"
                      onClick={() => setConfigReload((value) => value + 1)}
                    >
                      重试
                    </button>
                  </div>
                ) : configDraft ? (
                  <form className="settings-form" onSubmit={handleSaveSourceConfig}>
                    <div className="settings-inline-checks">
                      <label className="check-label">
                        <input
                          type="checkbox"
                          checked={configDraft.enabled}
                          onChange={(event) => patchConfig({ enabled: event.target.checked })}
                        />
                        启用此来源
                      </label>
                      <label className="check-label">
                        <input
                          type="checkbox"
                          checked={configDraft.recursive}
                          onChange={(event) => patchConfig({ recursive: event.target.checked })}
                        />
                        递归子目录
                      </label>
                    </div>

                    <div className="settings-grid">
                      <label className="settings-field">
                        <span>扫描方式</span>
                        <select
                          className="select-box"
                          value={configDraft.scanMode}
                          onChange={(event) =>
                            patchConfig({ scanMode: event.target.value as ScanMode })
                          }
                        >
                          {SCAN_MODES.map((item) => (
                            <option key={item.mode} value={item.mode}>
                              {item.label}
                            </option>
                          ))}
                        </select>
                      </label>
                      <label className="settings-field">
                        <span>轮询间隔（秒）</span>
                        <input
                          className="text-input"
                          type="number"
                          min={1}
                          step={1}
                          value={configDraft.pollIntervalSecs}
                          onChange={(event) =>
                            patchConfig({ pollIntervalSecs: event.target.value })
                          }
                        />
                      </label>
                      <label className="settings-field">
                        <span>单文件上限（MB）</span>
                        <input
                          className="text-input"
                          type="number"
                          min={0.01}
                          step={0.01}
                          value={configDraft.maxFileSizeMb}
                          onChange={(event) => patchConfig({ maxFileSizeMb: event.target.value })}
                        />
                      </label>
                    </div>

                    <label className="settings-field">
                      <span>扩展名（逗号或空格分隔，留空表示无扩展名限制）</span>
                      <input
                        className="text-input"
                        value={configDraft.extensions}
                        placeholder="pdf, md, txt"
                        onChange={(event) => patchConfig({ extensions: event.target.value })}
                      />
                    </label>

                    <label className="settings-field">
                      <span>排除目录（每行一个）</span>
                      <textarea
                        className="settings-textarea"
                        rows={3}
                        value={configDraft.excludedDirs}
                        onChange={(event) => patchConfig({ excludedDirs: event.target.value })}
                      />
                    </label>

                    <label className="settings-field">
                      <span>排除通配符（每行一个）</span>
                      <textarea
                        className="settings-textarea"
                        rows={3}
                        value={configDraft.excludeGlobs}
                        onChange={(event) => patchConfig({ excludeGlobs: event.target.value })}
                      />
                    </label>

                    <div className="settings-actions">
                      <button
                        type="submit"
                        className="primary-button small"
                        disabled={configSaving}
                      >
                        {configSaving ? "保存中…" : "保存来源配置"}
                      </button>
                    </div>
                  </form>
                ) : null}
              </div>
            )}
          </>
        )}
      </section>
    );
  }

  function renderOcr() {
    return (
      <section className="settings-section">
        <div className="settings-section-head">
          <div>
            <h3>OCR 识别</h3>
            <p className="settings-subtitle">
              保存后在下一次扫描生效；扫描方式为“仅手动”的来源需手动触发扫描。
            </p>
          </div>
        </div>

        {ocrError && (
          <p className="inline-error" role="alert">
            {ocrError}
          </p>
        )}
        {ocrNotice && (
          <p className="settings-notice" role="status">
            {ocrNotice}
          </p>
        )}

        <div className="settings-card">
          <label className="check-label">
            <input
              type="checkbox"
              checked={ocr.enabled}
              disabled={ocrLoading}
              onChange={(event) => patchOcr({ enabled: event.target.checked })}
            />
            自动识别扫描页与文字质量不足的 PDF 页面
          </label>
          <p className="form-hint">关闭后，下次扫描只索引 PDF 原生文本。</p>

          <div className="settings-grid">
            <label className="settings-field">
              <span>识别语言（需已安装对应模型）</span>
              <select
                className="select-box"
                value={ocr.languages}
                disabled={ocrLoading}
                onChange={(event) => patchOcr({ languages: event.target.value })}
              >
                {OCR_LANGUAGES.map((item) => (
                  <option key={item.value} value={item.value}>
                    {item.label}
                  </option>
                ))}
                {!OCR_LANGUAGES.some((item) => item.value === ocr.languages) && (
                  <option value={ocr.languages}>{ocr.languages}</option>
                )}
              </select>
            </label>
            <label className="settings-field">
              <span>并发页数（1–4）</span>
              <input
                className="text-input"
                type="number"
                min={1}
                max={4}
                step={1}
                value={ocr.maxConcurrency}
                disabled={ocrLoading}
                onChange={(event) => patchOcr({ maxConcurrency: event.target.value })}
              />
            </label>
            <label className="settings-field">
              <span>渲染 DPI（100–400）</span>
              <input
                className="text-input"
                type="number"
                min={100}
                max={400}
                step={1}
                value={ocr.dpi}
                disabled={ocrLoading}
                onChange={(event) => patchOcr({ dpi: event.target.value })}
              />
            </label>
            <label className="settings-field">
              <span>单页超时（秒，1–180）</span>
              <input
                className="text-input"
                type="number"
                min={1}
                max={180}
                step={1}
                value={ocr.pageTimeoutSecs}
                disabled={ocrLoading}
                onChange={(event) => patchOcr({ pageTimeoutSecs: event.target.value })}
              />
            </label>
          </div>
        </div>

        <div className="settings-actions">
          <button
            type="button"
            className="primary-button small"
            disabled={ocrSaving || ocrLoading}
            onClick={() => void handleSaveOcr()}
          >
            {ocrSaving ? "保存中…" : "保存 OCR 设置"}
          </button>
        </div>
      </section>
    );
  }

  function renderDesktop() {
    return (
      <section className="settings-section">
        <div className="settings-section-head">
          <div>
            <h3>桌面与字体</h3>
            <p className="settings-subtitle">
              字体修改即时预览；关闭/自启与持久化仅在点击保存后写入。
            </p>
          </div>
          <button
            type="button"
            className="mini-button"
            disabled={fontsLoading}
            onClick={() => void handleReloadFonts()}
          >
            {fontsLoading ? "读取中…" : "重新读取系统字体"}
          </button>
        </div>

        {desktopError && (
          <p className="inline-error" role="alert">
            {desktopError}
          </p>
        )}
        {fontsError && (
          <p className="inline-error" role="alert">
            {fontsError}
          </p>
        )}
        {desktopNotice && (
          <p className="settings-notice" role="status">
            {desktopNotice}
          </p>
        )}

        <div className="settings-card">
          <label className="check-label">
            <input
              type="checkbox"
              checked={desktop.closeToTray}
              onChange={(event) => patchDesktopPreference({ closeToTray: event.target.checked })}
            />
            关闭窗口时驻留系统托盘
          </label>
          <p className="form-hint">
            默认关闭即退出；驻留模式会继续后台增量索引，保存后生效。
          </p>
          <label className="check-label">
            <input
              type="checkbox"
              checked={desktop.launchOnStartup}
              onChange={(event) =>
                patchDesktopPreference({ launchOnStartup: event.target.checked })
              }
            />
            开机自动启动
          </label>
          <p className="form-hint">
            默认关闭。只有主动点击“保存桌面设置”才会请求系统登记自启。
          </p>
        </div>

        <div className="settings-card">
          {renderFontSelect("界面全局字体", desktop.uiFont, (font) =>
            patchFontSettings({ uiFont: font }),
          )}
          {renderFontSelect("文本/预览字体", desktop.previewFont, (font) =>
            patchFontSettings({ previewFont: font }),
          )}
          <label className="settings-field">
            <span>
              预览字号：{desktop.previewFontSize} px
            </span>
            <input
              type="range"
              min={MIN_PREVIEW_FONT_SIZE}
              max={MAX_PREVIEW_FONT_SIZE}
              step={1}
              value={desktop.previewFontSize}
              onChange={(event) =>
                patchFontSettings({ previewFontSize: Number(event.target.value) })
              }
            />
          </label>

          <div className="settings-specimen">
            <span className="form-hint">实时字体样张（缺失字体自动回退默认字体栈）</span>
            <div style={{ fontFamily: resolvedFonts.uiStack }}>
              [界面字体] IndexFox · 索引狐 — 本地只读检索
            </div>
            <div
              className="mono"
              style={{
                fontFamily: resolvedFonts.previewStack,
                fontSize: `${resolvedFonts.previewFontSize}px`,
              }}
            >
              [文本/预览] VIDIOC_STREAMON | dma_alloc_coherent() | 0O 1lI a_b #include
            </div>
          </div>

          {resolvedFonts.fallbacks.map((message) => (
            <p className="settings-warning" role="status" key={message}>
              {message}
            </p>
          ))}

          <div className="settings-actions">
            <button
              type="button"
              className="mini-button"
              onClick={handleResetFonts}
              disabled={desktopLoading}
            >
              恢复默认字体
            </button>
          </div>
        </div>

        <div className="settings-actions">
          <button
            type="button"
            className="primary-button small"
            disabled={desktopSaving || desktopLoading}
            onClick={() => void handleSaveDesktop()}
          >
            {desktopSaving ? "保存中…" : "保存桌面设置"}
          </button>
        </div>
      </section>
    );
  }

  function renderMaintenance() {
    return (
      <section className="settings-section">
        <div className="settings-section-head">
          <div>
            <h3>数据库维护</h3>
            <p className="settings-subtitle">数据库工作会影响索引可用性，操作前请确认当前任务。</p>
          </div>
          <button
            type="button"
            className="mini-button"
            disabled={dbPathLoading}
            onClick={() => void refreshDatabasePath()}
          >
            刷新路径
          </button>
        </div>

        <div className="settings-card">
          <span className="settings-label">当前数据库路径</span>
          {dbPathLoading ? (
            <p className="form-hint">正在读取…</p>
          ) : dbPathError ? (
            <p className="inline-error" role="alert">
              {dbPathError}
            </p>
          ) : (
            <code className="settings-code">{dbPath || "—"}</code>
          )}
        </div>

        <div className="settings-card settings-danger">
          <h4>迁移数据库</h4>
          <p className="settings-warning" role="note">
            迁移会复制、校验并切换数据库路径；过程中请勿结束进程。原始文件不会被修改，建议先暂停索引。
          </p>
          <label className="settings-field">
            <span>新的数据库路径</span>
            <input
              className="text-input mono"
              value={migratePath}
              placeholder="/path/to/index.db"
              onChange={(event) => {
                setMigratePath(event.target.value);
                setMigrateError(null);
                setMigrateNotice(null);
              }}
            />
          </label>
          {migrateError && (
            <p className="inline-error" role="alert">
              {migrateError}
            </p>
          )}
          {migrateNotice && (
            <p className="settings-notice" role="status">
              {migrateNotice}
            </p>
          )}
          <div className="settings-actions">
            <button
              type="button"
              className="settings-danger-button"
              disabled={migrateBusy}
              onClick={() => void handleMigrate()}
            >
              {migrateBusy ? "迁移中…" : "迁移并校验"}
            </button>
          </div>
        </div>

        <div className="settings-card settings-danger">
          <h4>清理正文缓存</h4>
          <p className="settings-warning" role="note">
            仅清理 SQLite 中的正文与分块缓存，不删除原始文件。清理后需重建索引才能恢复全文检索。
          </p>
          {cacheError && (
            <p className="inline-error" role="alert">
              {cacheError}
            </p>
          )}
          {cacheNotice && (
            <p className="settings-notice" role="status">
              {cacheNotice}
            </p>
          )}
          <div className="settings-actions">
            <button
              type="button"
              className="settings-danger-button"
              disabled={cacheBusy}
              onClick={() => void handleClearCache()}
            >
              {cacheBusy ? "清理中…" : "清理正文缓存"}
            </button>
          </div>
        </div>
      </section>
    );
  }

  return (
    <div
      className="settings-overlay"
      onPointerDown={(event) => {
        if (event.target === event.currentTarget) closeDialog();
      }}
    >
      <div
        className="settings-dialog"
        role="dialog"
        aria-modal="true"
        aria-label="IndexFox 设置"
      >
        <header className="settings-header">
          <div>
            <h2>设置</h2>
            <span className="form-hint">索引调度、来源、OCR、桌面字体与数据库维护</span>
          </div>
          <button type="button" className="icon-button" aria-label="关闭设置" onClick={closeDialog}>
            ✕
          </button>
        </header>

        <div className="settings-body">
          <nav className="settings-tabs" role="tablist" aria-label="设置分区">
            {TABS.map((item) => (
              <button
                key={item.id}
                type="button"
                role="tab"
                aria-selected={tab === item.id}
                className={`settings-tab ${tab === item.id ? "active" : ""}`}
                onClick={() => setTab(item.id)}
              >
                {item.label}
              </button>
            ))}
          </nav>

          <div className="settings-content">
            {tab === "runtime" && renderRuntime()}
            {tab === "sources" && renderSources()}
            {tab === "ocr" && renderOcr()}
            {tab === "desktop" && renderDesktop()}
            {tab === "maintenance" && renderMaintenance()}
          </div>
        </div>

        <footer className="settings-footer">
          <span>关闭窗口默认退出；数据库操作不修改原始文件。</span>
          <button type="button" className="secondary-button" onClick={closeDialog}>
            关闭
          </button>
        </footer>
      </div>
    </div>
  );
}

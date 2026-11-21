import type { FormEvent } from "react";
import type { SourceInfo } from "../types";
import type { IndexRuntime } from "../settings";

interface SourcesPaneProps {
  sources: SourceInfo[];
  sourceError: string | null;
  selectedSourceId: number | null;
  onSelectSource: (sourceId: number | null) => void;
  addOpen: boolean;
  onToggleAdd: () => void;
  sourcePath: string;
  onSourcePathChange: (value: string) => void;
  onSubmitPath: (event: FormEvent<HTMLFormElement>) => void;
  onPickFolder: () => void;
  onRemoveSource: (source: SourceInfo) => void;
  busy: boolean;
  indexError: string | null;
  runtime: IndexRuntime | null;
  onReload: () => void;
}

function onlineLabel(state: string): string {
  if (state === "online") return "在线";
  if (state === "offline") return "离线";
  if (state === "scanning") return "扫描中";
  if (state === "failed") return "扫描失败";
  return "状态未知";
}

export function SourcesPane({
  sources,
  sourceError,
  selectedSourceId,
  onSelectSource,
  addOpen,
  onToggleAdd,
  sourcePath,
  onSourcePathChange,
  onSubmitPath,
  onPickFolder,
  onRemoveSource,
  busy,
  indexError,
  runtime,
  onReload,
}: SourcesPaneProps) {
  return (
    <aside className="pane sources-pane" aria-label="已索引目录">
      <div className="pane-header">
        <div>
          <h2>索引目录</h2>
          <span className="pane-subtitle">{sources.length} 个来源</span>
        </div>
        <div className="pane-actions">
          <button
            type="button"
            className="icon-button"
            onClick={onReload}
            title="刷新来源列表"
            aria-label="刷新来源列表"
          >
            ↻
          </button>
          <button
            type="button"
            className="mini-button accent"
            onClick={onPickFolder}
            disabled={busy}
          >
            添加目录
          </button>
        </div>
      </div>

      <div className="pane-body source-body">
        <button
          type="button"
          className="mini-button source-path-toggle"
          onClick={onToggleAdd}
          aria-expanded={addOpen}
          disabled={busy}
        >
          {addOpen ? "收起路径输入" : "输入路径"}
        </button>
        {addOpen && (
          <form className="add-source-form" onSubmit={onSubmitPath}>
            <label htmlFor="new-source-path">输入 UNC、WSL 或绝对路径</label>
            <input
              id="new-source-path"
              className="text-input mono"
              value={sourcePath}
              onChange={(event) => onSourcePathChange(event.target.value)}
              placeholder={'\\\\server\\share 或 D:\\Docs 或 /home/me/docs'}
              autoComplete="off"
              disabled={busy}
            />
            <p className="form-hint">将目录加入后台索引队列。</p>
            <button type="submit" className="primary-button small" disabled={busy}>
              {busy ? "正在添加…" : "添加并索引"}
            </button>
          </form>
        )}

        {indexError && <p className="inline-error" role="alert">{indexError}</p>}
        {runtime && (runtime.paused || runtime.queued > 0 || runtime.active.length > 0) && (
          <div className="index-status" role="status">
            {runtime.paused ? "索引已暂停" : `正在索引 ${runtime.active.length} 个目录`}
            {runtime.queued > 0 && ` · ${runtime.queued} 个排队`}
          </div>
        )}

        {sourceError && (
          <p className="inline-error" role="alert">
            {sourceError}
          </p>
        )}

        <button
          type="button"
          className={`source-card ${selectedSourceId === null ? "active" : ""}`}
          onClick={() => onSelectSource(null)}
          aria-pressed={selectedSourceId === null}
        >
          <span className="source-title">
            <span className="source-name">全部来源</span>
            <span className="source-count">{sources.length}</span>
          </span>
        </button>

        {sources.length === 0 && !sourceError ? (
          <div className="pane-empty">
            <p>还没有索引目录。</p>
            <p className="muted">点击“添加目录”选择文件夹，或用“输入路径”手动填写。</p>
          </div>
        ) : (
          <div className="source-list">
            {sources.map((source) => (
              <div className="source-row" key={source.id}>
                <button
                  type="button"
                  className={`source-card ${selectedSourceId === source.id ? "active" : ""}`}
                  onClick={() => onSelectSource(source.id)}
                  aria-pressed={selectedSourceId === source.id}
                >
                  <span className="source-title">
                    <span className="source-name mono">{source.root_path}</span>
                    <span
                      className={`status-dot ${source.online_state}`}
                      title={onlineLabel(source.online_state)}
                      aria-label={onlineLabel(source.online_state)}
                    />
                  </span>
                  <span className="source-meta">
                    <span>{onlineLabel(source.online_state)}</span>
                    {runtime?.active.filter((job) => job.sourceId === source.id).map((job) => (
                      <span key={job.jobId}>
                        {job.total > 0 ? `${job.processed} / ${job.total}` : "正在枚举"}
                      </span>
                    ))}
                    {!source.enabled && <span className="tag tag-warn">已停用</span>}
                  </span>
                </button>
                <button
                  type="button"
                  className="mini-button source-remove"
                  aria-label={`移除 ${source.root_path}`}
                  title="移除索引来源（原始文件会保留）"
                  disabled={busy}
                  onClick={() => onRemoveSource(source)}
                >
                  移除
                </button>
              </div>
            ))}
          </div>
        )}

      </div>
    </aside>
  );
}

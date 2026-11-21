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
  indexing: boolean;
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
  indexing,
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
            onClick={onToggleAdd}
            aria-expanded={addOpen}
          >
            {addOpen ? "收起" : "+ 添加目录"}
          </button>
        </div>
      </div>

      <div className="pane-body source-body">
        {addOpen && (
          <form className="add-source-form" onSubmit={onSubmitPath}>
            <label htmlFor="new-source-path">目录绝对路径</label>
            <input
              id="new-source-path"
              className="text-input mono"
              value={sourcePath}
              onChange={(event) => onSourcePathChange(event.target.value)}
              placeholder="D:\Docs 或 /home/me/docs"
              autoComplete="off"
              disabled={indexing}
            />
            <p className="form-hint">将目录加入后台索引队列。</p>
            <button type="submit" className="primary-button small" disabled={indexing}>
              {indexing ? "正在添加…" : "添加并索引"}
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
            <p className="muted">使用“添加目录”输入本地路径，开始建立离线索引。</p>
          </div>
        ) : (
          <div className="source-list">
            {sources.map((source) => (
              <button
                key={source.id}
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
            ))}
          </div>
        )}

      </div>
    </aside>
  );
}

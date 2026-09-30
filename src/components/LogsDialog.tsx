import { useEffect, useRef } from 'react';

import type { LogEntry, ProjectView } from '../types';

import { Dialog } from './Dialog';

interface LogsDialogProps {
  view: ProjectView;
  logs: LogEntry[];
  loading: boolean;
  clearing: boolean;
  onClear: () => void;
  onClose: () => void;
}

function formatTime(timestamp: number): string {
  return timestampDate(timestamp).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

function timestampDate(timestamp: number): Date {
  const milliseconds = timestamp < 1_000_000_000_000 ? timestamp * 1000 : timestamp;
  return new Date(milliseconds);
}

export function LogsDialog({ view, logs, loading, clearing, onClear, onClose }: LogsDialogProps) {
  const outputRef = useRef<HTMLDivElement>(null);
  const executableMode = (view.project.launchMode ?? 'browser') === 'executable';
  const launchDetails = executableMode
    ? [view.project.executablePath, view.project.arguments].filter(Boolean).join(' ') || '可执行文件'
    : view.runtime.resolvedCommand ?? view.project.command;

  useEffect(() => {
    const output = outputRef.current;
    if (output) {
      output.scrollTop = output.scrollHeight;
    }
  }, [logs, loading]);

  return (
    <Dialog
      open
      title={`${view.project.name} 日志 Logs`}
      description={executableMode ? '可执行文件输出会实时追加；此模式不分配端口或等待网页就绪。' : '只保留最近的进程输出，stdout 与 stderr 会实时追加。'}
      onClose={onClose}
    >
      <div className="logs-toolbar">
        <code>{launchDetails}</code>
        <button type="button" className="button button-quiet" onClick={onClear} disabled={clearing || logs.length === 0}>
          {clearing ? '清理中…' : '清空日志 Clear'}
        </button>
      </div>
      <div className="log-output" ref={outputRef} role="log" aria-live="polite" aria-label={`${view.project.name} process logs`}>
        {loading ? <p className="log-placeholder">正在读取日志…</p> : null}
        {!loading && logs.length === 0 ? <p className="log-placeholder">暂无日志输出。</p> : null}
        {logs.map((entry) => (
          <div className={`log-line log-${entry.stream}`} key={`${entry.sequence}-${entry.timestamp}`}>
            <time dateTime={timestampDate(entry.timestamp).toISOString()}>{formatTime(entry.timestamp)}</time>
            <span className="log-stream">{entry.stream === 'stderr' ? 'ERR' : 'OUT'}</span>
            <span className="log-text">{entry.line}</span>
          </div>
        ))}
      </div>
    </Dialog>
  );
}

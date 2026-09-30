import type { KeyboardEvent, MouseEvent } from 'react';

import type { ProjectView } from '../types';

interface ProjectCardProps {
  view: ProjectView;
  busy: boolean;
  onActivate: () => void;
  onStart: () => void;
  onStop: () => void;
  onRestart: () => void;
  onLogs: () => void;
  onSettings: () => void;
}

const statusLabels = {
  stopped: '已停止 Stopped',
  starting: '启动中 Starting',
  running: '运行中 Running',
  error: '启动失败 Error',
} as const;

function stopCardEvent(event: MouseEvent<HTMLElement>) {
  event.stopPropagation();
}

function executableLabel(path: string): string {
  const segments = path.trim().split(/[\\/]/).filter(Boolean);
  return segments[segments.length - 1] ?? '可执行文件';
}

export function ProjectCard({
  view,
  busy,
  onActivate,
  onStart,
  onStop,
  onRestart,
  onLogs,
  onSettings,
}: ProjectCardProps) {
  const { project, runtime } = view;
  const launchMode = project.launchMode ?? 'browser';
  const browserMode = launchMode === 'browser';
  const displayPort = runtime.actualPort ?? project.preferredPort;
  const executableName = executableLabel(project.executablePath ?? '');
  const isRunning = runtime.status === 'running';
  const isStarting = runtime.status === 'starting';

  const handleKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key === 'Enter' && event.target === event.currentTarget && !busy) {
      event.preventDefault();
      onActivate();
    }
  };

  return (
    <article
      className={`project-card status-${runtime.status}`}
      tabIndex={busy ? -1 : 0}
      role="button"
      aria-label={`${project.name}，${statusLabels[runtime.status]}，${browserMode ? '浏览器项目' : '可执行文件项目'}`}
      onDoubleClick={onActivate}
      onKeyDown={handleKeyDown}
    >
      <div className="project-card-topline">
        <div className="project-icon" aria-hidden="true">
          {project.icon || '✦'}
        </div>
        <div className="project-card-heading">
          <h2>{project.name}</h2>
          <p className={`status-label status-label-${runtime.status}`}>
            <span className="status-dot" aria-hidden="true" />
            {statusLabels[runtime.status]}
          </p>
        </div>
        {browserMode ? (
          <span className="project-port" title={runtime.actualPort ? '当前实际端口' : '首选端口'}>
            :{displayPort}
          </span>
        ) : (
          <span className="project-launch-mode" title={project.executablePath || '可执行文件'}>
            {executableName}
          </span>
        )}
      </div>

      <div className="project-card-meta">
        <span className="project-path" title={project.path}>
          {project.path}
        </span>
        {runtime.pid ? <span className="project-pid">PID {runtime.pid}</span> : null}
      </div>

      {runtime.error ? <p className="project-error">{runtime.error}</p> : null}

      <div className="card-actions" aria-label="项目操作">
        <button
          type="button"
          className="card-action card-action-primary"
          onClick={(event) => {
            stopCardEvent(event);
            onStart();
          }}
          onDoubleClick={stopCardEvent}
          disabled={busy || isRunning || isStarting}
          title="启动项目"
          aria-label={`Start ${project.name}`}
        >
          ▶ <span>启动</span>
        </button>
        <button
          type="button"
          className="card-action"
          onClick={(event) => {
            stopCardEvent(event);
            onStop();
          }}
          onDoubleClick={stopCardEvent}
          disabled={!isRunning && !isStarting}
          title="停止项目"
          aria-label={`Stop ${project.name}`}
        >
          ■ <span>停止</span>
        </button>
        <button
          type="button"
          className="card-action"
          onClick={(event) => {
            stopCardEvent(event);
            onRestart();
          }}
          onDoubleClick={stopCardEvent}
          disabled={busy || isStarting}
          title="重启项目"
          aria-label={`Restart ${project.name}`}
        >
          ↻ <span>重启</span>
        </button>
        <button
          type="button"
          className="card-action"
          onClick={(event) => {
            stopCardEvent(event);
            onLogs();
          }}
          onDoubleClick={stopCardEvent}
          disabled={false}
          title="查看日志"
          aria-label={`Logs ${project.name}`}
        >
          &gt;_ <span>日志</span>
        </button>
        <button
          type="button"
          className="card-action card-action-settings"
          onClick={(event) => {
            stopCardEvent(event);
            onSettings();
          }}
          onDoubleClick={stopCardEvent}
          disabled={busy}
          title="项目设置"
          aria-label={`Settings ${project.name}`}
        >
          ⚙ <span>设置</span>
        </button>
      </div>
      <p className="card-hint">{browserMode ? '双击打开项目 · Enter 使用' : '双击运行文件 · Enter 使用'}</p>
    </article>
  );
}

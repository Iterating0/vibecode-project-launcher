import { useEffect, useRef, useState } from 'react';

import {
  activateProject,
  addProject,
  clearLogs,
  detectProjectLaunch,
  getLogs,
  isDesktopRuntime,
  listProjects,
  onProjectLog,
  onProjectState,
  pickExecutable,
  pickDirectory,
  removeProject,
  restartProject,
  startProject,
  stopProject,
  updateProject,
} from './api';
import { Dialog } from './components/Dialog';
import { LogsDialog } from './components/LogsDialog';
import { ProjectCard } from './components/ProjectCard';
import { ProjectForm } from './components/ProjectForm';
import type { LogEntry, ModalState, ProjectInput, ProjectLogEvent, ProjectStateEvent, ProjectView, RuntimeSnapshot } from './types';

const MAX_LOG_ENTRIES = 2_000;

function defaultProjectInput(path: string): ProjectInput {
  const normalizedPath = path.replace(/[\\/]+$/, '');
  const segments = normalizedPath.split(/[\\/]/).filter(Boolean);
  return {
    name: segments[segments.length - 1] ?? '新项目',
    path,
    workingDirectory: path,
    launchMode: 'browser',
    executablePath: '',
    arguments: '',
    command: '',
    preferredPort: 5173,
    urlTemplate: 'http://localhost:{port}',
    icon: null,
    autoOpenBrowser: true,
  };
}

function projectInputFromView(view: ProjectView): ProjectInput {
  const { project } = view;
  return {
    name: project.name,
    path: project.path,
    workingDirectory: project.workingDirectory,
    launchMode: project.launchMode ?? 'browser',
    executablePath: project.executablePath ?? '',
    arguments: project.arguments ?? '',
    command: project.command,
    preferredPort: project.preferredPort,
    urlTemplate: project.urlTemplate,
    icon: project.icon,
    autoOpenBrowser: project.autoOpenBrowser,
  };
}

function normalizeProjectView(view: ProjectView): ProjectView {
  return {
    ...view,
    project: {
      ...view.project,
      launchMode: view.project.launchMode ?? 'browser',
      executablePath: view.project.executablePath ?? '',
      arguments: view.project.arguments ?? '',
    },
  };
}

function mergeLogEntries(current: LogEntry[], incoming: LogEntry[]): LogEntry[] {
  const bySequence = new Map<number, LogEntry>();
  for (const entry of current) {
    bySequence.set(entry.sequence, entry);
  }
  for (const entry of incoming) {
    bySequence.set(entry.sequence, entry);
  }
  return [...bySequence.values()].sort((left, right) => left.sequence - right.sequence).slice(-MAX_LOG_ENTRIES);
}

function errorMessage(error: unknown): string {
  if (error instanceof Error && error.message) {
    return error.message;
  }
  return typeof error === 'string' ? error : '操作失败，请查看日志或重试。';
}

function actionNotice(action: string, projectName: string): string {
  return `${projectName}：${action}`;
}

export function App() {
  const desktop = isDesktopRuntime();
  const [projects, setProjects] = useState<ProjectView[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [modal, setModal] = useState<ModalState | null>(null);
  const [busyCounts, setBusyCounts] = useState<Record<string, number>>({});
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [pickerBusy, setPickerBusy] = useState(false);
  const [logs, setLogs] = useState<Record<string, LogEntry[]>>({});
  const [logsLoading, setLogsLoading] = useState<Record<string, boolean>>({});
  const [logsClearing, setLogsClearing] = useState<Record<string, boolean>>({});
  const mountedRef = useRef(false);
  const runtimeCacheRef = useRef<Record<string, RuntimeSnapshot>>({});
  const logEpochsRef = useRef<Record<string, number>>({});
  const logsDuringClearRef = useRef<Record<string, LogEntry[]>>({});
  const clearInFlightRef = useRef<Record<string, boolean>>({});
  const activationRequestsRef = useRef<Set<string>>(new Set());
  const activationReleaseTimersRef = useRef<Record<string, number>>({});

  const showNotice = (message: string) => {
    setNotice(message);
  };

  const handleProjectState = (snapshot: RuntimeSnapshot | ProjectStateEvent) => {
    if (!mountedRef.current) {
      return;
    }
    runtimeCacheRef.current[snapshot.projectId] = snapshot;
    setProjects((current) =>
      current.map((view) => (view.project.id === snapshot.projectId ? { ...view, runtime: snapshot } : view)),
    );
  };

  const handleProjectLog = (event: ProjectLogEvent) => {
    if (!mountedRef.current) {
      return;
    }
    if (clearInFlightRef.current[event.projectId]) {
      logsDuringClearRef.current[event.projectId] = mergeLogEntries(
        logsDuringClearRef.current[event.projectId] ?? [],
        [event.entry],
      );
    }
    setLogs((current) => ({
      ...current,
      [event.projectId]: mergeLogEntries(current[event.projectId] ?? [], [event.entry]),
    }));
  };

  useEffect(() => {
    mountedRef.current = true;
    if (!desktop) {
      setLoading(false);
      return () => {
        mountedRef.current = false;
      };
    }

    let active = true;
    const unlisteners: Array<() => void> = [];

    const load = async () => {
      // Listener registration intentionally completes before list_projects so
      // a state transition emitted during startup cannot be missed.
      const subscriptions = await Promise.allSettled([
        onProjectState((snapshot) => {
          if (active) {
            handleProjectState(snapshot);
          }
        }),
        onProjectLog((event) => {
          if (active) {
            handleProjectLog(event);
          }
        }),
      ]);
      for (const subscription of subscriptions) {
        if (subscription.status === 'fulfilled') {
          if (active) {
            unlisteners.push(subscription.value);
          } else {
            void subscription.value();
          }
        }
      }

      if (!active) {
        return;
      }

      try {
        const listedProjects = await listProjects();
        if (!active) {
          return;
        }
        setProjects(
          listedProjects.map(normalizeProjectView).map((view) => {
            const eventRuntime = runtimeCacheRef.current[view.project.id];
            return eventRuntime ? { ...view, runtime: eventRuntime } : view;
          }),
        );
        setLoadError(null);
      } catch (error) {
        if (active) {
          setLoadError(errorMessage(error));
        }
      } finally {
        if (active) {
          setLoading(false);
        }
      }
    };

    void load();
    return () => {
      active = false;
      mountedRef.current = false;
      for (const unlisten of unlisteners.splice(0)) {
        void unlisten();
      }
    };
  }, [desktop]);

  useEffect(() => {
    if (modal && modal.kind !== 'add' && !projects.some((view) => view.project.id === modal.projectId)) {
      setModal(null);
    }
  }, [modal, projects]);

  const setBusy = (projectId: string, delta: number) => {
    setBusyCounts((current) => {
      const nextCount = Math.max(0, (current[projectId] ?? 0) + delta);
      if (nextCount === 0) {
        const { [projectId]: _removed, ...rest } = current;
        return rest;
      }
      return { ...current, [projectId]: nextCount };
    });
  };

  const runProjectAction = async (
    view: ProjectView,
    action: () => Promise<void>,
    successMessage?: string,
  ) => {
    if (!desktop) {
      showNotice('需桌面程序执行启动操作');
      return;
    }

    setBusy(view.project.id, 1);
    try {
      await action();
      if (successMessage) {
        showNotice(actionNotice(successMessage, view.project.name));
      }
    } catch (error) {
      showNotice(`${view.project.name}：${errorMessage(error)}`);
    } finally {
      setBusy(view.project.id, -1);
    }
  };

  const handleActivate = (view: ProjectView) => {
    // The backend owns the start/open intent and coalesces it when a process
    // is already starting. Keeping this as one command also avoids a second
    // server when a user double-clicks during readiness checks.
    if (activationRequestsRef.current.has(view.project.id)) {
      return;
    }
    activationRequestsRef.current.add(view.project.id);
    void runProjectAction(view, () => activateProject(view.project.id)).finally(() => {
      // Keep a tiny deduplication window because a pair of native dblclick
      // gestures can arrive after an immediately-resolved IPC mock/backend
      // promise has already settled.
      const timer = window.setTimeout(() => {
        activationRequestsRef.current.delete(view.project.id);
        delete activationReleaseTimersRef.current[view.project.id];
      }, 500);
      activationReleaseTimersRef.current[view.project.id] = timer;
    });
  };

  const handleStart = (view: ProjectView) => {
    const openBrowser = (view.project.launchMode ?? 'browser') === 'browser' && view.project.autoOpenBrowser;
    void runProjectAction(view, () => startProject(view.project.id, openBrowser), '已发送启动请求');
  };

  const handleStop = (view: ProjectView) => {
    void runProjectAction(view, () => stopProject(view.project.id), '已发送停止请求');
  };

  const handleRestart = (view: ProjectView) => {
    void runProjectAction(view, () => restartProject(view.project.id), '已发送重启请求');
  };

  const handleAddClick = async () => {
    if (!desktop) {
      showNotice('需桌面程序执行启动操作');
      return;
    }

    setPickerBusy(true);
    try {
      const path = await pickDirectory();
      if (path) {
        setFormError(null);
        setModal({ kind: 'add', input: defaultProjectInput(path) });
      }
    } catch (error) {
      showNotice(`选择项目文件夹失败：${errorMessage(error)}`);
    } finally {
      setPickerBusy(false);
    }
  };

  const handlePickExecutable = (directory: string) => pickExecutable(directory);

  const handleOpenLogs = async (view: ProjectView) => {
    setModal({ kind: 'logs', projectId: view.project.id });
    if (!desktop) {
      showNotice('需桌面程序执行启动操作');
      return;
    }

    setLogsLoading((current) => ({ ...current, [view.project.id]: true }));
    const requestedEpoch = logEpochsRef.current[view.project.id] ?? 0;
    try {
      const snapshot = await getLogs(view.project.id);
      if (mountedRef.current && (logEpochsRef.current[view.project.id] ?? 0) === requestedEpoch) {
        setLogs((current) => ({
          ...current,
          [view.project.id]: mergeLogEntries(current[view.project.id] ?? [], snapshot),
        }));
      }
    } catch (error) {
      showNotice(`${view.project.name} 日志读取失败：${errorMessage(error)}`);
    } finally {
      if (mountedRef.current) {
        setLogsLoading((current) => ({ ...current, [view.project.id]: false }));
      }
    }
  };

  const handleClearLogs = async (view: ProjectView) => {
    if (!desktop) {
      showNotice('需桌面程序执行启动操作');
      return;
    }

    clearInFlightRef.current[view.project.id] = true;
    logsDuringClearRef.current[view.project.id] = [];
    setLogsClearing((current) => ({ ...current, [view.project.id]: true }));
    try {
      await clearLogs(view.project.id);
      logEpochsRef.current[view.project.id] = (logEpochsRef.current[view.project.id] ?? 0) + 1;
      // Entries emitted while the clear request was in flight belong to the
      // new log window and must survive the clear operation. Any snapshot
      // request started before this epoch is ignored when it resolves.
      const freshEntries = logsDuringClearRef.current[view.project.id] ?? [];
      setLogs((current) => ({
        ...current,
        [view.project.id]: freshEntries,
      }));
    } catch (error) {
      showNotice(`${view.project.name} 日志清理失败：${errorMessage(error)}`);
    } finally {
      clearInFlightRef.current[view.project.id] = false;
      delete logsDuringClearRef.current[view.project.id];
      setLogsClearing((current) => ({ ...current, [view.project.id]: false }));
    }
  };

  const handleSave = async (input: ProjectInput) => {
    if (!desktop || !modal || modal.kind === 'logs') {
      setFormError('需桌面程序执行启动操作');
      return;
    }

    setFormError(null);
    setSaving(true);
    try {
      if (modal.kind === 'add') {
        const created = normalizeProjectView(await addProject(input));
        setProjects((current) => [...current, created]);
        setFormError(null);
        setModal(null);
        showNotice(actionNotice('已添加', input.name));
      } else {
        const updated = normalizeProjectView(await updateProject(modal.projectId, input));
        setProjects((current) => current.map((view) => (view.project.id === updated.project.id ? updated : view)));
        setFormError(null);
        setModal(null);
        showNotice(actionNotice('已保存', input.name));
      }
    } catch (error) {
      setFormError(errorMessage(error));
    } finally {
      setSaving(false);
    }
  };

  const handleRemove = async (view: ProjectView) => {
    if (!desktop) {
      setFormError('需桌面程序执行启动操作');
      return;
    }
    if (!window.confirm(`确定移除「${view.project.name}」吗？只会移除 Launcher 配置，不会删除项目文件。`)) {
      return;
    }

    setSaving(true);
    try {
      await removeProject(view.project.id);
      setProjects((current) => current.filter((item) => item.project.id !== view.project.id));
      setFormError(null);
      setModal(null);
      showNotice(actionNotice('已移除', view.project.name));
    } catch (error) {
      setFormError(errorMessage(error));
    } finally {
      setSaving(false);
    }
  };

  const settingsView = modal?.kind === 'settings' ? projects.find((view) => view.project.id === modal.projectId) : undefined;
  const logsView = modal?.kind === 'logs' ? projects.find((view) => view.project.id === modal.projectId) : undefined;

  return (
    <main className="app-shell">
      <header className="app-header">
        <div className="brand-lockup">
          <div className="brand-mark" aria-hidden="true">✦</div>
          <div>
            <p className="eyebrow">VIBECODE LAUNCHER</p>
            <h1>我的项目 <span>My Projects</span></h1>
            <p className="page-intro">把散落在本机的 Web 小项目，放在一个轻量启动器里。</p>
          </div>
        </div>
        <button type="button" className="button button-primary add-button" onClick={() => void handleAddClick()} disabled={pickerBusy}>
          <span className="add-symbol" aria-hidden="true">＋</span>
          {pickerBusy ? '选择中…' : '添加项目 Add Project'}
        </button>
      </header>

      {!desktop ? (
        <div className="preview-banner" role="status">
          <span className="preview-dot" aria-hidden="true" />
          浏览器预览模式：需桌面程序执行启动操作。打开 Tauri 桌面版后，双击卡片即可启动并访问项目。
        </div>
      ) : null}

      {loadError ? (
        <div className="notice notice-error" role="alert">
          无法读取项目配置：{loadError}
        </div>
      ) : null}
      {notice ? (
        <div className="notice" role="status">
          <span>{notice}</span>
          <button type="button" className="notice-dismiss" onClick={() => setNotice(null)} aria-label="关闭提示">×</button>
        </div>
      ) : null}

      <section className="project-wall" aria-label="项目列表">
        {loading ? (
          <div className="loading-state" role="status">正在读取项目…</div>
        ) : projects.length === 0 ? (
          <div className="empty-state">
            <div className="empty-orbit" aria-hidden="true">＋</div>
            <h2>还没有项目 <span>No projects yet</span></h2>
            <p>选择一个本地项目文件夹，自动识别启动方式，再确认或修改。</p>
            <button type="button" className="button button-primary" onClick={() => void handleAddClick()} disabled={pickerBusy}>
              添加第一个项目 Add your first project
            </button>
          </div>
        ) : (
          <div className="project-grid">
            {projects.map((view) => (
              <ProjectCard
                key={view.project.id}
                view={view}
                busy={(busyCounts[view.project.id] ?? 0) > 0}
                onActivate={() => handleActivate(view)}
                onStart={() => handleStart(view)}
                onStop={() => handleStop(view)}
                onRestart={() => handleRestart(view)}
                onLogs={() => void handleOpenLogs(view)}
                onSettings={() => {
                  setFormError(null);
                  setModal({ kind: 'settings', projectId: view.project.id });
                }}
              />
            ))}
          </div>
        )}
      </section>

      <footer className="app-footer">
        <span><span className="footer-pulse" aria-hidden="true" />本地配置 · 进程由 Launcher 管理</span>
        <span>双击卡片使用 · Enter 使用</span>
      </footer>

      {modal?.kind === 'add' ? (
        <Dialog
          open
          title="添加项目 Add Project"
          description="自动识别项目入口并预填启动方式；你可以修改全部配置，确认后再添加。"
          closeDisabled={saving}
          onClose={() => {
            setFormError(null);
            setModal(null);
          }}
        >
          <ProjectForm
            initialValue={modal.input}
            onDetect={detectProjectLaunch}
            submitLabel="添加项目 Add Project"
            submitting={saving}
            error={formError}
            onPickExecutable={handlePickExecutable}
            onSubmit={(input) => void handleSave(input)}
            onClose={() => {
              setFormError(null);
              setModal(null);
            }}
          />
        </Dialog>
      ) : null}

      {settingsView && modal?.kind === 'settings' ? (
        <Dialog
          open
          title={`${settingsView.project.name} 设置 Settings`}
          description="运行中的项目请先停止，再修改配置；可在两种启动方式之间切换并保留各自草稿。"
          closeDisabled={saving}
          onClose={() => {
            setFormError(null);
            setModal(null);
          }}
        >
          <ProjectForm
            initialValue={projectInputFromView(settingsView)}
            submitLabel="保存设置 Save"
            submitting={saving}
            error={formError}
            onPickExecutable={handlePickExecutable}
            onSubmit={(input) => void handleSave(input)}
            onClose={() => {
              setFormError(null);
              setModal(null);
            }}
            onRemove={() => void handleRemove(settingsView)}
          />
        </Dialog>
      ) : null}

      {logsView && modal?.kind === 'logs' ? (
        <LogsDialog
          view={logsView}
          logs={logs[logsView.project.id] ?? []}
          loading={logsLoading[logsView.project.id] ?? false}
          clearing={logsClearing[logsView.project.id] ?? false}
          onClear={() => void handleClearLogs(logsView)}
          onClose={() => setModal(null)}
        />
      ) : null}
    </main>
  );
}

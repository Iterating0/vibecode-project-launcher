import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

import type {
  LaunchDetection,
  LogEntry,
  ProjectInput,
  ProjectLogEvent,
  ProjectStateEvent,
  ProjectView,
  RuntimeSnapshot,
} from './types';

export class DesktopUnavailableError extends Error {
  constructor() {
    super('需桌面程序执行启动操作');
    this.name = 'DesktopUnavailableError';
  }
}

/**
 * The same React bundle is useful in a normal browser for visual development.
 * Mutating commands remain deliberately unavailable there so the preview can
 * never pretend that a local process was started.
 */
export function isDesktopRuntime(): boolean {
  if (typeof window === 'undefined') {
    return false;
  }

  const tauriWindow = window as Window & { __TAURI_INTERNALS__?: unknown };
  return (
    Boolean(tauriWindow.__TAURI_INTERNALS__) ||
    window.location.protocol === 'tauri:' ||
    window.location.hostname === 'tauri.localhost' ||
    window.location.hostname.endsWith('.tauri.localhost')
  );
}

function requireDesktop(): void {
  if (!isDesktopRuntime()) {
    throw new DesktopUnavailableError();
  }
}

export function listProjects(): Promise<ProjectView[]> {
  requireDesktop();
  return invoke<ProjectView[]>('list_projects');
}

export function addProject(input: ProjectInput): Promise<ProjectView> {
  requireDesktop();
  return invoke<ProjectView>('add_project', { input });
}

export function updateProject(id: string, input: ProjectInput): Promise<ProjectView> {
  requireDesktop();
  return invoke<ProjectView>('update_project', { id, input });
}

export function removeProject(id: string): Promise<void> {
  requireDesktop();
  return invoke<void>('remove_project', { id });
}

export function startProject(id: string, openBrowser: boolean): Promise<void> {
  requireDesktop();
  return invoke<void>('start_project', { id, openBrowser });
}

export function stopProject(id: string): Promise<void> {
  requireDesktop();
  return invoke<void>('stop_project', { id });
}

export function restartProject(id: string): Promise<void> {
  requireDesktop();
  return invoke<void>('restart_project', { id });
}

export function activateProject(id: string): Promise<void> {
  requireDesktop();
  return invoke<void>('activate_project', { id });
}

export function getLogs(id: string): Promise<LogEntry[]> {
  requireDesktop();
  return invoke<LogEntry[]>('get_logs', { id });
}

export function clearLogs(id: string): Promise<void> {
  requireDesktop();
  return invoke<void>('clear_logs', { id });
}

export function pickDirectory(): Promise<string | null> {
  requireDesktop();
  return invoke<string | null>('pick_directory');
}

export function detectProjectLaunch(directory: string): Promise<LaunchDetection> {
  requireDesktop();
  return invoke<LaunchDetection>('detect_project_launch', { directory });
}

export function pickExecutable(directory: string): Promise<string | null> {
  requireDesktop();
  return invoke<string | null>('pick_executable', { directory });
}

export function onProjectState(handler: (snapshot: RuntimeSnapshot) => void): Promise<UnlistenFn> {
  if (!isDesktopRuntime()) {
    return Promise.resolve(() => undefined);
  }

  return listen<ProjectStateEvent>('project-state', (event) => {
    handler(event.payload);
  });
}

export function onProjectLog(handler: (event: ProjectLogEvent) => void): Promise<UnlistenFn> {
  if (!isDesktopRuntime()) {
    return Promise.resolve(() => undefined);
  }

  return listen<ProjectLogEvent>('project-log', (event) => {
    handler(event.payload);
  });
}

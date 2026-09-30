export type ProjectStatus = 'stopped' | 'starting' | 'running' | 'error';

export type LaunchMode = 'browser' | 'executable';

export interface Project {
  id: string;
  name: string;
  path: string;
  workingDirectory: string;
  launchMode: LaunchMode;
  executablePath: string;
  arguments: string;
  command: string;
  preferredPort: number;
  urlTemplate: string;
  icon: string | null;
  autoOpenBrowser: boolean;
}

export type ProjectInput = Omit<Project, 'id'>;

export type LaunchSettings = Pick<ProjectInput,
  'workingDirectory' | 'launchMode' | 'executablePath' | 'arguments' | 'command' |
  'preferredPort' | 'urlTemplate' | 'autoOpenBrowser'>;

export interface LaunchSuggestion {
  label: string;
  detail: string;
  settings: LaunchSettings;
}

export interface LaunchDetection {
  suggestions: LaunchSuggestion[];
  warnings: string[];
}

export interface RuntimeSnapshot {
  projectId: string;
  status: ProjectStatus;
  pid: number | null;
  actualPort: number | null;
  resolvedCommand: string | null;
  startedAt: number | null;
  error: string | null;
}

export interface ProjectView {
  project: Project;
  runtime: RuntimeSnapshot;
}

export type LogStream = 'stdout' | 'stderr';

export interface LogEntry {
  sequence: number;
  timestamp: number;
  stream: LogStream;
  line: string;
}

export interface ProjectStateEvent {
  projectId: string;
  status: ProjectStatus;
  pid: number | null;
  actualPort: number | null;
  resolvedCommand: string | null;
  startedAt: number | null;
  error: string | null;
}

export interface ProjectLogEvent {
  projectId: string;
  entry: LogEntry;
}

export type ModalState =
  | { kind: 'add'; input: ProjectInput }
  | { kind: 'settings'; projectId: string }
  | { kind: 'logs'; projectId: string };

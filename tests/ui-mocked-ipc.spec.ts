import { expect, test, type Page } from '@playwright/test';
import type { LaunchDetection } from '../src/types';

type ProjectStatus = 'stopped' | 'starting' | 'running' | 'error';
type LaunchMode = 'browser' | 'executable';

interface TestView {
  project: {
    id: string;
    name: string;
    path: string;
    workingDirectory: string;
    launchMode: LaunchMode;
    command: string;
    preferredPort: number;
    urlTemplate: string;
    icon: string | null;
    autoOpenBrowser: boolean;
    executablePath: string;
    arguments: string;
  };
  runtime: {
    projectId: string;
    status: ProjectStatus;
    pid: number | null;
    actualPort: number | null;
    resolvedCommand: string | null;
    startedAt: number | null;
    error: string | null;
  };
}

interface MockCall {
  command: string;
  args: Record<string, unknown>;
}

interface MockOptions {
  directory?: string;
  executablePath?: string | null;
  logs?: Array<{ sequence: number; timestamp: number; stream: string; line: string }>;
  rejectUpdate?: boolean;
  detection?: LaunchDetection;
  rejectDetection?: boolean;
  delayDetection?: boolean;
}

const project = {
  id: 'project-1',
  name: 'Dashboard',
  path: 'C:\\Projects\\dashboard',
  workingDirectory: 'C:\\Projects\\dashboard',
  launchMode: 'browser' as LaunchMode,
  command: 'node server.js --port {port}',
  preferredPort: 5173,
  urlTemplate: 'http://localhost:{port}',
  icon: null,
  autoOpenBrowser: true,
  executablePath: '',
  arguments: '',
};

function view(status: ProjectStatus = 'stopped', overrides: Partial<TestView['runtime']> = {}): TestView {
  return {
    project,
    runtime: {
      projectId: project.id,
      status,
      pid: status === 'running' || status === 'starting' ? 1234 : null,
      actualPort: status === 'running' ? 5173 : null,
      resolvedCommand: status === 'running' ? 'node server.js --port 5173' : null,
      startedAt: status === 'running' ? 1_700_000_000 : null,
      error: status === 'error' ? 'fixture startup failure' : null,
      ...overrides,
    },
  };
}

async function installMock(page: Page, initialViews: TestView[], options: MockOptions = {}) {
  const logs = options.logs ?? [
    {
      sequence: 1,
      timestamp: 1_700_000_000_000,
      stream: 'stderr',
      line: 'fixture startup failure',
    },
  ];

  await page.addInitScript(({ views, mockOptions, initialLogs }) => {
    type Callback = (event: { event: string; id: number; payload: unknown }) => void;
    const calls: Array<{ command: string; args: Record<string, unknown> }> = [];
    const callbacks = new Map<number, Callback>();
    const listeners = new Map<string, Set<number>>();
    let nextCallbackId = 1;

    const invoke = async (command: string, args: Record<string, unknown> = {}) => {
      calls.push({ command, args });

      if (command === 'list_projects') {
        return views;
      }
      if (command === 'get_logs') {
        return initialLogs;
      }
      if (command === 'pick_directory') {
        return mockOptions.directory ?? null;
      }
      if (command === 'detect_project_launch') {
        if (mockOptions.delayDetection) {
          await new Promise<void>((resolve) => {
            (window as Window & { __finishDetection?: () => void }).__finishDetection = resolve;
          });
        }
        if (mockOptions.rejectDetection) throw new Error('fixture folder unreadable');
        return mockOptions.detection ?? { suggestions: [], warnings: [] };
      }
      if (command === 'pick_executable') {
        return Object.prototype.hasOwnProperty.call(mockOptions, 'executablePath')
          ? mockOptions.executablePath
          : null;
      }
      if (command === 'add_project') {
        const input = (args.input ?? {}) as Record<string, unknown>;
        const addedView = {
          project: {
            id: 'added-project',
            ...input,
          },
          runtime: {
            projectId: 'added-project',
            status: 'stopped',
            pid: null,
            actualPort: null,
            resolvedCommand: null,
            startedAt: null,
            error: null,
          },
        };
        views.push(addedView);
        return addedView;
      }
      if (command === 'update_project' && mockOptions.rejectUpdate) {
        throw new Error('settings save failed');
      }
      if (command === 'update_project') {
        const input = (args.input ?? {}) as Record<string, unknown>;
        const updatedView = {
          project: {
            id: String(args.id),
            ...input,
          },
          runtime: {
            projectId: String(args.id),
            status: 'stopped',
            pid: null,
            actualPort: null,
            resolvedCommand: null,
            startedAt: null,
            error: null,
          },
        };
        const viewIndex = views.findIndex((view) => view.project.id === String(args.id));
        if (viewIndex >= 0) {
          views[viewIndex] = updatedView;
        }
        return updatedView;
      }
      if (command === 'plugin:event|listen') {
        const eventName = String(args.event);
        const callbackId = Number(args.handler);
        const eventListeners = listeners.get(eventName) ?? new Set<number>();
        eventListeners.add(callbackId);
        listeners.set(eventName, eventListeners);
        return callbackId;
      }
      if (command === 'plugin:event|unlisten') {
        const eventListeners = listeners.get(String(args.event));
        eventListeners?.delete(Number(args.eventId));
        return null;
      }
      if (command === 'plugin:event|emit') {
        const eventName = String(args.event);
        const eventListeners = listeners.get(eventName) ?? new Set<number>();
        for (const callbackId of eventListeners) {
          callbacks.get(callbackId)?.({
            event: eventName,
            id: callbackId,
            payload: args.payload,
          });
        }
        return null;
      }
      return undefined;
    };

    const mock = {
      calls,
      emit(eventName: string, payload: unknown) {
        const eventListeners = listeners.get(eventName) ?? new Set<number>();
        for (const callbackId of eventListeners) {
          callbacks.get(callbackId)?.({ event: eventName, id: callbackId, payload });
        }
      },
    };

    window.__TAURI_INTERNALS__ = {
      invoke,
      transformCallback(callback: Callback) {
        const callbackId = nextCallbackId++;
        callbacks.set(callbackId, callback);
        return callbackId;
      },
      unregisterCallback(callbackId: number) {
        callbacks.delete(callbackId);
      },
      runCallback(callbackId: number, event: { event: string; id: number; payload: unknown }) {
        callbacks.get(callbackId)?.(event);
      },
    };
    window.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
      unregisterListener(_event: string, callbackId: number) {
        callbacks.delete(callbackId);
      },
    };
    (window as Window & { __mockTauri?: typeof mock }).__mockTauri = mock;
  }, { views: initialViews, mockOptions: options, initialLogs: logs });

  await page.goto('/');
  if (initialViews.length > 0) {
    await expect(page.locator('article.project-card').first()).toBeVisible();
  } else {
    await expect(page.getByRole('heading', { name: /我的项目/ })).toBeVisible();
  }
}

async function callsFor(page: Page, command: string): Promise<MockCall[]> {
  return page.evaluate((wantedCommand) => {
    const mock = (window as Window & { __mockTauri?: { calls: MockCall[] } }).__mockTauri;
    return mock?.calls.filter((call) => call.command === wantedCommand) ?? [];
  }, command);
}

async function emitState(page: Page, status: ProjectStatus, overrides: Partial<TestView['runtime']> = {}) {
  await page.evaluate(
    ({ status: nextStatus, overrides: nextOverrides }) => {
      const mock = (
        window as Window & {
          __mockTauri?: { emit: (eventName: string, payload: unknown) => void };
        }
      ).__mockTauri;
      mock?.emit('project-state', {
        projectId: 'project-1',
        status: nextStatus,
        pid: nextStatus === 'running' || nextStatus === 'starting' ? 1234 : null,
        actualPort: nextStatus === 'running' ? 5174 : null,
        resolvedCommand: nextStatus === 'running' ? 'node server.js --port 5174' : null,
        startedAt: nextStatus === 'running' ? 1_700_000_000 : null,
        error: nextStatus === 'error' ? 'fixture startup failure' : null,
        ...nextOverrides,
      });
    },
    { status, overrides },
  );
}

async function emitLog(page: Page, entry: { sequence: number; timestamp: number; stream: string; line: string }) {
  await page.evaluate((nextEntry) => {
    const mock = (
      window as Window & {
        __mockTauri?: { emit: (eventName: string, payload: unknown) => void };
      }
    ).__mockTauri;
    mock?.emit('project-log', { projectId: 'project-1', entry: nextEntry });
  }, entry);
}

async function expectFieldHidden(dialog: ReturnType<Page['getByRole']>, label: RegExp) {
  const field = dialog.getByLabel(label);
  if (await field.count()) {
    await expect(field).toBeHidden();
  }
}

test.describe('UI with mocked Tauri IPC (not native process coverage)', () => {
  test('double click activates a stopped project once and accepts the running event', async ({ page }) => {
    await installMock(page, [view('stopped')]);
    const card = page.locator('article.project-card').first();

    await card.dblclick();
    await expect.poll(async () => (await callsFor(page, 'activate_project')).length).toBe(1);

    await emitState(page, 'running');
    await expect(card).toContainText('运行中 Running');
    await expect(card).toContainText(':5174');
  });

  test('a running project activation does not issue a second start command', async ({ page }) => {
    await installMock(page, [view('running')]);
    await page.locator('article.project-card').first().dblclick();

    await expect.poll(async () => (await callsFor(page, 'activate_project')).length).toBe(1);
    expect((await callsFor(page, 'start_project')).length).toBe(0);
  });

  test('two rapid card activations issue one backend activation request', async ({ page }) => {
    await installMock(page, [view('stopped')]);
    const card = page.locator('article.project-card').first();

    await Promise.all([card.dblclick(), card.dblclick()]);

    await expect.poll(async () => (await callsFor(page, 'activate_project')).length).toBe(1);
  });

  test('Enter on a card action invokes that action without activating the card', async ({ page }) => {
    await installMock(page, [view('stopped')]);
    const startButton = page.getByRole('button', { name: 'Start Dashboard', exact: true });

    await startButton.focus();
    await page.keyboard.press('Enter');

    await expect.poll(async () => (await callsFor(page, 'start_project')).length).toBe(1);
    expect((await callsFor(page, 'activate_project')).length).toBe(0);
  });

  test('Stop stays enabled while a project is starting', async ({ page }) => {
    await installMock(page, [view('starting')]);
    const stopButton = page.getByRole('button', { name: 'Stop Dashboard' });

    await expect(stopButton).toBeEnabled();
    await stopButton.click();
    await expect.poll(async () => (await callsFor(page, 'stop_project')).length).toBe(1);
  });

  test('settings draft survives a runtime event while the dialog remains open', async ({ page }) => {
    await installMock(page, [view('stopped')]);
    await page.getByRole('button', { name: 'Settings Dashboard' }).click();
    const dialog = page.getByRole('dialog');
    const nameField = dialog.getByLabel(/项目名称/);

    await nameField.fill('Edited locally');
    await emitState(page, 'starting');

    await expect(nameField).toHaveValue('Edited locally');
    await expect(dialog).toBeVisible();
  });

  test('error state exposes stderr in Logs without browser activation', async ({ page }) => {
    await installMock(page, [view('error')]);
    const card = page.locator('article.project-card').first();

    await expect(card).toContainText('启动失败 Error');
    await page.getByRole('button', { name: 'Logs Dashboard', exact: true }).click();
    await expect(page.getByRole('log')).toContainText('fixture startup failure');
    expect((await callsFor(page, 'activate_project')).length).toBe(0);
  });

  test('folder picker opens add form and sends the complete project input', async ({ page }) => {
    await installMock(page, [], { directory: 'D:\\Projects\\new-app', detection: webDetection() });
    await page.getByRole('button', { name: '添加项目 Add Project', exact: true }).first().click();

    const dialog = page.getByRole('dialog');
    await expect(dialog).toBeVisible();
    await expect(dialog.getByLabel(/项目路径/)).toHaveValue('D:\\Projects\\new-app');
    await expect(dialog.getByLabel(/工作目录/)).toHaveValue('D:\\Projects\\new-app');
    await expect(dialog.getByLabel(/启动命令/)).toHaveValue('npm run dev -- --port {port}');
    await expect(dialog.getByLabel(/首选端口/)).toHaveValue('5173');

    await dialog.getByRole('button', { name: '添加项目 Add Project', exact: true }).click();
    await expect.poll(async () => (await callsFor(page, 'add_project')).length).toBe(1);

    const addCall = (await callsFor(page, 'add_project'))[0];
    expect(addCall.args.input).toMatchObject({
      name: 'new-app',
      path: 'D:\\Projects\\new-app',
      workingDirectory: 'D:\\Projects\\new-app',
      command: 'npm run dev -- --port {port}',
      preferredPort: 5173,
      urlTemplate: 'http://localhost:{port}',
      autoOpenBrowser: true,
    });
    await expect(page.getByRole('heading', { name: 'new-app', exact: true })).toBeVisible();
  });

  test('settings save failure stays inside the open dialog', async ({ page }) => {
    await installMock(page, [view('stopped')], { rejectUpdate: true });
    await page.getByRole('button', { name: 'Settings Dashboard' }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByLabel(/项目名称/).fill('Edited name');
    await dialog.getByRole('button', { name: '保存设置 Save', exact: true }).click();

    await expect(dialog).toBeVisible();
    await expect(dialog.getByRole('alert')).toContainText('settings save failed');
    await expect(dialog.getByLabel(/项目名称/)).toHaveValue('Edited name');
  });

  test('a log emitted after Clear remains visible', async ({ page }) => {
    await installMock(page, [view('error')], {
      logs: [
        {
          sequence: 10,
          timestamp: 1_700_000_000_000,
          stream: 'stderr',
          line: 'old failure',
        },
      ],
    });
    await page.getByRole('button', { name: 'Logs Dashboard', exact: true }).click();
    const log = page.getByRole('log');
    await expect(log).toContainText('old failure');

    await page.getByRole('button', { name: '清空日志 Clear', exact: true }).click();
    await expect.poll(async () => (await callsFor(page, 'clear_logs')).length).toBe(1);
    await emitLog(page, {
      sequence: 11,
      timestamp: 1_700_000_000_100,
      stream: 'stdout',
      line: 'new output after clear',
    });

    await expect(log).toContainText('new output after clear');
    await expect(log).not.toContainText('old failure');
  });

  test('launch mode switch preserves shared and mode-specific fields', async ({ page }) => {
    await installMock(page, [view('stopped')]);
    await page.getByRole('button', { name: 'Settings Dashboard' }).click();
    const dialog = page.getByRole('dialog');

    await dialog.getByLabel(/项目名称/).fill('Shared name');
    await dialog.getByLabel(/项目路径/).fill('D:\\Projects\\shared');
    await dialog.getByLabel(/工作目录/).fill('D:\\Projects\\shared\\work');
    await dialog.getByLabel(/图标/).fill('🚀');
    await dialog.getByLabel(/启动方式 Launch mode/).selectOption('executable');

    await expect(dialog.getByLabel(/启动方式 Launch mode/)).toHaveValue('executable');
    await expect(dialog.getByLabel(/可执行文件 Executable/)).toBeVisible();
    await expect(dialog.getByLabel(/启动参数 Arguments/)).toBeVisible();
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.screenshot({ path: 'test-results/executable-form.png', fullPage: true });
    await expectFieldHidden(dialog, /启动命令/);
    await expectFieldHidden(dialog, /首选端口/);
    await expectFieldHidden(dialog, /浏览器地址/);
    await expectFieldHidden(dialog, /启动后打开浏览器/);

    await dialog.getByLabel(/可执行文件 Executable/).fill('D:\\Tools\\runner.cmd');
    await dialog.getByLabel(/启动参数 Arguments/).fill('--watch "demo"');
    await dialog.getByLabel(/启动方式 Launch mode/).selectOption('browser');

    await expect(dialog.getByLabel(/项目名称/)).toHaveValue('Shared name');
    await expect(dialog.getByLabel(/项目路径/)).toHaveValue('D:\\Projects\\shared');
    await expect(dialog.getByLabel(/工作目录/)).toHaveValue('D:\\Projects\\shared\\work');
    await expect(dialog.getByLabel(/图标/)).toHaveValue('🚀');
    await expect(dialog.getByLabel(/启动命令/)).toBeVisible();
    await expect(dialog.getByLabel(/首选端口/)).toBeVisible();
    await expect(dialog.getByLabel(/浏览器地址/)).toBeVisible();
    await expect(dialog.getByLabel(/启动后打开浏览器/)).toBeVisible();
    await dialog.getByLabel(/启动方式 Launch mode/).selectOption('executable');
    await expect(dialog.getByLabel(/可执行文件 Executable/)).toHaveValue('D:\\Tools\\runner.cmd');
    await expect(dialog.getByLabel(/启动参数 Arguments/)).toHaveValue('--watch "demo"');
  });

  test('executable picker cancellation leaves the field unchanged and passes directory', async ({ page }) => {
    await installMock(page, [], { directory: 'D:\\Projects\\new-app', executablePath: null });
    await page.getByRole('button', { name: '添加项目 Add Project', exact: true }).first().click();
    const dialog = page.getByRole('dialog');
    await dialog.getByLabel(/启动方式 Launch mode/).selectOption('executable');

    const executableField = dialog.getByLabel(/可执行文件 Executable/);
    await expect(executableField).toHaveValue('');
    await dialog.getByRole('button', { name: /可执行文件|executable/i }).click();

    await expect.poll(async () => (await callsFor(page, 'pick_executable')).length).toBe(1);
    const pickCall = (await callsFor(page, 'pick_executable'))[0];
    expect(pickCall.args).toMatchObject({ directory: 'D:\\Projects\\new-app' });
    await expect(executableField).toHaveValue('');
  });

  test('executable picker result submits executable input and hides the card port', async ({ page }) => {
    await installMock(page, [], {
      directory: 'D:\\Projects\\new-app',
      executablePath: 'D:\\Tools\\runner.cmd',
    });
    await page.getByRole('button', { name: '添加项目 Add Project', exact: true }).first().click();
    const dialog = page.getByRole('dialog');
    await dialog.getByLabel(/启动方式 Launch mode/).selectOption('executable');
    await dialog.getByRole('button', { name: /可执行文件|executable/i }).click();
    await expect(dialog.getByLabel(/可执行文件 Executable/)).toHaveValue('D:\\Tools\\runner.cmd');
    await dialog.getByLabel(/启动参数 Arguments/).fill('--profile demo --once');
    await dialog.getByLabel(/图标/).fill('⚡');

    await dialog.getByRole('button', { name: '添加项目 Add Project', exact: true }).click();
    await expect.poll(async () => (await callsFor(page, 'add_project')).length).toBe(1);
    const addCall = (await callsFor(page, 'add_project'))[0];
    expect(addCall.args.input).toMatchObject({
      name: 'new-app',
      path: 'D:\\Projects\\new-app',
      workingDirectory: 'D:\\Projects\\new-app',
      launchMode: 'executable',
      executablePath: 'D:\\Tools\\runner.cmd',
      arguments: '--profile demo --once',
      icon: '⚡',
    });

    const card = page.locator('article.project-card').filter({ hasText: 'new-app' });
    await expect(card).toBeVisible();
    await expect(card).not.toContainText(':5173');
  });

  test('executable settings save persists mode, path, and arguments', async ({ page }) => {
    await installMock(page, [view('stopped')]);
    await page.getByRole('button', { name: 'Settings Dashboard' }).click();
    let dialog = page.getByRole('dialog');
    await dialog.getByLabel(/启动方式 Launch mode/).selectOption('executable');
    await dialog.getByLabel(/可执行文件 Executable/).fill('C:\\Tools\\demo.exe');
    await dialog.getByLabel(/启动参数 Arguments/).fill('--portless --name test');
    await dialog.getByRole('button', { name: '保存设置 Save', exact: true }).click();

    await expect.poll(async () => (await callsFor(page, 'update_project')).length).toBe(1);
    const updateCall = (await callsFor(page, 'update_project'))[0];
    expect(updateCall.args.input).toMatchObject({
      launchMode: 'executable',
      executablePath: 'C:\\Tools\\demo.exe',
      arguments: '--portless --name test',
    });

    await page.getByRole('button', { name: 'Settings Dashboard' }).click();
    dialog = page.getByRole('dialog');
    await expect(dialog.getByLabel(/启动方式 Launch mode/)).toHaveValue('executable');
    await expect(dialog.getByLabel(/可执行文件 Executable/)).toHaveValue('C:\\Tools\\demo.exe');
    await expect(dialog.getByLabel(/启动参数 Arguments/)).toHaveValue('--portless --name test');
    await expectFieldHidden(dialog, /启动命令/);
    await expectFieldHidden(dialog, /首选端口/);
    await expectFieldHidden(dialog, /浏览器地址/);
  });
});

function webDetection(): LaunchDetection {
  return {
    suggestions: [{
      label: 'Vite · dev', detail: '来自 package.json 的 dev：vite。',
      settings: {
        workingDirectory: 'D:\\Projects\\new-app', launchMode: 'browser',
        executablePath: '', arguments: '', command: 'npm run dev -- --port {port}',
        preferredPort: 5173, urlTemplate: 'http://localhost:{port}', autoOpenBrowser: true,
      },
    }],
    warnings: [],
  };
}

test.describe('automatic launch detection', () => {
  test('detected values remain editable and only save after confirmation', async ({ page }, testInfo) => {
    await installMock(page, [], { directory: 'D:\\Projects\\new-app', detection: webDetection() });
    await page.getByRole('button', { name: '添加项目 Add Project', exact: true }).first().click();
    const dialog = page.getByRole('dialog');
    await expect(dialog.getByLabel('识别结果', { exact: true })).toHaveValue('0');
    expect((await callsFor(page, 'detect_project_launch'))[0].args).toEqual({ directory: 'D:\\Projects\\new-app' });
    expect(await callsFor(page, 'add_project')).toHaveLength(0);
    expect(await callsFor(page, 'start_project')).toHaveLength(0);
    await dialog.screenshot({ path: testInfo.outputPath('detected-web-form.png') });
    await dialog.getByLabel(/启动命令/).fill('pnpm run dev --port {port} --strictPort');
    await dialog.getByLabel(/首选端口/).fill('5188');
    await dialog.getByRole('button', { name: '添加项目 Add Project', exact: true }).click();
    await expect(dialog).toBeHidden();
    expect((await callsFor(page, 'add_project'))[0].args.input).toMatchObject({
      command: 'pnpm run dev --port {port} --strictPort', preferredPort: 5188,
    });
  });

  test('multiple entries switch the existing form and preserve name and icon', async ({ page }, testInfo) => {
    const detection = webDetection();
    detection.suggestions.push({ label: '启动脚本 · start.cmd', detail: '发现 Windows 启动文件。', settings: {
      ...detection.suggestions[0].settings, launchMode: 'executable',
      workingDirectory: 'D:\\Projects\\new-app\\dist', executablePath: 'D:\\Projects\\new-app\\dist\\start.cmd',
      arguments: '--demo', command: '', autoOpenBrowser: false,
    } });
    await installMock(page, [], { directory: 'D:\\Projects\\new-app', detection });
    await page.getByRole('button', { name: '添加项目 Add Project', exact: true }).first().click();
    const dialog = page.getByRole('dialog');
    await expect(dialog.getByLabel('识别结果', { exact: true })).toBeVisible();
    await dialog.getByLabel(/项目名称/).fill('我的项目');
    await dialog.getByLabel(/图标/).fill('🚀');
    await dialog.getByLabel('识别结果', { exact: true }).selectOption('1');
    await expect(dialog.getByLabel(/启动方式 Launch mode/)).toHaveValue('executable');
    await expect(dialog.getByLabel(/可执行文件 Executable/)).toHaveValue('D:\\Projects\\new-app\\dist\\start.cmd');
    await expect(dialog.getByLabel(/工作目录/)).toHaveValue('D:\\Projects\\new-app\\dist');
    await expect(dialog.getByLabel(/项目名称/)).toHaveValue('我的项目');
    await expect(dialog.getByLabel(/图标/)).toHaveValue('🚀');
    await dialog.screenshot({ path: testInfo.outputPath('detected-executable-form.png') });
    await dialog.getByLabel(/启动参数 Arguments/).fill('--edited');
    await dialog.getByRole('button', { name: '添加项目 Add Project', exact: true }).click();
    await expect(dialog).toBeHidden();
    expect((await callsFor(page, 'add_project'))[0].args.input).toMatchObject({ launchMode: 'executable', arguments: '--edited' });
  });

  for (const rejectDetection of [false, true]) {
    test(`manual form remains usable when detection ${rejectDetection ? 'fails' : 'finds nothing'}`, async ({ page }) => {
      await installMock(page, [], { directory: 'D:\\Projects\\new-app', rejectDetection });
      await page.getByRole('button', { name: '添加项目 Add Project', exact: true }).first().click();
      const dialog = page.getByRole('dialog');
      await expect(dialog.getByRole('status')).toContainText(rejectDetection ? '自动识别失败' : '未找到明确');
      await expect(dialog.getByLabel(/启动命令/)).toHaveValue('');
      await dialog.getByLabel(/启动命令/).fill('node server.js --port {port}');
      await dialog.getByRole('button', { name: '添加项目 Add Project', exact: true }).click();
      await expect(dialog).toBeHidden();
      expect((await callsFor(page, 'add_project'))[0].args.input).toMatchObject({ command: 'node server.js --port {port}' });
    });
  }

  test('skipping a pending detection protects manual edits from its late result', async ({ page }) => {
    await installMock(page, [], { directory: 'D:\\Projects\\new-app', delayDetection: true, detection: webDetection() });
    await page.getByRole('button', { name: '添加项目 Add Project', exact: true }).first().click();
    const dialog = page.getByRole('dialog');
    await expect(dialog.getByRole('status')).toHaveText('正在识别项目启动方式…');
    await expect(dialog.getByLabel(/启动命令/)).toBeDisabled();
    await dialog.getByRole('button', { name: '手动填写', exact: true }).click();
    await dialog.getByLabel(/启动命令/).fill('my-server --port {port}');
    await page.evaluate(() => (window as Window & { __finishDetection?: () => void }).__finishDetection?.());
    await expect(dialog.getByLabel(/启动命令/)).toHaveValue('my-server --port {port}');
    await expect(dialog.getByText('已跳过自动识别，请手动填写启动配置。')).toBeVisible();
    await dialog.getByRole('button', { name: '添加项目 Add Project', exact: true }).click();
    await expect(dialog).toBeHidden();
    expect((await callsFor(page, 'add_project'))[0].args.input).toMatchObject({ command: 'my-server --port {port}' });
  });

  test('closing during detection does not reopen the form or add a project', async ({ page }) => {
    await installMock(page, [], { directory: 'D:\\Projects\\new-app', delayDetection: true, detection: webDetection() });
    await page.getByRole('button', { name: '添加项目 Add Project', exact: true }).first().click();
    await expect(page.getByRole('dialog').getByRole('status')).toHaveText('正在识别项目启动方式…');
    await page.getByRole('button', { name: '取消 Cancel', exact: true }).click();
    await page.evaluate(() => (window as Window & { __finishDetection?: () => void }).__finishDetection?.());
    await expect(page.getByRole('dialog')).toBeHidden();
    expect(await callsFor(page, 'add_project')).toHaveLength(0);
  });

  test('changing the project path disables stale suggestions; existing settings do not auto-detect', async ({ page }) => {
    await installMock(page, [view()], { directory: 'D:\\Projects\\new-app', detection: webDetection() });
    await page.getByRole('button', { name: 'Settings Dashboard' }).click();
    await expect(page.getByRole('dialog').getByLabel(/启动命令/)).toHaveValue(project.command);
    expect(await callsFor(page, 'detect_project_launch')).toHaveLength(0);
    await page.getByRole('button', { name: '取消 Cancel', exact: true }).click();
    await page.getByRole('button', { name: '添加项目 Add Project', exact: true }).first().click();
    const dialog = page.getByRole('dialog');
    await expect(dialog.getByLabel('识别结果', { exact: true })).toBeEnabled();
    await dialog.getByLabel(/项目路径/).fill('D:\\Projects\\other');
    await expect(dialog.getByLabel('识别结果', { exact: true })).toBeDisabled();
    await expect(dialog.getByText(/项目路径已更改/)).toBeVisible();
  });
});

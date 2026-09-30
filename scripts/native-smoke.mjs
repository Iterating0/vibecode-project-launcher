// Real packaged Tauri + WebView2 IPC smoke test; no frontend/backend mocks.
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { access, mkdir, readFile, rename, stat, unlink, writeFile } from 'node:fs/promises';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium, expect } from '@playwright/test';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const executable = path.resolve(process.argv[2] ?? path.join(root, 'src-tauri/target/release/vibecode-project-launcher.exe'));
const runToken = randomUUID();
const work = path.join(root, 'test-results', `native-${runToken}`);
const configPath = process.env.APPDATA
  ? path.join(process.env.APPDATA, 'local.vibecode.project-launcher', 'projects.json')
  : undefined;
const delay = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));
const validPid = (pid) => Number.isInteger(pid) && pid > 0;
const validPort = (port) => Number.isInteger(port) && port > 0 && port <= 65535;
const parsePid = (value, label) => {
  const pid = Number(String(value).trim());
  if (!validPid(pid)) throw new Error(`${label} was not a valid process id: ${value}`);
  return pid;
};
const processExited = (pid) => {
  if (!validPid(pid)) return true;
  try {
    process.kill(pid, 0);
    return false;
  } catch (error) {
    // EPERM means that a process is still present but cannot be queried with
    // the current access token. Treating it as exited could make cleanup kill
    // an unrelated process after PID reuse.
    return error?.code !== 'EPERM';
  }
};
const waitForProcessExit = async (pid, timeout = 5000) => {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (processExited(pid)) return true;
    await delay(100);
  }
  return processExited(pid);
};

// Bind and release the actual port to prove that no listener still owns it.
// A failed bind means the port is still occupied, even if HTTP fetch happens
// to fail for that listener.
const canBindPort = (port) => new Promise((resolve) => {
  if (!validPort(port)) {
    resolve(false);
    return;
  }
  const server = net.createServer();
  let settled = false;
  const finish = (result) => {
    if (settled) return;
    settled = true;
    if (server.listening) {
      server.close(() => resolve(result));
    } else {
      resolve(result);
    }
  };
  server.once('error', (error) => {
    // IPv6 is optional on Windows. The native fixture is explicitly IPv4,
    // so an unavailable address family does not mean the IPv4 port is busy.
    finish(error.code === 'EADDRNOTAVAIL' || error.code === 'EAFNOSUPPORT');
  });
  server.listen({ port, host: '127.0.0.1', exclusive: true }, () => finish(true));
});
const portClosed = async (port) => canBindPort(port);

const freePort = (excluded = new Set()) => new Promise((resolve, reject) => {
  const server = net.createServer();
  server.on('error', reject);
  server.listen(0, '127.0.0.1', () => {
    const port = server.address().port;
    server.close(() => {
      if (excluded.has(port)) freePort(excluded).then(resolve, reject);
      else resolve(port);
    });
  });
});
const run = async () => {
  if (!configPath) throw new Error('APPDATA is not set; cannot verify native project config cleanup');
  const executableInfo = await stat(executable).catch(() => undefined);
  if (!executableInfo?.isFile()) {
    throw new Error(`Packaged executable does not exist: ${executable}`);
  }
  await access(executable);
  await mkdir(work, { recursive: true });

  const abortController = new AbortController();
  const onInterrupt = (name) => {
    if (!abortController.signal.aborted) {
      abortController.abort(new Error(`Native smoke test interrupted by ${name}`));
    }
  };
  const onSigint = () => onInterrupt('SIGINT');
  const onSigterm = () => onInterrupt('SIGTERM');
  process.once('SIGINT', onSigint);
  process.once('SIGTERM', onSigterm);
  const throwIfAborted = () => {
    if (abortController.signal.aborted) throw abortController.signal.reason;
  };
  const waitUntil = async (condition, message, timeout = 30000) => {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) {
      throwIfAborted();
      const result = await condition();
      if (result) return result;
      await delay(150);
    }
    throw new Error(message);
  };
  const readConfigProjects = async () => {
    const projects = JSON.parse(await readFile(configPath, 'utf8'));
    if (!Array.isArray(projects)) throw new Error(`Native project config is not an array: ${configPath}`);
    return projects;
  };
  const waitForConfigIds = async (ids) => waitUntil(async () => {
    try {
      const projects = await readConfigProjects();
      return ids.every((id) => projects.some((project) => project.id === id)) ? projects : false;
    } catch {
      return false;
    }
  }, `Native project config did not contain IDs for run ${runToken}`);
  const processMatchesExecutable = (pid) => {
    if (!validPid(pid)) return false;
    try {
      const actualPath = execFileSync('powershell.exe', ['-NoProfile', '-Command',
        '[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new(); $process = Get-Process -Id ([int]$env:VIBECODE_SMOKE_PID) -ErrorAction Stop; $process.Path'], {
        encoding: 'utf8', windowsHide: true,
        env: { ...process.env, VIBECODE_SMOKE_PID: String(pid) },
      }).trim();
      return path.normalize(actualPath).toLowerCase() === path.normalize(executable).toLowerCase();
    } catch {
      return false;
    }
  };
  let cdpPort;
  let preferredPort;
  let executablePreferredPort;
  let launchPid;
  let browser;
  let page;
  const addedIds = [];
  const rootPids = new Map();
  const childPids = new Map();
  const cleanupErrors = [];
  let testFailure;
  let resultPayload;
  const invoke = (command, args = {}) => page.evaluate(({ command: commandName, args: commandArgs }) =>
    window.__TAURI_INTERNALS__.invoke(commandName, commandArgs), { command, args });

  const cleanupConfig = async () => {
    if (!addedIds.length) return;
    const projects = await readConfigProjects();
    const remaining = projects.filter((project) => !addedIds.includes(project.id));
    if (remaining.length !== projects.length) {
      const temporary = `${configPath}.smoke-${runToken}.tmp`;
      try {
        await writeFile(temporary, `${JSON.stringify(remaining, null, 2)}\n`);
        await rename(temporary, configPath);
      } finally {
        await unlink(temporary).catch(() => undefined);
      }
    }
    const leftover = (await readConfigProjects()).filter((project) => addedIds.includes(project.id));
    if (leftover.length) {
      throw new Error(`Native smoke project records remained after cleanup: ${leftover.map((project) => project.id).join(', ')}`);
    }
  };

  try {
    cdpPort = await freePort();
    preferredPort = await freePort(new Set([cdpPort]));
    executablePreferredPort = await freePort(new Set([cdpPort, preferredPort]));
    launchPid = parsePid(execFileSync('powershell.exe', ['-NoProfile', '-Command',
      '$launched = Start-Process -FilePath $env:VIBECODE_SMOKE_EXE -WindowStyle Hidden -PassThru -ErrorAction Stop; if ($launched.Id -le 0) { throw "Start-Process returned an invalid PID" }; $launched.Id'], {
        encoding: 'utf8', windowsHide: true,
        env: { ...process.env, VIBECODE_SMOKE_EXE: executable,
          WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${cdpPort}`,
          WEBVIEW2_USER_DATA_FOLDER: path.join(work, 'webview') },
      }), 'launcher');
    const cdpVersion = await waitUntil(async () => {
      try {
        const response = await fetch(`http://127.0.0.1:${cdpPort}/json/version`, { signal: AbortSignal.timeout(500) });
        if (!response.ok) return false;
        const version = await response.json();
        return typeof version.Browser === 'string'
          && typeof version['Protocol-Version'] === 'string'
          && typeof version.webSocketDebuggerUrl === 'string'
          && version.webSocketDebuggerUrl.includes(`:${cdpPort}/`) ? version : false;
      } catch {
        return false;
      }
    }, 'WebView2 remote debugging did not expose a valid CDP version');
    browser = await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`);
    page = await waitUntil(async () => {
      for (const candidate of browser.contexts().flatMap((context) => context.pages())) {
        try {
          if (await candidate.evaluate(() => Boolean(window.__TAURI_INTERNALS__))) return candidate;
        } catch {
          // The page may be navigating while WebView2 creates the Tauri view.
        }
      }
      return false;
    }, `No Tauri launcher webview found through CDP ${cdpVersion.webSocketDebuggerUrl}`);
    await page.waitForFunction(() => Boolean(window.__TAURI_INTERNALS__));
    const fixture = path.join(root, 'tests/fixtures/fixture-server.cjs');
    const executableFixture = path.join(root, 'tests/fixtures/executable-worker.cjs');
    const names = {
      A: `Native acceptance ${runToken} A`,
      B: `Native acceptance ${runToken} B`,
      failure: `Native acceptance ${runToken} failure`,
      executable: `Native acceptance ${runToken} executable`,
      executableFailure: `Native acceptance ${runToken} executable failure`,
      batchStop: `Native acceptance ${runToken} batch stop`,
      batchExit: `Native acceptance ${runToken} batch exit`,
    };
    const childPidFile = (suffix) => path.join(work, `${suffix}.pid`);
    const makeInput = (name, extra) => ({
      name, path: root, workingDirectory: root,
      command: `npm --prefix "${path.dirname(fixture)}" run dev -- --port {port} ${extra}`,
      preferredPort, urlTemplate: 'http://127.0.0.1:{port}', icon: null, autoOpenBrowser: false,
    });
    const makeExecutableInput = (name, executablePath, argumentsValue) => ({
      name, path: root, workingDirectory: root,
      launchMode: 'executable', executablePath, arguments: argumentsValue,
      command: '', preferredPort: executablePreferredPort, urlTemplate: '',
      icon: null, autoOpenBrowser: false,
    });
    const waitForProjectStatus = async (id, status, message, timeout = 30000) => waitUntil(async () => {
      const view = (await invoke('list_projects')).find((candidate) => candidate.project.id === id);
      return view?.runtime.status === status ? view : false;
    }, message, timeout);
    for (const suffix of ['A', 'B']) {
      const view = await invoke('add_project', {
        input: makeInput(names[suffix], `--child --child-pid-file "${childPidFile(suffix)}"`),
      });
      assert.ok(view?.project?.id, `Native add_project did not return an ID for ${suffix}`);
      addedIds.push(view.project.id);
    }
    await waitForConfigIds(addedIds);

    await Promise.all(addedIds.map((id) => invoke('start_project', { id, openBrowser: false })));
    const active = await waitUntil(async () => {
      const own = (await invoke('list_projects')).filter((view) => addedIds.includes(view.project.id));
      return own.length === 2 && own.every((view) => view.runtime.status === 'running'
        && validPid(view.runtime.pid) && validPort(view.runtime.actualPort)) ? own : false;
    }, 'Two projects did not reach running');
    const activeById = new Map(active.map((view) => [view.project.id, view]));
    const projectA = activeById.get(addedIds[0]);
    const projectB = activeById.get(addedIds[1]);
    assert.ok(projectA, 'Project A was not returned by list_projects');
    assert.ok(projectB, 'Project B was not returned by list_projects');
    assert.notEqual(projectA.runtime.actualPort, projectB.runtime.actualPort);
    assert.ok(active.some((view) => view.runtime.actualPort === preferredPort), 'One concurrent project must receive the preferred port');
    rootPids.set(projectA.project.id, parsePid(projectA.runtime.pid, 'Project A root'));
    rootPids.set(projectB.project.id, parsePid(projectB.runtime.pid, 'Project B root'));
    for (const [suffix, id] of [['A', addedIds[0]], ['B', addedIds[1]]]) {
      childPids.set(id, await waitUntil(async () => {
        try {
          return parsePid(await readFile(childPidFile(suffix), 'utf8'), `${suffix} fixture child`);
        } catch {
          return false;
        }
      }, `${suffix} fixture child PID was not recorded`));
    }

    const fetchFixture = async (view, label) => waitUntil(async () => {
      try {
        const response = await fetch(`http://127.0.0.1:${view.runtime.actualPort}`, { signal: AbortSignal.timeout(1000) });
        if (!response.ok) return false;
        return (await response.text()) === 'fixture-ok';
      } catch {
        return false;
      }
    }, `${label} fixture endpoint was not ready`, 10000);
    await fetchFixture(projectA, 'Project A');
    await fetchFixture(projectB, 'Project B');

    const initialPid = rootPids.get(projectA.project.id);
    await invoke('start_project', { id: projectA.project.id, openBrowser: false });
    assert.equal((await invoke('list_projects')).find((view) => view.project.id === projectA.project.id).runtime.pid, initialPid);

    await invoke('stop_project', { id: projectB.project.id });
    await waitUntil(() => portClosed(projectB.runtime.actualPort), 'Stopped project B port remains open');
    await waitUntil(() => processExited(rootPids.get(projectB.project.id)), 'Stopped project B root process remains alive');
    await waitUntil(() => processExited(childPids.get(projectB.project.id)), 'Stopped project B child remains alive');
    await fetchFixture(projectA, 'Project A after stopping B');

    // Detect a real Vite entry without launching it, then start the exact
    // suggested command through ProjectManager. npm resolves the repository's
    // installed Vite from the fixture's ancestor node_modules/.bin directory.
    const detectionDirectory = path.join(work, 'detected web project');
    await mkdir(detectionDirectory, { recursive: true });
    await writeFile(path.join(detectionDirectory, 'package.json'), JSON.stringify({
      private: true, scripts: { dev: `vite --port ${projectA.runtime.actualPort}` },
    }));
    await writeFile(path.join(detectionDirectory, 'index.html'), '<h1>detected-vite-ready</h1>');
    await writeFile(path.join(detectionDirectory, 'start.cmd'), '@echo off\r\necho unexpected>should-not-exist.txt\r\n');
    const beforeDetection = await readFile(configPath, 'utf8');
    const detected = await invoke('detect_project_launch', { directory: detectionDirectory });
    assert.equal(await readFile(configPath, 'utf8'), beforeDetection, 'Detection must not save project configuration');
    assert.equal(await access(path.join(detectionDirectory, 'should-not-exist.txt')).then(() => true, () => false), false,
      'Detection must not execute a discovered batch file');
    const detectedWeb = detected.suggestions.find((suggestion) => suggestion.label === 'Vite · dev');
    const detectedBatch = detected.suggestions.find((suggestion) => suggestion.settings.launchMode === 'executable');
    assert.ok(detectedWeb, 'Real IPC did not return a Vite suggestion');
    assert.equal(detectedWeb.settings.command, 'npm run dev -- --port {port} --strictPort');
    assert.equal(detectedWeb.settings.preferredPort, projectA.runtime.actualPort);
    assert.equal(path.normalize(detectedBatch?.settings.executablePath ?? ''), path.join(detectionDirectory, 'start.cmd'));
    assert.equal(path.normalize(detectedWeb.settings.workingDirectory), detectionDirectory);
    const detectedView = await invoke('add_project', { input: {
      name: `Native detection ${runToken}`, path: detectionDirectory, icon: null,
      ...detectedWeb.settings, autoOpenBrowser: false,
    } });
    assert.ok(detectedView?.project?.id, 'Detected project did not return an ID');
    addedIds.push(detectedView.project.id);
    await invoke('start_project', { id: detectedView.project.id, openBrowser: false });
    const detectedRunning = await waitForProjectStatus(detectedView.project.id, 'running', 'Detected Vite command failed to start');
    assert.notEqual(detectedRunning.runtime.actualPort, projectA.runtime.actualPort, 'Detected port must adapt to an existing listener');
    rootPids.set(detectedView.project.id, parsePid(detectedRunning.runtime.pid, 'Detected Vite root'));
    const detectedResponse = await fetch(detectedWeb.settings.urlTemplate.replace('{port}', String(detectedRunning.runtime.actualPort)), { signal: AbortSignal.timeout(2000) });
    assert.equal(detectedResponse.status, 200);
    assert.match(await detectedResponse.text(), /detected-vite-ready/);
    await invoke('stop_project', { id: detectedView.project.id });
    await waitUntil(() => portClosed(detectedRunning.runtime.actualPort), 'Detected Vite port was not released');
    await waitUntil(() => processExited(detectedRunning.runtime.pid), 'Detected Vite process remained alive');

    const directPidFile = path.join(work, 'direct.pid');
    const directReadyFile = path.join(work, 'direct.ready');
    const directFailurePidFile = path.join(work, 'direct-failure.pid');
    const batchStopPath = path.join(work, 'batch-stop.bat');
    const batchStopPidFile = path.join(work, 'batch-stop.pid');
    const batchStopReadyFile = path.join(work, 'batch-stop.ready');
    const batchExitPath = path.join(work, 'batch-exit.bat');
    const batchExitPidFile = path.join(work, 'batch-exit.pid');
    const batchExitReadyFile = path.join(work, 'batch-exit.ready');
    const writeBatchFixture = async (batchPath, pidFile, readyFile, marker) => {
      await writeFile(batchPath, [
        '@echo off',
        'setlocal',
        'chcp 65001 >nul',
        `start "" /b "${process.execPath}" "${executableFixture}" --pid-file "${pidFile}" --ready-file "${readyFile}" --stdout ${marker} %*`,
        '',
      ].join('\r\n'));
    };
    await writeBatchFixture(batchStopPath, batchStopPidFile, batchStopReadyFile, 'BATCH_STOP_READY');
    await writeBatchFixture(batchExitPath, batchExitPidFile, batchExitReadyFile, 'BATCH_EXIT_READY');

    const addExecutable = async (input, label) => {
      const view = await invoke('add_project', { input });
      assert.ok(view?.project?.id, `Executable project did not return an ID: ${label}`);
      addedIds.push(view.project.id);
      return view;
    };
    const direct = await addExecutable(
      makeExecutableInput(
        names.executable,
        process.execPath,
        `"${executableFixture}" --pid-file "${directPidFile}" --ready-file "${directReadyFile}" --exit-after 5000 --exit-code 0 --stdout DIRECT_READY`,
      ),
      names.executable,
    );
    const directFailure = await addExecutable(
      makeExecutableInput(
        names.executableFailure,
        process.execPath,
        `"${executableFixture}" --pid-file "${directFailurePidFile}" --exit-after 750 --exit-code 23 --stderr "executable intentional failure"`,
      ),
      names.executableFailure,
    );
    const batchStop = await addExecutable(
      makeExecutableInput(names.batchStop, batchStopPath, '--batch-stop'),
      names.batchStop,
    );
    const batchExit = await addExecutable(
      makeExecutableInput(names.batchExit, batchExitPath, '--batch-exit'),
      names.batchExit,
    );
    await waitForConfigIds(addedIds);

    await invoke('start_project', { id: direct.project.id, openBrowser: false });
    const directRunning = await waitForProjectStatus(
      direct.project.id,
      'running',
      'Direct node executable did not reach running',
      10000,
    );
    assert.equal(directRunning.runtime.actualPort, null, 'Executable mode must not allocate a port');
    assert.equal(await portClosed(executablePreferredPort), true, 'Executable mode must leave its preferred port free');
    rootPids.set(direct.project.id, parsePid(directRunning.runtime.pid, 'Direct executable root'));
    await page.reload();
    await page.getByRole('heading', { name: names.executable, exact: true }).waitFor();
    const directCard = page.locator('article.project-card').filter({
      has: page.getByRole('heading', { name: names.executable, exact: true }),
    });
    await expect(directCard).toBeVisible();
    await expect(directCard).not.toContainText(`:${executablePreferredPort}`);
    let directAnsweredHttp = false;
    try {
      await fetch(`http://127.0.0.1:${executablePreferredPort}`, { signal: AbortSignal.timeout(500) });
      directAnsweredHttp = true;
    } catch {
      // The executable fixture deliberately has no HTTP listener.
    }
    assert.equal(directAnsweredHttp, false, 'Direct executable unexpectedly exposed an HTTP endpoint');
    await waitForProjectStatus(direct.project.id, 'stopped', 'Normal executable exit did not become stopped', 10000);
    await waitUntil(() => processExited(rootPids.get(direct.project.id)), 'Normal executable root process remains alive');

    await invoke('start_project', { id: directFailure.project.id, openBrowser: false });
    const directFailureError = await waitForProjectStatus(
      directFailure.project.id,
      'error',
      'Non-zero executable exit did not become error',
      10000,
    );
    assert.equal(directFailureError.runtime.actualPort, null, 'Failed executable must not report a port');
    await waitUntil(async () => (await invoke('get_logs', { id: directFailure.project.id }))
      .some((entry) => entry.stream === 'stderr' && entry.line.includes('executable intentional failure')),
    'Executable stderr was not captured');
    rootPids.set(directFailure.project.id, await waitUntil(async () => {
      try {
        return parsePid(await readFile(directFailurePidFile, 'utf8'), 'Direct failure executable');
      } catch {
        return false;
      }
    }, 'Direct failure executable PID was not recorded'));
    await waitUntil(() => processExited(rootPids.get(directFailure.project.id)), 'Failed executable root process remains alive');

    await invoke('start_project', { id: batchStop.project.id, openBrowser: false });
    const batchStopRunning = await waitForProjectStatus(
      batchStop.project.id,
      'running',
      'Batch executable did not reach running',
    );
    assert.equal(batchStopRunning.runtime.actualPort, null, 'Batch executable must not report a port');
    rootPids.set(batchStop.project.id, parsePid(batchStopRunning.runtime.pid, 'Batch stop root'));
    childPids.set(batchStop.project.id, await waitUntil(async () => {
      try {
        return parsePid(await readFile(batchStopPidFile, 'utf8'), 'Batch stop child');
      } catch {
        return false;
      }
    }, 'Batch stop child PID was not recorded'));
    const batchStopPid = rootPids.get(batchStop.project.id);
    await Promise.all([
      invoke('activate_project', { id: batchStop.project.id }),
      invoke('activate_project', { id: batchStop.project.id }),
    ]);
    assert.equal(
      (await invoke('list_projects')).find((view) => view.project.id === batchStop.project.id).runtime.pid,
      batchStopPid,
      'Repeated executable activation spawned a second process',
    );
    await invoke('stop_project', { id: batchStop.project.id });
    await waitForProjectStatus(batchStop.project.id, 'stopped', 'Batch Stop did not become stopped', 10000);
    await waitUntil(() => processExited(rootPids.get(batchStop.project.id)), 'Batch Stop left cmd.exe alive');
    await waitUntil(() => processExited(childPids.get(batchStop.project.id)), 'Batch Stop left its node child alive');

    await invoke('start_project', { id: batchExit.project.id, openBrowser: false });
    const batchExitRunning = await waitForProjectStatus(
      batchExit.project.id,
      'running',
      'Batch exit executable did not reach running',
    );
    assert.equal(batchExitRunning.runtime.actualPort, null, 'Batch exit executable must not report a port');
    rootPids.set(batchExit.project.id, parsePid(batchExitRunning.runtime.pid, 'Batch exit root'));
    childPids.set(batchExit.project.id, await waitUntil(async () => {
      try {
        return parsePid(await readFile(batchExitPidFile, 'utf8'), 'Batch exit child');
      } catch {
        return false;
      }
    }, 'Batch exit child PID was not recorded'));
    await waitUntil(() => processExited(rootPids.get(batchExit.project.id)), 'Batch exit cmd.exe did not exit before launcher close');
    assert.equal(await processExited(childPids.get(batchExit.project.id)), false, 'Batch exit child ended before launcher close');
    const batchExitView = (await invoke('list_projects')).find((view) => view.project.id === batchExit.project.id);
    assert.equal(batchExitView?.runtime.status, 'running', 'Batch child should keep executable card running after cmd.exe exits');
    const batchExitCard = page.locator('article.project-card').filter({ hasText: names.batchExit });
    await expect(batchExitCard).toContainText('运行中 Running');

    const failed = await invoke('add_project', { input: makeInput(names.failure, '--fail') });
    assert.ok(failed?.project?.id, 'Failure project did not return an ID');
    addedIds.push(failed.project.id);
    await waitForConfigIds(addedIds);
    let startupError;
    await invoke('start_project', { id: failed.project.id, openBrowser: false }).catch((error) => { startupError = String(error); });
    try {
      await waitUntil(async () => (await invoke('list_projects')).find((view) => view.project.id === failed.project.id)?.runtime.status === 'error', 'Failure did not produce error state', 10000);
    } catch (error) {
      const current = (await invoke('list_projects')).find((view) => view.project.id === failed.project.id);
      const output = await invoke('get_logs', { id: failed.project.id });
      throw new Error(`${error.message}: ${JSON.stringify({ startupError, current, output })}`);
    }
    await waitUntil(async () => (await invoke('get_logs', { id: failed.project.id })).some((entry) => entry.stream === 'stderr' && entry.line.includes('intentional')), 'stderr not captured');
    const failedView = (await invoke('list_projects')).find((view) => view.project.id === failed.project.id);
    if (validPid(failedView?.runtime?.pid)) {
      rootPids.set(failed.project.id, failedView.runtime.pid);
      await waitUntil(() => processExited(failedView.runtime.pid), 'Failed fixture root process remains alive');
    }
    // Reload synchronizes UI with projects added through the real native IPC.
    await page.reload();
    await page.getByRole('heading', { name: names.A, exact: true }).waitFor();
    await page.screenshot({ path: path.join(work, 'native-launcher.png'), fullPage: true });
    // Normal window close exercises the desktop exit hook while project A runs.
    execFileSync('powershell.exe', ['-NoProfile', '-Command',
      '$process = Get-Process -Id ([int]$env:VIBECODE_SMOKE_PID) -ErrorAction Stop; if (-not $process.CloseMainWindow()) { throw "CloseMainWindow returned false" }'], {
        windowsHide: true, env: { ...process.env, VIBECODE_SMOKE_PID: String(launchPid) },
      });
    await waitUntil(() => processExited(launchPid), 'Launcher did not exit on window close');
    await waitUntil(() => portClosed(projectA.runtime.actualPort), 'Exit did not release Project A port');
    await waitUntil(() => processExited(rootPids.get(projectA.project.id)), 'Exit left Project A root process alive');
    await waitUntil(() => processExited(childPids.get(projectA.project.id)), 'Exit left Project A child process alive');
    await waitUntil(() => processExited(rootPids.get(batchExit.project.id)), 'Exit left batch cmd.exe alive');
    await waitUntil(() => processExited(childPids.get(batchExit.project.id)), 'Exit left batch node child alive');
    resultPayload = {
      result: 'passed',
      tests: [
        'real IPC config write/read',
        'read-only launch detection through real IPC',
        'detected Vite command with fixed script port and dynamic port conflict',
        'concurrent browser port conflict',
        'executable mode without port or HTTP readiness',
        'direct node.exe arguments and normal exit',
        'executable non-zero failure and stderr',
        'duplicate executable activation',
        'batch Stop tree isolation',
        'batch normal window close cleanup',
        'browser failure stderr',
      ],
      screenshot: path.join(work, 'native-launcher.png'),
      addedIds,
      runToken,
    };
  } catch (error) {
    testFailure = error;
  } finally {
    // Terminate only the launcher created by this script if a test failed. Its
    // Job Object ownership must clean its own descendants even on forced exit.
    if (validPid(launchPid) && !processExited(launchPid)) {
      for (const id of addedIds) {
        try {
          await invoke('stop_project', { id });
        } catch (error) {
          cleanupErrors.push(`stop_project(${id}) failed: ${error.message}`);
        }
        try {
          await invoke('remove_project', { id });
        } catch (error) {
          cleanupErrors.push(`remove_project(${id}) failed: ${error.message}`);
        }
      }
      if (!processExited(launchPid)) {
        if (!processMatchesExecutable(launchPid)) {
          if (!processExited(launchPid)) {
            cleanupErrors.push(`refused taskkill for PID ${launchPid}: executable path did not match ${executable}`);
          }
        } else {
          try {
            execFileSync('taskkill.exe', ['/PID', String(launchPid), '/T', '/F'], { windowsHide: true, stdio: 'ignore' });
          } catch (error) {
            if (!processExited(launchPid)) {
              cleanupErrors.push(`taskkill for launcher PID ${launchPid} failed: ${error.message}`);
            }
          }
        }
      }
    }

    if (validPid(launchPid) && !await waitForProcessExit(launchPid)) {
      cleanupErrors.push(`launcher PID ${launchPid} remained alive during cleanup`);
    }
    for (const [id, pid] of [...rootPids, ...childPids]) {
      if (validPid(pid) && !await waitForProcessExit(pid)) {
        cleanupErrors.push(`managed process PID ${pid} for project ${id} remained alive during cleanup`);
      }
    }

    if (!validPid(launchPid) || processExited(launchPid)) {
      try {
        await cleanupConfig();
      } catch (error) {
        cleanupErrors.push(`native project config cleanup failed: ${error.message}`);
      }
    } else if (addedIds.length) {
      cleanupErrors.push('skipped config cleanup while the launcher process was still alive');
    }

    try {
      await browser?.close();
    } catch (error) {
      cleanupErrors.push(`CDP browser cleanup failed: ${error.message}`);
    }
    process.removeListener('SIGINT', onSigint);
    process.removeListener('SIGTERM', onSigterm);
  }

  if (cleanupErrors.length) {
    const cleanupFailure = new Error(cleanupErrors.join('; '));
    if (testFailure) console.error(`Native smoke cleanup also failed: ${cleanupFailure.message}`);
    else testFailure = cleanupFailure;
  }
  if (testFailure) throw testFailure;
  console.log(JSON.stringify(resultPayload, null, 2));
};

run().catch((error) => {
  console.error(`Native smoke test failed: ${error.stack ?? error.message ?? error}`);
  process.exitCode = 1;
});

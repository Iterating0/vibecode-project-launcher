#!/usr/bin/env node

// A dependency-free local server used by native process-lifecycle tests.
// It deliberately supports a slow readiness path, an intentional startup
// failure, and a long-lived child process so tests can exercise the launcher
// rather than a fake ProcessManager.

const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');

const args = process.argv.slice(2);

function hasFlag(...names) {
  return names.some((name) => args.includes(name) || args.some((arg) => arg.startsWith(`${name}=`)));
}

function option(...names) {
  for (const name of names) {
    const index = args.indexOf(name);
    if (index >= 0 && args[index + 1] !== undefined) {
      return args[index + 1];
    }

    const prefix = `${name}=`;
    const inline = args.find((arg) => arg.startsWith(prefix));
    if (inline) {
      return inline.slice(prefix.length);
    }
  }

  return undefined;
}

function integerOption(defaultValue, ...names) {
  const value = option(...names);
  if (value === undefined) {
    return defaultValue;
  }

  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 0) {
    throw new Error(`invalid integer for ${names.join(' / ')}: ${value}`);
  }

  return parsed;
}

function writeFileIfRequested(fileName, contents) {
  if (!fileName) {
    return;
  }

  fs.mkdirSync(path.dirname(path.resolve(fileName)), { recursive: true });
  fs.writeFileSync(fileName, `${contents}\n`, 'utf8');
}

function keepAlive() {
  // A timer keeps the worker alive without opening a pipe that would keep the
  // parent process alive after its HTTP server is closed.
  setInterval(() => {}, 60_000);
}

const workerMode = hasFlag('--worker');
const childPidFile = option('--child-pid-file', '--pid-file');
const markerFile = option('--marker-file');

if (workerMode) {
  writeFileIfRequested(childPidFile, process.pid);
  writeFileIfRequested(markerFile, 'worker-ready');
  process.stdout.write(`WORKER pid=${process.pid}\n`);
  keepAlive();
  return;
}

let port;
try {
  port = integerOption(0, '--port');
} catch (error) {
  process.stderr.write(`${error.message}\n`);
  process.exit(2);
}

const host = option('--host') || '127.0.0.1';
const readyDelay = integerOption(0, '--ready-delay', '--delay', '--startup-delay');
const failDelay = integerOption(0, '--fail-delay');
const failMode = hasFlag('--fail', '--fail-startup');
const childMode = hasFlag('--child', '--spawn-child');
const exitCode = integerOption(42, '--exit-code');
const responseBody = option('--body') || 'fixture-ok';
const readyFile = option('--ready-file');
const childMarkerFile = option('--child-marker-file');

let server;
let child;
let shuttingDown = false;

function shutdown(code = 0) {
  if (shuttingDown) {
    return;
  }
  shuttingDown = true;

  if (!server) {
    process.exit(code);
    return;
  }

  server.close(() => process.exit(code));
}

function startChild() {
  const childArgs = [__filename, '--worker'];
  if (childPidFile) {
    childArgs.push('--pid-file', childPidFile);
  }
  if (childMarkerFile) {
    childArgs.push('--marker-file', childMarkerFile);
  }

  child = spawn(process.execPath, childArgs, {
    cwd: process.cwd(),
    detached: false,
    stdio: ['ignore', 'ignore', 'ignore'],
    windowsHide: true,
  });
  // The parent remains responsible for the HTTP service, while the worker is
  // intentionally left alive if only the parent is terminated. A Windows Job
  // Object or an explicit tree kill must clean up both processes.
  child.unref();
  writeFileIfRequested(childMarkerFile, `parent=${process.pid}`);
}

function listen() {
  if (failMode) {
    const fail = () => {
      process.stderr.write('fixture startup failure (intentional)\n');
      process.exit(exitCode);
    };

    if (failDelay > 0) {
      setTimeout(fail, failDelay);
    } else {
      fail();
    }
    return;
  }

  server = http.createServer((request, response) => {
    if (request.url === '/health' || request.url === '/') {
      response.writeHead(200, { 'content-type': 'text/plain; charset=utf-8' });
      response.end(responseBody);
      return;
    }

    response.writeHead(404, { 'content-type': 'text/plain; charset=utf-8' });
    response.end('not-found');
  });

  server.once('error', (error) => {
    process.stderr.write(`fixture server error: ${error.message}\n`);
    process.exit(1);
  });

  server.listen(port, host, () => {
    if (childMode) {
      startChild();
    }

    const address = server.address();
    const actualPort = typeof address === 'object' && address ? address.port : port;
    writeFileIfRequested(readyFile, actualPort);
    process.stdout.write(`READY http://${host}:${actualPort}\n`);
  });
}

process.on('SIGINT', () => shutdown(0));
process.on('SIGTERM', () => shutdown(0));

if (readyDelay > 0) {
  setTimeout(listen, readyDelay);
} else {
  listen();
}

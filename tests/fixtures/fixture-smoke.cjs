#!/usr/bin/env node

// Small dependency-free contract check for fixture-server.cjs. Native Rust
// tests still own process-tree assertions; this script only checks that the
// fixture itself can bind, answer, delay readiness, and report failure.

const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

const fixture = path.join(__dirname, 'fixture-server.cjs');

function waitForExit(child) {
  return new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('exit', (code, signal) => resolve({ code, signal }));
  });
}

function get(url) {
  return new Promise((resolve, reject) => {
    const request = http.get(url, (response) => {
      const chunks = [];
      response.on('data', (chunk) => chunks.push(chunk));
      response.on('end', () => resolve({ status: response.statusCode, body: Buffer.concat(chunks).toString('utf8') }));
    });
    request.once('error', reject);
  });
}

async function waitForReady(child, timeoutMs = 5_000) {
  return new Promise((resolve, reject) => {
    let output = '';
    const timeout = setTimeout(() => reject(new Error(`fixture did not become ready; output=${output}`)), timeoutMs);
    child.stdout.on('data', (chunk) => {
      output += chunk.toString('utf8');
      const match = output.match(/READY http:\/\/[^:]+:(\d+)/);
      if (match) {
        clearTimeout(timeout);
        resolve(Number(match[1]));
      }
    });
  });
}

async function run() {
  const temporaryDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'vpl-fixture-'));
  const readyFile = path.join(temporaryDirectory, 'ready.txt');
  let server;
  let failedServer;

  try {
    server = spawn(process.execPath, [fixture, '--port', '0', '--ready-delay', '75', '--ready-file', readyFile], {
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    });
    const port = await waitForReady(server);
    if (!fs.existsSync(readyFile) || Number(fs.readFileSync(readyFile, 'utf8').trim()) !== port) {
      throw new Error('fixture ready-file did not contain the bound port');
    }
    const response = await get(`http://127.0.0.1:${port}/health`);
    if (response.status !== 200 || response.body !== 'fixture-ok') {
      throw new Error(`unexpected fixture response: ${JSON.stringify(response)}`);
    }

    failedServer = spawn(process.execPath, [fixture, '--port', '0', '--fail', '--fail-delay', '25'], {
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: true,
    });
    let stderr = '';
    failedServer.stderr.on('data', (chunk) => {
      stderr += chunk.toString('utf8');
    });
    const failure = await waitForExit(failedServer);
    if (failure.code !== 42 || !stderr.includes('fixture startup failure')) {
      throw new Error(`unexpected failure result: code=${failure.code}, stderr=${stderr}`);
    }

    console.log(`fixture smoke ok (port ${port}, delayed ready, stderr failure)`);
  } finally {
    if (server && server.exitCode === null) {
      server.kill();
      await waitForExit(server).catch(() => undefined);
    }
    if (failedServer && failedServer.exitCode === null) {
      failedServer.kill();
      await waitForExit(failedServer).catch(() => undefined);
    }
    fs.rmSync(temporaryDirectory, { recursive: true, force: true });
  }
}

run().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});


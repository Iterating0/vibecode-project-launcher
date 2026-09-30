#!/usr/bin/env node

// A dependency-free process used by the packaged executable-mode smoke test.
// It has no HTTP listener: the launcher must consider it running immediately
// after spawn and must report its eventual exit code through project-state.

const fs = require('node:fs');
const path = require('node:path');

const args = process.argv.slice(2);

function option(name, defaultValue = undefined) {
  const index = args.indexOf(name);
  if (index >= 0 && args[index + 1] !== undefined) return args[index + 1];
  const prefix = `${name}=`;
  const inline = args.find((arg) => arg.startsWith(prefix));
  return inline ? inline.slice(prefix.length) : defaultValue;
}

function integerOption(name, defaultValue) {
  const value = option(name);
  if (value === undefined) return defaultValue;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 0) {
    throw new Error(`invalid integer for ${name}: ${value}`);
  }
  return parsed;
}

function writeFile(fileName, value) {
  if (!fileName) return;
  fs.mkdirSync(path.dirname(path.resolve(fileName)), { recursive: true });
  fs.writeFileSync(fileName, `${value}\n`, 'utf8');
}

const pidFile = option('--pid-file');
const readyFile = option('--ready-file');
const exitAfter = integerOption('--exit-after', 0);
const exitCode = integerOption('--exit-code', 0);
const stderrLine = option('--stderr');
const stdoutLine = option('--stdout', 'EXECUTABLE_READY');

writeFile(pidFile, process.pid);
writeFile(readyFile, process.pid);
process.stdout.write(`${stdoutLine} pid=${process.pid}\n`);

if (stderrLine) {
  process.stderr.write(`${stderrLine}\n`);
}

if (exitAfter > 0) {
  setTimeout(() => process.exit(exitCode), exitAfter);
} else {
  // Keep the executable process alive for Stop/launcher-exit cleanup tests.
  setInterval(() => {}, 60_000);
}


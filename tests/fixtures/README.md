# Native test fixture

`fixture-server.cjs` is a dependency-free HTTP server for Windows lifecycle
tests. It is intentionally a real Node process, so the Rust tests can verify
the command/process-tree behavior of the launcher.

Examples from PowerShell (run from the repository root):

```powershell
$fixture = (Resolve-Path 'tests/fixtures/fixture-server.cjs').Path
node $fixture --port 5173
node $fixture --port 5173 --ready-delay 750 --ready-file "$env:TEMP\fixture-ready.txt"
node $fixture --port 5173 --fail 2> "$env:TEMP\fixture-error.txt"
node $fixture --port 5173 --child --child-pid-file "$env:TEMP\fixture-child.pid"
```

Supported options:

- `--port <number>` or `--port=<number>`: HTTP port; `0` asks the OS for an
  available port.
- `--ready-delay <ms>` (aliases `--delay`, `--startup-delay`): wait before
  binding the HTTP listener.
- `--fail` (alias `--fail-startup`): print an intentional error to stderr and
  exit with code `42` (use `--fail-delay` to delay that exit).
- `--child` (alias `--spawn-child`): spawn a long-lived worker process. The
  worker PID is written to `--child-pid-file` / `--pid-file`.
- `--ready-file <path>`: write the actual bound port after the listener is
  ready.
- `--body <text>`: response body for `/` and `/health`.

The parent intentionally does not clean up the worker when only the parent is
terminated. A test that stops a launcher-created service must therefore verify
that its Windows Job Object/process-tree cleanup removes both PIDs.

## Direct executable fixture

`executable-worker.cjs` has no HTTP listener. The packaged smoke test runs it
through the actual `node.exe` file and through a generated `.bat` that uses
`start` and then exits. It supports `--pid-file`, `--ready-file`, `--stdout`,
`--stderr`, `--exit-after` (milliseconds), and `--exit-code`. Without an exit
timer it remains alive for Stop and launcher-exit tests. All paths and project
IDs belong to the current test run and are cleaned by the native smoke script.

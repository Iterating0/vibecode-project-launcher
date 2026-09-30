# Acceptance plan

This is the executable acceptance checklist for the MVP. The native checks
must run on Windows because process-tree cleanup uses Windows Job Objects.
The Node fixture under `tests/fixtures/fixture-server.cjs` is a real HTTP
server and child process; it does not start or modify any user project.

## Preconditions

From the repository root:

```powershell
npm ci
node --check tests/fixtures/fixture-server.cjs
node tests/fixtures/fixture-smoke.cjs
npm test
cargo test --manifest-path src-tauri/Cargo.toml
npm run build
```

The fixture requires only the Node runtime already used by the launcher. Native
lifecycle tests must exercise the launcher's Rust `ProcessManager`/
`ProjectManager` APIs and inspect their returned state and logs; a standalone
fixture process or a browser mock is not evidence of native cleanup.

## Required native scenarios

1. **Start and readiness**: configure the fixture as `node fixture-server.cjs
   --port {port} --ready-delay 250`, start it, and assert that the state stays
   `starting` until the health/readiness check succeeds. The recorded actual
   port must be the allocated port and stdout must contain `READY`.
2. **Preferred-port fallback**: start Project A on preferred port `5173`, then
   start Project B with the same preferred port. Assert A remains on `5173`, B
   gets a free port (normally `5174`), and both HTTP endpoints respond.
3. **Concurrent start idempotence**: issue two start/use requests for one
   project before readiness. Assert one child process, one reserved port, and
   one browser-open decision. A second request after `running` must only open
   the existing URL.
4. **Stop during start**: use `--ready-delay 5000`, request stop before it is
   ready, and assert the process exits, the port reservation is released, no
   browser open is requested, and the project ends in a stable stopped/error
   state rather than returning to running.
5. **Failure and log boundary**: configure `--fail` (optionally with
   `--fail-delay`). Assert a non-zero exit and an `error` state promptly after
   the process exits (without waiting for the full readiness timeout), stderr
   containing `fixture startup failure`, no browser open, and bounded
   stdout/stderr. The error response must not lose the last stderr bytes.
6. **Port conflict/launch failure**: occupy a requested port outside the
   launcher, start a fixture with that preferred port, and assert the launcher
   chooses a free port or reports a clear allocation error without killing the
   unrelated process.
7. **Process-tree stop**: use `--child --child-pid-file <temp-file>`, wait for
   both parent and worker PIDs, stop the project, then poll with
   `Get-Process -Id` until both are gone. The worker check is required; checking
   only the root PID does not prove cleanup.
8. **Launcher exit cleanup**: run the same child fixture through the full Tauri
   application, close the launcher, and verify every PID recorded by the
   launcher is gone. Do not use `taskkill /IM node.exe`, which could terminate
   unrelated user processes.
9. **Restart lifecycle**: start a fixture, record its root/worker PIDs, request
   restart, and assert the old process tree exits before the new one reaches
   `running`. The project must expose only the new PID and one held port.
10. **Clear logs**: open Logs, clear the existing output, emit or produce a new
    stdout/stderr line, and assert the new line is visible. Clearing must not
    make later entries disappear because of a stale sequence cutoff.

## UI browser checks (mocked IPC only)

Playwright tests may run against the Vite page with a mocked Tauri `invoke` and
event bridge. Those tests can verify card rendering, double-click orchestration,
error/log/settings modals, duplicate-start suppression, and the absence of a
browser activation on failure. They must label the bridge as mocked and must
not be reported as proof of native process cleanup, port allocation, or Tauri
exit hooks. Native scenarios above remain mandatory.

Recommended mocked cases:

- stopped card → double click → exactly one `activate_project` call →
  `project-state(running)`;
- running card → double click → exactly one `activate_project` call without a
  second process start (the backend owns this decision);
- start error → error card and stderr in Logs, no browser activation;
- two quick double clicks → one `activate_project` call;
- focused card action button + Enter → action only, with no card activation;
- `starting` card → Stop is enabled and invokes `stop_project`;
- open settings, edit a field, deliver a runtime state event → the draft field
  remains unchanged until the user submits or closes the dialog.
- open Logs, clear it, deliver a later log event → the new line remains visible.
- directory picker result → add form → `add_project` receives the selected
  path as both path and working directory, the `{port}` command, and preferred
  port 5173; the new card appears.
- rejected settings save → dialog stays open with a role `alert` containing the
  backend error.
- settings save/remove updates the card grid without a page navigation.

## Review gates

- UI does not spawn/kill processes, inspect ports, or write JSON directly.
- A command with `{port}` receives the allocated port exactly once; URL
  templates resolve against the actual port.
- Stop/restart/exit paths act only on launcher-owned processes.
- Running projects are not restarted by refresh or a second double click.
- Logs have explicit byte/line limits and preserve stdout/stderr boundaries.
- `npm run build` and `cargo test --manifest-path src-tauri/Cargo.toml` pass on
  the target Windows machine.

## First-version verification record (`v0.1.0`)

On 2026-09-18 in the Windows workspace, the following checks passed:

- `node --check tests/fixtures/fixture-server.cjs`
- `node --check tests/fixtures/fixture-smoke.cjs`
- `node tests/fixtures/fixture-smoke.cjs` (delayed readiness and intentional
  stderr failure)
- `npm run build`
- `npm test` / `npx playwright test tests/ui-mocked-ipc.spec.ts` (10 mocked IPC
  tests, including add, duplicate activation, Enter propagation, starting Stop,
  settings draft/error, and log clear)
- `cargo test --manifest-path src-tauri/Cargo.toml` (32 Windows tests, including
  ProjectManager lifecycle cases, immediate stop/start generation isolation,
  startup failure stderr and browser suppression, Job Object child cleanup,
  and IPv6 listener ownership)
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`
- `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`

- `npm run desktop:build` (optimized Windows executable)
- `npm run test:native` (real packaged Tauri + WebView2, without mocked IPC):
  JSON persistence, two concurrently running projects with one preferred port,
  duplicate-start protection, isolated Stop with port release, failure stderr,
  and normal window-close cleanup. The fixture command runs through
  `npm run dev` and creates an additional Node child; both the recorded root
  and child PIDs are checked after Stop and launcher exit. Temporary project
  records are removed and cleanup is verified.

The Playwright UI tests use a mocked Tauri bridge. Native smoke uses the actual
desktop executable and native APIs. Browser-open intent and failure suppression
are checked in Rust with an injected browser opener; automated acceptance does
not launch the user's default browser.

## Executable-mode acceptance

- First-version JSON without `launchMode` loads as `browser`.
- The launch-mode dropdown changes the visible configuration fields; shared
  fields and inactive-mode drafts survive switching.
- Executable file selection supports `.exe`, `.bat`, and `.cmd`; cancelling
  the picker preserves the current path. Add and Settings persist the mode,
  executable path, arguments, and working directory.
- Executable cards show the file name instead of a browser port.
- Direct `node.exe` launches use real arguments and working directories,
  reach running without an HTTP listener, and allocate no port. A zero exit
  becomes stopped; a nonzero exit becomes error with stderr available.
- Repeated executable activation keeps one managed process tree and never
  asks the launcher browser opener to run.
- Batch scripts that start a child and then exit leave the child running
  under the same Job Object. Stop and launcher exit terminate those children.
- Unicode and space-containing paths are covered by native tests. UTF-8 batch
  fixtures explicitly select code page 65001; the launcher does not override
  the encoding of user-authored scripts.

Verified on Windows on 2026-09-19:

- `cargo test --manifest-path src-tauri/Cargo.toml`: 39 passed, 0 ignored.
- `npm test`: 14 mocked IPC tests passed.
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`: passed.
- `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`: passed.
- `npm run desktop:build`: passed, producing the updated Windows executable.
- `npm run test:native`: all 9 packaged native scenarios passed, including
  direct EXE arguments and exits, no HTTP readiness or port allocation,
  duplicate activation, batch root exit with live child, Stop isolation,
  and normal launcher-close cleanup. Browser-mode checks also passed.
- The executable-mode form and final native card wall were visually reviewed.
  Temporary native test records were removed by the smoke test.

## Launch-detection acceptance (2026-09-20)

- `npm run build`: passed.
- `npm run test:core`: 52 passed (13 launch-detector tests). Covers package
  managers, framework scripts, explicit ports, custom-script fallback, nested
  working directories, Python instance names and virtual environments, static
  sites, EXE/batch entries, malformed/oversized metadata, unknown directories,
  and compatibility with existing configuration validation.
- `npm test -- --workers=2 --timeout=60000`: 21 mocked IPC tests passed.
  Adding opens the original form immediately, prefills detected values, permits
  edits and candidate selection, and saves only on submission. Failed/empty
  detection remains manually editable. Skipping or closing ignores late
  results, changing the path disables stale candidates, and Settings retains
  saved values without re-detection. Web and executable forms were visually
  reviewed from Playwright screenshots.
- `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check` and
  `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`:
  passed.
- `npm run tauri -- build --debug --no-bundle`: passed; built the desktop app
  with bundled frontend assets. The existing release app was running and was
  left intact, so this acceptance uses the separate debug executable.
- `npm run test:native -- src-tauri/target/debug/vibecode-project-launcher.exe`:
  all 11 real Tauri/WebView2 scenarios passed. New native checks confirm that
  detection does not modify persisted configuration or execute discovered
  scripts, returns actual Vite and batch suggestions, and that the suggested
  Vite command starts successfully with HTTP 200 even when its script's fixed
  port is already occupied. Stop releases the allocated port and process.
  Existing browser/executable lifecycle checks also pass; temporary records and
  test-owned processes were cleaned up.

Detection does not promise arbitrary framework/configuration parsing or
multi-service orchestration. Custom scripts and unsupported entry layouts
remain editable suggestions or manual configuration, with the uncertainty
shown in the form.

## Public-source verification (2026-09-30)

The public-source snapshot was verified on Windows with Node.js 24.14.0 and
Cargo 1.98.1:

- Frontend production build and the Node server fixture smoke test passed.
- All 21 Playwright UI tests passed with mocked IPC.
- All 52 Rust core tests passed; formatting and Clippy checks passed.
- The packaged debug desktop build passed all 11 real Tauri/WebView2 native
  smoke scenarios, including launch detection, port conflict handling,
  executable launch, process-tree cleanup, and persistence cleanup.
- The portable shortcut launcher resolved Node.js without a machine-specific
  hardcoded path. Its UTF-8 BOM is preserved for Windows PowerShell 5.1.
- `npm audit` reported zero known vulnerabilities for the committed lockfile
  at the time of this check; this is not a guarantee against future advisories.

GitHub Actions runs a separate Windows check for the frontend build, fixture,
mocked UI tests, Rust core tests, formatting, and Clippy. The interactive
packaged-native smoke test is a local check and is not claimed as CI coverage.

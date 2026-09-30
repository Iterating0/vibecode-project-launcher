//! Process lifecycle and bounded output collection.
//!
//! Windows process ownership is implemented in [`super::windows_process`].
//! That module uses `CreateProcessW(CREATE_SUSPENDED)`, assigns the process to
//! a private Job Object before `ResumeThread`, and keeps the process and Job
//! handles alive for the full managed lifetime. This module coordinates that
//! native owner with the project-level event stream.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
#[cfg(not(windows))]
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use super::windows_process;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
pub enum ProcessEvent {
    Output {
        project_id: String,
        pid: u32,
        stream: OutputStream,
        line: String,
    },
    Exited {
        project_id: String,
        pid: u32,
        code: Option<i32>,
        requested: bool,
    },
}

#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub project_id: String,
    pub command: String,
    pub working_directory: PathBuf,
    pub executable_path: Option<PathBuf>,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub project_id: String,
    pub pid: u32,
    pub command: String,
    pub started_at: u64,
}

#[cfg(windows)]
type NativeProcess = windows_process::NativeProcess;

#[cfg(not(windows))]
struct NativeProcess {
    pid: u32,
    stdout: Option<File>,
    stderr: Option<File>,
    child: Mutex<Child>,
}

#[cfg(not(windows))]
impl NativeProcess {
    fn wait(&self) -> Result<i32, String> {
        self.child
            .lock()
            .map_err(|_| "child state lock poisoned".to_owned())?
            .wait()
            .map(|status| status.code().unwrap_or(-1))
            .map_err(|error| format!("wait failed: {error}"))
    }

    fn wait_for_tree(&self) -> Result<i32, String> {
        self.wait()
    }

    fn terminate(&self) -> Result<(), String> {
        let mut child = self
            .child
            .try_lock()
            .map_err(|_| "child is already being waited on".to_owned())?;
        child
            .kill()
            .map_err(|error| format!("terminate failed: {error}"))
    }

    fn owns_listening_port(&self, _port: u16) -> bool {
        true
    }
}

struct ManagedProcess {
    project_id: String,
    pid: u32,
    command: String,
    started_at: u64,
    native: Arc<NativeProcess>,
    readers: Mutex<Vec<thread::JoinHandle<()>>>,
    direct_mode: bool,
    running: AtomicBool,
    stop_requested: AtomicBool,
}

struct ProcessManagerInner {
    processes: Mutex<HashMap<String, Arc<ManagedProcess>>>,
    events: mpsc::SyncSender<ProcessEvent>,
}

struct ProcessOwner {
    inner: Arc<ProcessManagerInner>,
}

impl Drop for ProcessOwner {
    fn drop(&mut self) {
        terminate_all(&self.inner);
    }
}

/// Owns every process started by the launcher. Output uses a bounded channel
/// so a child that writes continuously cannot grow launcher memory.
pub struct ProcessManager {
    inner: Arc<ProcessManagerInner>,
    owner: Arc<ProcessOwner>,
    event_receiver: Mutex<Option<mpsc::Receiver<ProcessEvent>>>,
}

impl Clone for ProcessManager {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            owner: Arc::clone(&self.owner),
            event_receiver: Mutex::new(None),
        }
    }
}

impl ProcessManager {
    pub fn new() -> Self {
        let (events, event_receiver) = mpsc::sync_channel(2048);
        let inner = Arc::new(ProcessManagerInner {
            processes: Mutex::new(HashMap::new()),
            events,
        });
        let owner = Arc::new(ProcessOwner {
            inner: Arc::clone(&inner),
        });
        Self {
            inner,
            owner,
            event_receiver: Mutex::new(Some(event_receiver)),
        }
    }

    pub fn take_event_receiver(&self) -> Option<mpsc::Receiver<ProcessEvent>> {
        self.event_receiver.lock().ok()?.take()
    }

    pub fn start(&self, spec: ProcessSpec) -> Result<ProcessInfo, String> {
        if spec.project_id.trim().is_empty() {
            return Err("project id is empty".to_owned());
        }
        if spec.executable_path.is_none() && spec.command.trim().is_empty() {
            return Err("start command is empty".to_owned());
        }
        if !spec.working_directory.is_dir() {
            return Err(format!(
                "working directory does not exist: {}",
                spec.working_directory.display()
            ));
        }

        let mut processes = self
            .inner
            .processes
            .lock()
            .map_err(|_| "process state lock poisoned".to_owned())?;
        if processes
            .get(&spec.project_id)
            .is_some_and(|process| process.running.load(Ordering::Acquire))
        {
            return Err("project is already running or starting".to_owned());
        }

        let direct_mode = spec.executable_path.is_some();
        let mut native = spawn_native(&spec)?;
        let pid = native.pid;
        let stdout = native.stdout.take();
        let stderr = native.stderr.take();
        let process = Arc::new(ManagedProcess {
            project_id: spec.project_id.clone(),
            pid,
            command: spec.command.clone(),
            started_at: unix_seconds(),
            native: Arc::new(native),
            readers: Mutex::new(Vec::new()),
            direct_mode,
            running: AtomicBool::new(true),
            stop_requested: AtomicBool::new(false),
        });
        processes.insert(spec.project_id.clone(), Arc::clone(&process));
        drop(processes);

        if let Some(stdout) = stdout {
            if let Some(reader) = spawn_output_reader(
                stdout,
                spec.project_id.clone(),
                pid,
                OutputStream::Stdout,
                self.inner.events.clone(),
            ) {
                if let Ok(mut readers) = process.readers.lock() {
                    readers.push(reader);
                }
            }
        }
        if let Some(stderr) = stderr {
            if let Some(reader) = spawn_output_reader(
                stderr,
                spec.project_id.clone(),
                pid,
                OutputStream::Stderr,
                self.inner.events.clone(),
            ) {
                if let Ok(mut readers) = process.readers.lock() {
                    readers.push(reader);
                }
            }
        }

        let inner = Arc::clone(&self.inner);
        let process_for_thread = Arc::clone(&process);
        if let Err(error) = thread::Builder::new()
            .name(format!("launcher-wait-{pid}"))
            .spawn(move || wait_for_process(inner, process_for_thread))
        {
            let _ = process.native.terminate();
            if let Ok(mut map) = self.inner.processes.lock() {
                map.remove(&spec.project_id);
            }
            return Err(format!("failed to start process watcher: {error}"));
        }

        Ok(ProcessInfo {
            project_id: spec.project_id,
            pid,
            command: spec.command,
            started_at: process.started_at,
        })
    }

    pub fn stop(&self, project_id: &str) -> Result<bool, String> {
        let process = self
            .inner
            .processes
            .lock()
            .map_err(|_| "process state lock poisoned".to_owned())?
            .get(project_id)
            .cloned();
        let Some(process) = process else {
            return Ok(false);
        };
        if !process.running.load(Ordering::Acquire) {
            return Ok(false);
        }
        process.stop_requested.store(true, Ordering::Release);
        process.native.terminate()?;
        Ok(true)
    }

    pub fn stop_all(&self) {
        terminate_all(&self.inner);
    }

    pub fn is_running(&self, project_id: &str) -> bool {
        self.inner
            .processes
            .lock()
            .ok()
            .and_then(|map| map.get(project_id).cloned())
            .is_some_and(|process| process.running.load(Ordering::Acquire))
    }

    pub fn pid(&self, project_id: &str) -> Option<u32> {
        self.inner
            .processes
            .lock()
            .ok()
            .and_then(|map| map.get(project_id).map(|process| process.pid))
    }

    pub fn info(&self, project_id: &str) -> Option<ProcessInfo> {
        self.inner
            .processes
            .lock()
            .ok()
            .and_then(|map| map.get(project_id).cloned())
            .map(|process| ProcessInfo {
                project_id: process.project_id.clone(),
                pid: process.pid,
                command: process.command.clone(),
                started_at: process.started_at,
            })
    }

    pub fn owns_listening_port(&self, project_id: &str, port: u16) -> bool {
        let process = self
            .inner
            .processes
            .lock()
            .ok()
            .and_then(|map| map.get(project_id).cloned());
        process.is_some_and(|process| {
            process.running.load(Ordering::Acquire) && process.native.owns_listening_port(port)
        })
    }
}

fn terminate_all(inner: &Arc<ProcessManagerInner>) {
    let processes: Vec<_> = inner
        .processes
        .lock()
        .map(|map| map.values().cloned().collect())
        .unwrap_or_default();
    for process in processes {
        process.stop_requested.store(true, Ordering::Release);
        let _ = process.native.terminate();
    }
}

fn wait_for_process(inner: Arc<ProcessManagerInner>, process: Arc<ManagedProcess>) {
    // Browser-mode commands use the shell as the lifecycle root.  Preserve
    // that mode's existing behavior: publish after the root exits, then kill
    // any descendants.  Direct executable/script mode keeps the Job alive
    // until every descendant exits, so a batch file using `start` is still
    // represented as running until its started application exits.
    let result = if process.direct_mode {
        process.native.wait_for_tree()
    } else {
        let result = process.native.wait();
        let _ = process.native.terminate();
        result
    };
    if process.direct_mode && result.is_err() {
        let _ = process.native.terminate();
    }
    process.running.store(false, Ordering::Release);
    // Pipe EOF is observed before Exited is sent. This preserves the useful
    // ordering guarantee that the final stderr/stdout lines are already in
    // the bounded event queue when the project manager marks the run failed.
    if let Ok(mut readers) = process.readers.lock() {
        for reader in readers.drain(..) {
            let _ = reader.join();
        }
    }
    let code = result.ok();
    let requested = process.stop_requested.load(Ordering::Acquire);
    let _ = inner.events.send(ProcessEvent::Exited {
        project_id: process.project_id.clone(),
        pid: process.pid,
        code,
        requested,
    });
    if let Ok(mut processes) = inner.processes.lock() {
        if processes
            .get(&process.project_id)
            .is_some_and(|current| Arc::ptr_eq(current, &process))
        {
            processes.remove(&process.project_id);
        }
    }
}

fn spawn_output_reader(
    reader: File,
    project_id: String,
    pid: u32,
    stream: OutputStream,
    events: mpsc::SyncSender<ProcessEvent>,
) -> Option<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name(format!(
            "launcher-{}-{pid}",
            match stream {
                OutputStream::Stdout => "stdout",
                OutputStream::Stderr => "stderr",
            }
        ))
        .spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut bytes = Vec::with_capacity(256);
            loop {
                bytes.clear();
                match read_bounded_line(&mut reader, &mut bytes) {
                    Ok(true) => {
                        if events
                            .send(ProcessEvent::Output {
                                project_id: project_id.clone(),
                                pid,
                                stream,
                                line: String::from_utf8_lossy(&bytes).into_owned(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(false) | Err(_) => break,
                }
            }
        })
        .ok()
}

/// Read at most 16 KiB at a time without first allocating an unbounded line.
fn read_bounded_line<R: BufRead>(reader: &mut R, bytes: &mut Vec<u8>) -> std::io::Result<bool> {
    const MAX_LINE_BYTES: usize = 16 * 1024;
    loop {
        let (take, newline_ends_chunk, empty) = {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                (0, false, true)
            } else {
                let capacity = MAX_LINE_BYTES.saturating_sub(bytes.len());
                let newline = buffer.iter().position(|byte| *byte == b'\n');
                let wanted = newline.map(|index| index + 1).unwrap_or(buffer.len());
                let take = wanted.min(capacity);
                bytes.extend_from_slice(&buffer[..take]);
                (take, newline.is_some_and(|index| take == index + 1), false)
            }
        };
        if empty {
            return Ok(!bytes.is_empty());
        }
        reader.consume(take);
        if newline_ends_chunk {
            while bytes.last() == Some(&b'\n') || bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            return Ok(true);
        }
        if bytes.len() >= MAX_LINE_BYTES {
            return Ok(true);
        }
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(windows)]
fn spawn_native(spec: &ProcessSpec) -> Result<NativeProcess, String> {
    match spec.executable_path.as_deref() {
        Some(executable) => {
            windows_process::spawn_executable(executable, &spec.arguments, &spec.working_directory)
        }
        None => windows_process::spawn(&spec.command, &spec.working_directory),
    }
}

#[cfg(not(windows))]
fn spawn_native(spec: &ProcessSpec) -> Result<NativeProcess, String> {
    let mut child = match spec.executable_path.as_deref() {
        Some(executable) => {
            let mut command = Command::new(executable);
            command.args(spec.arguments.split_whitespace());
            command
        }
        None => {
            let mut command = Command::new("sh");
            command.arg("-c").arg(&spec.command);
            command
        }
    };
    child
        .current_dir(&spec.working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = child
        .spawn()
        .map_err(|error| format!("failed to start command: {error}"))?;
    let pid = child.id();
    Ok(NativeProcess {
        pid,
        stdout: child.stdout.take(),
        stderr: child.stderr.take(),
        child: Mutex::new(child),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn duplicate_project_start_is_rejected() {
        let directory = tempdir().expect("tempdir");
        let manager = ProcessManager::new();
        let command = if cfg!(windows) {
            "ping 127.0.0.1 -n 3 > nul"
        } else {
            "sleep 2"
        };
        let spec = ProcessSpec {
            project_id: "duplicate".to_owned(),
            command: command.to_owned(),
            working_directory: directory.path().to_path_buf(),
            executable_path: None,
            arguments: String::new(),
        };
        manager.start(spec.clone()).expect("first start");
        assert!(manager.start(spec).is_err());
        manager.stop_all();
    }

    #[test]
    fn output_and_exit_events_are_observable() {
        let directory = tempdir().expect("tempdir");
        let manager = ProcessManager::new();
        let receiver = manager.take_event_receiver().expect("receiver");
        let command = if cfg!(windows) {
            "echo launcher-test"
        } else {
            "printf launcher-test"
        };
        manager
            .start(ProcessSpec {
                project_id: "events".to_owned(),
                command: command.to_owned(),
                working_directory: directory.path().to_path_buf(),
                executable_path: None,
                arguments: String::new(),
            })
            .expect("start");
        let mut saw_output = false;
        let mut saw_exit = false;
        for _ in 0..10 {
            if let Ok(event) = receiver.recv_timeout(Duration::from_secs(1)) {
                match event {
                    ProcessEvent::Output { line, .. } if line.contains("launcher-test") => {
                        saw_output = true;
                    }
                    ProcessEvent::Exited { project_id, .. } if project_id == "events" => {
                        saw_exit = true;
                    }
                    _ => {}
                }
            }
            if saw_output && saw_exit {
                break;
            }
        }
        assert!(saw_output, "missing output event");
        assert!(saw_exit, "missing exit event");
    }
}

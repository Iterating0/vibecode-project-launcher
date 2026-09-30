use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use uuid::Uuid;

use super::config_manager::ConfigManager;
use super::models::{
    LaunchMode, LogEntry, Project, ProjectInput, ProjectStatus, ProjectView, RuntimeSnapshot,
};
use super::port_manager::PortManager;
use super::process_manager::{OutputStream, ProcessEvent, ProcessManager, ProcessSpec};
use crate::services::browser_service::{open_url, render_url};
use crate::services::readiness::wait_for_http_ready;

const MAX_LOG_ENTRIES: usize = 500;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub type BrowserOpener = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync + 'static>;

/// A small event bridge keeps this module independent of Tauri and makes the
/// process lifecycle straightforward to test without a UI runtime.
pub type EventSink = Arc<dyn Fn(&str, Value) + Send + Sync + 'static>;

struct ProjectManagerInner {
    config: ConfigManager,
    ports: PortManager,
    projects: RwLock<Vec<Project>>,
    runtimes: Mutex<HashMap<String, RuntimeSnapshot>>,
    logs: Mutex<HashMap<String, VecDeque<LogEntry>>>,
    held_ports: Mutex<HashMap<String, u16>>,
    active_pids: Mutex<HashMap<String, u32>>,
    browser_intents: Mutex<HashSet<String>>,
    generations: Mutex<HashMap<String, u64>>,
    cancel_tokens: Mutex<HashMap<String, Arc<AtomicBool>>>,
    cancel_requested: Mutex<HashSet<String>>,
    operations: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    log_sequences: Mutex<HashMap<String, u64>>,
    event_sink: Mutex<Option<EventSink>>,
    process_manager: ProcessManager,
    browser_opener: BrowserOpener,
    shutting_down: AtomicBool,
}

#[derive(Clone)]
pub struct ProjectManager {
    inner: Arc<ProjectManagerInner>,
}

impl ProjectManager {
    pub fn new(config_path: PathBuf) -> Result<Self, String> {
        Self::build(config_path, ProcessManager::new(), default_browser_opener())
    }

    /// Build a manager with a caller supplied process manager in tests or
    /// embedding hosts. The normal app uses [`Self::new`].
    pub fn with_process_manager(
        config_path: PathBuf,
        process_manager: ProcessManager,
    ) -> Result<Self, String> {
        Self::build(config_path, process_manager, default_browser_opener())
    }

    /// Test and embedding hook for observing browser intent without opening a
    /// real system browser.
    pub fn with_browser_opener(
        config_path: PathBuf,
        browser_opener: BrowserOpener,
    ) -> Result<Self, String> {
        Self::build(config_path, ProcessManager::new(), browser_opener)
    }

    /// Test and embedding hook combining a supplied process manager with a
    /// browser opener.
    pub fn with_process_manager_and_browser(
        config_path: PathBuf,
        process_manager: ProcessManager,
        browser_opener: BrowserOpener,
    ) -> Result<Self, String> {
        Self::build(config_path, process_manager, browser_opener)
    }

    fn build(
        config_path: PathBuf,
        process_manager: ProcessManager,
        browser_opener: BrowserOpener,
    ) -> Result<Self, String> {
        let config = ConfigManager::new(config_path);
        let projects = config.load()?;
        let mut runtimes = HashMap::new();
        let mut logs = HashMap::new();
        let mut log_sequences = HashMap::new();
        for project in &projects {
            runtimes.insert(project.id.clone(), stopped_snapshot(&project.id));
            logs.insert(project.id.clone(), VecDeque::new());
            log_sequences.insert(project.id.clone(), 0);
        }
        let receiver = process_manager
            .take_event_receiver()
            .ok_or_else(|| "process event receiver is already taken".to_owned())?;
        let inner = Arc::new(ProjectManagerInner {
            config,
            ports: PortManager::new(),
            projects: RwLock::new(projects),
            runtimes: Mutex::new(runtimes),
            logs: Mutex::new(logs),
            held_ports: Mutex::new(HashMap::new()),
            active_pids: Mutex::new(HashMap::new()),
            browser_intents: Mutex::new(HashSet::new()),
            generations: Mutex::new(HashMap::new()),
            cancel_tokens: Mutex::new(HashMap::new()),
            cancel_requested: Mutex::new(HashSet::new()),
            operations: Mutex::new(HashMap::new()),
            log_sequences: Mutex::new(log_sequences),
            event_sink: Mutex::new(None),
            process_manager,
            browser_opener,
            shutting_down: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&inner);
        thread::Builder::new()
            .name("launcher-process-events".to_owned())
            .spawn(move || process_event_loop(weak, receiver))
            .map_err(|error| format!("failed to start process event worker: {error}"))?;
        Ok(Self { inner })
    }

    pub fn set_event_sink(&self, sink: EventSink) {
        if let Ok(mut target) = self.inner.event_sink.lock() {
            *target = Some(sink);
        }
    }

    pub fn list_projects(&self) -> Vec<ProjectView> {
        let projects = self
            .inner
            .projects
            .read()
            .map(|value| value.clone())
            .unwrap_or_default();
        let runtimes = self.inner.runtimes.lock().ok();
        projects
            .into_iter()
            .map(|project| {
                let runtime = runtimes
                    .as_ref()
                    .and_then(|map| map.get(&project.id).cloned())
                    .unwrap_or_else(|| stopped_snapshot(&project.id));
                ProjectView { project, runtime }
            })
            .collect()
    }

    pub fn add_project(&self, input: ProjectInput) -> Result<ProjectView, String> {
        ConfigManager::validate_input(&input)?;
        let project = Project {
            id: Uuid::new_v4().to_string(),
            name: input.name,
            path: input.path,
            working_directory: input.working_directory,
            command: input.command,
            preferred_port: input.preferred_port,
            url_template: input.url_template,
            icon: input.icon,
            auto_open_browser: input.auto_open_browser,
            launch_mode: input.launch_mode,
            executable_path: input.executable_path,
            arguments: input.arguments,
        };
        let mut projects = self
            .inner
            .projects
            .write()
            .map_err(|_| "project state lock poisoned".to_owned())?;
        projects.push(project.clone());
        if let Err(error) = self.inner.config.save(&projects) {
            projects.pop();
            return Err(error);
        }
        drop(projects);
        if let Ok(mut runtimes) = self.inner.runtimes.lock() {
            runtimes.insert(project.id.clone(), stopped_snapshot(&project.id));
        }
        if let Ok(mut logs) = self.inner.logs.lock() {
            logs.insert(project.id.clone(), VecDeque::new());
        }
        if let Ok(mut sequences) = self.inner.log_sequences.lock() {
            sequences.insert(project.id.clone(), 0);
        }
        Ok(self.view_for(&project.id))
    }

    pub fn update_project(&self, id: &str, input: ProjectInput) -> Result<ProjectView, String> {
        ConfigManager::validate_input(&input)?;
        let operation = self.operation_for(id)?;
        let _operation = operation
            .lock()
            .map_err(|_| "project operation lock poisoned".to_owned())?;
        if self.inner.process_manager.is_running(id)
            || self
                .runtime(id)
                .is_some_and(|runtime| runtime.status == ProjectStatus::Starting)
        {
            return Err("stop the project before editing its settings".to_owned());
        }
        let mut projects = self
            .inner
            .projects
            .write()
            .map_err(|_| "project state lock poisoned".to_owned())?;
        let index = projects
            .iter()
            .position(|project| project.id == id)
            .ok_or_else(|| "project not found".to_owned())?;
        let previous = projects[index].clone();
        projects[index] = Project {
            id: id.to_owned(),
            name: input.name,
            path: input.path,
            working_directory: input.working_directory,
            command: input.command,
            preferred_port: input.preferred_port,
            url_template: input.url_template,
            icon: input.icon,
            auto_open_browser: input.auto_open_browser,
            launch_mode: input.launch_mode,
            executable_path: input.executable_path,
            arguments: input.arguments,
        };
        if let Err(error) = self.inner.config.save(&projects) {
            projects[index] = previous;
            return Err(error);
        }
        drop(projects);
        Ok(self.view_for(id))
    }

    pub fn remove_project(&self, id: &str) -> Result<(), String> {
        self.project(id)?;
        self.request_cancel(id);
        let operation = self.operation_for(id)?;
        let _operation = operation
            .lock()
            .map_err(|_| "project operation lock poisoned".to_owned())?;
        self.invalidate_generation(id);
        if let Ok(mut intents) = self.inner.browser_intents.lock() {
            intents.remove(id);
        }
        self.stop_process_and_wait(id)?;
        self.clear_cancel_state(id);
        self.mark_stopped(id);

        let mut projects = self
            .inner
            .projects
            .write()
            .map_err(|_| "project state lock poisoned".to_owned())?;
        let previous = projects.clone();
        projects.retain(|project| project.id != id);
        if let Err(error) = self.inner.config.save(&projects) {
            *projects = previous;
            return Err(error);
        }
        drop(projects);
        if let Ok(mut runtimes) = self.inner.runtimes.lock() {
            runtimes.remove(id);
        }
        if let Ok(mut logs) = self.inner.logs.lock() {
            logs.remove(id);
        }
        if let Ok(mut sequences) = self.inner.log_sequences.lock() {
            sequences.remove(id);
        }
        Ok(())
    }

    pub fn start_project(&self, id: &str, open_browser: bool) -> Result<(), String> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err("launcher is shutting down".to_owned());
        }
        let project = self.project(id)?;
        let operation = self.operation_for(id)?;
        let _operation = operation
            .lock()
            .map_err(|_| "project operation lock poisoned".to_owned())?;
        let wants_browser = project.launch_mode == LaunchMode::Browser
            && (open_browser || project.auto_open_browser);
        if self.is_cancel_requested(id) {
            return Ok(());
        }
        if self.inner.process_manager.is_running(id) {
            if project.launch_mode == LaunchMode::Browser
                && wants_browser
                && !self.is_cancel_requested(id)
            {
                if let Ok(mut intents) = self.inner.browser_intents.lock() {
                    intents.insert(id.to_owned());
                }
                if let Some(runtime) = self.runtime(id) {
                    if runtime.status == ProjectStatus::Running {
                        let result = self.open_when_running(&project, &runtime);
                        if result.is_err() {
                            if let Ok(mut intents) = self.inner.browser_intents.lock() {
                                intents.remove(id);
                            }
                        }
                        result?;
                    }
                }
            }
            return Ok(());
        }
        if self
            .runtime(id)
            .is_some_and(|runtime| runtime.status == ProjectStatus::Starting)
        {
            if project.launch_mode == LaunchMode::Browser && wants_browser {
                if let Ok(mut intents) = self.inner.browser_intents.lock() {
                    intents.insert(id.to_owned());
                }
            }
            return Ok(());
        }

        let (port, resolved_command) = match &project.launch_mode {
            LaunchMode::Browser => {
                let port = self.inner.ports.reserve(project.preferred_port)?;
                let command = project.command.replace("{port}", &port.to_string());
                (Some(port), command)
            }
            LaunchMode::Executable => (None, executable_command(&project)),
        };
        let generation = self.next_generation(id);
        let cancel_token = Arc::new(AtomicBool::new(false));
        if self.is_cancel_requested(id) {
            cancel_token.store(true, Ordering::Release);
        }
        if let Ok(mut tokens) = self.inner.cancel_tokens.lock() {
            tokens.insert(id.to_owned(), Arc::clone(&cancel_token));
        }
        if let Some(port) = port {
            if let Ok(mut held) = self.inner.held_ports.lock() {
                held.insert(id.to_owned(), port);
            }
        }
        if let Ok(mut intents) = self.inner.browser_intents.lock() {
            if wants_browser {
                intents.insert(id.to_owned());
            } else {
                intents.remove(id);
            }
        }
        self.set_runtime(
            id,
            RuntimeSnapshot {
                project_id: id.to_owned(),
                status: ProjectStatus::Starting,
                pid: None,
                actual_port: port,
                resolved_command: Some(resolved_command.clone()),
                started_at: None,
                error: None,
            },
        );

        let worker_manager = self.clone();
        let failure_manager = self.clone();
        let worker_operation = Arc::clone(&operation);
        thread::Builder::new()
            .name(format!("launcher-start-{id}"))
            .spawn(move || {
                worker_manager.start_worker(
                    project,
                    port,
                    resolved_command,
                    generation,
                    cancel_token,
                    worker_operation,
                )
            })
            .map_err(|error| {
                failure_manager.fail_start(id, generation, port, error.to_string(), true);
                error.to_string()
            })?;
        Ok(())
    }

    pub fn stop_project(&self, id: &str) -> Result<(), String> {
        self.project(id)?;
        self.request_cancel(id);
        let operation = self.operation_for(id)?;
        let _operation = operation
            .lock()
            .map_err(|_| "project operation lock poisoned".to_owned())?;
        self.invalidate_generation(id);
        if let Ok(mut intents) = self.inner.browser_intents.lock() {
            intents.remove(id);
        }
        self.stop_process_and_wait(id)?;
        self.mark_stopped(id);
        self.clear_cancel_state(id);
        Ok(())
    }

    pub fn restart_project(&self, id: &str) -> Result<(), String> {
        self.stop_project(id)?;
        self.start_project(id, false)
    }

    pub fn activate_project(&self, id: &str) -> Result<(), String> {
        let project = self.project(id)?;
        if self.inner.process_manager.is_running(id) {
            let operation = self.operation_for(id)?;
            let _operation = operation
                .lock()
                .map_err(|_| "project operation lock poisoned".to_owned())?;
            if self.is_cancel_requested(id) {
                return Ok(());
            }
            if project.launch_mode == LaunchMode::Browser {
                if let Some(runtime) = self.runtime(id) {
                    if runtime.status == ProjectStatus::Running {
                        return self.open_when_running(&project, &runtime);
                    }
                }
                if let Ok(mut intents) = self.inner.browser_intents.lock() {
                    intents.insert(id.to_owned());
                }
            }
            return Ok(());
        }
        self.start_project(id, true)
    }

    pub fn get_logs(&self, id: &str) -> Result<Vec<LogEntry>, String> {
        self.project(id)?;
        Ok(self
            .inner
            .logs
            .lock()
            .map_err(|_| "log state lock poisoned".to_owned())?
            .get(id)
            .map(|logs| logs.iter().cloned().collect())
            .unwrap_or_default())
    }

    pub fn clear_logs(&self, id: &str) -> Result<(), String> {
        self.project(id)?;
        let operation = self.operation_for(id)?;
        let _operation = operation
            .lock()
            .map_err(|_| "project operation lock poisoned".to_owned())?;
        if let Ok(mut logs) = self.inner.logs.lock() {
            logs.entry(id.to_owned()).or_default().clear();
        }
        Ok(())
    }

    pub fn shutdown(&self) {
        if self.inner.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut generations) = self.inner.generations.lock() {
            let ids = self
                .inner
                .projects
                .read()
                .map(|projects| {
                    projects
                        .iter()
                        .map(|project| project.id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for id in ids {
                let next = generations.get(&id).copied().unwrap_or(0).saturating_add(1);
                generations.insert(id, next);
            }
        }
        if let Ok(tokens) = self.inner.cancel_tokens.lock() {
            for token in tokens.values() {
                token.store(true, Ordering::Release);
            }
        }
        if let Ok(mut intents) = self.inner.browser_intents.lock() {
            intents.clear();
        }
        self.inner.process_manager.stop_all();
    }

    fn start_worker(
        &self,
        project: Project,
        port: Option<u16>,
        command: String,
        generation: u64,
        cancel_token: Arc<AtomicBool>,
        operation: Arc<Mutex<()>>,
    ) {
        let operation_guard = match operation.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        if self.is_cancelled(&project.id, generation, &cancel_token) {
            return;
        }
        let spec = ProcessSpec {
            project_id: project.id.clone(),
            command: command.clone(),
            working_directory: Path::new(&project.working_directory).to_path_buf(),
            executable_path: match &project.launch_mode {
                LaunchMode::Browser => None,
                LaunchMode::Executable => Some(Path::new(&project.executable_path).to_path_buf()),
            },
            arguments: project.arguments.clone(),
        };
        let info = match self.inner.process_manager.start(spec) {
            Ok(info) => info,
            Err(error) => {
                self.fail_start(&project.id, generation, port, error, true);
                return;
            }
        };
        if self.is_cancelled(&project.id, generation, &cancel_token) {
            let _ = self.inner.process_manager.stop(&project.id);
            return;
        }
        if let Ok(mut runtimes) = self.inner.runtimes.lock() {
            if let Some(runtime) = runtimes.get_mut(&project.id) {
                runtime.pid = Some(info.pid);
                runtime.started_at = Some(info.started_at);
            }
        }
        if let Ok(mut pids) = self.inner.active_pids.lock() {
            pids.insert(project.id.clone(), info.pid);
        }
        self.emit_runtime(&project.id);

        if project.launch_mode == LaunchMode::Executable {
            if self.is_cancelled_or_exited(&project.id, generation, &cancel_token) {
                let _ = self.inner.process_manager.stop(&project.id);
                return;
            }
            if let Ok(mut runtimes) = self.inner.runtimes.lock() {
                if let Some(runtime) = runtimes.get_mut(&project.id) {
                    runtime.status = ProjectStatus::Running;
                    runtime.error = None;
                }
            }
            self.emit_runtime(&project.id);
            return;
        }

        let Some(port) = port else {
            self.fail_start(
                &project.id,
                generation,
                None,
                "browser project has no reserved port".to_owned(),
                false,
            );
            let _ = self.inner.process_manager.stop(&project.id);
            return;
        };

        let url = match render_url(&project.url_template, port) {
            Ok(url) => url,
            Err(error) => {
                self.fail_start(&project.id, generation, Some(port), error, false);
                let _ = self.inner.process_manager.stop(&project.id);
                return;
            }
        };
        // Keep the per-project lock around spawn, PID publication, and URL
        // validation, then release it while readiness is polled. Output/exit
        // events and a concurrent stop must make progress during a slow start.
        drop(operation_guard);
        let manager = self.clone();
        let project_id = project.id.clone();
        let ready = wait_for_http_ready(
            &url,
            STARTUP_TIMEOUT,
            || {
                manager
                    .inner
                    .process_manager
                    .owns_listening_port(&project_id, port)
            },
            || manager.is_cancelled_or_exited(&project_id, generation, &cancel_token),
        );
        let Ok(_operation) = operation.lock() else {
            return;
        };
        // A stop/restart may have completed while readiness was polling.  An
        // obsolete worker must not touch the replacement generation or its
        // reused port/process.
        if !manager.is_generation_current(&project_id, generation) {
            return;
        }
        if let Err(error) = ready {
            if manager.is_cancelled_or_exited(&project_id, generation, &cancel_token) {
                let _ = manager.inner.process_manager.stop(&project_id);
                return;
            }
            manager.fail_start(&project_id, generation, Some(port), error, false);
            let _ = manager.inner.process_manager.stop(&project_id);
            return;
        }
        if manager.is_cancelled_or_exited(&project_id, generation, &cancel_token) {
            let _ = manager.inner.process_manager.stop(&project_id);
            return;
        }
        let mut should_open = false;
        if let Ok(mut runtimes) = manager.inner.runtimes.lock() {
            if let Some(runtime) = runtimes.get_mut(&project_id) {
                runtime.status = ProjectStatus::Running;
                runtime.error = None;
                if !manager.is_cancelled(&project_id, generation, &cancel_token) {
                    should_open = manager
                        .inner
                        .browser_intents
                        .lock()
                        .ok()
                        .is_some_and(|mut intents| intents.remove(&project_id));
                }
            }
        }
        manager.emit_runtime(&project_id);
        if should_open {
            let _ = (manager.inner.browser_opener)(&url);
        }
    }

    fn fail_start(
        &self,
        id: &str,
        generation: u64,
        port: Option<u16>,
        error: String,
        release_port: bool,
    ) {
        if !self.is_generation_current(id, generation) {
            return;
        }
        if let Ok(mut runtimes) = self.inner.runtimes.lock() {
            if let Some(runtime) = runtimes.get_mut(id) {
                runtime.status = ProjectStatus::Error;
                runtime.pid = None;
                runtime.started_at = None;
                if release_port {
                    runtime.actual_port = None;
                    runtime.resolved_command = None;
                }
                runtime.error = Some(error);
            }
        }
        if release_port {
            if let Some(port) = port {
                self.release_held_port_if(id, port);
            } else {
                self.release_held_port(id);
            }
        }
        if let Ok(mut intents) = self.inner.browser_intents.lock() {
            intents.remove(id);
        }
        self.emit_runtime(id);
    }

    fn open_when_running(
        &self,
        project: &Project,
        runtime: &RuntimeSnapshot,
    ) -> Result<(), String> {
        if project.launch_mode == LaunchMode::Executable {
            return Ok(());
        }
        let port = runtime
            .actual_port
            .ok_or_else(|| "running project has no port".to_owned())?;
        let url = render_url(&project.url_template, port)?;
        (self.inner.browser_opener)(&url)
    }

    fn project(&self, id: &str) -> Result<Project, String> {
        self.inner
            .projects
            .read()
            .map_err(|_| "project state lock poisoned".to_owned())?
            .iter()
            .find(|project| project.id == id)
            .cloned()
            .ok_or_else(|| "project not found".to_owned())
    }

    fn view_for(&self, id: &str) -> ProjectView {
        let project = self.project(id).expect("view project should exist");
        ProjectView {
            runtime: self.runtime(id).unwrap_or_else(|| stopped_snapshot(id)),
            project,
        }
    }

    fn runtime(&self, id: &str) -> Option<RuntimeSnapshot> {
        self.inner.runtimes.lock().ok()?.get(id).cloned()
    }

    fn set_runtime(&self, id: &str, snapshot: RuntimeSnapshot) {
        if let Ok(mut runtimes) = self.inner.runtimes.lock() {
            runtimes.insert(id.to_owned(), snapshot);
        }
        self.emit_runtime(id);
    }

    fn emit_runtime(&self, id: &str) {
        let Some(snapshot) = self.runtime(id) else {
            return;
        };
        emit(
            &self.inner,
            "project-state",
            serde_json::to_value(snapshot).unwrap_or(Value::Null),
        );
    }

    fn next_generation(&self, id: &str) -> u64 {
        let mut generations = self.inner.generations.lock().expect("generation lock");
        let next = generations.get(id).copied().unwrap_or(0).saturating_add(1);
        generations.insert(id.to_owned(), next);
        next
    }

    fn invalidate_generation(&self, id: &str) {
        let _ = self.next_generation(id);
    }

    fn is_generation_current(&self, id: &str, generation: u64) -> bool {
        self.inner
            .generations
            .lock()
            .ok()
            .and_then(|map| map.get(id).copied())
            .is_some_and(|current| current == generation)
    }

    fn operation_for(&self, id: &str) -> Result<Arc<Mutex<()>>, String> {
        operation_for_inner(&self.inner, id)
    }

    fn request_cancel(&self, id: &str) {
        if let Ok(tokens) = self.inner.cancel_tokens.lock() {
            if let Some(token) = tokens.get(id) {
                token.store(true, Ordering::Release);
            }
        }
        if let Ok(mut requested) = self.inner.cancel_requested.lock() {
            requested.insert(id.to_owned());
        }
    }

    fn clear_cancel_state(&self, id: &str) {
        if let Ok(mut tokens) = self.inner.cancel_tokens.lock() {
            tokens.remove(id);
        }
        if let Ok(mut requested) = self.inner.cancel_requested.lock() {
            requested.remove(id);
        }
    }

    fn is_cancel_requested(&self, id: &str) -> bool {
        self.inner
            .cancel_requested
            .lock()
            .ok()
            .is_some_and(|requested| requested.contains(id))
    }

    fn is_cancelled(&self, id: &str, generation: u64, token: &AtomicBool) -> bool {
        token.load(Ordering::Acquire)
            || !self.is_generation_current(id, generation)
            || self.inner.shutting_down.load(Ordering::Acquire)
    }

    fn is_cancelled_or_exited(&self, id: &str, generation: u64, token: &AtomicBool) -> bool {
        self.is_cancelled(id, generation, token) || !self.inner.process_manager.is_running(id)
    }

    fn stop_process_and_wait(&self, id: &str) -> Result<(), String> {
        let _ = self.inner.process_manager.stop(id)?;
        let deadline = Instant::now() + STOP_TIMEOUT;
        while self.inner.process_manager.is_running(id) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if self.inner.process_manager.is_running(id) {
            return Err("timed out waiting for project process to stop".to_owned());
        }

        if let Ok(mut pids) = self.inner.active_pids.lock() {
            pids.remove(id);
        }
        self.release_held_port(id);
        Ok(())
    }

    fn mark_stopped(&self, id: &str) {
        if let Ok(mut runtime) = self.inner.runtimes.lock() {
            if let Some(snapshot) = runtime.get_mut(id) {
                snapshot.status = ProjectStatus::Stopped;
                snapshot.pid = None;
                snapshot.started_at = None;
                snapshot.resolved_command = None;
                snapshot.actual_port = None;
                snapshot.error = None;
            }
        }
        self.emit_runtime(id);
    }

    fn release_held_port_if(&self, id: &str, port: u16) {
        if let Ok(mut held) = self.inner.held_ports.lock() {
            if held.get(id).copied() == Some(port) {
                held.remove(id);
                self.inner.ports.release(port);
            }
        }
    }

    fn release_held_port(&self, id: &str) {
        if let Ok(mut held) = self.inner.held_ports.lock() {
            if let Some(port) = held.remove(id) {
                self.inner.ports.release(port);
            }
        }
    }
}

impl Drop for ProjectManager {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.shutdown();
        }
    }
}

fn operation_for_inner(
    inner: &Arc<ProjectManagerInner>,
    id: &str,
) -> Result<Arc<Mutex<()>>, String> {
    let mut operations = inner
        .operations
        .lock()
        .map_err(|_| "project operations lock poisoned".to_owned())?;
    Ok(operations
        .entry(id.to_owned())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone())
}

fn process_event_loop(
    weak: std::sync::Weak<ProjectManagerInner>,
    receiver: std::sync::mpsc::Receiver<ProcessEvent>,
) {
    while let Ok(event) = receiver.recv() {
        let Some(inner) = weak.upgrade() else { break };
        match event {
            ProcessEvent::Output {
                project_id,
                pid,
                stream,
                line,
            } => {
                if let Some(entry) = append_log(&inner, &project_id, pid, stream, line) {
                    emit(
                        &inner,
                        "project-log",
                        json!({ "projectId": project_id, "entry": entry }),
                    );
                }
            }
            ProcessEvent::Exited {
                project_id,
                pid,
                code,
                requested,
            } => handle_exit(&inner, &project_id, pid, code, requested),
        }
    }
}

fn handle_exit(
    inner: &Arc<ProjectManagerInner>,
    id: &str,
    pid: u32,
    code: Option<i32>,
    requested: bool,
) {
    let operation = match operation_for_inner(inner, id) {
        Ok(operation) => operation,
        Err(_) => return,
    };
    let Ok(_operation) = operation.lock() else {
        return;
    };

    let matches_pid = inner
        .active_pids
        .lock()
        .ok()
        .and_then(|pids| pids.get(id).copied())
        .is_some_and(|active_pid| active_pid == pid);
    if !matches_pid {
        return;
    }
    if let Ok(mut pids) = inner.active_pids.lock() {
        pids.remove(id);
    }
    if let Ok(mut tokens) = inner.cancel_tokens.lock() {
        if let Some(token) = tokens.remove(id) {
            token.store(true, Ordering::Release);
        }
    }

    let launch_mode = inner.projects.read().ok().and_then(|projects| {
        projects
            .iter()
            .find(|project| project.id == id)
            .map(|project| project.launch_mode)
    });
    let mut should_emit = false;
    if let Ok(mut runtimes) = inner.runtimes.lock() {
        if let Some(runtime) = runtimes.get_mut(id) {
            if !requested && runtime.status != ProjectStatus::Stopped {
                if launch_mode == Some(LaunchMode::Executable) && code == Some(0) {
                    runtime.status = ProjectStatus::Stopped;
                    runtime.error = None;
                } else if runtime.status != ProjectStatus::Error || runtime.error.is_none() {
                    runtime.status = ProjectStatus::Error;
                    runtime.error = Some(match code {
                        Some(code) => format!("process exited with code {code}"),
                        None => "process exited before becoming ready".to_owned(),
                    });
                }
            } else if requested && runtime.status != ProjectStatus::Error {
                runtime.status = ProjectStatus::Stopped;
                runtime.error = None;
            }
            runtime.pid = None;
            runtime.actual_port = None;
            runtime.resolved_command = None;
            runtime.started_at = None;
            should_emit = true;
        }
    }
    if let Ok(mut intents) = inner.browser_intents.lock() {
        intents.remove(id);
    }
    if let Ok(mut held) = inner.held_ports.lock() {
        if let Some(port) = held.remove(id) {
            inner.ports.release(port);
        }
    }
    if should_emit {
        if let Some(snapshot) = inner
            .runtimes
            .lock()
            .ok()
            .and_then(|map| map.get(id).cloned())
        {
            emit(
                inner,
                "project-state",
                serde_json::to_value(snapshot).unwrap_or(Value::Null),
            );
        }
    }
}

fn append_log(
    inner: &Arc<ProjectManagerInner>,
    id: &str,
    pid: u32,
    stream: OutputStream,
    line: String,
) -> Option<LogEntry> {
    let active_pid = inner.active_pids.lock().ok()?.get(id).copied();
    let runtime_pid = inner
        .runtimes
        .lock()
        .ok()?
        .get(id)
        .and_then(|runtime| runtime.pid);
    if active_pid != Some(pid) && runtime_pid != Some(pid) {
        return None;
    }

    let mut sequences = inner.log_sequences.lock().ok()?;
    let sequence = sequences.entry(id.to_owned()).or_insert(0);
    *sequence = sequence.saturating_add(1);
    let sequence_value = *sequence;
    drop(sequences);

    let mut logs = inner.logs.lock().ok()?;
    let queue = logs.get_mut(id)?;
    let entry = LogEntry {
        sequence: sequence_value,
        timestamp: unix_millis(),
        stream: match stream {
            OutputStream::Stdout => "stdout".to_owned(),
            OutputStream::Stderr => "stderr".to_owned(),
        },
        line,
    };
    queue.push_back(entry.clone());
    while queue.len() > MAX_LOG_ENTRIES {
        queue.pop_front();
    }
    Some(entry)
}

fn emit(inner: &Arc<ProjectManagerInner>, event: &str, payload: Value) {
    let sink = inner
        .event_sink
        .lock()
        .ok()
        .and_then(|target| target.clone());
    if let Some(sink) = sink {
        sink(event, payload);
    }
}

fn stopped_snapshot(id: &str) -> RuntimeSnapshot {
    RuntimeSnapshot {
        project_id: id.to_owned(),
        status: ProjectStatus::Stopped,
        pid: None,
        actual_port: None,
        resolved_command: None,
        started_at: None,
        error: None,
    }
}

fn executable_command(project: &Project) -> String {
    let path = project.executable_path.trim();
    let arguments = project.arguments.trim();
    if arguments.is_empty() {
        path.to_owned()
    } else {
        format!("{path} {arguments}")
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn default_browser_opener() -> BrowserOpener {
    Arc::new(open_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::AtomicUsize;
    use tempfile::tempdir;

    fn project(directory: &Path, id: &str, command: &str, auto_open_browser: bool) -> Project {
        Project {
            id: id.to_owned(),
            name: id.to_owned(),
            path: directory.display().to_string(),
            working_directory: directory.display().to_string(),
            launch_mode: LaunchMode::Browser,
            executable_path: String::new(),
            arguments: String::new(),
            command: command.to_owned(),
            preferred_port: 45_000,
            url_template: "http://localhost:{port}".to_owned(),
            icon: None,
            auto_open_browser,
        }
    }

    fn manager_for(
        directory: &Path,
        projects: &[Project],
        browser_opener: BrowserOpener,
    ) -> ProjectManager {
        let config_path = directory.join("projects.json");
        ConfigManager::new(config_path.clone())
            .save(projects)
            .expect("seed project config");
        ProjectManager::with_browser_opener(config_path, browser_opener)
            .expect("create project manager")
    }

    fn browser_counter() -> (Arc<AtomicUsize>, BrowserOpener) {
        let counter = Arc::new(AtomicUsize::new(0));
        let shared = Arc::clone(&counter);
        let opener: BrowserOpener = Arc::new(move |_url| {
            shared.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        (counter, opener)
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
    }

    fn failing_command() -> &'static str {
        if cfg!(windows) {
            "echo fixture startup failure 1>&2 & exit /b 7"
        } else {
            "printf 'fixture startup failure\\n' >&2; exit 7"
        }
    }

    fn sleeping_command() -> &'static str {
        if cfg!(windows) {
            "echo slow-start & ping 127.0.0.1 -n 30 > nul"
        } else {
            "printf 'slow-start\\n'; sleep 30"
        }
    }

    #[test]
    fn log_sequence_survives_clear_and_queue_is_bounded() {
        let directory = tempdir().expect("tempdir");
        let project = project(directory.path(), "logs", "echo logs", false);
        let (_, opener) = browser_counter();
        let manager = manager_for(directory.path(), &[project], opener);
        manager
            .inner
            .active_pids
            .lock()
            .expect("active pid lock")
            .insert("logs".to_owned(), 42);

        let first = append_log(
            &manager.inner,
            "logs",
            42,
            OutputStream::Stdout,
            "first".to_owned(),
        )
        .expect("first log");
        manager.clear_logs("logs").expect("clear logs");
        let second = append_log(
            &manager.inner,
            "logs",
            42,
            OutputStream::Stderr,
            "second".to_owned(),
        )
        .expect("second log");
        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 2);

        for index in 0..(MAX_LOG_ENTRIES + 5) {
            let _ = append_log(
                &manager.inner,
                "logs",
                42,
                OutputStream::Stdout,
                format!("line-{index}"),
            );
        }
        let logs = manager.get_logs("logs").expect("logs");
        assert_eq!(logs.len(), MAX_LOG_ENTRIES);
        assert_eq!(logs.first().expect("oldest log").sequence, 8);
        assert_eq!(logs.last().expect("newest log").sequence, 507);
    }

    #[test]
    fn remove_rolls_back_in_memory_projects_when_save_fails() {
        let directory = tempdir().expect("tempdir");
        let project = project(directory.path(), "rollback", "echo rollback", false);
        let (_, opener) = browser_counter();
        let config_path = directory.path().join("projects.json");
        ConfigManager::new(config_path.clone())
            .save(std::slice::from_ref(&project))
            .expect("seed config");
        let manager = ProjectManager::with_browser_opener(config_path.clone(), opener)
            .expect("create manager");

        fs::remove_file(&config_path).expect("remove config file");
        fs::create_dir(&config_path).expect("replace config with directory");
        assert!(manager.remove_project("rollback").is_err());
        assert_eq!(manager.list_projects().len(), 1);
        assert_eq!(manager.list_projects()[0].project.id, "rollback");
    }

    #[test]
    fn concurrent_starts_deduplicate_and_release_failed_start_port() {
        let directory = tempdir().expect("tempdir");
        let first = project(directory.path(), "first", failing_command(), false);
        let second = project(directory.path(), "second", failing_command(), false);
        let (_, opener) = browser_counter();
        let manager = Arc::new(manager_for(directory.path(), &[first, second], opener));

        let left = {
            let manager = Arc::clone(&manager);
            thread::spawn(move || manager.start_project("first", false))
        };
        let right = {
            let manager = Arc::clone(&manager);
            thread::spawn(move || manager.start_project("first", false))
        };
        assert!(left.join().expect("left start").is_ok());
        assert!(right.join().expect("right start").is_ok());
        assert!(wait_until(|| {
            manager
                .runtime("first")
                .is_some_and(|runtime| runtime.status == ProjectStatus::Error)
        }));
        assert!(wait_until(|| {
            manager.get_logs("first").unwrap().iter().any(|entry| {
                entry.stream == "stderr" && entry.line.contains("fixture startup failure")
            })
        }));
        assert!(manager
            .inner
            .held_ports
            .lock()
            .expect("held port lock")
            .get("first")
            .is_none());

        manager
            .start_project("second", false)
            .expect("second failed start should be accepted");
        assert!(wait_until(|| {
            manager
                .runtime("second")
                .is_some_and(|runtime| runtime.status == ProjectStatus::Error)
        }));
        assert!(manager
            .inner
            .held_ports
            .lock()
            .expect("held port lock")
            .is_empty());
    }

    #[test]
    fn stop_during_start_cancels_without_opening_browser() {
        let directory = tempdir().expect("tempdir");
        let project = project(directory.path(), "cancel", sleeping_command(), true);
        let (counter, opener) = browser_counter();
        let manager = manager_for(directory.path(), &[project], opener);

        manager
            .start_project("cancel", false)
            .expect("start should be accepted");
        assert!(wait_until(|| !manager
            .get_logs("cancel")
            .unwrap()
            .is_empty()));
        manager
            .stop_project("cancel")
            .expect("stop should cancel start");
        assert!(wait_until(|| {
            manager
                .runtime("cancel")
                .is_some_and(|runtime| runtime.status == ProjectStatus::Stopped)
        }));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert!(manager
            .inner
            .held_ports
            .lock()
            .expect("held port lock")
            .is_empty());
    }

    #[test]
    fn stop_then_immediate_start_ignores_obsolete_readiness_worker() {
        let node = std::process::Command::new("node")
            .arg("--version")
            .status()
            .expect("Node.js is required for project-manager integration tests");
        assert!(node.success(), "Node.js --version failed");
        let fixture_directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("tests")
            .join("fixtures");
        assert!(
            fixture_directory.is_dir(),
            "fixture directory is missing: {}",
            fixture_directory.display()
        );

        let directory = tempdir().expect("tempdir");
        let command = "node fixture-server.cjs --port {port} --ready-delay 500";
        let mut project = project(&fixture_directory, "slow-restart", command, false);
        project.preferred_port = 45_110;
        let (_, opener) = browser_counter();
        let manager = manager_for(directory.path(), &[project], opener);

        manager
            .start_project("slow-restart", false)
            .expect("slow start should be accepted");
        assert!(wait_until(|| {
            manager
                .runtime("slow-restart")
                .and_then(|runtime| runtime.pid)
                .is_some()
        }));
        manager
            .stop_project("slow-restart")
            .expect("stop should finish before restart");
        manager
            .start_project("slow-restart", false)
            .expect("immediate restart should be accepted");
        assert!(
            wait_until(|| {
                manager
                    .runtime("slow-restart")
                    .is_some_and(|runtime| runtime.status == ProjectStatus::Running)
            }),
            "runtime: {:?}, logs: {:?}",
            manager.runtime("slow-restart"),
            manager.get_logs("slow-restart")
        );
        manager
            .stop_project("slow-restart")
            .expect("stop restarted fixture");
    }

    #[cfg(windows)]
    #[test]
    fn executable_batch_is_deduplicated_and_job_stoppable() {
        let directory = tempdir().expect("tempdir");
        let script = directory.path().join("controlled-launcher.bat");
        fs::write(
            &script,
            "@echo off\r\necho controlled-batch-start\r\nping 127.0.0.1 -n 30 > nul\r\n",
        )
        .expect("write controlled batch fixture");
        let mut project = project(directory.path(), "batch", "", false);
        project.launch_mode = LaunchMode::Executable;
        project.executable_path = script.display().to_string();
        project.arguments.clear();
        project.preferred_port = 0;
        let (counter, opener) = browser_counter();
        let manager = manager_for(directory.path(), &[project], opener);

        manager
            .start_project("batch", true)
            .expect("batch start should be accepted");
        assert!(wait_until(|| {
            manager
                .runtime("batch")
                .is_some_and(|runtime| runtime.status == ProjectStatus::Running)
        }));
        let first_pid = manager.runtime("batch").and_then(|runtime| runtime.pid);
        assert!(first_pid.is_some());
        assert_eq!(
            manager
                .runtime("batch")
                .and_then(|runtime| runtime.actual_port),
            None
        );
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert!(wait_until(|| {
            manager.get_logs("batch").unwrap().iter().any(|entry| {
                entry.stream == "stdout" && entry.line.contains("controlled-batch-start")
            })
        }));

        manager
            .activate_project("batch")
            .expect("activating a running executable should deduplicate");
        assert_eq!(
            manager.runtime("batch").and_then(|runtime| runtime.pid),
            first_pid
        );
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        manager.stop_project("batch").expect("stop batch fixture");
        assert!(wait_until(|| {
            manager
                .runtime("batch")
                .is_some_and(|runtime| runtime.status == ProjectStatus::Stopped)
        }));
        manager
            .restart_project("batch")
            .expect("restart batch fixture");
        assert!(wait_until(|| {
            manager
                .runtime("batch")
                .is_some_and(|runtime| runtime.status == ProjectStatus::Running)
        }));
        manager.stop_project("batch").expect("stop restarted batch");
    }

    #[cfg(windows)]
    #[test]
    fn executable_node_has_no_port_or_browser_and_clean_exit_is_stopped() {
        let node = std::process::Command::new("node")
            .arg("--version")
            .status()
            .expect("Node.js is required for executable-mode integration tests");
        assert!(node.success(), "Node.js --version failed");
        let node_path = std::process::Command::new("where.exe")
            .arg("node.exe")
            .output()
            .expect("where.exe should resolve node.exe");
        assert!(node_path.status.success(), "where.exe node.exe failed");
        let node_path = String::from_utf8_lossy(&node_path.stdout)
            .lines()
            .next()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .expect("where.exe should return a node.exe path");

        let directory = tempdir().expect("tempdir");
        let script = directory.path().join("controlled-node.js");
        fs::write(
            &script,
            "console.log('controlled-node-start'); setTimeout(() => process.exit(0), 3000);\n",
        )
        .expect("write controlled node fixture");
        let mut project = project(directory.path(), "node-executable", "", false);
        project.launch_mode = LaunchMode::Executable;
        project.executable_path = node_path.display().to_string();
        project.arguments = format!("\"{}\"", script.display());
        project.preferred_port = 0;
        let (counter, opener) = browser_counter();
        let manager = manager_for(directory.path(), &[project], opener);

        manager
            .activate_project("node-executable")
            .expect("node executable start should be accepted");
        assert!(wait_until(|| {
            manager
                .runtime("node-executable")
                .is_some_and(|runtime| runtime.status == ProjectStatus::Running)
        }));
        let first_pid = manager
            .runtime("node-executable")
            .and_then(|runtime| runtime.pid);
        assert!(first_pid.is_some());
        assert_eq!(
            manager
                .runtime("node-executable")
                .and_then(|runtime| runtime.actual_port),
            None
        );
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert!(wait_until(|| {
            manager
                .get_logs("node-executable")
                .unwrap()
                .iter()
                .any(|entry| entry.line.contains("controlled-node-start"))
        }));

        manager
            .activate_project("node-executable")
            .expect("running executable activation should deduplicate");
        assert_eq!(
            manager
                .runtime("node-executable")
                .and_then(|runtime| runtime.pid),
            first_pid
        );
        assert!(wait_until(|| {
            manager.runtime("node-executable").is_some_and(|runtime| {
                runtime.status == ProjectStatus::Stopped && runtime.error.is_none()
            })
        }));

        let failure_script = directory.path().join("controlled-node-failure.js");
        fs::write(
            &failure_script,
            "console.error('controlled-node-failure'); process.exit(7);\n",
        )
        .expect("write controlled node failure fixture");
        let failure_id = manager
            .add_project(ProjectInput {
                name: "node-executable-failure".to_owned(),
                path: directory.path().display().to_string(),
                working_directory: directory.path().display().to_string(),
                launch_mode: LaunchMode::Executable,
                executable_path: node_path.display().to_string(),
                arguments: format!("\"{}\"", failure_script.display()),
                command: String::new(),
                preferred_port: 0,
                url_template: "http://localhost:{port}".to_owned(),
                icon: None,
                auto_open_browser: true,
            })
            .expect("add controlled node failure fixture")
            .project
            .id;
        manager
            .start_project(&failure_id, false)
            .expect("node failure start should be accepted");
        assert!(wait_until(|| {
            manager
                .runtime(&failure_id)
                .is_some_and(|runtime| runtime.status == ProjectStatus::Error)
        }));
        let failure = manager
            .runtime(&failure_id)
            .and_then(|runtime| runtime.error)
            .expect("non-zero executable exit should retain an error");
        assert!(failure.contains("process exited with code 7"));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        manager
            .stop_project(&failure_id)
            .expect("stop failed node fixture");

        manager
            .restart_project("node-executable")
            .expect("node executable restart should be accepted");
        assert!(wait_until(|| {
            manager
                .runtime("node-executable")
                .is_some_and(|runtime| runtime.status == ProjectStatus::Running)
        }));
        manager
            .stop_project("node-executable")
            .expect("stop restarted node executable");
    }

    #[test]
    fn node_fixture_restart_and_browser_intent() {
        let node = std::process::Command::new("node")
            .arg("--version")
            .status()
            .expect("Node.js is required for project-manager integration tests");
        assert!(node.success(), "Node.js --version failed");
        let fixture_directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("tests")
            .join("fixtures");
        assert!(
            fixture_directory.is_dir(),
            "fixture directory is missing: {}",
            fixture_directory.display()
        );

        let directory = tempdir().expect("tempdir");
        let command = "node fixture-server.cjs --port {port} --child";
        let mut project = project(&fixture_directory, "node", command, false);
        project.preferred_port = 45_111;
        let (counter, opener) = browser_counter();
        let manager = manager_for(directory.path(), &[project], opener);

        manager.activate_project("node").expect("start and open");
        assert!(wait_until(|| {
            manager
                .runtime("node")
                .is_some_and(|runtime| runtime.status == ProjectStatus::Running)
        }));
        let first_pid = manager.runtime("node").and_then(|runtime| runtime.pid);
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        manager.restart_project("node").expect("restart");
        assert!(wait_until(|| {
            manager
                .runtime("node")
                .is_some_and(|runtime| runtime.status == ProjectStatus::Running)
        }));
        let second_pid = manager.runtime("node").and_then(|runtime| runtime.pid);
        assert_ne!(first_pid, second_pid);
        manager.stop_project("node").expect("stop fixture");
    }

    #[test]
    fn node_fixture_failure_reports_stderr_without_opening_browser() {
        let node = std::process::Command::new("node")
            .arg("--version")
            .status()
            .expect("Node.js is required for project-manager integration tests");
        assert!(node.success(), "Node.js --version failed");
        let fixture_directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("tests")
            .join("fixtures");
        assert!(
            fixture_directory.is_dir(),
            "fixture directory is missing: {}",
            fixture_directory.display()
        );

        let directory = tempdir().expect("tempdir");
        let command = "node fixture-server.cjs --port {port} --fail";
        let mut project = project(&fixture_directory, "node-fail", command, true);
        project.preferred_port = 45_112;
        let (counter, opener) = browser_counter();
        let manager = manager_for(directory.path(), &[project], opener);

        manager
            .start_project("node-fail", false)
            .expect("failure start should be accepted");
        assert!(wait_until(|| {
            manager
                .runtime("node-fail")
                .is_some_and(|runtime| runtime.status == ProjectStatus::Error)
        }));
        assert!(wait_until(|| {
            manager.get_logs("node-fail").unwrap().iter().any(|entry| {
                entry.stream == "stderr" && entry.line.contains("fixture startup failure")
            })
        }));
        let error = manager
            .runtime("node-fail")
            .and_then(|runtime| runtime.error)
            .expect("failed fixture should retain its process exit reason");
        assert!(
            error.contains("process exited"),
            "unexpected error: {error}"
        );
        assert!(!error.to_ascii_lowercase().contains("cancel"));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        assert!(manager
            .inner
            .held_ports
            .lock()
            .expect("held port lock")
            .is_empty());
    }
}

mod core;
mod services;

use std::path::PathBuf;
use std::sync::Arc;

use core::launch_detector::{detect_launch, LaunchDetection};
use core::{LogEntry, ProjectInput, ProjectManager, ProjectView};
use tauri::{Emitter, Manager, RunEvent, State, WindowEvent};

pub struct AppState {
    pub manager: ProjectManager,
}

impl AppState {
    fn shutdown(&self) {
        self.manager.shutdown();
    }
}

#[tauri::command]
fn list_projects(state: State<'_, AppState>) -> Vec<ProjectView> {
    state.manager.list_projects()
}

#[tauri::command]
async fn add_project(
    state: State<'_, AppState>,
    input: ProjectInput,
) -> Result<ProjectView, String> {
    let manager = state.manager.clone();
    tauri::async_runtime::spawn_blocking(move || manager.add_project(input))
        .await
        .map_err(|error| format!("add project worker failed: {error}"))?
}

#[tauri::command]
async fn update_project(
    state: State<'_, AppState>,
    id: String,
    input: ProjectInput,
) -> Result<ProjectView, String> {
    let manager = state.manager.clone();
    tauri::async_runtime::spawn_blocking(move || manager.update_project(&id, input))
        .await
        .map_err(|error| format!("update project worker failed: {error}"))?
}

#[tauri::command]
async fn remove_project(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let manager = state.manager.clone();
    tauri::async_runtime::spawn_blocking(move || manager.remove_project(&id))
        .await
        .map_err(|error| format!("remove project worker failed: {error}"))?
}

#[tauri::command(rename_all = "camelCase")]
async fn start_project(
    state: State<'_, AppState>,
    id: String,
    open_browser: bool,
) -> Result<(), String> {
    let manager = state.manager.clone();
    tauri::async_runtime::spawn_blocking(move || manager.start_project(&id, open_browser))
        .await
        .map_err(|error| format!("start project worker failed: {error}"))?
}

#[tauri::command]
async fn stop_project(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let manager = state.manager.clone();
    tauri::async_runtime::spawn_blocking(move || manager.stop_project(&id))
        .await
        .map_err(|error| format!("stop project worker failed: {error}"))?
}

#[tauri::command]
async fn restart_project(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let manager = state.manager.clone();
    tauri::async_runtime::spawn_blocking(move || manager.restart_project(&id))
        .await
        .map_err(|error| format!("restart project worker failed: {error}"))?
}

#[tauri::command]
async fn activate_project(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let manager = state.manager.clone();
    tauri::async_runtime::spawn_blocking(move || manager.activate_project(&id))
        .await
        .map_err(|error| format!("activate project worker failed: {error}"))?
}

#[tauri::command]
async fn get_logs(state: State<'_, AppState>, id: String) -> Result<Vec<LogEntry>, String> {
    let manager = state.manager.clone();
    tauri::async_runtime::spawn_blocking(move || manager.get_logs(&id))
        .await
        .map_err(|error| format!("get logs worker failed: {error}"))?
}

#[tauri::command]
async fn clear_logs(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let manager = state.manager.clone();
    tauri::async_runtime::spawn_blocking(move || manager.clear_logs(&id))
        .await
        .map_err(|error| format!("clear logs worker failed: {error}"))?
}

#[tauri::command]
async fn pick_directory() -> Option<String> {
    tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .pick_folder()
            .map(|path| path.to_string_lossy().into_owned())
    })
    .await
    .ok()
    .flatten()
}

#[tauri::command]
async fn detect_project_launch(directory: String) -> Result<LaunchDetection, String> {
    tauri::async_runtime::spawn_blocking(move || detect_launch(&directory))
        .await
        .map_err(|error| format!("启动方式识别失败：{error}"))?
}

#[tauri::command]
async fn pick_executable(directory: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut dialog = rfd::FileDialog::new()
            .set_title("选择可执行文件")
            .add_filter("可执行文件 (*.exe, *.bat, *.cmd)", &["exe", "bat", "cmd"]);
        let initial = PathBuf::from(directory);
        if initial.is_absolute() && initial.is_dir() {
            dialog = dialog.set_directory(initial);
        }
        dialog
            .pick_file()
            .map(|path| path.to_string_lossy().into_owned())
    })
    .await
    .map_err(|error| format!("executable picker failed: {error}"))
}

fn config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|directory| directory.join("projects.json"))
        .map_err(|error| format!("failed to resolve app data directory: {error}"))
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let path = config_path(app.app_handle()).map_err(std::io::Error::other)?;
            let manager = match ProjectManager::new(path) {
                Ok(manager) => manager,
                Err(error) => {
                    let _ = rfd::MessageDialog::new()
                        .set_title("Vibecode Launcher")
                        .set_description(format!("Failed to load project configuration: {error}"))
                        .show();
                    return Err(std::io::Error::other(error).into());
                }
            };
            let handle = app.app_handle().clone();
            manager.set_event_sink(Arc::new(move |event, payload| {
                let _ = handle.emit(event, payload);
            }));
            app.manage(AppState { manager });
            Ok(())
        })
        .on_window_event(|window, event| {
            if matches!(event, WindowEvent::CloseRequested { .. }) {
                if let Some(state) = window.app_handle().try_state::<AppState>() {
                    state.shutdown();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_projects,
            add_project,
            update_project,
            remove_project,
            start_project,
            stop_project,
            restart_project,
            activate_project,
            get_logs,
            clear_logs,
            pick_directory,
            detect_project_launch,
            pick_executable
        ])
        .build(tauri::generate_context!())
        .expect("error while building Vibecode Launcher")
        .run(|app, event| {
            if matches!(event, RunEvent::Exit) {
                if let Some(state) = app.try_state::<AppState>() {
                    state.shutdown();
                }
            }
        });
}

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    #[default]
    Browser,
    Executable,
}

/// A project persisted by the launcher.
///
/// Configuration fields deliberately stay small and string based so the JSON
/// file remains portable between the Rust backend and the UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub working_directory: String,
    #[serde(default)]
    pub launch_mode: LaunchMode,
    #[serde(default)]
    pub executable_path: String,
    #[serde(default)]
    pub arguments: String,
    pub command: String,
    pub preferred_port: u16,
    pub url_template: String,
    pub icon: Option<String>,
    pub auto_open_browser: bool,
}

/// Input used when creating or editing a project. The launcher assigns the id
/// after the input has passed validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInput {
    pub name: String,
    pub path: String,
    pub working_directory: String,
    #[serde(default)]
    pub launch_mode: LaunchMode,
    #[serde(default)]
    pub executable_path: String,
    #[serde(default)]
    pub arguments: String,
    pub command: String,
    pub preferred_port: u16,
    pub url_template: String,
    pub icon: Option<String>,
    pub auto_open_browser: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Stopped,
    Starting,
    Running,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSnapshot {
    pub project_id: String,
    pub status: ProjectStatus,
    pub pid: Option<u32>,
    pub actual_port: Option<u16>,
    pub resolved_command: Option<String>,
    pub started_at: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectView {
    pub project: Project,
    pub runtime: RuntimeSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub sequence: u64,
    pub timestamp: u64,
    pub stream: String,
    pub line: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_uses_camel_case_json_keys() {
        let project = Project {
            id: "project-1".to_owned(),
            name: "Dashboard".to_owned(),
            path: r"C:\Projects\dashboard".to_owned(),
            working_directory: r"C:\Projects\dashboard".to_owned(),
            launch_mode: LaunchMode::Browser,
            executable_path: String::new(),
            arguments: String::new(),
            command: "npm run dev -- --port {port}".to_owned(),
            preferred_port: 5173,
            url_template: "http://localhost:{port}".to_owned(),
            icon: Some("icon.svg".to_owned()),
            auto_open_browser: true,
        };

        let value = serde_json::to_value(project).expect("project should serialize");
        assert_eq!(value["workingDirectory"], "C:\\Projects\\dashboard");
        assert_eq!(value["preferredPort"], 5173);
        assert_eq!(value["urlTemplate"], "http://localhost:{port}");
        assert_eq!(value["autoOpenBrowser"], true);
        assert_eq!(value["launchMode"], "browser");
        assert!(value.get("working_directory").is_none());
    }

    #[test]
    fn status_uses_snake_case_json_values() {
        assert_eq!(
            serde_json::to_string(&ProjectStatus::Stopped).unwrap(),
            "\"stopped\""
        );
        assert_eq!(
            serde_json::to_string(&ProjectStatus::Starting).unwrap(),
            "\"starting\""
        );
        assert_eq!(
            serde_json::to_string(&ProjectStatus::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&ProjectStatus::Error).unwrap(),
            "\"error\""
        );
    }

    #[test]
    fn runtime_snapshot_and_log_entry_round_trip() {
        let view = ProjectView {
            project: Project {
                id: "p".to_owned(),
                name: "P".to_owned(),
                path: ".".to_owned(),
                working_directory: ".".to_owned(),
                launch_mode: LaunchMode::Browser,
                executable_path: String::new(),
                arguments: String::new(),
                command: "server --port {port}".to_owned(),
                preferred_port: 3000,
                url_template: "http://localhost:{port}".to_owned(),
                icon: None,
                auto_open_browser: false,
            },
            runtime: RuntimeSnapshot {
                project_id: "p".to_owned(),
                status: ProjectStatus::Running,
                pid: Some(42),
                actual_port: Some(3000),
                resolved_command: Some("server --port 3000".to_owned()),
                started_at: Some(1_700_000_000),
                error: None,
            },
        };

        let encoded = serde_json::to_string(&view).expect("view should serialize");
        let decoded: ProjectView = serde_json::from_str(&encoded).expect("view should deserialize");
        assert_eq!(decoded, view);
        assert!(encoded.contains("actualPort"));
        assert!(encoded.contains("resolvedCommand"));
    }

    #[test]
    fn version_one_projects_default_to_browser_mode() {
        let legacy = serde_json::json!({
            "id": "legacy", "name": "Existing project", "path": "C:\\Projects",
            "workingDirectory": "C:\\Projects", "command": "npm run dev -- --port {port}",
            "preferredPort": 5173, "urlTemplate": "http://localhost:{port}",
            "icon": null, "autoOpenBrowser": true
        });
        let project: Project = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(project.launch_mode, LaunchMode::Browser);
        assert!(project.executable_path.is_empty());
        assert!(project.arguments.is_empty());
        let input: ProjectInput = serde_json::from_value(legacy).unwrap();
        assert_eq!(input.launch_mode, LaunchMode::Browser);
    }
}

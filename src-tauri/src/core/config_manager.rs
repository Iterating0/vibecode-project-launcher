use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::Error as JsonError;
use url::Url;
use uuid::Uuid;

use super::models::{LaunchMode, Project, ProjectInput};

/// Owns the on-disk project configuration.
///
/// A missing configuration file is treated as a first-run empty list. Any
/// existing file that cannot be decoded is reported to the caller so a damaged
/// configuration can never be silently replaced with an empty one.
#[derive(Clone, Debug)]
pub struct ConfigManager {
    path: PathBuf,
}

impl ConfigManager {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<Vec<Project>, String> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(format!(
                    "failed to read project configuration {}: {error}",
                    self.path.display()
                ));
            }
        };

        serde_json::from_str(&contents).map_err(|error: JsonError| {
            format!(
                "failed to parse project configuration {}: {error}",
                self.path.display()
            )
        })
    }

    /// Save the complete project list using a same-directory temporary file
    /// and an atomic replacement. Keeping the temporary file beside the target
    /// avoids cross-volume rename failures and makes the replacement atomic on
    /// the supported filesystems.
    pub fn save(&self, projects: &[Project]) -> Result<(), String> {
        let contents = serde_json::to_vec_pretty(projects)
            .map_err(|error| format!("failed to serialize project configuration: {error}"))?;

        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create configuration directory {}: {error}",
                parent.display()
            )
        })?;

        let temporary_path = temporary_path(parent, self.path.file_name());
        let write_result = write_temporary_file(&temporary_path, &contents);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary_path);
            return Err(format!(
                "failed to write project configuration temporary file {}: {error}",
                temporary_path.display()
            ));
        }

        let replace_result = atomic_replace(&temporary_path, &self.path);
        if let Err(error) = replace_result {
            let _ = fs::remove_file(&temporary_path);
            return Err(format!(
                "failed to replace project configuration {}: {error}",
                self.path.display()
            ));
        }

        Ok(())
    }

    /// Validate user supplied project settings before assigning an id or
    /// persisting them.
    ///
    /// URL templates are intentionally restricted to local loopback hosts:
    /// this launcher starts local development servers and should not turn a
    /// typo in a configuration into an unexpected remote browser navigation.
    pub fn validate_input(input: &ProjectInput) -> Result<(), String> {
        if input.name.trim().is_empty() {
            return Err("project name must not be empty".to_owned());
        }

        let project_path = Path::new(&input.path);
        validate_directory("path", project_path)?;

        let working_directory = Path::new(&input.working_directory);
        validate_directory("workingDirectory", working_directory)?;

        if input.launch_mode == LaunchMode::Executable {
            return validate_executable(&input.executable_path, &input.arguments);
        }

        if input.command.trim().is_empty() {
            return Err("command must not be empty".to_owned());
        }

        if input.preferred_port == 0 {
            return Err("preferredPort must be greater than zero".to_owned());
        }

        validate_url_template(&input.url_template)
    }
}

fn validate_executable(value: &str, arguments: &str) -> Result<(), String> {
    if value.contains(['\0', '\r', '\n', '"']) || arguments.contains(['\0', '\r', '\n']) {
        return Err(
            "executablePath and arguments must contain a single valid command line".to_owned(),
        );
    }
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err("executablePath must be an absolute file path".to_owned());
    }
    let supported = path
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            ["exe", "bat", "cmd"]
                .iter()
                .any(|allowed| extension.eq_ignore_ascii_case(allowed))
        });
    if !supported {
        return Err("executablePath must be an .exe, .bat, or .cmd file".to_owned());
    }
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err("executablePath must point to a file".to_owned()),
        Err(error) => Err(format!(
            "cannot access executablePath {}: {error}",
            path.display()
        )),
    }
}

fn validate_directory(field: &str, path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err(format!("{field} must not be empty"));
    }

    if !path.is_absolute() {
        return Err(format!("{field} must be an absolute directory path"));
    }

    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(format!(
            "{field} must point to a directory: {}",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(format!(
            "{field} directory does not exist: {}",
            path.display()
        )),
        Err(error) => Err(format!(
            "failed to inspect {field} directory {}: {error}",
            path.display()
        )),
    }
}

fn validate_url_template(template: &str) -> Result<(), String> {
    if !template.contains("{port}") {
        return Err("urlTemplate must contain the {port} placeholder".to_owned());
    }

    // `{port}` is not valid URL syntax by itself. Replace it with valid ports
    // before parsing, while retaining the original template for the
    // placeholder check above. Two different sentinels ensure that a literal
    // port plus a path/query placeholder cannot pass as the authority port.
    let candidate = template.replace("{port}", "65534");
    let second_candidate = template.replace("{port}", "65535");
    let parsed = Url::parse(&candidate)
        .map_err(|error| format!("urlTemplate must be a valid URL: {error}"))?;
    let second_parsed = Url::parse(&second_candidate)
        .map_err(|error| format!("urlTemplate must be a valid URL: {error}"))?;

    if parsed.scheme() != "http" {
        return Err("urlTemplate scheme must be http for local dev servers".to_owned());
    }

    if parsed.username() != "" || parsed.password().is_some() {
        return Err("urlTemplate must not contain username or password".to_owned());
    }

    if parsed.port() != Some(65534) || second_parsed.port() != Some(65535) {
        return Err("urlTemplate must place {port} in the authority port".to_owned());
    }

    let host = parsed
        .host()
        .ok_or_else(|| "urlTemplate must include a host".to_owned())?;
    let is_loopback = match host {
        url::Host::Domain(domain) => domain
            .trim_end_matches('.')
            .eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    };
    if !is_loopback {
        return Err(
            "urlTemplate host must be localhost, 127.0.0.1, or [::1] for a local project"
                .to_owned(),
        );
    }

    Ok(())
}

fn temporary_path(parent: &Path, file_name: Option<&OsStr>) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(file_name.unwrap_or_else(|| OsStr::new("projects.json")));
    name.push(format!(".{}.tmp", Uuid::new_v4()));
    parent.join(name)
}

fn write_temporary_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(contents)?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.sync_all()
}

#[cfg(windows)]
fn atomic_replace(temporary_path: &Path, destination: &Path) -> io::Result<()> {
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = temporary_path
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect();
    let target: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect();

    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    let replaced = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), flags) };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn atomic_replace(temporary_path: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary_path, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::Project;
    use tempfile::tempdir;

    fn valid_input(directory: &Path) -> ProjectInput {
        ProjectInput {
            name: "Dashboard".to_owned(),
            path: directory.display().to_string(),
            working_directory: directory.display().to_string(),
            launch_mode: LaunchMode::Browser,
            executable_path: String::new(),
            arguments: String::new(),
            command: "npm run dev -- --port {port}".to_owned(),
            preferred_port: 5173,
            url_template: "http://localhost:{port}".to_owned(),
            icon: None,
            auto_open_browser: true,
        }
    }

    fn project_from(input: &ProjectInput) -> Project {
        Project {
            id: "project-1".to_owned(),
            name: input.name.clone(),
            path: input.path.clone(),
            working_directory: input.working_directory.clone(),
            launch_mode: input.launch_mode,
            executable_path: input.executable_path.clone(),
            arguments: input.arguments.clone(),
            command: input.command.clone(),
            preferred_port: input.preferred_port,
            url_template: input.url_template.clone(),
            icon: input.icon.clone(),
            auto_open_browser: input.auto_open_browser,
        }
    }

    #[test]
    fn missing_config_is_an_empty_first_run() {
        let directory = tempdir().unwrap();
        let config = ConfigManager::new(directory.path().join("projects.json"));
        assert_eq!(config.load().unwrap(), Vec::<Project>::new());
    }

    #[test]
    fn save_and_load_round_trip() {
        let directory = tempdir().unwrap();
        let config = ConfigManager::new(directory.path().join("nested").join("projects.json"));
        let input = valid_input(directory.path());
        let projects = vec![project_from(&input)];

        config.save(&projects).unwrap();
        assert_eq!(config.load().unwrap(), projects);
        assert!(config
            .path
            .parent()
            .expect("config parent")
            .join("projects.json")
            .is_file());
    }

    #[test]
    fn malformed_existing_config_is_an_error() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("projects.json");
        fs::write(&path, b"{ this is not valid JSON").unwrap();
        let config = ConfigManager::new(path);

        let error = config
            .load()
            .expect_err("malformed config must be surfaced");
        assert!(error.contains("failed to parse"));
    }

    #[test]
    fn validation_accepts_local_project_input() {
        let directory = tempdir().unwrap();
        assert!(ConfigManager::validate_input(&valid_input(directory.path())).is_ok());
    }

    #[test]
    fn validation_rejects_missing_directory_and_bad_port() {
        let directory = tempdir().unwrap();
        let mut input = valid_input(directory.path());
        input.path = directory.path().join("missing").display().to_string();
        assert!(ConfigManager::validate_input(&input).is_err());

        input.path = directory.path().display().to_string();
        input.preferred_port = 0;
        let error = ConfigManager::validate_input(&input).unwrap_err();
        assert!(error.contains("preferredPort"));
    }

    #[test]
    fn validation_rejects_relative_working_directory() {
        let directory = tempdir().unwrap();
        let mut input = valid_input(directory.path());
        input.working_directory = ".".to_owned();
        assert!(ConfigManager::validate_input(&input)
            .unwrap_err()
            .contains("absolute"));
    }

    #[test]
    fn validation_rejects_non_loopback_or_missing_placeholder() {
        let directory = tempdir().unwrap();
        let mut input = valid_input(directory.path());

        input.url_template = "http://example.com:{port}".to_owned();
        assert!(ConfigManager::validate_input(&input).is_err());

        input.url_template = "http://localhost:5173".to_owned();
        assert!(ConfigManager::validate_input(&input).is_err());

        input.url_template = "ftp://localhost:{port}".to_owned();
        assert!(ConfigManager::validate_input(&input).is_err());
    }

    #[test]
    fn validation_requires_placeholder_in_authority_port() {
        let directory = tempdir().unwrap();
        let mut input = valid_input(directory.path());

        input.url_template = "http://localhost:1234/{port}".to_owned();
        let error = ConfigManager::validate_input(&input).unwrap_err();
        assert!(error.contains("authority port"));

        // A literal sentinel port must not make a path placeholder look valid.
        input.url_template = "http://localhost:65535/{port}".to_owned();
        let error = ConfigManager::validate_input(&input).unwrap_err();
        assert!(error.contains("authority port"));

        input.url_template = "http://user:secret@localhost:{port}/".to_owned();
        let error = ConfigManager::validate_input(&input).unwrap_err();
        assert!(error.contains("username or password"));

        input.url_template = "http://localhost:{port}/app".to_owned();
        assert!(ConfigManager::validate_input(&input).is_ok());
    }

    #[test]
    fn validation_accepts_ipv6_loopback() {
        let directory = tempdir().unwrap();
        let mut input = valid_input(directory.path());
        input.url_template = "http://[::1]:{port}".to_owned();
        assert!(ConfigManager::validate_input(&input).is_ok());
    }

    #[test]
    fn executable_mode_ignores_inactive_browser_settings_and_round_trips() {
        let directory = tempdir().unwrap();
        let executable = directory.path().join("启动 程序.BAT");
        fs::write(&executable, "@exit /b 0\r\n").unwrap();
        let mut input = valid_input(directory.path());
        input.launch_mode = LaunchMode::Executable;
        input.executable_path = executable.display().to_string();
        input.arguments = "--config \"file with spaces.json\"".to_owned();
        input.command.clear();
        input.preferred_port = 0;
        input.url_template.clear();
        ConfigManager::validate_input(&input).unwrap();
        let config = ConfigManager::new(directory.path().join("projects.json"));
        let project = project_from(&input);
        config.save(std::slice::from_ref(&project)).unwrap();
        assert_eq!(config.load().unwrap(), vec![project]);

        input.launch_mode = LaunchMode::Browser;
        assert!(ConfigManager::validate_input(&input).is_err());
    }

    #[test]
    fn executable_validation_rejects_missing_relative_and_unsupported_files() {
        let directory = tempdir().unwrap();
        let mut input = valid_input(directory.path());
        input.launch_mode = LaunchMode::Executable;
        for path in [
            PathBuf::from("app.exe"),
            directory.path().join("missing.exe"),
            directory.path().join("readme.txt"),
        ] {
            input.executable_path = path.display().to_string();
            assert!(ConfigManager::validate_input(&input).is_err());
        }
        let executable = directory.path().join("app.exe");
        fs::write(&executable, "fixture").unwrap();
        input.executable_path = executable.display().to_string();
        input.arguments = "first\r\nsecond".to_owned();
        assert!(ConfigManager::validate_input(&input).is_err());
    }
}

//! Read-only, bounded launch suggestions. Detection never executes project code.
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::models::LaunchMode;

const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 256;
const MAX_SUGGESTIONS: usize = 24;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchSettings {
    pub working_directory: String,
    pub launch_mode: LaunchMode,
    pub executable_path: String,
    pub arguments: String,
    pub command: String,
    pub preferred_port: u16,
    pub url_template: String,
    pub auto_open_browser: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchSuggestion {
    pub label: String,
    pub detail: String,
    pub settings: LaunchSettings,
    #[serde(skip)]
    priority: u8,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchDetection {
    pub suggestions: Vec<LaunchSuggestion>,
    pub warnings: Vec<String>,
}

/// Inspect only the selected root, common application folders, and known output
/// locations. Do not walk dependency trees, follow directory links, or read .env.
pub fn detect_launch(directory: &str) -> Result<LaunchDetection, String> {
    let root = Path::new(directory);
    if !root.is_absolute() || !root.is_dir() {
        return Err("请选择存在的绝对路径文件夹；也可以手动填写启动方式。".to_owned());
    }
    let mut result = LaunchDetection::default();
    inspect_project(root, root, &mut result);
    for child in ["frontend", "client", "web", "backend", "server", "app"] {
        let path = root.join(child);
        if regular_directory(&path) {
            inspect_project(root, &path, &mut result);
        }
    }
    result.suggestions.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then(left.label.cmp(&right.label))
    });
    if result.suggestions.len() > MAX_SUGGESTIONS {
        result.suggestions.truncate(MAX_SUGGESTIONS);
        result
            .warnings
            .push("入口较多，仅显示前 24 个建议；仍可手动填写其他入口。".to_owned());
    }
    result.warnings.sort();
    result.warnings.dedup();
    Ok(result)
}

fn regular_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir() && !is_link(&metadata))
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Includes junctions as well as symbolic links.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file() && !is_link(&metadata))
}

fn read_small(path: &Path, result: &mut LaunchDetection) -> Option<String> {
    let read = || -> Result<Option<String>, String> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        if !metadata.is_file() || is_link(&metadata) {
            return Ok(None);
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err("文件超过 256 KiB，已跳过".to_owned());
        }
        let mut contents = String::new();
        File::open(path)
            .map_err(|error| error.to_string())?
            .take(MAX_FILE_BYTES + 1)
            .read_to_string(&mut contents)
            .map_err(|error| error.to_string())?;
        if contents.len() as u64 > MAX_FILE_BYTES {
            return Err("文件超过 256 KiB，已跳过".to_owned());
        }
        Ok(Some(contents.trim_start_matches('\u{feff}').to_owned()))
    };
    match read() {
        Ok(contents) => contents,
        Err(error) => {
            result
                .warnings
                .push(format!("无法识别 {}：{error}", path.display()));
            None
        }
    }
}

fn browser_settings(directory: &Path, command: String, port: u16) -> LaunchSettings {
    LaunchSettings {
        working_directory: directory.to_string_lossy().into_owned(),
        launch_mode: LaunchMode::Browser,
        executable_path: String::new(),
        arguments: String::new(),
        command,
        preferred_port: port,
        // localhost lets readiness try both IPv4 and IPv6 (Vite may bind ::1).
        url_template: "http://localhost:{port}".to_owned(),
        auto_open_browser: true,
    }
}

fn suggest(
    result: &mut LaunchDetection,
    root: &Path,
    directory: &Path,
    label: &str,
    detail: String,
    settings: LaunchSettings,
    priority: u8,
) {
    let relative = directory.strip_prefix(root).unwrap_or(directory);
    let suffix = if relative.as_os_str().is_empty() {
        String::new()
    } else {
        format!(" · {}", relative.display())
    };
    result.suggestions.push(LaunchSuggestion {
        label: format!("{label}{suffix}"),
        detail,
        settings,
        priority: priority.saturating_sub(if directory == root { 0 } else { 2 }),
    });
}

fn inspect_project(root: &Path, directory: &Path, result: &mut LaunchDetection) {
    let has_package = regular_file(&directory.join("package.json"));
    if let Some(contents) = read_small(&directory.join("package.json"), result) {
        match serde_json::from_str::<Value>(&contents) {
            Ok(package) if package.is_object() => detect_node(root, directory, &package, result),
            _ => result.warnings.push(format!(
                "{} 不是有效的 package.json，已跳过 Node.js 识别。",
                directory.display()
            )),
        }
    }
    detect_python(root, directory, result);
    detect_executables(root, directory, result);
    // A Vite source index.html is not a deployable static site.
    if !has_package && regular_file(&directory.join("index.html")) {
        suggest(
            result,
            root,
            directory,
            "静态网页",
            "发现 index.html；使用 Python 提供本地 HTTP 服务。".to_owned(),
            browser_settings(
                directory,
                format!(
                    "{} -m http.server {{port}} --bind 127.0.0.1",
                    python_command(directory)
                ),
                8000,
            ),
            45,
        );
    }
}

fn package_manager(
    directory: &Path,
    package: &Value,
    result: &mut LaunchDetection,
) -> &'static str {
    if let Some(declared) = package.get("packageManager").and_then(Value::as_str) {
        let name = declared.split('@').next().unwrap_or("");
        if let Some(manager) = ["npm", "pnpm", "yarn", "bun"]
            .into_iter()
            .find(|manager| *manager == name)
        {
            return manager;
        }
        result.warnings.push(format!(
            "不支持 packageManager={declared}；请确认建议中的包管理器。"
        ));
    }
    let found: Vec<_> = [
        ("pnpm", &["pnpm-lock.yaml"][..]),
        ("yarn", &["yarn.lock"][..]),
        ("bun", &["bun.lock", "bun.lockb"][..]),
        ("npm", &["package-lock.json", "npm-shrinkwrap.json"][..]),
    ]
    .into_iter()
    .filter(|(_, files)| files.iter().any(|file| regular_file(&directory.join(file))))
    .map(|(manager, _)| manager)
    .collect();
    if found.len() > 1 {
        result.warnings.push(format!(
            "{} 有多种 lockfile；暂用 {}，请确认。",
            directory.display(),
            found[0]
        ));
    }
    found.first().copied().unwrap_or("npm")
}

fn detect_node(root: &Path, directory: &Path, package: &Value, result: &mut LaunchDetection) {
    let Some(scripts) = package.get("scripts").and_then(Value::as_object) else {
        return;
    };
    let manager = package_manager(directory, package, result);
    for (name, base_priority) in [("dev", 90), ("start", 85), ("serve", 80), ("preview", 60)] {
        let Some(script) = scripts
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let runner = format!("{manager} run {name}");
        let forward = if manager == "npm" { " --" } else { "" };
        let words: Vec<_> = script.split_whitespace().collect();
        // Only append framework-specific arguments to a single direct CLI.
        // Compound commands and custom wrappers retain their original script.
        let simple = !script.contains(['&', '|', ';', '>', '<', '\n', '\r']);
        let framework = if simple {
            match words.as_slice() {
                ["vite", ..] if !words.contains(&"build") => {
                    Some(("Vite", 5173, "--port {port} --strictPort"))
                }
                ["next", "dev" | "start", ..] => Some(("Next.js", 3000, "--port {port}")),
                ["astro", "dev" | "preview", ..] => Some(("Astro", 4321, "--port {port}")),
                ["nuxt" | "nuxi", "dev" | "start" | "preview", ..] => {
                    Some(("Nuxt", 3000, "--port {port}"))
                }
                ["ng", "serve", ..] => Some(("Angular", 4200, "--port {port}")),
                ["react-scripts", "start"] => Some(("Create React App", 3000, "")),
                _ => None,
            }
        } else {
            None
        };
        // HTTPS cannot satisfy the launcher's HTTP-only readiness contract.
        if words.iter().any(|word| word.contains("https")) {
            result.warnings.push(format!(
                "{name} 脚本使用 HTTPS，请手动调整为本地 HTTP 启动方式。"
            ));
            continue;
        }
        if let Some((framework, default_port, flags)) = framework {
            let port = explicit_port(&words).unwrap_or(if script.starts_with("vite preview") {
                4173
            } else {
                default_port
            });
            // Vite explicitly keeps the last repeated CLI option. For other
            // CLIs, avoid assuming how duplicate fixed port flags are parsed.
            let fixed_port = words
                .iter()
                .any(|word| *word == "--port" || *word == "-p" || word.starts_with("--port="));
            if fixed_port && framework != "Vite" {
                suggest_custom_script(root, directory, result, name, script, runner, port);
                continue;
            }
            let command = if flags.is_empty() {
                format!("set \"PORT={{port}}\" && set \"BROWSER=none\" && {runner}")
            } else {
                format!("{runner}{forward} {flags}")
            };
            let build_note = if name == "preview"
                || script.starts_with("next start")
                || script.contains(" preview")
            {
                " 此入口需要先完成项目构建。"
            } else {
                ""
            };
            suggest(
                result,
                root,
                directory,
                &format!("{framework} · {name}"),
                format!("来自 package.json 的 {name}：{script}。{build_note}"),
                browser_settings(directory, command, port),
                base_priority,
            );
        } else if !(script.starts_with("tauri ") || script.starts_with("electron ")) {
            suggest_custom_script(root, directory, result, name, script, runner, 3000);
        }
    }
}

fn explicit_port(words: &[&str]) -> Option<u16> {
    words.iter().enumerate().find_map(|(index, word)| {
        let value = if *word == "--port" || *word == "-p" {
            words.get(index + 1).copied()
        } else {
            word.strip_prefix("--port=")
        };
        value
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|port| *port > 0)
    })
}

fn suggest_custom_script(
    root: &Path,
    directory: &Path,
    result: &mut LaunchDetection,
    name: &str,
    script: &str,
    runner: String,
    port: u16,
) {
    suggest(result, root, directory, &format!("自定义脚本 · {name}（需确认）"),
        format!("来自 package.json：{script}。无法确定服务类型和端口参数；已提供 PORT 环境变量，请确认程序会读取它，或手动改成含 {{port}} 的命令。桌面程序请切换启动方式。"),
        browser_settings(directory, format!("set \"PORT={{port}}\" && {runner}"), port), 25);
}

fn python_command(directory: &Path) -> String {
    for executable in [".venv/Scripts/python.exe", "venv/Scripts/python.exe"] {
        if regular_file(&directory.join(executable)) {
            // Keep paths relative to the suggested working directory; quoting
            // also avoids embedding metacharacters from an arbitrary root path.
            return format!("\"{}\"", executable.replace('/', "\\"));
        }
    }
    "python".to_owned()
}

fn detect_python(root: &Path, directory: &Path, result: &mut LaunchDetection) {
    let python = python_command(directory);
    if let Some(manage) = read_small(&directory.join("manage.py"), result) {
        if manage.contains("django.core.management") {
            suggest(
                result,
                root,
                directory,
                "Django",
                "发现 manage.py 中的 Django 管理入口。".to_owned(),
                browser_settings(
                    directory,
                    format!("{python} manage.py runserver 127.0.0.1:{{port}} --noreload"),
                    8000,
                ),
                88,
            );
        }
    }
    for file in ["main.py", "app.py", "server.py", "wsgi.py", "app/main.py"] {
        if file.contains('/') && !regular_directory(&directory.join("app")) {
            continue;
        }
        let Some(source) = read_small(&directory.join(file), result) else {
            continue;
        };
        let module = file.trim_end_matches(".py").replace('/', ".");
        for (constructor, framework, port) in
            [("FastAPI", "FastAPI", 8000), ("Flask", "Flask", 5000)]
        {
            if let Some(variable) = python_instance(&source, constructor) {
                let command = if constructor == "FastAPI" {
                    format!(
                        "{python} -m uvicorn {module}:{variable} --host 127.0.0.1 --port {{port}}"
                    )
                } else {
                    format!("{python} -m flask --app {module}:{variable} run --host 127.0.0.1 --port {{port}} --no-reload")
                };
                suggest(
                    result,
                    root,
                    directory,
                    &format!("{framework} · {module}:{variable}"),
                    format!("发现 {file} 中的 {constructor} 应用实例；请确认项目依赖已安装。"),
                    browser_settings(directory, command, port),
                    86,
                );
            }
        }
    }
}

fn python_instance(source: &str, constructor: &str) -> Option<String> {
    // Deliberately conservative: top-level named instances, not comments,
    // indented examples, factories, or a guessed main:app based on dependencies.
    python_code(source).lines().find_map(|line| {
        if line.starts_with(char::is_whitespace) {
            return None;
        }
        let (left, right) = line.split_once('=')?;
        let name = left.trim().split(':').next()?.trim();
        let valid = !name.is_empty()
            && name.chars().enumerate().all(|(index, ch)| {
                ch == '_' || ch.is_ascii_alphabetic() || (index > 0 && ch.is_ascii_digit())
            });
        if !valid {
            return None;
        }
        let callee = right.trim().split_once('(')?.0.trim();
        let qualified = if constructor == "FastAPI" {
            "fastapi.FastAPI"
        } else {
            "flask.Flask"
        };
        (callee == constructor || callee == qualified).then(|| name.to_owned())
    })
}

/// Remove Python comments and string bodies while retaining line/indentation
/// boundaries. This is lexical inspection only; no imports or code evaluation.
fn python_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut code = bytes.to_vec();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'#' {
            while index < bytes.len() && bytes[index] != b'\n' {
                code[index] = b' ';
                index += 1;
            }
        } else if bytes[index] == b'\'' || bytes[index] == b'"' {
            let quote = bytes[index];
            let width = if bytes.get(index..index + 3) == Some(&[quote; 3]) {
                3
            } else {
                1
            };
            code[index..index + width].fill(b' ');
            index += width;
            while index < bytes.len() {
                if bytes[index] == b'\\' {
                    code[index] = b' ';
                    index += 1;
                    if index < bytes.len() {
                        if bytes[index] != b'\n' {
                            code[index] = b' ';
                        }
                        index += 1;
                    }
                } else if bytes
                    .get(index..index + width)
                    .is_some_and(|slice| slice.iter().all(|byte| *byte == quote))
                {
                    code[index..index + width].fill(b' ');
                    index += width;
                    break;
                } else {
                    if bytes[index] != b'\n' {
                        code[index] = b' ';
                    }
                    index += 1;
                }
            }
        } else {
            index += 1;
        }
    }
    // Every byte in a removed string/comment is replaced, including UTF-8.
    String::from_utf8(code).expect("removing complete string bodies retains UTF-8")
}

fn detect_executables(root: &Path, directory: &Path, result: &mut LaunchDetection) {
    for folder in [
        "",
        "scripts",
        "dist",
        "build",
        "bin",
        "release",
        "target/release",
        "src-tauri/target/release",
    ] {
        let location = directory.join(folder);
        // Check each ancestor too, so a target or src-tauri junction is skipped.
        let mut checked = directory.to_path_buf();
        if !folder
            .split('/')
            .filter(|part| !part.is_empty())
            .all(|part| {
                checked.push(part);
                regular_directory(&checked)
            })
        {
            continue;
        }
        let entries = match fs::read_dir(&location) {
            Ok(entries) => entries,
            Err(error) => {
                result
                    .warnings
                    .push(format!("无法读取 {}：{error}", location.display()));
                continue;
            }
        };
        let mut files: Vec<PathBuf> = Vec::new();
        for (index, entry) in entries.enumerate() {
            if index >= MAX_DIRECTORY_ENTRIES {
                result.warnings.push(format!(
                    "{} 文件较多，仅检查前 256 项。",
                    location.display()
                ));
                break;
            }
            if let Ok(entry) = entry {
                let path = entry.path();
                if regular_file(&path) {
                    files.push(path)
                }
            }
        }
        files.sort();
        for file in files {
            let extension = file
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !["exe", "bat", "cmd"].contains(&extension.as_str()) {
                continue;
            }
            let stem = file
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if [
                "install", "setup", "unins", "update", "crash", "helper", "test", "cleanup",
                "unpack",
            ]
            .iter()
            .any(|word| stem.contains(word))
            {
                continue;
            }
            let launcher = ["run", "start", "launch", "dev"].iter().any(|prefix| {
                stem == *prefix
                    || stem.starts_with(&format!("{prefix}_"))
                    || stem.starts_with(&format!("{prefix}-"))
            });
            if extension != "exe" && !launcher {
                continue;
            }
            let relative = file.strip_prefix(directory).unwrap_or(&file);
            let mut settings = browser_settings(directory, String::new(), 5173);
            settings.launch_mode = LaunchMode::Executable;
            settings.executable_path = file.to_string_lossy().into_owned();
            settings.auto_open_browser = false;
            if extension == "exe" {
                settings.working_directory = location.to_string_lossy().into_owned();
            }
            suggest(result, root, directory, &format!("{} · {}", if extension == "exe" { "可执行程序" } else { "启动脚本" }, relative.display()),
                "发现 Windows 启动文件；请确认文件、参数和工作目录。脚本如需等待网页就绪，可改用浏览器部署。".to_owned(),
                settings, if launcher { 95 } else { 75 });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config_manager::ConfigManager;
    use crate::core::models::ProjectInput;
    use tempfile::{tempdir, TempDir};

    fn fixture(files: &[(&str, &str)]) -> TempDir {
        let directory = tempdir().unwrap();
        for (name, contents) in files {
            let path = directory.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        directory
    }

    fn detect(directory: &TempDir) -> LaunchDetection {
        detect_launch(directory.path().to_str().unwrap()).unwrap()
    }

    #[test]
    fn vite_uses_declared_manager_and_dynamic_port_without_mutating_files() {
        let manifest = r#"{"packageManager":"pnpm@10.0.0","scripts":{"dev":"vite --port 5180","build":"vite build"}}"#;
        let directory = fixture(&[
            ("package.json", manifest),
            ("package-lock.json", "{}"),
            ("index.html", "source"),
        ]);
        let result = detect(&directory);
        assert_eq!(result.suggestions.len(), 1);
        let settings = &result.suggestions[0].settings;
        assert_eq!(settings.command, "pnpm run dev --port {port} --strictPort");
        assert_eq!(settings.preferred_port, 5180);
        assert_eq!(settings.launch_mode, LaunchMode::Browser);
        assert_eq!(
            fs::read_to_string(directory.path().join("package.json")).unwrap(),
            manifest
        );
    }

    #[test]
    fn node_managers_forward_arguments_and_dev_precedes_preview() {
        for (lock, expected) in [
            ("package-lock.json", "npm run dev --"),
            ("yarn.lock", "yarn run dev"),
            ("bun.lockb", "bun run dev"),
            ("pnpm-lock.yaml", "pnpm run dev"),
        ] {
            let directory = fixture(&[
                (
                    "package.json",
                    r#"{"scripts":{"preview":"next start","dev":"next dev"}}"#,
                ),
                (lock, ""),
            ]);
            let result = detect(&directory);
            assert_eq!(result.suggestions.len(), 2);
            assert_eq!(
                result.suggestions[0].settings.command,
                format!("{expected} --port {{port}}")
            );
            assert_eq!(result.suggestions[0].settings.preferred_port, 3000);
            assert!(result.suggestions[1].detail.contains("构建"));
        }
    }

    #[test]
    fn recognizes_common_cli_defaults_and_cra_environment() {
        for (script, port) in [
            ("astro dev", 4321),
            ("nuxt dev", 3000),
            ("ng serve", 4200),
            ("vite preview", 4173),
            ("react-scripts start", 3000),
        ] {
            let package = serde_json::json!({"scripts": {"dev": script}}).to_string();
            let directory = fixture(&[("package.json", &package)]);
            let result = detect(&directory);
            assert_eq!(result.suggestions.len(), 1);
            let suggestion = &result.suggestions[0];
            assert_eq!(suggestion.settings.preferred_port, port);
            assert!(suggestion.settings.command.contains("{port}"));
            if script.starts_with("react-scripts") {
                assert!(suggestion.settings.command.contains("BROWSER=none"));
                assert!(!suggestion.settings.command.contains("--port"));
            }
        }
    }

    #[test]
    fn custom_and_fixed_port_scripts_are_not_given_guessed_cli_flags() {
        for script in [
            "node server.js",
            "echo vite && node server.js",
            "next dev -p 4567",
            "concurrently vite api",
            "cross-env MODE=dev vite",
        ] {
            let package =
                serde_json::json!({"scripts": {"dev": script}, "devDependencies": {"vite": "*"}})
                    .to_string();
            let directory = fixture(&[("package.json", &package)]);
            let result = detect(&directory);
            assert_eq!(
                result.suggestions[0].settings.command,
                "set \"PORT={port}\" && npm run dev"
            );
            assert!(result.suggestions[0].detail.contains("无法确定"));
            assert!(result.suggestions[0].label.contains("需确认"));
        }
    }

    #[test]
    fn nested_project_keeps_its_own_working_directory() {
        let directory = fixture(&[
            (
                "frontend/package.json",
                "\u{feff}{\"scripts\":{\"dev\":\"vite\"}}",
            ),
            ("frontend/pnpm-lock.yaml", ""),
        ]);
        let result = detect(&directory);
        let suggestion = &result.suggestions[0];
        assert!(suggestion.label.contains("frontend"));
        assert_eq!(
            suggestion.settings.working_directory,
            directory.path().join("frontend").to_string_lossy()
        );
        assert!(suggestion.settings.command.starts_with("pnpm"));
    }

    #[test]
    fn python_detects_real_instance_names_and_local_virtual_environment() {
        let directory = fixture(&[
            (
                "main.py",
                "from fastapi import FastAPI\napi: FastAPI = FastAPI()\n",
            ),
            (
                "app.py",
                "from flask import Flask\nwebsite = Flask(__name__)\n",
            ),
            (".venv/Scripts/python.exe", "fixture"),
        ]);
        let result = detect(&directory);
        assert_eq!(result.suggestions.len(), 2);
        assert!(result.suggestions.iter().any(|suggestion| suggestion.settings.command == "\".venv\\Scripts\\python.exe\" -m uvicorn main:api --host 127.0.0.1 --port {port}"));
        assert!(result.suggestions.iter().any(|suggestion| suggestion
            .settings
            .command
            .contains("--app app:website run")));
    }

    #[test]
    fn python_does_not_guess_from_dependencies_or_comments() {
        let directory = fixture(&[
            ("requirements.txt", "fastapi\nflask\n"),
            (
                "main.py",
                "# app = FastAPI()\n\"\"\"中文示例\napp = FastAPI()\n\"\"\"\ndef factory():\n    app = FastAPI()\n    return app\n",
            ),
        ]);
        assert!(detect(&directory).suggestions.is_empty());
    }

    #[test]
    fn django_and_static_pages_have_explicit_loopback_ports() {
        let django = fixture(&[(
            "manage.py",
            "from django.core.management import execute_from_command_line",
        )]);
        assert_eq!(
            detect(&django).suggestions[0].settings.command,
            "python manage.py runserver 127.0.0.1:{port} --noreload"
        );
        let html = fixture(&[("index.html", "<h1>hello</h1>")]);
        assert_eq!(
            detect(&html).suggestions[0].settings.command,
            "python -m http.server {port} --bind 127.0.0.1"
        );
    }

    #[test]
    fn executables_are_ranked_and_installers_and_dependency_trees_are_ignored() {
        let directory = fixture(&[
            ("run.bat", "@echo this must never be executed"),
            ("dist/My App.exe", "fixture"),
            ("dist/setup.exe", "fixture"),
            ("uninstall.cmd", "fixture"),
            ("node_modules/tool/run.cmd", "fixture"),
            (".git/start.cmd", "fixture"),
            ("target/release/tool.exe", "fixture"),
        ]);
        let result = detect(&directory);
        assert_eq!(result.suggestions.len(), 3);
        assert!(result.suggestions[0]
            .settings
            .executable_path
            .ends_with("run.bat"));
        assert_eq!(
            result.suggestions[0].settings.working_directory,
            directory.path().to_string_lossy()
        );
        for suggestion in result.suggestions {
            assert_eq!(suggestion.settings.launch_mode, LaunchMode::Executable);
            assert!(!suggestion.settings.auto_open_browser);
            assert!(suggestion.settings.command.is_empty());
            if suggestion.settings.executable_path.ends_with("My App.exe") {
                assert_eq!(
                    suggestion.settings.working_directory,
                    directory.path().join("dist").to_string_lossy()
                );
            }
        }
    }

    #[test]
    fn malformed_and_oversized_metadata_fall_back_without_hiding_other_entries() {
        for contents in [
            "{ broken".to_owned(),
            " ".repeat(MAX_FILE_BYTES as usize + 1),
        ] {
            let directory = fixture(&[
                ("package.json", &contents),
                ("start.cmd", "@echo off"),
                ("index.html", "source"),
            ]);
            let result = detect(&directory);
            assert_eq!(result.suggestions.len(), 1);
            assert!(!result.warnings.is_empty());
            assert_eq!(
                result.suggestions[0].settings.launch_mode,
                LaunchMode::Executable
            );
        }
    }

    #[test]
    fn empty_missing_and_relative_directories_are_handled() {
        let directory = tempdir().unwrap();
        assert!(detect(&directory).suggestions.is_empty());
        assert!(detect_launch(".").is_err());
        assert!(detect_launch(directory.path().join("missing").to_str().unwrap()).is_err());
    }

    #[test]
    fn ambiguous_managers_and_https_report_actionable_information() {
        let directory = fixture(&[
            ("package.json", r#"{"scripts":{"dev":"vite --https"}}"#),
            ("yarn.lock", ""),
            ("package-lock.json", ""),
        ]);
        let result = detect(&directory);
        assert!(result.suggestions.is_empty());
        assert_eq!(result.warnings.len(), 2);
    }

    #[test]
    fn suggested_settings_pass_the_existing_save_contract() {
        let directory = fixture(&[
            ("package.json", r#"{"scripts":{"dev":"vite"}}"#),
            ("start.cmd", "@echo off"),
        ]);
        for suggestion in detect(&directory).suggestions {
            let settings = suggestion.settings;
            let input = ProjectInput {
                name: "fixture".to_owned(),
                path: directory.path().to_string_lossy().into_owned(),
                working_directory: settings.working_directory,
                launch_mode: settings.launch_mode,
                executable_path: settings.executable_path,
                arguments: settings.arguments,
                command: settings.command,
                preferred_port: settings.preferred_port,
                url_template: settings.url_template,
                auto_open_browser: settings.auto_open_browser,
                icon: None,
            };
            ConfigManager::validate_input(&input).unwrap();
        }
    }
}

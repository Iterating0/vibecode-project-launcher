# 架构与开发记录

## 范围

Windows 本地项目启动器。Tauri 2 + Rust 管理本地资源，React + TypeScript 只展示卡片和弹窗。项目目录仅被引用，不复制、不自动安装依赖；添加时有限读取项目配置和常见启动入口，用于预填表单。

## 模块边界

- `core/models.rs`：配置、运行状态、日志的数据契约。
- `core/config_manager.rs`：验证配置、JSON 持久化与原子替换。
- `core/launch_detector.rs`：有界、只读的启动入口识别；返回排序后的配置建议和诊断，不执行项目代码、不保存项目。
- `core/port_manager.rs`：启动时探测端口、保留并发分配记录。
- `core/process_manager.rs`：进程生命周期、有限日志和退出事件。
- `core/windows_process.rs`：Windows Job Object、进程树终止和监听端口归属。
- `core/project_manager.rs`：配置与运行实例的协调，启动/停止/重启/使用项目。
- `services/browser_service.rs`：系统默认浏览器。
- `services/readiness.rs`：启动期间的本地 HTTP 就绪检测。
- `src-tauri/src/lib.rs`：Tauri IPC、事件和退出清理。
- `src/`：前端，不直接访问文件系统或进程。

## Windows 进程树

每个项目单独拥有 Job Object，并配置 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`。根命令以挂起状态创建，加入 Job 后才恢复，后代自动继承 Job。Stop/退出终止整个 Job；仅操作 Launcher 自己创建的进程。

## 数据与事件

JSON 只保存项目配置，运行状态不跨启动恢复。`project-state` 推送状态；`project-log` 推送有界日志。空闲状态没有目录或端口轮询。

`launchMode` 区分 `browser` 与 `executable`，缺少此字段的第一版配置默认 `browser`。浏览器模式沿用端口分配、命令占位符、HTTP 就绪与浏览器打开流程。可执行文件模式使用 `executablePath` 和 `arguments`，不分配端口，也不进行 HTTP 就绪检查；两种模式共用工作目录、日志和 Job Object 生命周期。浏览器字段保留以支持切换配置草稿。

## 启动方式识别

添加弹窗通过 `detect_project_launch` 在阻塞工作线程读取有限配置，前端等待结果后只预填本次草稿。用户可跳过识别、取消弹窗或编辑建议；请求序号阻止过期结果覆盖手动输入。没有结果和读取失败都保留原表单，只有用户提交才调用 `add_project`。设置弹窗不触发识别，已有项目配置不受影响。

命令参数参考 [Vite CLI](https://vite.dev/guide/cli)、[Next.js CLI](https://nextjs.org/docs/app/api-reference/cli/next)、[Astro CLI](https://docs.astro.build/zh-cn/reference/cli-reference/)、[Nuxt CLI](https://nuxt.com/docs/4.x/api/commands/dev)、[Angular CLI](https://angular.dev/cli/serve)、[Flask CLI](https://flask.palletsprojects.com/en/stable/cli/) 和 [Uvicorn 配置](https://www.uvicorn.org/settings/)。复合命令、不能可靠追加端口的脚本保留为需确认建议，不改写项目文件。

## 开发检查点

1. 工程骨架、项目模型与 JSON 保存。
2. 独立端口分配和 Windows 进程管理。
3. 卡片墙、双击编排、日志与设置。
4. Windows 生命周期集成验证、UI 验收和发布构建。

并行开发按可构建的集成检查点提交；不把尚未通过检查的工作描述为已通过。

## 官方参考

- [Tauri Windows 前置要求](https://v2.tauri.app/start/prerequisites/)
- [Tauri 配置](https://v2.tauri.app/reference/config/)
- [Microsoft Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)

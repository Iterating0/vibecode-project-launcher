$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProjectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
Set-Location -LiteralPath $ProjectRoot
$ProjectExe = Join-Path $ProjectRoot 'src-tauri\target\release\vibecode-project-launcher.exe'
$StateRoot = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'VibecodeProjectLauncher'
$ProjectKeyAlgorithm = [System.Security.Cryptography.SHA256]::Create()
try {
    $ProjectKey = [string]::Join('', ($ProjectKeyAlgorithm.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($ProjectRoot.ToLowerInvariant())) | ForEach-Object { $_.ToString('x2') }))
}
finally {
    $ProjectKeyAlgorithm.Dispose()
}
$ProjectStateRoot = Join-Path (Join-Path $StateRoot 'projects') $ProjectKey
$BuildStatePath = Join-Path $ProjectStateRoot 'build-state.json'
$DependencyStatePath = Join-Path $ProjectStateRoot 'dependency-state.json'
$BuildLog = $null
$Mutex = $null
$HasMutex = $false
$ExitCode = 0

function Get-InputFiles {
    $filesByPath = @{}
    $explicitFiles = @(
        'package.json',
        'package-lock.json',
        'index.html',
        'vite.config.ts',
        'vite.config.js',
        'vite.config.mjs',
        'vite.config.cjs',
        'src-tauri\Cargo.toml',
        'src-tauri\Cargo.lock',
        'src-tauri\build.rs',
        'src-tauri\tauri.conf.json'
    )

    foreach ($relativePath in $explicitFiles) {
        $fullPath = Join-Path $ProjectRoot $relativePath
        if (Test-Path -LiteralPath $fullPath -PathType Leaf) {
            $file = Get-Item -LiteralPath $fullPath
            $filesByPath[$file.FullName.ToLowerInvariant()] = $file
        }
    }

    foreach ($pattern in @('vite.config.*', 'tsconfig*.json', '.env', '.env.*')) {
        foreach ($file in (Get-ChildItem -LiteralPath $ProjectRoot -File -Filter $pattern -Force -ErrorAction SilentlyContinue)) {
            $filesByPath[$file.FullName.ToLowerInvariant()] = $file
        }
    }

    $inputDirectories = @(
        'src',
        'public',
        'src-tauri\src',
        'src-tauri\capabilities',
        'src-tauri\icons',
        'src-tauri\resources',
        '.cargo',
        'src-tauri\.cargo'
    )
    foreach ($relativeDirectory in $inputDirectories) {
        $fullDirectory = Join-Path $ProjectRoot $relativeDirectory
        if (Test-Path -LiteralPath $fullDirectory -PathType Container) {
            foreach ($file in (Get-ChildItem -LiteralPath $fullDirectory -File -Recurse -Force)) {
                $filesByPath[$file.FullName.ToLowerInvariant()] = $file
            }
        }
    }

    return @($filesByPath.Values | Sort-Object -Property FullName)
}

function Get-ContentFingerprint {
    param([System.IO.FileInfo[]]$Files)

    $entries = foreach ($file in $Files) {
        $relativePath = $file.FullName.Substring($ProjectRoot.Length).TrimStart('\', '/')
        $digest = Get-Sha256Hex -Path $file.FullName
        '{0}={1}' -f $relativePath.ToLowerInvariant(), $digest
    }
    $aggregateBytes = [System.Text.Encoding]::UTF8.GetBytes([string]::Join("`n", $entries))
    $algorithm = [System.Security.Cryptography.SHA256]::Create()
    try {
        return [string]::Join('', ($algorithm.ComputeHash($aggregateBytes) | ForEach-Object { $_.ToString('x2') }))
    }
    finally {
        $algorithm.Dispose()
    }
}

function Get-Sha256Hex {
    param([string]$Path)

    $stream = [System.IO.File]::OpenRead($Path)
    $algorithm = [System.Security.Cryptography.SHA256]::Create()
    try {
        return [string]::Join('', ($algorithm.ComputeHash($stream) | ForEach-Object { $_.ToString('x2') }))
    }
    finally {
        $algorithm.Dispose()
        $stream.Dispose()
    }
}

function Read-State {
    param([string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }
    try {
        return (Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json)
    }
    catch {
        return $null
    }
}

function Write-State {
    param(
        [string]$Path,
        [object]$Value
    )

    $temporaryPath = '{0}.{1}.tmp' -f $Path, [Guid]::NewGuid().ToString('N')
    try {
        $json = ConvertTo-Json -InputObject $Value -Depth 5
        [System.IO.File]::WriteAllText($temporaryPath, $json, [System.Text.Encoding]::UTF8)
        Move-Item -LiteralPath $temporaryPath -Destination $Path -Force
    }
    finally {
        if (Test-Path -LiteralPath $temporaryPath) {
            Remove-Item -LiteralPath $temporaryPath -Force
        }
    }
}

function Find-NodeRuntime {
    $candidates = @()
    if ($env:VIBE_NODE_EXE) { $candidates += $env:VIBE_NODE_EXE }
    if ($env:ProgramFiles) { $candidates += (Join-Path $env:ProgramFiles 'nodejs\node.exe') }
    if ($env:LOCALAPPDATA) { $candidates += (Join-Path $env:LOCALAPPDATA 'Programs\nodejs\node.exe') }
    $pathNode = Get-Command node.exe -ErrorAction SilentlyContinue
    if ($pathNode) { $candidates += $pathNode.Source }
    # Read persisted PATH too: Explorer can retain an older environment after installation.
    foreach ($scope in @('User', 'Machine')) {
        $savedPath = [Environment]::GetEnvironmentVariable('Path', $scope)
        foreach ($entry in ($savedPath -split ';')) {
            if (-not [string]::IsNullOrWhiteSpace($entry)) {
                $directory = [Environment]::ExpandEnvironmentVariables($entry.Trim().Trim('"'))
                $candidates += (Join-Path $directory 'node.exe')
            }
        }
    }

    foreach ($candidate in ($candidates | Select-Object -Unique)) {
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) { continue }
        $fullNodePath = (Resolve-Path -LiteralPath $candidate).Path
        $npmCli = Join-Path (Split-Path -Parent $fullNodePath) 'node_modules\npm\bin\npm-cli.js'
        if (-not (Test-Path -LiteralPath $npmCli -PathType Leaf)) { continue }

        $previousPreference = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $versionText = (& $fullNodePath --version 2>&1 | Out-String).Trim()
            $versionExitCode = $LASTEXITCODE
        }
        finally {
            $ErrorActionPreference = $previousPreference
        }
        if ($versionExitCode -ne 0 -or $versionText -notmatch '^v?(\d+)\.(\d+)\.') {
            continue
        }
        $major = [int]$Matches[1]
        $minor = [int]$Matches[2]
        if (($major -lt 22) -or ($major -eq 22 -and $minor -lt 12)) {
            throw "Node.js 版本需要为 22.12 或更新版本。当前版本：$versionText（路径：$fullNodePath）"
        }

        return [pscustomobject]@{
            NodePath = $fullNodePath
            NpmCli = $npmCli
            BinDirectory = Split-Path -Parent $fullNodePath
            Version = $versionText
        }
    }

    throw '找不到可用的 Node.js 和 npm。请将 Node.js 加入 PATH，或设置 VIBE_NODE_EXE 为 node.exe 的完整路径。'
}

function Find-CargoRuntime {
    $candidates = @()
    if ($env:VIBE_CARGO_EXE) { $candidates += $env:VIBE_CARGO_EXE }
    if ($env:USERPROFILE) { $candidates += (Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe') }
    $pathCargo = Get-Command cargo.exe -ErrorAction SilentlyContinue
    if ($pathCargo) { $candidates += $pathCargo.Source }

    foreach ($candidate in ($candidates | Select-Object -Unique)) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            $fullCargoPath = (Resolve-Path -LiteralPath $candidate).Path
            return [pscustomobject]@{
                CargoPath = $fullCargoPath
                BinDirectory = Split-Path -Parent $fullCargoPath
            }
        }
    }

    throw '找不到 Rust Cargo。请安装 Rust MSVC 工具链，或设置 VIBE_CARGO_EXE。'
}

function Add-BuildLogLine {
    param([string]$Line)

    if ($script:BuildLog) {
        [System.IO.File]::AppendAllText($script:BuildLog, $Line + [Environment]::NewLine, [System.Text.Encoding]::UTF8)
    }
}

function Invoke-LoggedCommand {
    param(
        [string]$Executable,
        [string[]]$Arguments
    )

    $renderedArguments = [string]::Join(' ', $Arguments)
    $header = "> `"$Executable`" $renderedArguments"
    Write-Host $header -ForegroundColor Cyan
    Add-BuildLogLine $header
    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Executable @Arguments 2>&1 | ForEach-Object {
            $line = $_.ToString()
            Write-Host $line
            Add-BuildLogLine $line
        }
        $commandExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousPreference
    }
    Add-BuildLogLine "Exit code: $commandExitCode"
    return [int]$commandExitCode
}

function Import-VisualStudioEnvironment {
    if ((Get-Command link.exe -ErrorAction SilentlyContinue) -and $env:WindowsSdkDir) {
        return
    }

    $vswhereCandidates = @()
    if (${env:ProgramFiles(x86)}) {
        $vswhereCandidates += (Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe')
    }
    if ($env:ProgramFiles) {
        $vswhereCandidates += (Join-Path $env:ProgramFiles 'Microsoft Visual Studio\Installer\vswhere.exe')
    }
    $vswhere = $vswhereCandidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
    if (-not $vswhere) {
        throw '找不到 Visual Studio Installer 的 vswhere.exe；无法为 Rust MSVC 编译器加载 C++ 工具环境。'
    }

    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $installationPath = (& $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>&1 | Out-String).Trim()
        $vswhereExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousPreference
    }
    if ($vswhereExitCode -ne 0 -or -not $installationPath) {
        throw '找不到 Visual Studio C++ 桌面开发工具。请安装 MSVC x64/x86 build tools。'
    }
    $developerCommand = Join-Path $installationPath 'Common7\Tools\VsDevCmd.bat'
    if (-not (Test-Path -LiteralPath $developerCommand -PathType Leaf)) {
        throw "找不到 Visual Studio 开发环境脚本：$developerCommand"
    }

    Write-Host '正在加载 Visual Studio C++ 编译环境…' -ForegroundColor DarkCyan
    $environmentDump = & $env:ComSpec /d /c ('call "{0}" -no_logo -arch=x64 -host_arch=x64 >nul && set' -f $developerCommand) 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw 'Visual Studio C++ 编译环境加载失败。'
    }
    foreach ($line in $environmentDump) {
        $text = [string]$line
        $separator = $text.IndexOf('=')
        if ($separator -le 0) { continue }
        $name = $text.Substring(0, $separator)
        if ($name -notmatch '^[A-Za-z_][A-Za-z0-9_]*$') { continue }
        Set-Item -LiteralPath ("Env:{0}" -f $name) -Value $text.Substring($separator + 1)
    }
    if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        throw 'Visual Studio 环境已加载，但仍找不到 link.exe。'
    }
}

function Find-RunningLauncher {
    $expectedPath = [System.IO.Path]::GetFullPath($ProjectExe)
    $matchingProcesses = @()
    $unresolvedProcesses = @()
    foreach ($process in (Get-Process -Name 'vibecode-project-launcher' -ErrorAction SilentlyContinue)) {
        $actualPath = $null
        try { $actualPath = $process.Path } catch { }
        if (-not $actualPath) {
            try { $actualPath = $process.MainModule.FileName } catch { }
        }
        if (-not $actualPath) {
            $unresolvedProcesses += $process
            continue
        }
        if ([string]::Equals([System.IO.Path]::GetFullPath($actualPath), $expectedPath, [StringComparison]::OrdinalIgnoreCase)) {
            $matchingProcesses += $process
        }
    }
    return [pscustomobject]@{
        Matching = @($matchingProcesses)
        Unresolved = @($unresolvedProcesses)
    }
}

function Start-UserApplication {
    if (-not (Test-Path -LiteralPath $ProjectExe -PathType Leaf)) {
        throw "Release 程序不存在：$ProjectExe"
    }
    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $ProjectExe
    $startInfo.WorkingDirectory = $ProjectRoot
    $startInfo.UseShellExecute = $true
    $startedProcess = [System.Diagnostics.Process]::Start($startInfo)
    if (-not $startedProcess) {
        throw 'Windows 未能启动项目启动器。'
    }
    Write-Host '已启动 Vibecode 项目启动器。' -ForegroundColor Green
}

function Show-UserMessage {
    param(
        [string]$Message,
        [string]$Title,
        [string]$Icon = 'Warning'
    )

    try {
        Add-Type -AssemblyName System.Windows.Forms
        $iconValue = [System.Enum]::Parse([System.Windows.Forms.MessageBoxIcon], $Icon)
        [void][System.Windows.Forms.MessageBox]::Show($Message, $Title, [System.Windows.Forms.MessageBoxButtons]::OK, $iconValue)
    }
    catch {
        Write-Host $Message -ForegroundColor Yellow
    }
}

try {
    if (-not (Test-Path -LiteralPath $StateRoot -PathType Container)) {
        New-Item -ItemType Directory -Path $StateRoot -Force | Out-Null
    }
    if (-not (Test-Path -LiteralPath $ProjectStateRoot -PathType Container)) {
        New-Item -ItemType Directory -Path $ProjectStateRoot -Force | Out-Null
    }

    $mutexName = 'Local\VibecodeProjectLauncher-{0}' -f $ProjectKey.Substring(0, 24)
    $Mutex = [System.Threading.Mutex]::new($false, $mutexName)
    try {
        $HasMutex = $Mutex.WaitOne(0)
    }
    catch [System.Threading.AbandonedMutexException] {
        $HasMutex = $true
    }
    if (-not $HasMutex) {
        Show-UserMessage -Message '已有一个启动或构建任务在运行。请等待它完成，再使用桌面快捷方式。' -Title 'Vibecode 项目启动器正在运行'
        $ExitCode = 2
    }
    else {
        $inputFiles = Get-InputFiles
        $inputFingerprint = Get-ContentFingerprint -Files $inputFiles
        $buildState = Read-State -Path $BuildStatePath
        $savedInputFingerprint = $null
        $savedExecutableFingerprint = $null
        if ($buildState -and $buildState.PSObject.Properties['InputFingerprint']) {
            $savedInputFingerprint = $buildState.InputFingerprint
        }
        if ($buildState -and $buildState.PSObject.Properties['ExecutableFingerprint']) {
            $savedExecutableFingerprint = $buildState.ExecutableFingerprint
        }
        $currentExecutableFingerprint = $null
        if (Test-Path -LiteralPath $ProjectExe -PathType Leaf) {
            $currentExecutableFingerprint = Get-Sha256Hex -Path $ProjectExe
        }
        $needsBuild = (-not $currentExecutableFingerprint) -or ($savedInputFingerprint -ne $inputFingerprint) -or ($savedExecutableFingerprint -ne $currentExecutableFingerprint)
        $runningLauncher = Find-RunningLauncher

        if ($runningLauncher.Matching.Count -gt 0 -or $runningLauncher.Unresolved.Count -gt 0) {
            if ($needsBuild) {
                Show-UserMessage -Message '检测到源码或构建配置已有更新，但项目启动器仍在运行。请先从窗口正常退出，再双击桌面图标重新构建。脚本不会关闭它或它启动的项目服务。' -Title '请先退出 Vibecode 项目启动器'
                $ExitCode = 3
            }
            else {
                $ExitCode = 0
            }
        }
        elseif (-not $needsBuild) {
            Write-Host '源码与构建配置未变化，直接启动当前 release。' -ForegroundColor DarkCyan
            Start-UserApplication
        }
        else {
            $logDirectory = Join-Path $StateRoot 'logs'
            if (-not (Test-Path -LiteralPath $logDirectory -PathType Container)) {
                New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null
            }
            $BuildLog = Join-Path $logDirectory ('build-{0}.log' -f (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
            [System.IO.File]::WriteAllText($BuildLog, "Vibecode Project Launcher build`r`nProject: $ProjectRoot`r`nStarted: $([DateTime]::Now.ToString('o'))`r`n", [System.Text.Encoding]::UTF8)
            Write-Host "构建日志：$BuildLog" -ForegroundColor DarkGray
            Add-BuildLogLine ("输入文件数：{0}" -f $inputFiles.Count)
            foreach ($file in $inputFiles) {
                Add-BuildLogLine ('  {0}' -f $file.FullName.Substring($ProjectRoot.Length).TrimStart('\', '/'))
            }

            $nodeRuntime = Find-NodeRuntime
            $cargoRuntime = Find-CargoRuntime
            Write-Host ("Node.js {0}：{1}" -f $nodeRuntime.Version, $nodeRuntime.NodePath) -ForegroundColor DarkGray
            Write-Host ("Cargo：{0}" -f $cargoRuntime.CargoPath) -ForegroundColor DarkGray
            Add-BuildLogLine ("Node.js: {0} ({1})" -f $nodeRuntime.Version, $nodeRuntime.NodePath)
            Add-BuildLogLine ("Cargo: {0}" -f $cargoRuntime.CargoPath)
            $env:Path = '{0};{1};{2}' -f $nodeRuntime.BinDirectory, $cargoRuntime.BinDirectory, $env:Path
            Import-VisualStudioEnvironment

            $dependencyFiles = @(
                (Get-Item -LiteralPath (Join-Path $ProjectRoot 'package.json')),
                (Get-Item -LiteralPath (Join-Path $ProjectRoot 'package-lock.json'))
            )
            $dependencyFingerprint = Get-ContentFingerprint -Files $dependencyFiles
            $dependencyState = Read-State -Path $DependencyStatePath
            $installedDependencyFingerprint = $null
            if ($dependencyState -and $dependencyState.PSObject.Properties['DependencyFingerprint']) {
                $installedDependencyFingerprint = $dependencyState.DependencyFingerprint
            }
            $nodeModules = Join-Path $ProjectRoot 'node_modules'
            $needsNpmCi = (-not (Test-Path -LiteralPath $nodeModules -PathType Container)) -or ($installedDependencyFingerprint -ne $dependencyFingerprint)

            if ($needsNpmCi) {
                Write-Host '检测到依赖未安装或 package/lockfile 已变化，运行 npm ci…' -ForegroundColor DarkCyan
                $npmExitCode = Invoke-LoggedCommand -Executable $nodeRuntime.NodePath -Arguments @($nodeRuntime.NpmCli, 'ci')
                if ($npmExitCode -ne 0) {
                    throw "npm ci 失败，退出码 $npmExitCode。"
                }
                $dependencyFingerprintAfterInstall = Get-ContentFingerprint -Files $dependencyFiles
                if ($dependencyFingerprintAfterInstall -ne $dependencyFingerprint) {
                    throw 'npm ci 期间 package.json 或 package-lock.json 发生变化；请确认文件后再次双击。'
                }
                Write-State -Path $DependencyStatePath -Value ([pscustomobject]@{
                    DependencyFingerprint = $dependencyFingerprint
                    InstalledAt = [DateTime]::UtcNow.ToString('o')
                })
            }
            else {
                Write-Host '依赖文件未变化，跳过 npm ci。' -ForegroundColor DarkCyan
            }

            Write-Host '正在构建最新 release…' -ForegroundColor DarkCyan
            $buildExitCode = Invoke-LoggedCommand -Executable $nodeRuntime.NodePath -Arguments @($nodeRuntime.NpmCli, 'run', 'desktop:build')
            if ($buildExitCode -ne 0) {
                throw "release 构建失败，退出码 $buildExitCode。"
            }
            if (-not (Test-Path -LiteralPath $ProjectExe -PathType Leaf)) {
                throw "构建命令已结束，但未生成预期程序：$ProjectExe"
            }
            $inputFingerprintAfterBuild = Get-ContentFingerprint -Files (Get-InputFiles)
            if ($inputFingerprintAfterBuild -ne $inputFingerprint) {
                throw '构建期间源码或配置发生变化；为避免启动不完整版本，请保存变更后再次双击。'
            }
            $executableFingerprintAfterBuild = Get-Sha256Hex -Path $ProjectExe

            Write-State -Path $BuildStatePath -Value ([pscustomobject]@{
                InputFingerprint = $inputFingerprint
                ExecutableFingerprint = $executableFingerprintAfterBuild
                BuiltAt = [DateTime]::UtcNow.ToString('o')
                Executable = $ProjectExe
            })
            Write-Host 'Release 构建成功。' -ForegroundColor Green
            Start-UserApplication
        }
    }
}
catch {
    $ExitCode = 1
    Write-Host ''
    Write-Host ("失败：{0}" -f $_.Exception.Message) -ForegroundColor Red
    if ($BuildLog) {
        Add-BuildLogLine ("FAILED: {0}" -f $_.Exception.ToString())
    }
    $dialogMessage = "未能启动最新版本。`r`n`r`n错误：$($_.Exception.Message)"
    if ($BuildLog) {
        $recentOutput = (Get-Content -LiteralPath $BuildLog -Tail 22 -ErrorAction SilentlyContinue | Out-String).Trim()
        $dialogMessage += "`r`n`r`n构建日志：$BuildLog"
        if ($recentOutput) { $dialogMessage += "`r`n`r`n最近输出：`r`n$recentOutput" }
    }
    Show-UserMessage -Message $dialogMessage -Title 'Vibecode 项目启动器出错' -Icon 'Error'
}
finally {
    if ($HasMutex -and $Mutex) {
        try { $Mutex.ReleaseMutex() } catch { }
    }
    if ($Mutex) { $Mutex.Dispose() }
}

exit $ExitCode

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProjectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$LauncherScript = Join-Path $ProjectRoot 'scripts\launch-latest.ps1'
if (-not (Test-Path -LiteralPath $LauncherScript -PathType Leaf)) {
    throw "找不到桌面启动脚本：$LauncherScript"
}

$DesktopDirectory = [Environment]::GetFolderPath([Environment+SpecialFolder]::DesktopDirectory)
if (-not $DesktopDirectory) {
    throw 'Windows 未能定位当前用户的桌面目录。'
}
$ShortcutPath = Join-Path $DesktopDirectory 'Vibecode 项目启动器.lnk'
$PowerShellExe = Join-Path $PSHOME 'powershell.exe'
if (-not (Test-Path -LiteralPath $PowerShellExe -PathType Leaf)) {
    $PowerShellExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
}
if (-not (Test-Path -LiteralPath $PowerShellExe -PathType Leaf)) {
    throw '找不到 Windows PowerShell 5.1。'
}

$Shell = New-Object -ComObject WScript.Shell
$Shortcut = $Shell.CreateShortcut($ShortcutPath)
$Shortcut.TargetPath = $PowerShellExe
$Shortcut.Arguments = '-WindowStyle Hidden -NoLogo -NoProfile -ExecutionPolicy Bypass -File "{0}"' -f $LauncherScript
$Shortcut.WorkingDirectory = $ProjectRoot
$Shortcut.Description = '自动构建并打开本地最新的 Vibecode 项目启动器'
$Shortcut.WindowStyle = 0
$ReleaseExe = Join-Path $ProjectRoot 'src-tauri\target\release\vibecode-project-launcher.exe'
if (Test-Path -LiteralPath $ReleaseExe -PathType Leaf) {
    $Shortcut.IconLocation = '{0},0' -f $ReleaseExe
}
else {
    $Shortcut.IconLocation = Join-Path $env:SystemRoot 'System32\shell32.dll,215'
}
$Shortcut.Save()

Write-Host "已在桌面创建快捷方式：$ShortcutPath" -ForegroundColor Green
Write-Host "快捷方式指向源码启动脚本：$LauncherScript"

# Builds the Windows installer: release binaries + settings app + NSIS bundle.
# Output: target\release\bundle\nsis\Mimic_<version>_x64-setup.exe
#
# Incremental: Rust builds reuse target\, and the UI's npm dependencies are only (re)installed
# when package-lock.json changed (never wiped, so a dev server or editor holding a file in
# node_modules can't break the build). Every step stops the script on failure.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# Native tools (cargo, npm, tauri) print progress on stderr; Windows PowerShell would turn that
# into a terminating error under "Stop". Run them under "Continue" and judge by exit code.
function Step([string]$what, [scriptblock]$run) {
    Write-Host "== $what" -ForegroundColor DarkYellow
    $global:LASTEXITCODE = 0
    $saved = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try { & $run } finally { $ErrorActionPreference = $saved }
    if ($LASTEXITCODE) { throw "$what failed (exit $LASTEXITCODE)" }
}

Step "cargo build (daemon, CLI)" { cargo build --release -p mimic-daemon -p mimic-cli }

$triple = "x86_64-pc-windows-msvc"
$bin = "ui\src-tauri\binaries"
New-Item -ItemType Directory -Force $bin | Out-Null
Copy-Item target\release\mimic-daemon.exe "$bin\mimic-daemon-$triple.exe" -Force
Copy-Item target\release\mimic.exe "$bin\mimic-$triple.exe" -Force

Set-Location ui
$lock = Get-Item package-lock.json
$stamp = "node_modules\.package-lock.json"
if (-not (Test-Path $stamp) -or (Get-Item $stamp).LastWriteTime -lt $lock.LastWriteTime -or -not (Test-Path "node_modules\.bin\vite.cmd")) {
    Step "npm install" { npm install --no-audit --no-fund }
}
Step "UI build" { npm run build }
Step "tauri bundle" { npx tauri build --config src-tauri/tauri.bundle.conf.json }
Set-Location $root
$exe = Get-ChildItem "target\release\bundle\nsis\*.exe" | Sort-Object LastWriteTime | Select-Object -Last 1
"built $($exe.FullName) ($([math]::Round($exe.Length / 1MB, 1)) MB)"

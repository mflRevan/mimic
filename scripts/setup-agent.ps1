# Sets up the agent's local runtimes and models under %LOCALAPPDATA%\Mimic:
#   runtime\llama   llama.cpp server (CUDA build)                          -> the local brain
#   runtime\drama   drama-rs built for your GPU, with Mimic's int4-offload patch -> the voice
#   models\gemma-4-e4b  ggml-org/gemma-4-E4B-it-GGUF (Q8_0, ~8 GB)
#   models\drama-3-q4  apiplant/drama-q4 (Drama 3, int4 experts, ~22 GB)
# The small voice models (VAD, end of turn, speaker embeddings, ~30 MB) download on first use.
#
# Needs: the `hf` CLI (pip install huggingface_hub), git, Rust, the CUDA toolkit (12.8+ for RTX 50xx)
# and the Visual Studio C++ build tools. Re-running skips what is already there.
#
#   powershell -File scripts\setup-agent.ps1 [-CudaArch sm_120] [-LlamaBuild b12110]

param(
    [string]$CudaArch = "sm_120",
    [string]$LlamaBuild = "b12110",
    [string]$CudaVersion = "13.4"
)
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$root = Join-Path $env:LOCALAPPDATA "Mimic"
$runtime = Join-Path $root "runtime"
$models = Join-Path $root "models"
New-Item -ItemType Directory -Force $runtime, $models | Out-Null

# --- llama.cpp ---------------------------------------------------------------
$llama = Join-Path $runtime "llama"
if (-not (Test-Path (Join-Path $llama "llama-server.exe"))) {
    New-Item -ItemType Directory -Force $llama | Out-Null
    foreach ($zip in @("llama-$LlamaBuild-bin-win-cuda-$CudaVersion-x64.zip", "cudart-llama-bin-win-cuda-$CudaVersion-x64.zip")) {
        $tmp = Join-Path $env:TEMP $zip
        Write-Host "downloading $zip"
        Invoke-WebRequest "https://github.com/ggml-org/llama.cpp/releases/download/$LlamaBuild/$zip" -OutFile $tmp
        Expand-Archive $tmp -DestinationPath $llama -Force
        Remove-Item $tmp
    }
}
& (Join-Path $llama "llama-server.exe") --version

# --- models ------------------------------------------------------------------
$brain = Join-Path $models "gemma-4-e4b"
if (-not (Test-Path (Join-Path $brain "gemma-4-E4B-it-Q8_0.gguf"))) {
    hf download ggml-org/gemma-4-E4B-it-GGUF gemma-4-E4B-it-Q8_0.gguf --local-dir $brain
}
$drama = Join-Path $models "drama-3-q4"
if (-not (Test-Path (Join-Path $drama "drama.json"))) {
    hf download apiplant/drama-q4 --local-dir $drama
}

# --- drama-rs (+ int4 expert offload: DRAMA_Q4_VRAM_LAYERS) -----------------
$dramaBin = Join-Path $runtime "drama\drama.exe"
if (-not (Test-Path $dramaBin)) {
    $src = Join-Path $root "build\drama-rs"
    if (-not (Test-Path $src)) {
        git clone https://github.com/apiplant/drama-rs $src
    }
    Push-Location $src
    git apply --check (Join-Path $repo "scripts\drama-rs-q4-offload.patch") 2>$null
    if ($LASTEXITCODE -eq 0) { git apply (Join-Path $repo "scripts\drama-rs-q4-offload.patch") }
    Pop-Location
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    $vs = & $vswhere -latest -products * -property installationPath
    $cuda = (Get-ChildItem "$env:ProgramFiles\NVIDIA GPU Computing Toolkit\CUDA" | Sort-Object Name | Select-Object -Last 1).FullName
    $cmd = "call `"$vs\VC\Auxiliary\Build\vcvars64.bat`" >nul && set `"CUDA_PATH=$cuda`" && set DRAMA_CUDA_ARCH=$CudaArch && cd /d `"$src`" && cargo build --release"
    cmd /c $cmd
    if ($LASTEXITCODE -ne 0) { throw "building drama-rs failed" }
    New-Item -ItemType Directory -Force (Split-Path $dramaBin) | Out-Null
    Copy-Item (Join-Path $src "target\release\drama.exe") $dramaBin
}
Write-Host "agent runtimes ready in $root"

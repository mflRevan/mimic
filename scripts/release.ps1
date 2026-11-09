# Publishes a release: version bump, tests, installer, tag, GitHub release with the installer.
# Running Mimics pick it up by themselves (general.auto_update) or with `mimic update`.
#
#   powershell -File scripts\release.ps1                 next patch version (0.3.0 -> 0.3.1)
#   powershell -File scripts\release.ps1 -Version 0.4.0
#   ... -SkipTests        skip `cargo test`
#   ... -Update           afterwards, update this PC through the app's own updater
#
# Needs a clean working tree on main and the GitHub CLI logged in (`gh auth login`).
param(
    [string]$Version = "",
    [switch]$SkipTests,
    [switch]$Update
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$t0 = Get-Date

function Step([string]$what, [scriptblock]$run) {
    Write-Host "== $what" -ForegroundColor DarkYellow
    & $run
    if ($LASTEXITCODE) { throw "$what failed (exit $LASTEXITCODE)" }
}

# --- preconditions -----------------------------------------------------------
if ((git rev-parse --abbrev-ref HEAD) -ne "main") { throw "release from main" }
if (git status --porcelain) { throw "the working tree has changes; commit them first" }
gh auth status 2>&1 | Out-Null
if ($LASTEXITCODE) { throw "the GitHub CLI isn't logged in (gh auth login)" }
$repo = gh repo view --json nameWithOwner -q .nameWithOwner

$current = ([regex]'(?m)^version = "([^"]+)"').Match((Get-Content Cargo.toml -Raw)).Groups[1].Value
if (-not $Version) {
    $p = $current.Split("-")[0].Split(".")
    $Version = "$($p[0]).$($p[1]).$([int]$p[2] + 1)"
}
$tag = "v$Version"
if (git tag -l $tag) { throw "$tag already exists" }
Write-Host "releasing $tag (from $current) to $repo" -ForegroundColor Cyan

# --- build -------------------------------------------------------------------
if ($Version -ne $current) {
    Step "bump version" { powershell -NoProfile -File scripts\bump-version.ps1 $Version }
}
if (-not $SkipTests) {
    Step "tests" { cargo test --release --workspace -q }
}
Step "installer" { powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build-installer.ps1 }
$exe = Get-Item "target\release\bundle\nsis\Mimic_${Version}_x64-setup.exe"

# --- publish -----------------------------------------------------------------
$prev = git describe --tags --abbrev=0 2>$null
$range = if ($prev) { "$prev..HEAD" } else { "HEAD" }
$notes = (git log $range --no-merges --pretty="- %s" | Where-Object { $_ -notmatch '^- Release v' }) -join "`n"
if (-not $notes) { $notes = "- Maintenance" }
$notesFile = Join-Path $env:TEMP "mimic-release-notes.md"
[IO.File]::WriteAllText($notesFile, $notes)

if ($Version -ne $current) {
    Step "commit" { git commit -q -am "Release $tag" }
}
Step "tag" { git tag -a $tag -m "Mimic $Version" }
Step "push" { git push -q origin main $tag }
Step "GitHub release" { gh release create $tag $exe.FullName --repo $repo --title "Mimic $Version" --notes-file $notesFile }

# GitHub computes the asset's SHA-256, which the updater verifies against.
$digest = gh release view $tag --repo $repo --json assets -q '.assets[0].digest'
$local = "sha256:" + (Get-FileHash $exe.FullName -Algorithm SHA256).Hash.ToLower()
if ($digest -ne $local) { throw "the uploaded installer's checksum ($digest) doesn't match the local one ($local)" }
Write-Host ("published {0} in {1:n0} s: {2}" -f $tag, ((Get-Date) - $t0).TotalSeconds, "https://github.com/$repo/releases/tag/$tag") -ForegroundColor Green

if ($Update) {
    $mimic = Join-Path $env:LOCALAPPDATA "Mimic\mimic.exe"
    if (Test-Path $mimic) {
        Step "update this PC" { & $mimic update --install }
    }
}

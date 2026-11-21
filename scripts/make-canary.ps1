<#
.SYNOPSIS
    Creates the canary project for the trust gate test (docs/trust-gate-canary-test.md).

.DESCRIPTION
    Builds I:\canary-test (or -Root) with a markers folder and four canaries: Rust,
    npm, Python and .NET. Every canary does exactly one thing when something runs
    it: create an EMPTY marker file in the markers folder. None of them reads,
    changes or deletes anything else.

      Rust    .cargo\config.toml names rustc-wrapper.cmd, which creates
              cargo-wrapper.marker and exits. Cargo runs the wrapper when it
              compiles or checks (the Rust panel's Check button), not for
              `cargo metadata --no-deps`, which is what the panel's scan uses.
      npm     package.json has a preinstall script that creates
              npm-preinstall.marker. It runs on npm install, not on npm ls/outdated.
      Python  .venv\Scripts\python.exe is a tiny program (compiled with rustc) that
              creates python-stub.marker, and .venv\pyvenv.cfg is empty.
      dotnet  Directory.Build.targets has an Exec target (run as an initial target)
              that creates dotnet-msbuild.marker whenever MSBuild builds, restores
              or lists packages for Canary.csproj.

    The script only writes files under -Root. It never deletes anything, except the
    *.marker files in the markers folder when you pass -ClearMarkers. It does run
    `rustc` (to compile the Python stub) and `dotnet --version` (to pick the target
    framework), both from your temp folder, never from the canary.

.PARAMETER Root
    Where to build it. Default E:\canary-test. Pick a folder whose parents are not
    trusted in Forge Super.

.PARAMETER ClearMarkers
    Delete the *.marker files already in the markers folder first.

.PARAMETER TargetFramework
    Target framework for Canary.csproj. Default: the installed SDK's (net<major>.0),
    or net8.0 when dotnet cannot be found.

.EXAMPLE
    .\scripts\make-canary.ps1
    .\scripts\make-canary.ps1 -ClearMarkers
#>
[CmdletBinding()]
param(
    [string]$Root = 'I:\canary-test',
    [switch]$ClearMarkers,
    [string]$TargetFramework
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ([System.Environment]::OSVersion.Platform -ne 'Win32NT') {
    throw 'make-canary.ps1 builds the Windows canary (.cmd wrapper, python.exe stub). Run it on Windows.'
}
if ($Root -match "['`"&%^<>|;]") {
    throw "The root path must not contain quotes or shell characters: $Root"
}
$Root = [System.IO.Path]::GetFullPath($Root).TrimEnd('\')
if ($Root.Length -le 3) {
    throw "Refusing to use a drive root as the canary folder: $Root"
}

$markers = Join-Path $Root 'markers'
$project = Join-Path $Root 'project'
$dotnet = Join-Path $project 'dotnet'
$venv = Join-Path $project '.venv'

$markerFiles = [ordered]@{
    cargo  = Join-Path $markers 'cargo-wrapper.marker'
    npm    = Join-Path $markers 'npm-preinstall.marker'
    python = Join-Path $markers 'python-stub.marker'
    dotnet = Join-Path $markers 'dotnet-msbuild.marker'
}

function Write-File([string]$Path, [string]$Text) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
    # UTF-8 without a byte order mark: Cargo and npm do not like one.
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding($false)))
    Write-Host ("  wrote {0}" -f $Path.Substring($Root.Length).TrimStart('\'))
}

function ToForwardSlashes([string]$Path) { $Path.Replace('\', '/') }

Write-Host "Canary project: $Root"

# --- Folders ----------------------------------------------------------------
foreach ($folder in @($Root, $markers, $project, (Join-Path $project 'src'), (Join-Path $project '.cargo'), (Join-Path $venv 'Scripts'), $dotnet)) {
    New-Item -ItemType Directory -Force -Path $folder | Out-Null
}
$existing = @(Get-ChildItem -Path $markers -Filter '*.marker' -File -ErrorAction SilentlyContinue)
if ($ClearMarkers -and $existing.Count -gt 0) {
    $existing | Remove-Item -Force
    Write-Host ("  cleared {0} old marker file(s)" -f $existing.Count)
    $existing = @()
}
if ($existing.Count -gt 0) {
    Write-Warning ("The markers folder already holds {0} marker file(s). Pass -ClearMarkers for a clean start." -f $existing.Count)
}

# --- Rust: a compiler wrapper that only creates a marker -----------------------
Write-Host 'Rust canary'
Write-File (Join-Path $project 'Cargo.toml') @"
[package]
name = "canary"
version = "0.1.0"
edition = "2021"
"@
Write-File (Join-Path $project 'src\main.rs') "fn main() {}`n"
$wrapper = Join-Path $Root 'rustc-wrapper.cmd'
Write-File $wrapper @"
@echo off
type nul > "$($markerFiles.cargo)"
exit /b 1
"@
Write-File (Join-Path $project '.cargo\config.toml') @"
[build]
rustc-wrapper = "$(ToForwardSlashes $wrapper)"
"@

# --- npm: an install script that only creates a marker -------------------------
Write-Host 'npm canary'
$markerForNode = ToForwardSlashes $markerFiles.npm
$preinstall = "node -e `"require('fs').closeSync(require('fs').openSync('$markerForNode','w'))`""
$package = [ordered]@{
    name    = 'canary'
    version = '1.0.0'
    scripts = [ordered]@{ preinstall = $preinstall }
}
Write-File (Join-Path $project 'package.json') (($package | ConvertTo-Json -Depth 5) + "`n")

# --- Python: a virtual environment whose interpreter only creates a marker -------
Write-Host 'Python canary'
Write-File (Join-Path $project 'requirements.txt') ''
Write-File (Join-Path $venv 'pyvenv.cfg') ''
$stubSource = Join-Path $Root 'canary-python.rs'
Write-File $stubSource @"
fn main() {
    let _ = std::fs::File::create(r"$($markerFiles.python)");
}
"@
$stubExe = Join-Path $venv 'Scripts\python.exe'
if (Get-Command rustc -ErrorAction SilentlyContinue) {
    Push-Location $env:TEMP
    try {
        & rustc --edition 2021 -C debuginfo=0 -C opt-level=0 $stubSource -o $stubExe 2>&1 | ForEach-Object { Write-Host "  rustc: $_" }
    }
    finally {
        Pop-Location
    }
}
if (Test-Path $stubExe) {
    Write-Host '  built .venv\Scripts\python.exe'
}
else {
    Write-Warning ("The Python stub was NOT built (rustc not found or it failed). The Python part of the test will not run. Install Rust, or compile {0} yourself to {1}." -f $stubSource, $stubExe)
}

# --- .NET: an MSBuild Exec target that only creates a marker ---------------------
Write-Host '.NET canary'
if (-not $TargetFramework) {
    $TargetFramework = 'net8.0'
    if (Get-Command dotnet -ErrorAction SilentlyContinue) {
        Push-Location $env:TEMP
        try {
            $version = (& dotnet --version 2>$null | Select-Object -First 1)
            if ($version -match '^(\d+)\.') { $TargetFramework = "net$($Matches[1]).0" }
        }
        catch { }
        finally { Pop-Location }
    }
}
Write-File (Join-Path $dotnet 'Canary.csproj') @"
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Library</OutputType>
    <TargetFramework>$TargetFramework</TargetFramework>
  </PropertyGroup>
</Project>
"@
$execCommand = [System.Security.SecurityElement]::Escape("type nul > `"$($markerFiles.dotnet)`"")
Write-File (Join-Path $dotnet 'Directory.Build.targets') @"
<Project InitialTargets="CanaryMarker">
  <!-- Canary: the only thing this does is create an empty marker file. -->
  <Target Name="CanaryMarker">
    <Exec Command="$execCommand" IgnoreExitCode="true" StandardOutputImportance="low" />
  </Target>
</Project>
"@

# --- Done ----------------------------------------------------------------------
Write-Host ''
Write-Host 'Done. Every canary only creates an empty marker file in:'
Write-Host "  $markers"
Write-Host ''
Write-Host 'Next:'
Write-Host "  1. Start the process watcher:  .\scripts\watch-processes.ps1 -LogPath $Root\process-log.txt"
Write-Host '  2. In Forge Super run "workspace: clear trusted worktrees" and restart.'
Write-Host "  3. Open $project and choose Stay in Restricted Mode."
Write-Host "  4. Wait two minutes, then check that $markers is empty."
Write-Host '  5. Trust the project and watch the markers appear (see docs\trust-gate-canary-test.md).'
Write-Host ''
Write-Host 'Markers: python-stub and dotnet-msbuild come from the panels scans once the project is trusted;'
Write-Host 'cargo-wrapper only when you click Check in the Rust panel; npm-preinstall only when you click Install.'
Write-Host 'Evidence: the markers, then the forge_trust_gate lines in the Forge log. The process watcher can miss short processes.'

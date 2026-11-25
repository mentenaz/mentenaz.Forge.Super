<#
.SYNOPSIS
  Makes a draft "package" of a TypeScript service library, in a NEW folder, so that
  Forge Super's Edit solution > Packages page can import it into an SPFx solution.

.DESCRIPTION
  Reads the library at -Source (a folder with a package.json and a src folder). Writes
  a package to -Out: a manifest.json, a package.json of what the package needs, and the
  source files the manifest names. The original is never changed, and -Out must not
  exist yet. Nothing is sent anywhere: this only reads and writes on this machine.

  The draft is a starting point. Open manifest.json afterwards and check:
    * which folders are "core" (copied every time) and which are "modules" (chosen);
    * the module titles; and the notes shown after the import.

  Rules it applies, from the engine's package guide (docs/import-packages.md):
    * Files at the top of src (ctx.ts, errors.ts...) and the folders named by
      -CoreFolders are the CORE. Every other folder under src is a MODULE.
    * A root index.ts that re-exports every module is NOT copied: a package imported
      with only some modules cannot have it. Each module's own index.ts is kept.
    * Tests, build output and lock files are never copied (only src is read).
    * In package.json: @microsoft/* packages move from dependencies to
      peerDependencies (the solution supplies them), typescript and @types/* move to
      devDependencies (the package author's own tools; the import never adds them).

.PARAMETER Source
  The library folder (it has package.json and src).
.PARAMETER Out
  The new folder to write the package to. Must not exist.
.PARAMETER CoreFolders
  Folder names under src that belong to the core, for example ServiceTypes.
.PARAMETER Id
  The package id. Default: made from the name in package.json.
.PARAMETER Title
  The package title. Default: the name in package.json.

.EXAMPLE
  pwsh -File script\make-package-draft.ps1 -Source D:\libs\my-services -Out D:\packages\my-services -CoreFolders types
#>
param(
    [Parameter(Mandatory)] [string] $Source,
    [Parameter(Mandatory)] [string] $Out,
    [string[]] $CoreFolders = @(),
    [string] $Id,
    [string] $Title
)

$ErrorActionPreference = 'Stop'

$Source = (Resolve-Path -LiteralPath $Source).Path
$srcDir = Join-Path $Source 'src'
$pkgFile = Join-Path $Source 'package.json'
if (-not (Test-Path -LiteralPath $pkgFile)) { throw "No package.json in $Source" }
if (-not (Test-Path -LiteralPath $srcDir)) { throw "No src folder in $Source" }
if (Test-Path -LiteralPath $Out) { throw "$Out already exists. Choose a new folder: nothing is overwritten." }

$pkg = Get-Content -LiteralPath $pkgFile -Raw | ConvertFrom-Json

# The id: lower case, a scope or a slash becomes a hyphen.
if (-not $Id) {
    $base = if ($pkg.name) { [string]$pkg.name } else { Split-Path -Leaf $Source }
    $Id = ($base.ToLowerInvariant() -replace '^@', '' -replace '[^a-z0-9]+', '-').Trim('-')
}
if (-not $Title) { $Title = if ($pkg.name) { [string]$pkg.name } else { $Id } }
$version = if ($pkg.version) { [string]$pkg.version } else { '1.0.0' }

# --- core and modules, from the top level of src ---
$coreFiles = @(Get-ChildItem -LiteralPath $srcDir -File | Where-Object { $_.Extension -in '.ts', '.tsx' -and $_.Name -ne 'index.ts' } | ForEach-Object { $_.Name })
$skipped = @()
if (Test-Path -LiteralPath (Join-Path $srcDir 'index.ts')) { $skipped += 'index.ts (a root file that re-exports every module is not copied)' }
$coreFolderSet = @($CoreFolders | ForEach-Object { $_.ToLowerInvariant() })
$folders = @(Get-ChildItem -LiteralPath $srcDir -Directory | Where-Object { $_.Name -notin 'tests', 'test', '__tests__', 'node_modules', 'dist', 'lib' })
$coreFolderNames = @($folders | Where-Object { $coreFolderSet -contains $_.Name.ToLowerInvariant() } | ForEach-Object { $_.Name })
$moduleFolders = @($folders | Where-Object { $coreFolderSet -notcontains $_.Name.ToLowerInvariant() })
foreach ($asked in $CoreFolders) {
    if ($coreFolderNames -notcontains $asked -and -not ($coreFolderNames | Where-Object { $_ -ieq $asked })) {
        Write-Warning "-CoreFolders names '$asked', but there is no such folder under src."
    }
}

# --- what the code imports, to find the SPFx packages nobody declared ---
$declared = @{}
foreach ($section in 'dependencies', 'peerDependencies', 'devDependencies') {
    if ($pkg.$section) { $pkg.$section.PSObject.Properties | ForEach-Object { $declared[$_.Name] = $true } }
}
$imported = @{}
$importPattern = '(?:from|import|require)\s*\(?\s*["'']([^"'']+)["'']'
Get-ChildItem -LiteralPath $srcDir -Recurse -File -Include *.ts, *.tsx, *.js | ForEach-Object {
    foreach ($m in [regex]::Matches((Get-Content -LiteralPath $_.FullName -Raw), $importPattern)) {
        $spec = $m.Groups[1].Value
        if ($spec.StartsWith('.') -or $spec.StartsWith('/') -or $spec.StartsWith('node:')) { continue }
        $name = if ($spec.StartsWith('@')) { ($spec -split '/')[0..1] -join '/' } else { ($spec -split '/')[0] }
        $imported[$name] = $true
    }
}
$undeclared = @($imported.Keys | Where-Object { -not $declared.ContainsKey($_) } | Sort-Object)

# --- the package.json: needs only ---
$dependencies = [ordered]@{}
$peers = [ordered]@{}
$dev = [ordered]@{}
if ($pkg.dependencies) {
    foreach ($p in $pkg.dependencies.PSObject.Properties) {
        if ($p.Name -like '@microsoft/*') { $peers[$p.Name] = $p.Value }
        elseif ($p.Name -eq 'typescript' -or $p.Name -like '@types/*') { $dev[$p.Name] = $p.Value }
        else { $dependencies[$p.Name] = $p.Value }
    }
}
if ($pkg.peerDependencies) { foreach ($p in $pkg.peerDependencies.PSObject.Properties) { $peers[$p.Name] = $p.Value } }
if ($pkg.devDependencies) { foreach ($p in $pkg.devDependencies.PSObject.Properties) { if (-not $dev.Contains($p.Name)) { $dev[$p.Name] = $p.Value } } }
# SPFx packages that the code imports but nobody declared: the solution supplies them.
$undeclaredOther = @()
foreach ($name in $undeclared) {
    if ($name -like '@microsoft/*') { $peers[$name] = '*' }
    else { $undeclaredOther += $name }
}

$newPkg = [ordered]@{
    name    = $Id
    version = $version
}
# An empty description is left out: a manifest does not take an empty one.
if ($pkg.description) { $newPkg['description'] = [string]$pkg.description }
if ($dependencies.Count) { $newPkg['dependencies'] = $dependencies }
if ($peers.Count) { $newPkg['peerDependencies'] = $peers }
if ($dev.Count) { $newPkg['devDependencies'] = $dev }

# --- the manifest ---
$modules = @($moduleFolders | ForEach-Object {
    [ordered]@{ name = $_.Name; title = (($_.Name -creplace '([a-z0-9])([A-Z])', '$1 $2')); paths = @($_.Name) }
})
$manifest = [ordered]@{
    id      = $Id
    version = $version
    title   = $Title
}
if ($pkg.description) { $manifest['description'] = [string]$pkg.description }
$manifest['tags'] = @('spfx', 'service')
$manifest['prompts'] = @()
$manifest['import'] = [ordered]@{
    kind    = 'service'
    root    = 'src'
    core    = @($coreFiles + $coreFolderNames)
    modules = $modules
    notes   = @("Import from a module folder, for example: import { ... } from './services/$Id/<Module>';")
}

# --- write, to the NEW folder only ---
New-Item -ItemType Directory -Path $Out | Out-Null
$outSrc = Join-Path $Out 'src'
New-Item -ItemType Directory -Path $outSrc | Out-Null
foreach ($f in $coreFiles) { Copy-Item -LiteralPath (Join-Path $srcDir $f) -Destination $outSrc }
foreach ($d in @($coreFolderNames + ($moduleFolders | ForEach-Object { $_.Name }))) {
    Copy-Item -LiteralPath (Join-Path $srcDir $d) -Destination (Join-Path $outSrc $d) -Recurse
}
# Test files that sit beside the source are left out as well.
Get-ChildItem -LiteralPath $outSrc -Recurse -File | Where-Object { $_.Name -match '\.(test|spec)\.(ts|tsx|js)$' } | Remove-Item

$utf8 = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText((Join-Path $Out 'manifest.json'), (($manifest | ConvertTo-Json -Depth 10) + "`n"), $utf8)
[System.IO.File]::WriteAllText((Join-Path $Out 'package.json'), (($newPkg | ConvertTo-Json -Depth 10) + "`n"), $utf8)

Write-Host ''
Write-Host "Draft package written to $Out" -ForegroundColor Green
Write-Host "  id: $Id   version: $version"
Write-Host "  core:    $((@($coreFiles + $coreFolderNames)) -join ', ')"
Write-Host "  modules: $(($moduleFolders | ForEach-Object { $_.Name }) -join ', ')"
if ($skipped.Count) { Write-Host '  not copied:' ; $skipped | ForEach-Object { Write-Host "    $_" } }
if ($peers.Count) { Write-Host "  peers (the solution supplies them): $($peers.Keys -join ', ')" }
if ($dev.Count) { Write-Host "  dev only (never added to a solution): $($dev.Keys -join ', ')" }
if ($undeclaredOther.Count) {
    Write-Host '  imported by the code but declared nowhere (decide: add to dependencies, or remove the import):' -ForegroundColor Yellow
    $undeclaredOther | ForEach-Object { Write-Host "    $_" -ForegroundColor Yellow }
}
Write-Host ''
Write-Host 'Next: open manifest.json, check the core and the modules, then choose this folder on'
Write-Host 'the Packages page of the Edit solution tab. The original was not changed.'

<#
.SYNOPSIS
    Logs every NEW cargo, rustc, npm, node, dotnet, python or git process, with its
    command line, as it starts.

.DESCRIPTION
    Use it next to the trust gate canary test: run it before you open the canary
    project and read the log afterwards. While the project is untrusted the log
    should show none of these programs starting for it.

    It takes a snapshot every -IntervalMs milliseconds (Win32_Process through CIM)
    and logs each process that was not there before: the time, its id, its parent's
    id and name, its name and its full command line. Processes already running when
    it starts are counted but not logged. It needs no administrator rights and
    changes nothing.

    npm is usually started through a shell (cmd /c npm ...), so shells whose command
    line names one of the tools are logged too, marked "via shell". Pass -NoShells to
    leave them out.

    Limit: a process that starts and ends inside one interval is missed. The default
    of 100 ms catches nearly everything a scan starts, but it is a polling watcher,
    not a guarantee; Sysmon or ETW would be the tool for one.

    Stop it with Ctrl+C. It prints a count when it ends.

.PARAMETER LogPath
    Where to append the log. Default E:\canary-test\process-log.txt. The folder is
    created if needed.

.PARAMETER IntervalMs
    Time between snapshots, in milliseconds. Default 100.

.PARAMETER DurationSeconds
    Stop by itself after this many seconds. Default 0 (run until Ctrl+C).

.PARAMETER NoShells
    Do not log shells (cmd, powershell, pwsh, sh, bash) that mention a tool.

.EXAMPLE
    .\scripts\watch-processes.ps1
    .\scripts\watch-processes.ps1 -LogPath E:\canary-test\process-log.txt -DurationSeconds 600
#>
[CmdletBinding()]
param(
    [string]$LogPath = 'I:\canary-test\process-log.txt',
    [ValidateRange(20, 10000)]
    [int]$IntervalMs = 100,
    [int]$DurationSeconds = 0,
    [switch]$NoShells
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ([System.Environment]::OSVersion.Platform -ne 'Win32NT') {
    throw 'watch-processes.ps1 uses Windows process information. Run it on Windows.'
}

$tools = 'cargo', 'rustc', 'npm', 'node', 'dotnet', 'python', 'git'

# The programs themselves, by name. LIKE catches cargo-clippy, python3, pythonw,
# git-remote-https and the like.
$byName = @(
    "Name LIKE 'cargo%'", "Name LIKE 'rustc%'", "Name LIKE 'npm%'", "Name = 'node.exe'",
    "Name LIKE 'dotnet%'", "Name LIKE 'python%'", "Name LIKE 'git%'"
) -join ' OR '

# Shells whose command line mentions a tool (npm runs as "cmd /c npm ...").
$shells = "Name = 'cmd.exe' OR Name = 'powershell.exe' OR Name = 'pwsh.exe' OR Name = 'sh.exe' OR Name = 'bash.exe'"
$mentions = ($tools | ForEach-Object { "CommandLine LIKE '%$_%'" }) -join ' OR '
$filter = "($byName)"
if (-not $NoShells) {
    $filter += " OR (($shells) AND ($mentions))"
}

New-Item -ItemType Directory -Force -Path (Split-Path -Parent $LogPath) | Out-Null

function Get-Watched {
    Get-CimInstance -ClassName Win32_Process -Filter $filter -ErrorAction SilentlyContinue |
        Where-Object { $_.ProcessId -ne $PID }
}

function Get-Key($Process) { '{0}:{1}' -f $Process.ProcessId, $Process.CreationDate.Ticks }

function Log([string]$Line, [string]$Colour = 'Gray') {
    Add-Content -Path $LogPath -Value $Line -Encoding utf8
    Write-Host $Line -ForegroundColor $Colour
}

$parentNames = @{}
function Get-ParentName([uint32]$ParentId) {
    if ($parentNames.ContainsKey($ParentId)) { return $parentNames[$ParentId] }
    $parent = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $ParentId" -ErrorAction SilentlyContinue
    $name = if ($parent) { $parent.Name } else { '(already gone)' }
    $parentNames[$ParentId] = $name
    $name
}

$seen = New-Object 'System.Collections.Generic.HashSet[string]'
$baseline = @(Get-Watched)
foreach ($process in $baseline) { [void]$seen.Add((Get-Key $process)) }

$started = Get-Date
Log ('=== watch started {0}  interval {1} ms  shells {2}' -f $started.ToString('yyyy-MM-dd HH:mm:ss'), $IntervalMs, $(if ($NoShells) { 'off' } else { 'on' })) 'Cyan'
Log ('=== watching: {0}' -f ($tools -join ', ')) 'Cyan'
Log ('=== {0} matching process(es) were already running and are not logged' -f $baseline.Count) 'Cyan'
Log ('=== log: {0}   (Ctrl+C to stop)' -f $LogPath) 'Cyan'

$logged = 0
try {
    while ($true) {
        if ($DurationSeconds -gt 0 -and ((Get-Date) - $started).TotalSeconds -ge $DurationSeconds) { break }

        $now = @(Get-Watched)
        $present = New-Object 'System.Collections.Generic.HashSet[string]'
        foreach ($process in $now) {
            $key = Get-Key $process
            [void]$present.Add($key)
            if ($seen.Add($key)) {
                $logged++
                $isShell = $process.Name -match '^(cmd|powershell|pwsh|sh|bash)\.exe$'
                $commandLine = if ($process.CommandLine) { $process.CommandLine } else { '(command line not readable)' }
                $parent = Get-ParentName $process.ParentProcessId
                $via = if ($isShell) { '  [via shell]' } else { '' }
                Log ('{0}  pid={1} ppid={2} ({3})  {4}{5}  {6}' -f (Get-Date).ToString('yyyy-MM-dd HH:mm:ss.fff'), $process.ProcessId, $process.ParentProcessId, $parent, $process.Name, $via, $commandLine) 'Yellow'
            }
        }
        # Forget processes that have ended, so the set stays small and a reused id is new.
        $seen.IntersectWith($present)

        Start-Sleep -Milliseconds $IntervalMs
    }
}
finally {
    Log ('=== watch ended {0}  new processes logged: {1}' -f (Get-Date).ToString('yyyy-MM-dd HH:mm:ss'), $logged) 'Cyan'
}

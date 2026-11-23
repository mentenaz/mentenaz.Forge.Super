# Trust gate: canary test

A manual test that shows Forge's panels do not run a project's own tooling
until the project is trusted. It uses a harmless **canary project**: every
program in it does exactly one thing, create an **empty marker file**, and
nothing else. No marker should appear while the project is untrusted; after
**Trust and Continue** the scans run and the markers appear.

Background: Zed opens an unknown folder in Restricted Mode. Forge's panels run
`cargo`, `npm`, `dotnet` and `python` in the project, and those tools read the
project's own configuration. The gate (`forge_trust_gate`, driven by
`forge_trust`) holds those programs back until the editor has marked the folder
trusted. See "Panels: run a project's own tooling only in a trusted project" in
`CLAUDE.md`.

`scripts\make-canary.ps1` builds the project below for you, and
`scripts\watch-processes.ps1` watches for the tools starting. Run the test in an
isolated build (`script\run-isolated.ps1`), not in your installed Forge.

## The evidence: markers and the gate log

Two things prove the result, in this order of weight:

1. **The marker files.** A marker is made by the canary program itself, so a marker
   that exists is proof the program ran, and an empty marker folder is proof it
   did not.
2. **The gate log.** Every decision the gate makes is written to the Forge log
   (`%LOCALAPPDATA%\ForgeSuperBuild\logs\ForgeSuperBuild.log` for an isolated
   build). Lines start with `forge_trust_gate:` and name the decision, the program
   and its arguments, the folder, who asked, and why:

   ```
   forge_trust_gate: BLOCKED cargo metadata --no-deps --format-version 1 | folder: E:\canary-test\project | asked by: cargo_backend::run_cargo_output (crates\cargo_backend\src\cargo_backend.rs:112) | because the folder is in Restricted Mode
   forge_trust_gate: ALLOWED cargo metadata --no-deps --format-version 1 | folder: E:\canary-test\project | asked by: cargo_backend::run_cargo_output (...) | because the folder is trusted
   ```

   BLOCKED lines are warnings and ALLOWED lines are info. If you see none of them,
   the log level may be hiding them; start the build with
   `RUST_LOG=forge_trust_gate=info`. "Asked by" is the backend function and the
   file and line of the call, not the panel; the panel that asked is the one that
   owns that backend call.

**The process watcher is supporting evidence only.** It polls every 100 ms, so a
process that starts and ends inside one poll is missed, and some command lines
cannot be read. An empty watcher log does not prove nothing ran; a marker or a
gate-log line does.

## Before you start

- Use a folder whose **parents are not trusted**. Trusting a folder also trusts
  what is inside it, so build the canary somewhere new, such as
  `E:\canary-test\project`. In the trust dialog, do not tick the option that
  trusts the parent folder.
- Start from a clean answer: run **workspace: clear trusted worktrees** from the
  command palette (it also forgets "stay in restricted mode" choices), then
  restart, so Forge Super asks about the canary folder as if it were new.
- Keep `"session": { "trust_all_worktrees": false }` (the default). With `true`
  the gate allows everything, by design.
- Everything below writes into one folder, `E:\canary-test\markers`. Create it
  empty (or run `scripts\make-canary.ps1 -ClearMarkers`).

## The canary project

All paths are examples; keep the marker folder the same everywhere.

### 1. Rust: a compiler wrapper that only writes a marker

`E:\canary-test\project\Cargo.toml`

```toml
[package]
name = "canary"
version = "0.1.0"
edition = "2021"
```

`E:\canary-test\project\src\main.rs`

```rust
fn main() {}
```

`E:\canary-test\project\.cargo\config.toml`

```toml
[build]
rustc-wrapper = "E:/canary-test/rustc-wrapper.cmd"
```

`E:\canary-test\rustc-wrapper.cmd` (it creates the marker and exits; it does not
start the real compiler)

```bat
@echo off
type nul > "E:\canary-test\markers\cargo-wrapper.marker"
exit /b 1
```

**When it fires.** Cargo runs the wrapper when it compiles or checks, not for
`cargo metadata --no-deps`. The Rust panel's scan uses `cargo metadata --no-deps`
(manifests only), so **the scan does not create this marker**. It appears when
you click **Check** in the Rust panel, which runs `cargo check` through the Script
Runner. The wrapper then stops the build, so Cargo shows an error: expected and
harmless.

### 2. npm: an install script that only writes a marker

`E:\canary-test\project\package.json`

```json
{
  "name": "canary",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "node -e \"require('fs').closeSync(require('fs').openSync('E:/canary-test/markers/npm-preinstall.marker','w'))\""
  }
}
```

An install script runs on `npm install`, not on the read-only scans (`npm ls`,
`npm outdated`, `npm audit`). So this marker tests the **click** path: nothing
may run it while the project is untrusted, and it appears only if you click
Install or Update in the Node panel after trusting.

### 3. Python: a virtual-environment interpreter that only writes a marker

The Python panel finds a `.venv` inside the project and runs its `python` to
list packages. Make that interpreter a stub.

`E:\canary-test\project\requirements.txt` (empty) so the folder looks like a
Python project, and an empty `E:\canary-test\project\.venv\pyvenv.cfg`.

Windows needs a real `.exe`, so compile a three-line program:

`E:\canary-test\canary-python.rs`

```rust
fn main() {
    let _ = std::fs::File::create(r"E:\canary-test\markers\python-stub.marker");
}
```

```powershell
New-Item -ItemType Directory -Force E:\canary-test\project\.venv\Scripts | Out-Null
rustc E:\canary-test\canary-python.rs -o E:\canary-test\project\.venv\Scripts\python.exe
```

The compiler also leaves a `python.pdb` next to it. It is harmless.

### 4. .NET: an MSBuild target that only writes a marker

`E:\canary-test\project\dotnet\Canary.csproj`

```xml
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Library</OutputType>
    <TargetFramework>net10.0</TargetFramework>
  </PropertyGroup>
</Project>
```

(Use the target framework of your installed SDK; `scripts\make-canary.ps1` picks it.)

`E:\canary-test\project\dotnet\Directory.Build.targets`

```xml
<Project InitialTargets="CanaryMarker">
  <!-- Canary: the only thing this does is create an empty marker file. -->
  <Target Name="CanaryMarker">
    <Exec Command="type nul &gt; &quot;E:\canary-test\markers\dotnet-msbuild.marker&quot;" IgnoreExitCode="true" StandardOutputImportance="low" />
  </Target>
</Project>
```

**When it fires.** The .NET panel's scan (`dotnet list package`) runs MSBuild
targets, so **the scan itself creates this marker**, with no click, as soon as
the project is trusted.

### 5. npm install offer: the same `package.json`, no extra setup

The `npm-preinstall.marker` of case 2 also tests the **install offer**
(`npm_bootstrap`), which is not a panel scan: when a folder with a `package.json`
is opened and trusted, a toast offers `npm install`, and the Scaffold panel can
start it by itself when "Run install afterwards" is on. It runs in the Script
Runner, which asks the gate, and the offer itself asks the gate before it shows.
The log lines start with `npm_bootstrap:`.

**When it fires.** Never while the folder is untrusted (no toast, no install).
After Trust and Continue the toast appears on its own, and the marker appears only
once the install is started, by clicking the toast's button or by "Run install
afterwards".

### 6. git: a repository config that runs a program (not yet run)

*Added 23 November 2026 after the audit found Helm's `git` ungated. The steps below
have not been run end to end; treat the first run as part of the test.*

Git can run a program named in a repository's own config on a plain `git status`:
`core.fsmonitor`. Helm runs `git` in the project, so it must neither run `git` in
an untrusted folder nor let that setting fire in a trusted one.

In the canary project (`git init` it), set a local config whose `fsmonitor` command
creates `E:\canary-test\markers\git-fsmonitor.marker` and does nothing else (the
exact quoting depends on the shell Git for Windows uses; confirm with a manual
`git status` in a scratch repo first, and check the marker appears there, so you
know the canary can fire).

**Expected.** Untrusted: no Helm `git` call at all (the gate log has `BLOCKED`
lines asked by Helm's `run_git`), and the marker is absent. Trusted: Helm's
`git` calls are `ALLOWED`, and the marker is **still absent**, because Helm passes
`-c core.fsmonitor=false`.

## The test

### Phase A: untrusted

1. Open `E:\canary-test\project` in Forge Super.
2. In the trust dialog choose **Stay in Restricted Mode** (or dismiss it).
3. Wait two minutes. Open the Rust, Node, .NET, Python and Script Runner panels.
4. **Expected:**
   - The Rust, Node, .NET, Python and Script Runner panels show *"... is waiting
     for you to trust this project"* with a **Trust this project to scan it**
     button. The Rust panel's Check button is out of reach.
   - `E:\canary-test\markers` is **empty**.
   - The gate log has `BLOCKED` lines for `E:\canary-test\project` (typically
     `cargo metadata`, `dotnet list package`, the Python `pip` calls) and **no**
     `ALLOWED` line for that folder.
5. Also try, from the npm manager and Cargo manager tabs, to open and refresh.
   They show the "not trusted yet" message. Still no marker.
6. **No install toast** appears for the `package.json`, and the log has no
   `npm_bootstrap:` line saying it started an install.
7. Helm shows its **Restricted Mode banner** and runs no `git`; the Git panel's
   own (upstream) behaviour is not Helm's.

### Phase B: trusted

1. Click **Trust this project to scan it**, then **Trust and Continue**.
2. **Expected, within a few seconds, with no clicks:**
   - the panels leave the waiting state and scan;
   - `python-stub.marker` appears (the Python panel runs the `.venv` interpreter);
   - `dotnet-msbuild.marker` appears (the .NET scan runs MSBuild targets);
   - the gate log switches to `ALLOWED ... because the folder is trusted`.
   - `cargo-wrapper.marker` does **not** appear yet: the Rust scan does not run the
     wrapper.
3. In the Rust panel click **Check**. **Expected:** `cargo-wrapper.marker`
   appears, and Cargo reports the wrapper's error.
4. In the Node panel click **Install** (or update). **Expected:**
   `npm-preinstall.marker` appears. It did not appear from scanning alone.
5. **The install offer:** after trusting, the install toast appears by itself.
   **Expected:** `npm-preinstall.marker` is still absent until you start the
   install from the toast. (Delete the marker between steps 4 and 5 to see this.)
6. **Helm and `git`:** `git-fsmonitor.marker` does not appear, trusted or not.

### Result

| Moment | Marker | Pass if |
|---|---|---|
| Opened, untrusted | none | the markers folder is empty and the gate log shows only BLOCKED |
| After Trust and Continue, no click | `python-stub`, `dotnet-msbuild` | they appear on their own |
| After clicking Check | `cargo-wrapper` | it appears only now |
| After clicking Install | `npm-preinstall` | it appears only now |
| Trusted, install toast shown, not clicked | none new | no marker until the install starts |
| Helm open, untrusted or trusted | `git-fsmonitor` | it never appears |

If **any marker appears while the project is untrusted, the gate has failed.**
Keep the markers, note the order they appeared in, and attach the gate log and the
watcher log (`E:\canary-test\process-log.txt`).

## What the first run taught us

- **`cargo metadata --no-deps` does not run the rustc wrapper.** The Rust panel's
  scan is manifests only, so the Rust canary fires on **Check** (`cargo check`),
  not on the scan. A first draft of this test expected it from the scan.
- **The .NET scan does run MSBuild targets.** `dotnet list package` evaluates and
  builds the project, so a build file in an untrusted project can run a program
  during a scan. That is why the .NET panel is gated.
- **The first audit missed two callers.** It covered the panels' backends, and
  afterwards two places that start a program were found outside them: the npm
  install offer (`npm_bootstrap`) ran `npm install` in a folder that was not
  trusted, and Helm's `git` had no check. Both now ask the gate; cases 5 and 6 are
  there so a new caller of either kind is caught by this test.
- **The process watcher can miss very short processes.** It polls rather than
  listens, so it is a hint, not proof. The markers and the gate log are the
  evidence.

## Reset and clean up

- To run it again: delete the marker files (or `scripts\make-canary.ps1
  -ClearMarkers`), then **workspace: clear trusted worktrees** and restart.
- When done, delete `E:\canary-test` and remove it from the recent projects.
  Nothing outside that folder was touched.

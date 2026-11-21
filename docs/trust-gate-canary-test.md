# Trust gate: canary test

A manual test that shows Forge's panels do not run a project's own tooling
until the project is trusted. It uses a harmless **canary project**: every
program in it does exactly one thing, create an **empty marker file**, and
nothing else. No marker should appear while the project is untrusted; after
**Trust and Continue** the scans run and the markers can appear.

Background: Zed opens an unknown folder in Restricted Mode. Forge's panels run
`cargo`, `npm`, `dotnet` and `python` in the project, and those tools read the
project's own configuration. The gate (`forge_trust_gate`, driven by
`forge_trust`) holds those programs back until the editor has marked the folder
trusted. See "Panels: run a project's own tooling only in a trusted project" in
`CLAUDE.md`.

## Before you start

- Use a folder whose **parents are not trusted**. Trusting a folder also trusts
  what is inside it, so build the canary somewhere new, such as
  `C:\canary-test\project`. In the trust dialog, do not tick the option that
  trusts the parent folder.
- Start from a clean answer: run **workspace: clear trusted worktrees** from the
  command palette (it also forgets "stay in restricted mode" choices), then
  restart, so Forge Super asks about the canary folder as if it were new.
- Keep `"session": { "trust_all_worktrees": false }` (the default). With `true`
  the gate allows everything, by design.
- Everything below writes into one folder, `C:\canary-test\markers`. Create it
  empty. On Linux and macOS use `/tmp/canary-test/markers` and `/` paths.

## The canary project

All paths are examples; keep the marker folder the same everywhere.

### 1. Rust: a compiler wrapper that only writes a marker

`C:\canary-test\project\Cargo.toml`

```toml
[package]
name = "canary"
version = "0.1.0"
edition = "2021"
```

`C:\canary-test\project\src\main.rs`

```rust
fn main() {}
```

`C:\canary-test\project\.cargo\config.toml`

```toml
[build]
rustc-wrapper = "C:/canary-test/rustc-wrapper.cmd"
```

`C:\canary-test\rustc-wrapper.cmd` (it creates the marker and exits; it does not
start the real compiler)

```bat
@echo off
type nul > "C:\canary-test\markers\cargo-wrapper.marker"
exit /b 1
```

Linux and macOS: `/tmp/canary-test/rustc-wrapper.sh`, made executable
(`chmod +x`), with `rustc-wrapper` pointing at it:

```sh
#!/bin/sh
: > /tmp/canary-test/markers/cargo-wrapper.marker
exit 1
```

Cargo runs the wrapper when it asks `rustc` about itself, which `cargo metadata`
does. So after the project is trusted, the Rust panel's scan creates
`cargo-wrapper.marker` and then shows a Cargo error, because the wrapper stops
there. That error is expected and harmless.

### 2. npm: an install script that only writes a marker

`C:\canary-test\project\package.json`

```json
{
  "name": "canary",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "node -e \"require('fs').closeSync(require('fs').openSync('C:/canary-test/markers/npm-preinstall.marker','w'))\""
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

`C:\canary-test\project\requirements.txt` (empty) so the folder looks like a
Python project.

Windows needs a real `.exe`, so compile a three-line program:

`C:\canary-test\canary.rs`

```rust
fn main() {
    let _ = std::fs::File::create(r"C:\canary-test\markers\python-stub.marker");
}
```

```powershell
New-Item -ItemType Directory -Force C:\canary-test\project\.venv\Scripts | Out-Null
rustc C:\canary-test\canary.rs -o C:\canary-test\project\.venv\Scripts\python.exe
```

Linux and macOS: `/tmp/canary-test/project/.venv/bin/python`, executable:

```sh
#!/bin/sh
: > /tmp/canary-test/markers/python-stub.marker
```

## The test

### Phase A: untrusted

1. Open `C:\canary-test\project` in Forge Super.
2. In the trust dialog choose **Stay in Restricted Mode** (or dismiss it).
3. Wait two minutes. Open the Rust, Node, .NET, Python and Script Runner panels.
4. **Expected:**
   - The Rust, Node, .NET, Python and Script Runner panels show *"... is waiting
     for you to trust this project"* with a **Trust this project to scan it**
     button.
   - `C:\canary-test\markers` is **empty**.
5. Also try, from the npm manager and Cargo manager tabs, to open and refresh.
   They show the "not trusted yet" message. Still no marker.

### Phase B: trusted

1. Click **Trust this project to scan it**, then **Trust and Continue**.
2. **Expected, within a few seconds, with no clicks:**
   - the panels leave the waiting state and scan;
   - `cargo-wrapper.marker` appears (the Rust panel's `cargo metadata`), and the
     Rust panel shows a Cargo error from the stub wrapper;
   - `python-stub.marker` appears (the Python panel running the `.venv`
     interpreter).
3. Click **Install** (or update) in the Node panel. **Expected:**
   `npm-preinstall.marker` appears. It did not appear from scanning alone.

### Result

| Moment | Marker that may exist | Pass if |
|---|---|---|
| Opened, untrusted | none | the folder is empty |
| After Trust and Continue | `cargo-wrapper`, `python-stub` | they appear without any click |
| After clicking Install | `npm-preinstall` | it appears only now |

If **any marker appears while the project is untrusted, the gate has failed.**
Keep the markers, note the order they appeared in, and attach the log
(`%LOCALAPPDATA%\ForgeSuper\logs\ForgeSuper.log`, or `ForgeSuperBuild` for an
isolated build).

## Reset and clean up

- To run it again: delete the marker files, then **workspace: clear trusted
  worktrees** and restart.
- When done, delete `C:\canary-test` and remove it from the recent projects.
  Nothing outside that folder was touched.

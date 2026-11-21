# Mentenaz Forge Super

[![Latest release](https://img.shields.io/github/v/release/mentenaz/mentenaz.Forge.Super?label=release)](https://github.com/mentenaz/mentenaz.Forge.Super/releases/latest)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](#license)

> **A developer cockpit built directly into the Zed editor: databases, GitHub, package managers, runtimes, visual workflows and system monitoring as native parts of the workspace, written in Rust and GPUI.**

![Mentenaz Forge Super](docs/screenshots/Forge_Workspace.png)

Mentenaz Forge Super is a fork of the [Zed](https://github.com/zed-industries/zed) editor. It keeps Zed's editor and adds the tools that normally live in separate applications (a database client, a GitHub client, package managers, a process monitor, a workflow engine) as panels inside the same window.

It is made by Francois Huyzers ([@mentenaz](https://github.com/mentenaz)). It is **not affiliated with, endorsed by, or associated with Zed Industries.**

| | |
| --- | --- |
| **What it is** | A fork of Zed 1.31 with a developer cockpit added |
| **Built with** | Rust and GPUI |
| **Platforms** | Windows (x86_64), Linux (Debian, Ubuntu, Kali; amd64), macOS (Apple Silicon and Intel) |
| **Cost and accounts** | Free. No account, no sign-in, no usage data sent |
| **License** | GPL-3.0-or-later |
| **Status** | Experimental. Developed and tested mainly on Windows; Linux and macOS are newer. See [Status](#status) |
| **Website and docs** | <https://forge.mentenaz-server.com> |

---

## Download and install

Every release is on the [releases page](https://github.com/mentenaz/mentenaz.Forge.Super/releases/latest). Builds are named `dev-vN` (for example `dev-v59`), and the installer version is `1.30.0.N`.

**The builds are not code-signed yet**, so each system warns the first time you open one. The steps are below.

### Windows

Download `Mentenaz-Forge-Super-x86_64.exe` and run it. It installs into your user profile and does not need administrator rights.

Windows SmartScreen may say "Windows protected your PC". Choose **More info**, then **Run anyway**.

### Linux (Debian, Ubuntu, Kali)

```sh
curl -fsSL https://downloads.mentenaz-server.com/install.sh | sudo sh
```

This registers the Mentenaz package repository (its signing key is checked against a fingerprint built into the script) and installs the `forge-super` package. Then start it with `forge-super`. Updates arrive with `sudo apt-get update && sudo apt-get upgrade`.

The repository has two channels. **stable** (the default) only receives a build after it has been tried; **edge** gets every build as soon as it is made (`... | sudo sh -s -- --edge`).

A `.deb` and a `.tar.gz` are also on every release.

### macOS

macOS support is new and has had very little testing on real Macs. Pick the disk image for your Mac: `Mentenaz-Forge-Super-aarch64.dmg` for Apple Silicon (M1 or newer), `Mentenaz-Forge-Super-x86_64.dmg` for Intel.

1. Open the disk image and drag **Mentenaz Forge Super** into **Applications**.
2. The first time, macOS blocks it ("Apple could not verify..."). Open **System Settings**, **Privacy & Security**, and choose **Open Anyway**. On macOS 14 and earlier you can right-click the app and choose **Open** instead.
3. Or, in Terminal: `xattr -cr "/Applications/Mentenaz Forge Super.app"`, then open it as usual.

If it does not start, please [open an issue](https://github.com/mentenaz/mentenaz.Forge.Super/issues) with any message you see.

### Updating

Forge Super does not update itself. It installs side by side with Zed and uses its own folders and link scheme (`forgesuper://`). It can tell you when a new release exists (an optional check against GitHub, at most once a day). See [Install](https://forge.mentenaz-server.com/docs/install) and [Updating](https://forge.mentenaz-server.com/docs/updating).

---

## What is inside

| Area | What it does |
| --- | --- |
| **Developer Cockpit** | One operational view of the project, its runtimes and the machine |
| **Dashboard** | Runtime versions, security findings, outdated dependencies, Git status and system health, shown as healthy, unknown or failed |
| **Database** | SQLite, PostgreSQL, MySQL and MariaDB, Microsoft SQL Server and Oracle: connections, schema explorer, SQL workbench, schema graph |
| **Visual workflows (Flows)** | Executable workflows as an interactive graph: conditions, loops, try/catch, variables, HTTP actions, port waiting |
| **Runtime panels** | Node, .NET, Python and Rust project tooling: run, build, test, environments |
| **Package managers** | npm, NuGet, pip and Cargo: search, install, update, remove, vulnerability scanning |
| **Helm (GitHub)** | A native GitHub client: repositories, issues, pull requests, Actions, clone, and a read-only repository Workspace |
| **Git Insights** | Commit activity, most-changed files and collaborators |
| **Process Monitor** | CPU, memory, per-core load, disk, network and running processes |
| **Script Runner** | Run project scripts from the workspace, with clickable links in the output |
| **Themes** | Forge Dark and Forge Light, plus Zed's own themes |

Each panel opens from the status bar and from the command palette, and most have a key (for example `Ctrl+K` then `D` for Databases, `Ctrl+K` then `H` for Helm). The [tutorials](https://forge.mentenaz-server.com/docs/tutorials) walk through each one.

---

## A look inside

### Dashboard

Project, dependency, Git and system health in one place. A project that has not been scanned does not appear healthy; it shows as unknown.

![Dashboard](docs/screenshots/DashBoard.png)

### Visual workflows

Workflows are stored as JSON and rendered as a graph that actually runs: sequential steps, `if / else`, loops, try/catch, error paths, variables, environment variables, HTTP actions and port waiting, with per-node results. The graph is backed by a dedicated `workflow_engine` crate.

![Workflow engine](docs/screenshots/Flow_Running.png)

### Database

Connect to SQLite, PostgreSQL, MySQL and MariaDB, SQL Server or Oracle, with several connections open at once. Browse tables, columns and relationships, run SQL in the workbench, and see how tables relate in a schema graph.

![Schema graph](docs/screenshots/Database_Schema_Graph.png)
![SQL workbench](docs/screenshots/Database_Work_Bench.png)

### Runtimes and package managers

Each panel targets the active project and watches it on disk: when a manifest or lockfile changes, the panel rescans on its own.

![Node](docs/screenshots/NodeJs.png)
![Rust](docs/screenshots/Rust_Panel.png)
![Cargo manager](docs/screenshots/Cargo_Manager.png)
![npm manager](docs/screenshots/Node_Package_Manager.png)

The Rust and Cargo panels read `Cargo.toml` and `Cargo.lock` directly and do not need `rust-analyzer`. The Cargo, npm and NuGet managers show security advisories (Cargo's come from the RustSec database through OSV.dev), and every change is confirmed before it is made.

### Helm: GitHub inside the editor

Sign in with GitHub's device flow or a token (stored in the operating system's credential store). Helm covers profiles, organizations, repository search and browsing, issues, pull requests (read, create, edit, close, reopen, merge and comment), Actions runs, releases, traffic, collaborators and cloning with a folder picker.

![Helm home](docs/screenshots/Helm_Home.png)
![Helm repository](docs/screenshots/Helm_Repo_Detail.png)

A read-only **Workspace** tab opens any repository without cloning it: a file tree, rendered READMEs, syntax-highlighted previews, commit history, diffs, comparisons between refs and code search.

![Helm workspace](docs/screenshots/Helm_Workspace.png)

### Git Insights and process monitoring

![Git insights](docs/screenshots/Git_Insights.png)
![Processes](docs/screenshots/Processes.png)

---

## Privacy and accounts

- **No account and no sign-in.** There is no collaboration service and no Zed-hosted AI. Forge Super does not contact Zed's servers for those.
- **No usage data is sent**, and no crash reports are uploaded. Crashes are written to a local log only.
- **AI is optional.** The agent panel is there, but you connect a provider yourself, and `"disable_ai": true` removes the interface entirely.
- **It does not update itself.** The optional daily update check only asks `api.github.com` whether a newer release exists.

The full list of what the app contacts is on the [Privacy](https://forge.mentenaz-server.com/docs/privacy) page.

## What differs from Zed

The editor, languages, themes, terminal, tasks, debugger, Git panel and key bindings are Zed's. Forge Super removes or switches off sign-in, collaboration, Zed-hosted AI, telemetry upload and the built-in updater, changes a few defaults, adds the panels above, and uses its own extension registry. [What's different from Zed](https://forge.mentenaz-server.com/docs/different-from-zed) lists it all, and says when Zed's own documentation applies.

---

## Status

Forge Super is **experimental**: a personal research fork, not a finished commercial product.

- **Windows** is the main development and test platform.
- **Linux** is built and published through an apt repository on every release, and used, but tested less.
- **macOS** builds are produced and published by CI (Apple Silicon and Intel), but have **not been tested on real hardware by the maintainer yet**.
- Builds are **not code-signed** on any platform (see [Download and install](#download-and-install)).
- Automated test coverage is limited; most testing is hands-on.
- Workflow, database and SQL language-server tooling work but are still evolving.

Bug reports and feedback are welcome on the [issue tracker](https://github.com/mentenaz/mentenaz.Forge.Super/issues).

---

## Build from source

Build it the way you would build Zed: see Zed's development guides in this repository for [Windows](docs/src/development/windows.md), [Linux](docs/src/development/linux.md) and [macOS](docs/src/development/macos.md). The application binary is `forgedev`:

```sh
cargo build --release -p zed --bin forgedev
```

On Windows, `script/run-isolated.ps1` builds and runs a separate copy with its own data folder, so a development build does not touch an installed one.

Releases are built by the [`build_installers`](.github/workflows/build_installers.yml) workflow. Pushing a tag matching `dev-v*` builds Windows, Linux and macOS and publishes a release; the workflow can also be run by hand from the Actions tab. Product names (display name, folders, executable and command names, URL scheme) come from [`editions.json`](editions.json).

---

## Architecture

Forge Super is implemented inside the Zed codebase, not as an external program talking to Zed, so its panels use Zed's GPUI, workspace, project, Git, settings and language infrastructure directly. Each tool is its own crate, usually split into a `*_panel` crate (GPUI rendering) and a `*_backend` crate (database, process, Git and network logic) to keep merges with upstream Zed manageable.

```text
cockpit_panel   dashboard_panel   processes_panel   script_runner_panel   designer_panel
database_panel  database_backend
flows_panel     workflow_engine   gpui_flow
node_panel      dotnet_panel      python_panel      rust_panel
npm_manager_panel  nuget_manager_panel  python_manager_panel  cargo_manager_panel  cargo_backend
helm_panel      helm_backend      helm_ui           helm_workspace
```

Existing Zed crates it modifies include `gpui`, `workspace`, `project`, `git`, `git_ui`, `settings`, `languages`, `remote` and `zed`. Forge Super also extends GPUI with zoom, pan and fit-to-view for its graph views, and the fork merges upstream Zed regularly.

---

## Why it exists

The question behind the project: *what happens when the tools around a code editor become part of the editor itself?* A developer normally moves between an editor, a terminal, a Git client, GitHub, package managers, a database client, a process monitor and runtime managers. Forge Super explores whether those boundaries can shrink, by building the tools natively inside Zed's Rust and GPUI architecture instead of as a separate application. The panels are working Rust components, not mock-ups.

---

## Relationship to Zed

Forge Super is a personal fork of [Zed](https://github.com/zed-industries/zed). It is **not an official Zed project** and is not affiliated with, endorsed by, sponsored by, or otherwise associated with Zed Industries. Zed is a trademark of its owners; it is named here only to say what this project is based on.

## License

The combined work is distributed under **GPL-3.0-or-later**, inherited from the Zed crates it modifies. Some components keep their own licenses: `gpui` and the GPUI component libraries are Apache-2.0, and `gpui_flow` is MIT. See the license files in the repository for the exact terms.

## Links

- Website: <https://forge.mentenaz-server.com>
- Documentation and tutorials: <https://forge.mentenaz-server.com/docs>
- Releases: <https://github.com/mentenaz/mentenaz.Forge.Super/releases>
- Issues: <https://github.com/mentenaz/mentenaz.Forge.Super/issues>

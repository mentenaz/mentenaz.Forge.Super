//! .NET panel — SDK detection, project scanning, quick actions, installed /
//! outdated / vulnerable NuGet package lists, and a live process list.
//!
//! Ported standalone (no host-app wiring yet): the source panel depended on
//! a host `AppState` for a few things, none of which exist here:
//!
//! - The initial/live-updating workspace root (`state.workspace_root`,
//!   `state.workspace_tx`) — this panel just uses its own process cwd
//!   instead, same as the other standalone-first ports.
//! - A live process list fed by subscribing to the Cockpit panel's broadcast
//!   tick — reimplemented here as this panel's own lightweight `sysinfo`
//!   poll filtered by "dotnet" process names (see `spawn_processes_poll`).
//! - `run_in_script_runner` — the script run / quick-action hook. Unlike the
//!   Node panel (whose buttons stayed disabled because nothing held both
//!   entities live at port time), the dotnet quick actions are wired for
//!   real: `script_runner_panel::run_external` is the concrete streaming
//!   target, with a silent `background_run` fallback when the dock panel
//!   isn't loaded yet — the npm manager's §4.2 wiring.
//!
//! The detection/scanning/package JSON logic lives in `dotnet_backend`,
//! ported alongside this panel — see that crate's own doc comment. The full
//! NuGet registry/browse experience lives in `nuget_manager_panel`, which
//! this panel's quick action opens.

mod dotnet_panel_settings;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use dotnet_backend::{
    DotnetProject, InstalledPackage, OutdatedPackage, UpdateKind, VulnerablePackage,
    classify_update, list_outdated, list_vulnerable, query_runtime, read_installed_packages,
    resolve_csproj, scan_dotnet_projects,
};
use gpui::{
    Action, App, AppContext as _, AsyncApp, AsyncWindowContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Render, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window,
    actions, div, prelude::FluentBuilder as _, px, svg,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    collapsible::Collapsible,
    h_flex,
    panel_header::PanelHeader,
    spinner::Spinner,
    tag::Tag,
};
use script_runner_panel::ScriptRunnerPanel;
use script_runner_panel::command::{check_package_name, check_version};
use sysinfo::System;
pub use dotnet_panel_settings::DotnetPanelSettings;
use settings::{DockSide, Settings as _};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

/// How often the local dotnet-process poll re-scans the system.
const DOTNET_PROCS_TICK_INTERVAL: Duration = Duration::from_secs(2);

/// Displays a path by its final component (folder/file name), falling back
/// to the full path when it has none — shared by the dock panels' dashboard
/// accessors.
fn path_file_name(dir: &str) -> String {
    std::path::Path::new(dir)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.to_string())
}

/// Files that mark a `.NET` project: a change to either can mean a project
/// was added or removed, so the Projects list is re-scanned for one.
fn is_dotnet_project_file(name: &str) -> bool {
    name.ends_with(".csproj") || name.ends_with(".sln")
}

/// Files the panel derives package data from for the active project: the
/// project file itself, or a NuGet manifest next to it. A change to any of
/// these means the installed/outdated/vulnerable lists are stale.
fn is_dotnet_package_manifest(name: &str) -> bool {
    is_dotnet_project_file(name)
        || name.eq_ignore_ascii_case("packages.config")
        || name.eq_ignore_ascii_case("Directory.Packages.props")
}

// ── Types ─────────────────────────────────────────────────────────────

/// One dotnet-related process row — a process whose name contains "dotnet"
/// (case-insensitive).
struct DotnetProcess {
    name: String,
    pid: u32,
    cpu_percent: f32,
    memory_mb: f64,
}

/// A background-loaded package list with its three UI states.
enum PackagesState<T> {
    Loading,
    Ready(Vec<T>),
    Error(String),
}

// ── Panel ─────────────────────────────────────────────────────────────

actions!(
    dotnet_panel,
    [
        /// Toggles focus on the Dotnet panel.
        ToggleFocus
    ]
);

/// Registers the Dotnet panel's actions on every workspace. Call once at app
/// startup, alongside the other panels' `init` functions.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<DotNetPanel>(window, cx);
        });
    })
    .detach();
}

/// Resolves the folder the Dotnet scan should be rooted on: the **currently
/// opened** project's first worktree, mirroring the Forge original's live
/// `workspace_root`. Returns `None` when no worktree is open yet (so the
/// caller can fall back to the process cwd).
fn project_root(workspace: &Workspace, cx: &mut Context<Workspace>) -> Option<String> {
    workspace
        .project()
        .read(cx)
        .worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
}

pub struct DotNetPanel {
    focus_handle: FocusHandle,

    workspace: WeakEntity<Workspace>,

    dotnet_ver: Option<String>,
    not_found: bool,

    cwd: String,
    projects: Vec<DotnetProject>,
    selected_project: Option<String>,
    scanning: bool,

    /// The resolved `.csproj` the panel's operations target. `None` while
    /// scanning and when no project was found — quick actions and the package
    /// lists then show their empty/missing-project states.
    csproj: Option<String>,

    packages: Vec<InstalledPackage>,
    outdated_packages: PackagesState<OutdatedPackage>,
    vulnerable_packages: PackagesState<VulnerablePackage>,

    /// Label of the quick action currently streaming in the Script Runner
    /// (or running silently in the background), if any.
    running_action: Option<String>,
    error: Option<String>,

    dotnet_procs: Vec<DotnetProcess>,
    /// False until the first background process scan lands, so the
    /// Processes section shows a loading row instead of claiming nothing
    /// is running.
    procs_loaded: bool,

    open: HashMap<String, bool>,

    /// Active observer on the Script Runner dock panel, replaced on every run
    /// that streams for it — fires when [`ScriptRunnerPanel`] notifies, which
    /// it does on each output line and when a run finishes (mirrors the npm
    /// manager's §4.2 wiring).
    run_subscription: Option<Subscription>,

    /// Debounces the filesystem-watcher rescan: replaced on every relevant
    /// `WorktreeUpdatedEntries` event, so a burst of updates collapses into a
    /// single `scan_projects` run once it goes quiet. Held only for its `Drop`
    /// (replacing it cancels the pending run).
    _rescan_task: Option<gpui::Task<()>>,

    /// Same debounce, but for reloading the active project's package data when
    /// its project file/manifest changes on disk.
    _data_reload_task: Option<gpui::Task<()>>,

    _procs_poll: gpui::Task<()>,
}

impl DotNetPanel {
    /// Detected .NET SDK version, for a future dashboard-style "runtime
    /// versions" summary.
    pub fn dotnet_version(&self) -> Option<&str> {
        self.dotnet_ver.as_deref()
    }

    /// Every detected .NET project, for a future dashboard-style "runtime
    /// versions" summary.
    pub fn detected_projects(&self) -> &[DotnetProject] {
        &self.projects
    }

    /// Loads the panel for a workspace, following the same
    /// `WeakEntity<Workspace>` + `AsyncWindowContext` convention as the
    /// other dock panels' `load` functions (see `initialize_panels` in
    /// `zed::zed`), so it can be added to the dock alongside them.
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            DotNetPanel::new(workspace, window, cx)
        })
    }

    pub fn new(
        _workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        // Root the scan on the currently-opened project worktrees (matching the
        // Forge original's live `workspace_root`), falling back to the process
        // cwd when no worktree is open yet.
        let cwd = project_root(&*_workspace, cx).unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        });

        // The live worktrees watch the filesystem (the same machinery the
        // project panel relies on); subscribing here lets `scan_projects`
        // rerun when a new `.csproj`/`.sln` appears, instead of only once at
        // startup.
        let project = _workspace.project().clone();

        let workspace = cx.entity().downgrade();

        cx.new(|cx| {
            cx.subscribe(&project, |this: &mut DotNetPanel, _project, event, cx| {
                if let project::Event::WorktreeUpdatedEntries(_, entries) = event {
                    let mut project_file_touched = false;
                    let mut selected_manifest_touched = false;
                    for (path, _, _) in entries.iter() {
                        let Some(name) = path.file_name() else {
                            continue;
                        };
                        if is_dotnet_project_file(name) {
                            project_file_touched = true;
                        }
                        if is_dotnet_package_manifest(name)
                            && this.manifest_change_is_selected(path.as_unix_str())
                        {
                            selected_manifest_touched = true;
                        }
                    }
                    // A new/removed project file can mean a new project, so the
                    // Projects list is always re-scanned for one.
                    if project_file_touched {
                        this.schedule_project_rescan(cx);
                    }
                    // Packages only change the *active* project's data, so only
                    // reload that one (avoids re-running the `dotnet list
                    // package` probes for unrelated edits elsewhere).
                    if selected_manifest_touched {
                        this.schedule_data_reload(cx);
                    }
                }
            })
            .detach();

            let mut open = HashMap::new();
            // Keep all sections open by default
            open.insert("Projects".to_string(), true);
            open.insert("Quick Actions".to_string(), true);
            open.insert("Packages".to_string(), true);
            open.insert("Outdated".to_string(), true);
            open.insert("Vulnerabilities".to_string(), true);
            open.insert("Processes".to_string(), true);

            let panel = DotNetPanel {
                focus_handle: cx.focus_handle(),
                workspace: workspace.clone(),
                dotnet_ver: None,
                not_found: false,
                cwd: cwd.clone(),
                projects: Vec::new(),
                selected_project: None,
                scanning: false,
                csproj: None,
                packages: Vec::new(),
                outdated_packages: PackagesState::Loading,
                vulnerable_packages: PackagesState::Loading,
                running_action: None,
                error: None,
                dotnet_procs: Vec::new(),
                procs_loaded: false,
                open,
                run_subscription: None,
                _rescan_task: None,
                _data_reload_task: None,
                _procs_poll: Self::spawn_processes_poll(cx),
            };

            init_discovery(cx);

            // Scan again when the project is trusted (or stop when it is not).
            forge_trust::on_trust_changed(cx, |this: &mut DotNetPanel, cx| {
                this.schedule_project_rescan(cx)
            })
            .detach();

            panel
        })
    }

    /// Refreshes `dotnet_procs` from a locally-owned `sysinfo::System` every
    /// `DOTNET_PROCS_TICK_INTERVAL`, filtered to process names containing
    /// "dotnet" — replaces subscribing to a host cockpit panel's broadcast
    /// tick (see the module doc).
    fn spawn_processes_poll(cx: &mut Context<Self>) -> gpui::Task<()> {
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            // Every sysinfo call here walks the whole process table, which
            // takes long enough to stall the UI, so the scans run on the
            // background executor and only the resulting rows hop back to
            // the main thread via `this.update`.
            let mut sys = cx.background_spawn(async { System::new_all() }).await;
            loop {
                let (scanned, procs) = cx
                    .background_spawn(async move {
                        sys.refresh_cpu_all();
                        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
                        let cpu_count = sys.cpus().len() as f32;
                        let procs: Vec<DotnetProcess> = sys
                            .processes()
                            .iter()
                            .filter(|(_, p)| {
                                p.name().to_string_lossy().to_lowercase().contains("dotnet")
                            })
                            .map(|(pid, p)| DotnetProcess {
                                name: p.name().to_string_lossy().to_string(),
                                pid: pid.as_u32(),
                                cpu_percent: p.cpu_usage() / cpu_count,
                                memory_mb: p.memory() as f64 / 1_048_576.0,
                            })
                            .collect();
                        (sys, procs)
                    })
                    .await;
                sys = scanned;

                let alive = this
                    .update(cx, |this, cx| {
                        this.dotnet_procs = procs;
                        this.procs_loaded = true;
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }

                cx.background_executor()
                    .timer(DOTNET_PROCS_TICK_INTERVAL)
                    .await;
            }
        })
    }

    fn is_open(&self, key: &str, default: bool) -> bool {
        *self.open.get(key).unwrap_or(&default)
    }

    fn toggle(&mut self, key: &str) {
        let e = self.open.entry(key.to_string()).or_insert(true);
        *e = !*e;
    }

    /// The directory `dotnet` commands run in — the resolved csproj's parent
    /// (a `.csproj` owns at most one project, which `dotnet list package`
    /// requires), else the scan root. Empty when there's nothing to operate
    /// on.
    fn project_dir(&self) -> String {
        self.csproj
            .as_ref()
            .and_then(|p| {
                std::path::Path::new(p)
                    .parent()
                    .map(|d| d.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| self.cwd.clone())
    }

    fn scan_projects(&mut self, cx: &mut Context<Self>) {
        if self.cwd.is_empty() {
            self.projects.clear();
            return;
        }
        self.scanning = true;
        cx.notify();

        let root = self.cwd.clone();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let projects = cx
                .background_spawn(async move { scan_dotnet_projects(&root, 0) })
                .await;
            let _ = cx.update(|app| {
                if let Some(panel) = this.upgrade() {
                    let _ = panel.update(app, |panel, cx| {
                        panel.scanning = false;
                        panel.projects = projects;
                        // Keep the current selection across rescans; only
                        // (re)select when it's empty or the project it pointed
                        // at disappeared (e.g. a scaffold removed it).
                        let selection_still_exists = panel
                            .selected_project
                            .as_ref()
                            .is_some_and(|selected| {
                                panel.projects.iter().any(|p| &p.path == selected)
                            });
                        if !selection_still_exists {
                            if let Some(first) = panel.projects.first().cloned() {
                                panel.select_project(first.path, cx);
                            } else {
                                panel.selected_project = None;
                                panel.csproj = None;
                                panel.reload_data(cx);
                            }
                        }
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }

    fn select_project(&mut self, path: String, cx: &mut Context<Self>) {
        self.selected_project = Some(path.clone());
        self.csproj = resolve_csproj(&path);
        self.error = None;
        self.reload_data(cx);
        cx.notify();
    }

    /// Re-reads everything that depends on the resolved csproj: installed
    /// packages plus the two `dotnet list package` probes.
    fn reload_data(&mut self, cx: &mut Context<Self>) {
        self.reload_packages(cx);
        self.reload_outdated(cx);
        self.reload_vulnerable(cx);
    }

    fn reload_packages(&mut self, cx: &mut Context<Self>) {
        let Some(csproj) = self.csproj.as_ref().cloned() else {
            self.packages = Vec::new();
            return;
        };
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let packages = cx
                .background_spawn(async move { read_installed_packages(&csproj) })
                .await;
            let _ = cx.update(|app| {
                if let Some(panel) = this.upgrade() {
                    let _ = panel.update(app, |panel, cx| {
                        panel.packages = packages;
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }

    fn reload_outdated(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.project_dir().into_nonempty() else {
            self.outdated_packages = PackagesState::Ready(Vec::new());
            return;
        };
        self.outdated_packages = PackagesState::Loading;
        let this = cx.weak_entity();
        cx.spawn(async move |_: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = cx.background_spawn(async move { list_outdated(dir) }).await;
            let _ = cx.update(|app| {
                if let Some(panel) = this.upgrade() {
                    let _ = panel.update(app, |panel, cx| {
                        panel.outdated_packages = match result {
                            Ok(list) => PackagesState::Ready(list),
                            Err(e) => PackagesState::Error(e),
                        };
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }

    fn reload_vulnerable(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.project_dir().into_nonempty() else {
            self.vulnerable_packages = PackagesState::Ready(Vec::new());
            return;
        };
        self.vulnerable_packages = PackagesState::Loading;
        let this = cx.weak_entity();
        cx.spawn(async move |_: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = cx
                .background_spawn(async move { list_vulnerable(dir) })
                .await;
            let _ = cx.update(|app| {
                if let Some(panel) = this.upgrade() {
                    let _ = panel.update(app, |panel, cx| {
                        panel.vulnerable_packages = match result {
                            Ok(list) => PackagesState::Ready(list),
                            Err(e) => PackagesState::Error(e),
                        };
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }

    /// Coalesces the burst of `WorktreeUpdatedEntries` events a scaffold
    /// produces into one rescan: each call replaces (and so cancels) the
    /// pending task, and the last one to survive the debounce reruns
    /// `scan_projects`.
    fn schedule_project_rescan(&mut self, cx: &mut Context<Self>) {
        self._rescan_task = Some(cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let _ = cx.update(|app| {
                if let Some(panel) = this.upgrade() {
                    let _ = panel.update(app, |panel, cx| {
                        panel.scan_projects(cx);
                    });
                }
            });
        }));
    }

    /// Whether a changed worktree-relative path (`/`-separated) is the active
    /// project's own project file or a NuGet manifest sitting next to it —
    /// i.e. it belongs to the resolved csproj, not some other project under
    /// the same workspace.
    fn manifest_change_is_selected(&self, rel_unix: &str) -> bool {
        let Some(csproj) = self.csproj.as_deref() else {
            return false;
        };
        let root = self.cwd.replace('\\', "/");
        let changed = format!("{}/{}", root.trim_end_matches('/'), rel_unix);
        let Some((changed_dir, changed_name)) = changed.rsplit_once('/') else {
            return false;
        };
        let csproj_unix = csproj.replace('\\', "/");
        let Some((csproj_dir, _)) = csproj_unix.rsplit_once('/') else {
            return false;
        };
        changed_dir == csproj_dir && is_dotnet_package_manifest(changed_name)
    }

    /// Reloads the active project's installed/outdated/vulnerable package
    /// lists after its project file or a package manifest changed on disk —
    /// an install/remove/update run outside the panel (terminal,
    /// `nuget_manager_panel`, `dotnet` CLI), or a hand edit. Debounced like
    /// `schedule_project_rescan`, and skipped while a quick action is
    /// streaming so it can't race that run's own reload.
    fn schedule_data_reload(&mut self, cx: &mut Context<Self>) {
        self._data_reload_task =
            Some(cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let _ = cx.update(|app| {
                    if let Some(panel) = this.upgrade() {
                        let _ = panel.update(app, |panel, cx| {
                            if panel.running_action.is_some() {
                                return;
                            }
                            panel.reload_data(cx);
                        });
                    }
                });
            }));
    }

    /// The live Script Runner dock panel for this workspace, if it's been
    /// loaded yet (`initialize_panels` creates it shortly after startup).
    fn runner(&self, cx: &App) -> Option<Entity<ScriptRunnerPanel>> {
        let workspace = self.workspace.upgrade()?;
        workspace.read(cx).panel::<ScriptRunnerPanel>(cx)
    }

    /// Runs a `dotnet` command for the active project, streaming its output
    /// through the live Script Runner dock panel (the npm manager's §4.2
    /// wiring) — opening that dock so the output is visible — and reloads the
    /// package lists when it finishes.
    ///
    /// Falls back to a silent background run with a spinner when the Script
    /// Runner dock panel isn't loaded yet.
    fn kick_run(
        &mut self,
        label: &str,
        command: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.running_action.is_some() {
            return;
        }
        if self.project_dir().is_empty() {
            self.error = Some("No .NET project found in this workspace.".into());
            cx.notify();
            return;
        }
        let dir = self.project_dir();
        self.running_action = Some(label.to_string());
        self.error = None;
        cx.notify();

        let Some(runner) = self.runner(cx) else {
            return self.background_run(command, cx);
        };

        // The runner can only stream one command at a time — never pile a run
        // onto an already-streaming one.
        if runner.read(cx).is_running() {
            self.running_action = None;
            self.error = Some("Script Runner is busy — wait for it to finish first.".into());
            cx.notify();
            return;
        }

        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.open_panel::<ScriptRunnerPanel>(window, cx);
            });
        }

        runner.update(cx, |runner, cx| runner.run_external(command, dir, cx));

        // Reload the lists when the run finishes: the runner notifies on every
        // output line AND on completion, so this fires exactly then.
        self.run_subscription = Some(cx.observe(&runner, |this, runner, cx| {
            if !runner.read(cx).is_running() {
                this.running_action = None;
                this.reload_data(cx);
                cx.notify();
            }
        }));

        // A blindingly-fast run can finish between `run_external` above and
        // subscribing, which would miss the completion notify — re-check once.
        if !runner.read(cx).is_running() {
            self.running_action = None;
            self.reload_data(cx);
            cx.notify();
        }
    }

    /// Fallback when no Script Runner dock panel exists yet: split the shell
    /// command into its `dotnet` invocations (guarding on the `&&`-chained
    /// bulk updates) and run each through the backend synchronously on a
    /// background task, showing only a spinner and an error on failure.
    fn background_run(&mut self, command: String, cx: &mut Context<Self>) {
        let dir = self.project_dir();
        let this = cx.weak_entity();
        cx.spawn(async move |_: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = cx
                .background_spawn(async move {
                    for segment in command.split(" && ") {
                        let mut parts = segment.split_whitespace();
                        // Every command this panel builds starts with `dotnet`.
                        let _dotnet = parts.next();
                        let args: Vec<String> = parts.map(String::from).collect();
                        dotnet_backend::run_dotnet_args(&args, &dir)?;
                    }
                    Ok::<(), String>(())
                })
                .await;
            let _ = cx.update(|app| {
                if let Some(panel) = this.upgrade() {
                    let _ = panel.update(app, |panel, cx| {
                        panel.running_action = None;
                        match result {
                            Ok(()) => panel.reload_data(cx),
                            Err(e) => {
                                panel.error = Some(e);
                                cx.notify();
                            }
                        }
                    });
                }
            });
        })
        .detach();
    }

    /// Bulk-updates every outdated package — or only the patch-only ones when
    /// `ty` is "patch", else every patch/minor — by running one
    /// `dotnet add package` per target in a single Script Runner run (chained
    /// with `&&`, which the pwsh shell the runner uses understands).
    fn update_all(&mut self, ty: &str, window: &mut Window, cx: &mut Context<Self>) {
        let PackagesState::Ready(list) = &self.outdated_packages else {
            return;
        };
        match update_all_command(list, ty) {
            Ok(Some(command)) => self.kick_run("update all", command, window, cx),
            Ok(None) => {}
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }
}

// ── Background init ───────────────────────────────────────────────────

fn init_discovery(cx: &mut Context<DotNetPanel>) {
    cx.spawn(
        async move |this: WeakEntity<DotNetPanel>, cx: &mut AsyncApp| {
            let dotnet_ver = cx
                .background_spawn(async { query_runtime("dotnet".into()).ok() })
                .await;
            let not_found = dotnet_ver.is_none();

            let _ = cx.update(|app| {
                if let Some(panel) = this.upgrade() {
                    let _ = panel.update(app, |panel, cx| {
                        panel.dotnet_ver = dotnet_ver;
                        panel.not_found = not_found;
                        if !not_found {
                            panel.scan_projects(cx);
                        }
                        cx.notify();
                    });
                }
            });
        },
    )
    .detach();
}

// ── Section header helper ─────────────────────────────────────────────

fn section_container(
    cx: &Context<DotNetPanel>,
    title: &str,
    count: usize,
    open: bool,
    body: impl IntoElement,
) -> impl IntoElement {
    let key = title.to_string();

    let header = div()
        .id(format!("dotnet-section-{key}"))
        .flex()
        .items_center()
        .gap_1()
        .px_3()
        .py_1()
        .text_xs()
        .cursor_pointer()
        .hover(|d| d.bg(cx.theme().list_hover))
        .child(
            Icon::new(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .xsmall()
            .text_color(cx.theme().muted_foreground),
        )
        .child(
            div()
                .flex_1()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(cx.theme().foreground)
                .child(title.to_string()),
        )
        .child(Tag::secondary().xsmall().child(count.to_string()))
        .on_click(cx.listener(move |this, _e, _w, _cx| {
            this.toggle(&key);
        }));

    Collapsible::new()
        .open(open)
        .child(header)
        .content(body)
        .flex()
        .flex_col()
        .w_full()
        .border_b_1()
        .border_color(cx.theme().border)
}

// ── Render ────────────────────────────────────────────────────────────

impl Render for DotNetPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel_header = PanelHeader::new("Dotnet").icon(
            svg()
                .path("icons/dotnet.svg")
                .size(px(16.0))
                .text_color(cx.theme().foreground),
        );

        if self.not_found {
            return div()
                .id("dotnet-panel-empty")
                .track_focus(&self.focus_handle(cx))
                .flex()
                .flex_col()
                .w_full()
                .h_full()
                .bg(cx.theme().background)
                .child(panel_header)
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .items_center()
                        .justify_center()
                        .p_4()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("dotnet not found in PATH"),
                )
                .into_any_element();
        }

        let projects_section = section_container(
            cx,
            "Projects",
            self.projects.len(),
            self.is_open("Projects", true),
            self.render_projects(cx),
        );

        let actions_section = section_container(
            cx,
            "Quick Actions",
            0,
            self.is_open("Quick Actions", true),
            self.render_quick_actions(window, cx),
        );

        let packages_section = section_container(
            cx,
            "Packages",
            self.packages.len(),
            self.is_open("Packages", true),
            self.render_packages(cx),
        );

        let outdated_section = section_container(
            cx,
            "Outdated",
            self.outdated_count(),
            self.is_open("Outdated", true),
            self.render_outdated(cx),
        );

        let vulnerable_section = section_container(
            cx,
            "Vulnerabilities",
            self.vulnerable_count(),
            self.is_open("Vulnerabilities", true),
            self.render_vulnerabilities(cx),
        );

        let processes_section = section_container(
            cx,
            "Processes",
            self.dotnet_procs.len(),
            self.is_open("Processes", true),
            self.render_processes(cx),
        );

        let mut body = div()
            .flex()
            .flex_col()
            .child(self.render_versions(cx))
            .child(projects_section)
            .child(actions_section)
            .child(packages_section)
            .child(outdated_section)
            .child(vulnerable_section)
            .child(processes_section);

        if let Some(err) = &self.error {
            body = body.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(err.clone()),
            );
        }

        div()
            .id("dotnet-panel")
            .track_focus(&self.focus_handle(cx))
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .overflow_hidden()
            .bg(cx.theme().background)
            .child(panel_header)
            .child(
                div()
                    .id("dotnet-scroll")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .child(body),
            )
            .into_any_element()
    }
}

impl Focusable for DotNetPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DotNetPanel {}

impl Panel for DotNetPanel {
    fn persistent_name() -> &'static str {
        "Dotnet Panel"
    }

    fn panel_key() -> &'static str {
        "DotnetPanel"
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        match DotnetPanelSettings::get_global(cx).dock {
            DockSide::Left => DockPosition::Left,
            DockSide::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        settings::update_settings_file(<dyn fs::Fs>::global(cx), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left | DockPosition::Bottom => DockSide::Left,
                DockPosition::Right => DockSide::Right,
            };
            settings.dotnet_panel.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<ui::IconName> {
        DotnetPanelSettings::get_global(cx).button.then_some(ui::IconName::Dotnet)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Dotnet")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        27
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.dotnet_panel.get_or_insert_default().button = Some(false);
        }))
    }
}

// ── Section renderers ─────────────────────────────────────────────────

impl DotNetPanel {
    fn render_versions(&self, cx: &Context<Self>) -> impl IntoElement {
        let text = match &self.dotnet_ver {
            Some(v) => format!("dotnet {v}"),
            None => "dotnet not detected".into(),
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .font_family("Cascadia Mono")
                    .text_xs()
                    .text_color(cx.theme().primary)
                    .child(text),
            )
    }

    fn render_projects(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut col = div().flex().flex_col();

        if self.scanning {
            col = col.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .px_3()
                    .py_2()
                    .child(Spinner::new().xsmall())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Scanning\u{2026}"),
                    ),
            );
        } else {
            if self.projects.is_empty() {
                col = col.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("No .NET projects found"),
                );
            }
            for proj in &self.projects {
                let selected = self.selected_project.as_deref() == Some(&proj.path);
                let color = if selected {
                    cx.theme().primary
                } else {
                    cx.theme().foreground
                };
                let proj_path = proj.path.clone();
                let is_solution = proj.is_solution;
                col = col.child(
                    div()
                        .id(format!("proj-{}", proj.path))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_0p5()
                        .text_xs()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(cx.theme().list_active))
                        .hover(|d| d.bg(cx.theme().list_hover))
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.select_project(proj_path.clone(), cx);
                        }))
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .font_family("Cascadia Mono")
                                .text_color(color)
                                .child(proj.name.clone()),
                        )
                        .when(is_solution, |d| {
                            d.child(Tag::secondary().xsmall().child("sln"))
                        }),
                );
            }
        }

        col
    }

    fn render_quick_actions(&self, window: &mut Window, cx: &Context<Self>) -> impl IntoElement {
        let _ = window; // inputs created in click handlers
        let project_ready = self.csproj.is_some();
        let busy = self.running_action.is_some();
        let busy_label = self.running_action.clone();

        let mut row = div().flex().flex_row().flex_wrap().gap_1().px_3().py_1();

        let actions: [(&str, &str); 4] = [
            ("build", "dotnet build"),
            ("run", "dotnet run"),
            ("test", "dotnet test"),
            ("restore", "dotnet restore"),
        ];

        for (label, command) in actions {
            let command = command.to_string();
            row = row.child(
                Button::new(format!("quick-{label}"))
                    .secondary()
                    .xsmall()
                    .label(label)
                    .disabled(!project_ready || busy)
                    .tooltip(format!("dotnet {label} — runs in the Script Runner"))
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.kick_run(&label, command.clone(), window, cx);
                    })),
            );
        }

        // `open_nuget_manager` (re-added, mirroring the Node panel's npm
        // manager button): opens the NuGet manager workspace tab rooted on the
        // current project.
        row = row.child(
            Button::new("quick-action-nuget-mgr")
                .secondary()
                .xsmall()
                .label("NuGet Manager")
                .tooltip("Open NuGet Manager")
                .on_click(cx.listener(|this, _e, window, cx| {
                    // Use the `Window` already handed to this click handler
                    // rather than `WeakEntity::update_in`'s own internal
                    // window lookup, which turned out unreliable for this
                    // kind of cross-entity call — see `node_panel`'s
                    // equivalent "Package Manager" button fix.
                    let root = this.project_dir();
                    if root.is_empty() {
                        return;
                    }
                    if let Some(workspace) = this.workspace.upgrade() {
                        workspace.update(cx, |workspace, cx| {
                            nuget_manager_panel::open(root, workspace, window, cx);
                        });
                    }
                })),
        );

        if let Some(label) = &busy_label {
            row = row.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .px_1()
                    .child(Spinner::new().xsmall())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{label}\u{2026}")),
                    ),
            );
        }

        row
    }

    fn render_packages(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut col = div().flex().flex_col();

        if self.packages.is_empty() {
            col = col.child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("No installed NuGet packages"),
            );
        } else {
            for pkg in &self.packages {
                col = col.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_0p5()
                        .text_xs()
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .font_family("Cascadia Mono")
                                .text_color(cx.theme().foreground)
                                .child(pkg.id.clone()),
                        )
                        .child(
                            div()
                                .font_family("Cascadia Mono")
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("@{}", pkg.version)),
                        ),
                );
            }
        }

        col
    }

    /// Outdated-package count, also used by the Cockpit dashboard's
    /// Runtimes section (see `dashboard_panel`).
    pub fn outdated_count(&self) -> usize {
        match &self.outdated_packages {
            PackagesState::Ready(list) => list.len(),
            _ => 0,
        }
    }

    /// Vulnerable-package count, also used by the Cockpit dashboard's
    /// Runtimes section (see `dashboard_panel`).
    pub fn vulnerable_count(&self) -> usize {
        match &self.vulnerable_packages {
            PackagesState::Ready(list) => list.len(),
            _ => 0,
        }
    }

    /// The active scan target's display label — the resolved project's
    /// folder name when one was found, else the scan root's. Empty when
    /// there's nothing to scan.
    pub fn active_project_label(&self) -> String {
        path_file_name(&self.project_dir())
    }

    /// The active project's vulnerable-package findings, when the scan
    /// finished (see `vulnerable_loading`/`vulnerable_error`). `None` while
    /// loading, on error, or with no project resolved — the Cockpit
    /// dashboard's Security section reads this for per-finding rows.
    pub fn vulnerable_findings(&self) -> Option<&[VulnerablePackage]> {
        match &self.vulnerable_packages {
            PackagesState::Ready(list) => Some(list),
            _ => None,
        }
    }

    /// Whether the vulnerability scan is currently in flight.
    pub fn vulnerable_loading(&self) -> bool {
        matches!(&self.vulnerable_packages, PackagesState::Loading)
    }

    /// The scan's error message, if the last scan failed.
    pub fn vulnerable_error(&self) -> Option<&str> {
        match &self.vulnerable_packages {
            PackagesState::Error(e) => Some(e),
            _ => None,
        }
    }

    /// Re-runs the vulnerability scan for the active project, driven by the
    /// Cockpit dashboard's Security section (`reload_vulnerable` does the
    /// work).
    pub fn rescan_vulnerabilities(&mut self, cx: &mut Context<Self>) {
        self.reload_vulnerable(cx);
    }

    fn render_outdated(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut col = div().flex().flex_col();

        match &self.outdated_packages {
            PackagesState::Loading => {
                col = col.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .px_3()
                        .py_2()
                        .child(Spinner::new().xsmall())
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Checking for updates\u{2026}"),
                        ),
                );
            }
            PackagesState::Error(err) => {
                col = col.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .text_color(cx.theme().danger)
                                .child(err.clone()),
                        )
                        .child(
                            Button::new("outdated-retry")
                                .secondary()
                                .xsmall()
                                .label("Retry")
                                .on_click(cx.listener(|this, _e, _w, cx| {
                                    this.reload_outdated(cx);
                                })),
                        ),
                );
            }
            PackagesState::Ready(list) => {
                if list.is_empty() {
                    col = col.child(
                        div()
                            .px_3()
                            .py_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("All packages up to date"),
                    );
                } else {
                    col = col.child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .items_center()
                            .px_3()
                            .py_1()
                            .child(
                                Button::new("update-all-safe")
                                    .secondary()
                                    .xsmall()
                                    .label("Update all (minor)")
                                    .on_click(cx.listener(|this, _e, window, cx| {
                                        this.update_all("safe", window, cx);
                                    })),
                            )
                            .child(
                                Button::new("update-all-patch")
                                    .secondary()
                                    .xsmall()
                                    .label("Update all (patch)")
                                    .on_click(cx.listener(|this, _e, window, cx| {
                                        this.update_all("patch", window, cx)
                                    })),
                            ),
                    );

                    for pkg in list {
                        let kind = pkg.update_kind;
                        // Red is reserved for the Vulnerabilities section —
                        // "major" here doesn't mean "broken", just "bigger
                        // diff to review", so it gets amber instead. Minor
                        // and patch use two distinct, quieter colors rather
                        // than both reading as "fine" in the same shade.
                        let (kind_color, kind_label) = match kind {
                            UpdateKind::Major => (cx.theme().warning, "major"),
                            UpdateKind::Minor => (cx.theme().info, "minor"),
                            UpdateKind::Patch => (cx.theme().muted_foreground, "patch"),
                        };
                        col = col.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_0p5()
                                .text_xs()
                                .child(
                                    div()
                                        .flex_1()
                                        .truncate()
                                        .font_family("Cascadia Mono")
                                        .text_color(cx.theme().foreground)
                                        .child(pkg.id.clone()),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .font_family("Cascadia Mono")
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!(
                                            "{} \u{2192} {}",
                                            pkg.installed, pkg.latest
                                        )),
                                )
                                .child(
                                    div()
                                        .rounded_md()
                                        .px_1()
                                        .py_0p5()
                                        .bg(kind_color.opacity(0.2))
                                        .text_color(kind_color)
                                        .child(kind_label),
                                ),
                        );
                    }
                }
            }
        }

        col
    }

    fn render_vulnerabilities(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut col = div().flex().flex_col();

        match &self.vulnerable_packages {
            PackagesState::Loading => {
                col = col.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .px_3()
                        .py_2()
                        .child(Spinner::new().xsmall())
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Scanning for vulnerabilities\u{2026}"),
                        ),
                );
            }
            PackagesState::Error(err) => {
                col = col.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .text_color(cx.theme().danger)
                                .child(err.clone()),
                        )
                        .child(
                            Button::new("vulnerable-retry")
                                .secondary()
                                .xsmall()
                                .label("Retry")
                                .on_click(cx.listener(|this, _e, _w, cx| {
                                    this.reload_vulnerable(cx);
                                })),
                        ),
                );
            }
            PackagesState::Ready(list) => {
                if list.is_empty() {
                    col = col.child(
                        div()
                            .px_3()
                            .py_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("No known vulnerabilities"),
                    );
                } else {
                    for vuln in list {
                        let severity_color = match vuln.severity.as_str() {
                            "critical" | "high" => cx.theme().danger,
                            "moderate" => cx.theme().warning,
                            _ => cx.theme().success,
                        };
                        col = col.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_0p5()
                                .text_xs()
                                .child(
                                    div()
                                        .flex_1()
                                        .truncate()
                                        .font_family("Cascadia Mono")
                                        .text_color(cx.theme().foreground)
                                        .child(vuln.id.clone()),
                                )
                                .child(
                                    div()
                                        .rounded_md()
                                        .px_1()
                                        .py_0p5()
                                        .bg(severity_color.opacity(0.2))
                                        .text_color(severity_color)
                                        .child(vuln.severity.clone()),
                                ),
                        );
                        col = col.child(
                            div()
                                .px_3()
                                .pb_1()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(vuln.advisory_url.clone()),
                        );
                    }
                }
            }
        }

        col
    }

    fn render_processes(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut col = div().flex().flex_col();

        if !self.procs_loaded {
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(Spinner::new().xsmall())
                    .child("Loading processes\u{2026}"),
            );
        } else if self.dotnet_procs.is_empty() {
            col = col.child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("No dotnet processes running"),
            );
        } else {
            col = col.child(
                div()
                    .flex()
                    .px_3()
                    .py_0p5()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(div().w_16().child("PID"))
                    .child(div().flex_1().child("Name"))
                    .child(div().w_16().text_right().child("CPU"))
                    .child(div().w_16().text_right().child("Mem")),
            );

            for proc in &self.dotnet_procs {
                col = col.child(
                    div()
                        .flex()
                        .px_3()
                        .py_0p5()
                        .text_xs()
                        .text_color(cx.theme().foreground)
                        .child(
                            div()
                                .w_16()
                                .font_family("Cascadia Mono")
                                .child(proc.pid.to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .font_family("Cascadia Mono")
                                .child(proc.name.clone()),
                        )
                        .child(
                            div()
                                .w_16()
                                .text_right()
                                .font_family("Cascadia Mono")
                                .child(format!("{:.1}%", proc.cpu_percent)),
                        )
                        .child(
                            div()
                                .w_16()
                                .text_right()
                                .font_family("Cascadia Mono")
                                .child(format!("{:.0}MB", proc.memory_mb)),
                        ),
                );
            }
        }

        col
    }
}

// ── Small helpers ─────────────────────────────────────────────────────

trait NonEmpty {
    fn into_nonempty(self) -> Option<Self>
    where
        Self: Sized;
}

impl NonEmpty for String {
    /// `Some(self)` when non-empty. Lets the `letter?` reload guards read
    /// naturally while keeping `project_dir` returning a plain string.
    fn into_nonempty(self) -> Option<Self> {
        if self.is_empty() { None } else { Some(self) }
    }
}

/// The command for the Outdated section's bulk buttons: one
/// `dotnet add package <id> --version <latest>` per qualifying package,
/// chained with `&&` — patch-only updates when `ty` is `"patch"`, otherwise
/// patch and minor. `Ok(None)` when nothing qualifies; an error (and nothing
/// run) if any qualifying entry isn't shell-plain.
fn update_all_command(outdated: &[OutdatedPackage], ty: &str) -> Result<Option<String>, String> {
    let mut segments = Vec::new();
    for package in outdated.iter().filter(|o| {
        let kind = classify_update(&o.installed, &o.latest);
        match ty {
            "patch" => kind == UpdateKind::Patch,
            _ => matches!(kind, UpdateKind::Patch | UpdateKind::Minor),
        }
    }) {
        check_package_name(&package.id)?;
        check_version(&package.latest)?;
        segments.push(format!(
            "dotnet add package {} --version {}",
            package.id, package.latest
        ));
    }
    Ok((!segments.is_empty()).then(|| segments.join(" && ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outdated(id: &str, installed: &str, latest: &str) -> OutdatedPackage {
        OutdatedPackage {
            id: id.to_string(),
            installed: installed.to_string(),
            latest: latest.to_string(),
            update_kind: classify_update(installed, latest),
        }
    }

    #[test]
    fn path_file_name_is_the_last_component() {
        assert_eq!(path_file_name("C:/work/src/App/App.csproj"), "App.csproj");
        assert_eq!(path_file_name("single"), "single");
        assert_eq!(path_file_name("/"), "/");
    }

    #[test]
    fn update_all_chains_one_command_per_package() {
        let list = [
            outdated("Dapper", "2.1.0", "2.1.35"),
            outdated("Serilog", "3.0.0", "3.1.0"),
            outdated("Polly", "7.0.0", "8.0.0"),
        ];
        assert_eq!(
            update_all_command(&list, "minor").unwrap().as_deref(),
            Some(
                "dotnet add package Dapper --version 2.1.35 && \
                 dotnet add package Serilog --version 3.1.0"
            )
        );
        assert_eq!(
            update_all_command(&list, "patch").unwrap().as_deref(),
            Some("dotnet add package Dapper --version 2.1.35")
        );
    }

    #[test]
    fn update_all_with_nothing_safe_to_update() {
        assert_eq!(update_all_command(&[outdated("Polly", "7.0.0", "8.0.0")], "minor"), Ok(None));
        assert_eq!(update_all_command(&[], "patch"), Ok(None));
    }

    #[test]
    fn update_all_runs_nothing_if_any_entry_is_unsafe() {
        let list = [outdated("Dapper", "2.1.0", "2.1.35"), outdated("Bad && calc", "1.0.0", "1.0.1")];
        assert!(update_all_command(&list, "minor").is_err());
    }

    #[test]
    fn dotnet_manifest_files_are_recognized() {
        assert!(is_dotnet_project_file("App.csproj"));
        assert!(is_dotnet_project_file("Everything.sln"));
        assert!(!is_dotnet_project_file("App.fsproj"));
        assert!(is_dotnet_package_manifest("App.csproj"));
        assert!(is_dotnet_package_manifest("packages.config"));
        assert!(is_dotnet_package_manifest("Directory.Packages.props"));
        assert!(!is_dotnet_package_manifest("Program.cs"));
        assert!(!is_dotnet_package_manifest("App.csproj.bak"));
    }
}

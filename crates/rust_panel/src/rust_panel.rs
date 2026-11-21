//! Rust panel — toolchain versions, the workspace's crates, and for the
//! selected crate its dependencies, what is behind the registry, and the
//! security advisories that apply.
//!
//! Not a Forge port: written for this tree from
//! `docs/Rust_Manager_Design_Note.md`, in the shape of the other runtime
//! panels (`dotnet_panel` is the closest sibling). All of the working-out
//! lives in `cargo_backend`; this file is the dock panel and the wiring to
//! the Script Runner. The HTTP requests `cargo_backend` leaves to its host
//! are shared with the manager tab, in `cargo_manager_panel::registry`.
//!
//! The rule from the design note that shapes this panel: **it never depends
//! on rust-analyzer.** A Rust project is recognized by its `Cargo.toml` on
//! disk, lists come from Cargo's own files, and nothing waits for a language
//! server. Three consequences visible below:
//!
//! - Opening the panel runs one cheap local command
//!   (`cargo metadata --no-deps`) and reads `Cargo.lock`. Nothing compiles.
//! - The registry check for newer versions runs when a crate is selected,
//!   one small request per crates.io dependency, cached for ten minutes
//!   (the lifetime the index itself asks for).
//! - The advisory scan is **on demand** — a button, not automatic. It asks
//!   OSV about every package reachable from the crate (1,425 for `zed`),
//!   which is too much to fire on every click through the crate list. Until
//!   it has run, the section says "not scanned yet"; it never looks clean by
//!   default.
//!
//! "Does it still build?" is likewise only ever an explicit quick action.
//!
//! Path and git dependencies are not listed (design decision 10); a single
//! line says how many were left out.

mod rust_panel_settings;

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use cargo_backend::{
    CrateInfo, DependencyKind, DependencyList, Finding, FindingCounts, FindingKind,
    LockedPackage, Lockfile, RustCompat, Severity, UpdateTarget, direct_dependencies,
    has_lockfile, is_cargo_project, load_workspace, merge_findings, query_cargo, query_rustc,
    read_lockfile,
};
use cargo_manager_panel::registry::{
    AdvisoryRecords, AdvisoryState, IndexCache, OutdatedRow, OutdatedState, ScanResult,
    SharedScans, fetch_index_entries, outdated_rows, publish_scan, run_advisory_scan, scan_key,
    shared_scan, stale_names, store_index_entries,
};
use gpui::{
    Action, App, AppContext as _, AsyncApp, AsyncWindowContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity,
    Window, actions, div, prelude::FluentBuilder as _, px, svg,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    collapsible::Collapsible,
    h_flex,
    input::{Input, InputEvent, InputState},
    panel_header::PanelHeader,
    spinner::Spinner,
    tag::Tag,
};
use project::Project;
use script_runner_panel::ScriptRunnerPanel;
use script_runner_panel::command::check_package_name;
pub use rust_panel_settings::RustPanelSettings;
use settings::{DockSide, Settings as _};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    rust_panel,
    [
        /// Toggles focus on the Rust panel.
        ToggleFocus
    ]
);

/// Registers the Rust panel's actions on every workspace. Call once at app
/// startup, alongside the other panels' `init` functions.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<RustPanel>(window, cx);
        });
    })
    .detach();
}

// ── Pure helpers ───────────────────────────────────────────────────────

/// The quick actions, as (label, cargo subcommand).
const QUICK_ACTIONS: [&str; 4] = ["check", "build", "run", "test"];

/// `cargo <action> -p <crate>`, run from the workspace root.
///
/// `-p` rather than changing directory: in a workspace it is what scopes the
/// command to one crate while still using the shared `target` directory and
/// lockfile. The crate name is validated like every other value this fork
/// places into a Script Runner command.
fn cargo_command(action: &str, crate_name: &str) -> Result<String, String> {
    check_package_name(crate_name)?;
    Ok(format!("cargo {action} -p {crate_name}"))
}

/// The crates whose name contains `query`, case-insensitively. An empty
/// query keeps them all.
fn filter_crates<'a>(crates: &'a [CrateInfo], query: &str) -> Vec<&'a CrateInfo> {
    let query = query.trim().to_lowercase();
    crates
        .iter()
        .filter(|krate| query.is_empty() || krate.name.to_lowercase().contains(&query))
        .collect()
}

/// Whether a changed file is one this panel reads — the trigger for
/// reloading without polling.
fn is_manifest_file(file_name: &str) -> bool {
    file_name == cargo_backend::MANIFEST_FILE || file_name == cargo_backend::LOCK_FILE
}

// ── State ──────────────────────────────────────────────────────────────

/// Where the local load (workspace + lockfile) stands.
enum LoadState {
    Loading,
    /// The opened folder has no `Cargo.toml` at its root.
    NoManifest,
    Ready,
    Failed(String),
}

pub struct RustPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,

    rustc_version: Option<String>,
    cargo_version: Option<String>,
    /// `cargo` could not be run at all.
    cargo_missing: bool,

    /// The folder the scan is rooted on: the project's first worktree.
    root: String,
    load: LoadState,
    cargo_workspace: Option<cargo_backend::Workspace>,
    lockfile: Option<Lockfile>,
    selected_crate: Option<String>,
    dependencies: DependencyList,

    index: IndexCache,
    outdated: OutdatedState,
    advisories: AdvisoryState,
    advisory_records: AdvisoryRecords,

    crate_filter: Entity<InputState>,
    open: HashMap<String, bool>,

    /// Label of the quick action currently streaming in the Script Runner.
    running_action: Option<String>,
    error: Option<String>,
    run_subscription: Option<Subscription>,

    // Dropping a task cancels it, so replacing one of these is how a newer
    // request supersedes an older one.
    load_task: Option<Task<()>>,
    outdated_task: Option<Task<()>>,
    advisory_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

fn first_worktree_root(project: &Entity<Project>, cx: &App) -> Option<String> {
    project
        .read(cx)
        .worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
}

impl RustPanel {
    /// Loads the panel for a workspace, following the same
    /// `WeakEntity<Workspace>` + `AsyncWindowContext` convention as the
    /// other dock panels' `load` functions (see `initialize_panels` in
    /// `zed::zed`).
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            RustPanel::new(workspace, window, cx)
        })
    }

    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let root = first_worktree_root(&project, cx).unwrap_or_default();
        let workspace = cx.entity().downgrade();

        cx.new(|cx| {
            let crate_filter =
                cx.new(|cx| InputState::new(window, cx).placeholder("Filter crates…"));

            let filter_subscription = cx.subscribe(
                &crate_filter,
                |_: &mut Self, _: Entity<InputState>, event: &InputEvent, cx: &mut Context<Self>| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            );

            // Reload when a manifest or the lockfile changes on disk, however
            // it changed (this panel, a terminal, a git checkout). This is
            // what replaces polling.
            let project_subscription = cx.subscribe(
                &project,
                |this: &mut Self, project, event: &project::Event, cx| match event {
                    project::Event::WorktreeUpdatedEntries(_, changes) => {
                        let touched = changes.iter().any(|(path, _, change)| {
                            !matches!(change, project::PathChange::Loaded)
                                && path.file_name().is_some_and(is_manifest_file)
                        });
                        if touched {
                            this.reload(cx);
                        }
                    }
                    project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => {
                        let root = first_worktree_root(&project, cx).unwrap_or_default();
                        if root != this.root {
                            this.root = root;
                            this.selected_crate = None;
                            this.reload(cx);
                        }
                    }
                    _ => {}
                },
            );

            let scan_subscription =
                cx.observe_global::<SharedScans>(|this: &mut Self, cx| this.adopt_shared_scan(cx));

            // Scan again when the project is trusted (or stop when it is not).
            let trust_subscription = forge_trust::on_trust_changed(cx, |this: &mut Self, cx| {
                this.detect_toolchain(cx);
                this.reload(cx);
            });

            let mut open = HashMap::new();
            for section in [
                "Crates",
                "Quick Actions",
                "Dependencies",
                "Outdated",
                "Advisories",
            ] {
                open.insert(section.to_string(), true);
            }

            let mut panel = RustPanel {
                focus_handle: cx.focus_handle(),
                workspace,
                rustc_version: None,
                cargo_version: None,
                cargo_missing: false,
                root,
                load: LoadState::Loading,
                cargo_workspace: None,
                lockfile: None,
                selected_crate: None,
                dependencies: DependencyList::default(),
                index: IndexCache::new(),
                outdated: OutdatedState::Idle,
                advisories: AdvisoryState::NotScanned,
                advisory_records: AdvisoryRecords::new(),
                crate_filter,
                open,
                running_action: None,
                error: None,
                run_subscription: None,
                load_task: None,
                outdated_task: None,
                advisory_task: None,
                _subscriptions: vec![
                    filter_subscription,
                    project_subscription,
                    scan_subscription,
                    trust_subscription,
                ],
            };
            panel.detect_toolchain(cx);
            panel.reload(cx);
            panel
        })
    }

    // ── Read-only accessors, for the Dashboard ──

    /// The installed `rustc` version, once detected.
    pub fn rustc_version(&self) -> Option<&str> {
        self.rustc_version.as_deref()
    }

    /// How many crates the open workspace has.
    pub fn crate_count(&self) -> usize {
        self.cargo_workspace
            .as_ref()
            .map_or(0, |workspace| workspace.crates.len())
    }

    /// The selected crate's name.
    pub fn selected_crate_name(&self) -> Option<&str> {
        self.selected_crate.as_deref()
    }

    /// How many of the selected crate's dependencies are behind the
    /// registry, as far as the last check got.
    pub fn outdated_count(&self) -> usize {
        self.outdated_rows().len()
    }

    /// The selected crate's vulnerability count — `None` until a scan has
    /// finished, so "not scanned" is never read as zero.
    pub fn vulnerability_count(&self) -> Option<usize> {
        match &self.advisories {
            AdvisoryState::Done { findings, .. } => {
                Some(FindingCounts::of(findings).vulnerabilities)
            }
            _ => None,
        }
    }

    /// The selected crate's findings of every kind (vulnerabilities, then
    /// unsound, unmaintained and other notices) — `None` until a scan has
    /// finished.
    pub fn advisory_findings(&self) -> Option<&[Finding]> {
        match &self.advisories {
            AdvisoryState::Done { findings, .. } => Some(findings),
            _ => None,
        }
    }

    /// Whether an advisory scan is running.
    pub fn advisories_scanning(&self) -> bool {
        matches!(self.advisories, AdvisoryState::Scanning)
    }

    /// Why the last advisory scan failed, if it did.
    pub fn advisory_scan_error(&self) -> Option<&str> {
        match &self.advisories {
            AdvisoryState::Failed(error) => Some(error),
            _ => None,
        }
    }

    /// Scans the selected crate for advisories, as the panel's own Scan
    /// button does. Does nothing while a scan is already running.
    pub fn rescan_advisories(&mut self, cx: &mut Context<Self>) {
        if !self.advisories_scanning() {
            self.scan_advisories(cx);
        }
    }

    fn is_open(&self, key: &str) -> bool {
        *self.open.get(key).unwrap_or(&true)
    }

    fn toggle(&mut self, key: &str) {
        let open = self.open.entry(key.to_string()).or_insert(true);
        *open = !*open;
    }

    fn selected(&self) -> Option<&CrateInfo> {
        let workspace = self.cargo_workspace.as_ref()?;
        workspace.find(self.selected_crate.as_deref()?)
    }

    fn outdated_rows(&self) -> Vec<OutdatedRow> {
        outdated_rows(
            &self.dependencies,
            &self.index,
            self.rustc_version.as_deref(),
        )
    }

    // ── Toolchain ──

    fn detect_toolchain(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let (cargo, rustc) = cx
                .background_spawn(async { (query_cargo(), query_rustc()) })
                .await;
            this.update(cx, |this, cx| {
                this.cargo_missing = cargo.is_err();
                this.cargo_version = cargo.ok();
                this.rustc_version = rustc.ok();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ── Local load: workspace + lockfile ──

    /// Re-reads the workspace and lockfile from disk. Cheap (about a second
    /// on this fork) and local: no network, nothing compiled.
    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.root.is_empty() {
            self.load = LoadState::NoManifest;
            self.cargo_workspace = None;
            self.lockfile = None;
            self.dependencies = DependencyList::default();
            cx.notify();
            return;
        }
        if self.cargo_workspace.is_none() {
            self.load = LoadState::Loading;
        }
        cx.notify();

        let root = self.root.clone();
        self.load_task = Some(cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = cx
                .background_spawn(async move {
                    if !is_cargo_project(&root) {
                        return Ok(None);
                    }
                    let workspace = load_workspace(&root)?;
                    let lockfile = if has_lockfile(&workspace.root) {
                        Some(read_lockfile(&workspace.root)?)
                    } else {
                        None
                    };
                    Ok::<_, String>(Some((workspace, lockfile)))
                })
                .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(Some((workspace, lockfile))) => {
                        // Keep the selection across a reload when the crate
                        // still exists; otherwise fall back to the default.
                        let selection = this
                            .selected_crate
                            .take()
                            .filter(|name| workspace.find(name).is_some())
                            .or_else(|| workspace.default_crate().map(|krate| krate.name.clone()));
                        this.cargo_workspace = Some(workspace);
                        this.lockfile = lockfile;
                        this.load = LoadState::Ready;
                        match selection {
                            Some(name) => this.select_crate(name, cx),
                            None => this.dependencies = DependencyList::default(),
                        }
                    }
                    Ok(None) => {
                        this.load = LoadState::NoManifest;
                        this.cargo_workspace = None;
                        this.lockfile = None;
                        this.dependencies = DependencyList::default();
                    }
                    Err(error) => this.load = LoadState::Failed(error),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn select_crate(&mut self, name: String, cx: &mut Context<Self>) {
        let changed = self.selected_crate.as_deref() != Some(name.as_str());
        self.selected_crate = Some(name);
        self.dependencies = self
            .selected()
            .map(|krate| direct_dependencies(krate, self.lockfile.as_ref()))
            .unwrap_or_default();
        self.error = None;

        // A scan's findings belong to one crate at one lockfile state. Any
        // reload or reselection puts the section back to "not scanned" —
        // stale results would look like a clean bill of health.
        if changed || matches!(self.advisories, AdvisoryState::Done { .. }) {
            self.advisory_task = None;
            self.advisories = AdvisoryState::NotScanned;
        }
        // Unless that same set of packages has already been scanned.
        self.adopt_shared_scan(cx);
        self.check_outdated(cx);
        cx.notify();
    }

    // ── Registry check for newer versions ──

    /// Looks up, in the crates.io sparse index, every listed dependency that
    /// isn't already cached and fresh.
    fn check_outdated(&mut self, cx: &mut Context<Self>) {
        let names = stale_names(&self.dependencies, &self.index, Instant::now());
        if names.is_empty() {
            self.outdated_task = None;
            self.outdated = if self.dependencies.listed.is_empty() {
                OutdatedState::Idle
            } else {
                OutdatedState::Done { failed: 0 }
            };
            return;
        }

        self.outdated = OutdatedState::Checking;
        let client = cx.http_client();
        self.outdated_task = Some(cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let results = fetch_index_entries(&client, names).await;
            this.update(cx, |this, cx| {
                let failed = store_index_entries(&mut this.index, results);
                this.outdated = OutdatedState::Done { failed };
                cx.notify();
            })
            .ok();
        }));
    }

    // ── Advisory scan ──

    /// Shows the result of a scan already run for exactly these packages,
    /// here or in the crate's other view, instead of "not scanned yet".
    fn adopt_shared_scan(&mut self, cx: &mut Context<Self>) {
        if matches!(self.advisories, AdvisoryState::Scanning) {
            return;
        }
        let (Some(krate), Some(lockfile)) = (self.selected(), &self.lockfile) else {
            return;
        };
        let reachable: Vec<LockedPackage> = lockfile
            .reachable_crates_io_packages(&krate.name, &krate.version)
            .into_iter()
            .cloned()
            .collect();
        if let Some(result) = shared_scan(scan_key(&reachable), cx) {
            self.advisories = result.into_state();
            cx.notify();
        }
    }

    /// Asks OSV about every crates.io package reachable from the selected
    /// crate, fetches the records it names, and merges them into findings.
    fn scan_advisories(&mut self, cx: &mut Context<Self>) {
        let Some(krate) = self.selected() else {
            return;
        };
        let Some(lockfile) = &self.lockfile else {
            self.advisories = AdvisoryState::Failed(
                "There is no Cargo.lock yet, so there are no exact versions to check. \
                 Build the project once to create it."
                    .to_string(),
            );
            cx.notify();
            return;
        };
        let reachable: Vec<LockedPackage> = lockfile
            .reachable_crates_io_packages(&krate.name, &krate.version)
            .into_iter()
            .cloned()
            .collect();
        let scanned = reachable.len();
        let key = scan_key(&reachable);

        self.advisories = AdvisoryState::Scanning;
        cx.notify();

        let client = cx.http_client();
        let cached = self.advisory_records.clone();
        self.advisory_task = Some(cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let outcome = run_advisory_scan(&client, &reachable, cached).await;
            this.update(cx, |this, cx| {
                match outcome {
                    Ok((hits, records)) => {
                        let known: Vec<_> =
                            records.values().map(|(_, record)| record.clone()).collect();
                        let findings = merge_findings(&hits, &known);
                        this.advisory_records = records;
                        let result = ScanResult {
                            findings,
                            scanned,
                            truncated: hits.iter().any(|hit| hit.truncated),
                        };
                        // Shared, so the same crate's other view shows it too.
                        publish_scan(key, result.clone(), cx);
                        this.advisories = result.into_state();
                    }
                    Err(error) => this.advisories = AdvisoryState::Failed(error),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // ── Quick actions ──

    fn runner(&self, cx: &App) -> Option<Entity<ScriptRunnerPanel>> {
        self.workspace
            .upgrade()?
            .read(cx)
            .panel::<ScriptRunnerPanel>(cx)
    }

    /// Runs a Cargo command for the selected crate in the Script Runner,
    /// from the workspace root. One at a time (design rule 1).
    fn run_quick_action(&mut self, action: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if self.running_action.is_some() {
            return;
        }
        let (Some(name), Some(workspace_root)) = (
            self.selected_crate.clone(),
            self.cargo_workspace.as_ref().map(|workspace| workspace.root.clone()),
        ) else {
            return;
        };
        let command = match cargo_command(action, &name) {
            Ok(command) => command,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(runner) = self.runner(cx) else {
            self.error = Some("The Script Runner panel isn't available yet.".into());
            cx.notify();
            return;
        };
        // The runner streams one command at a time.
        if runner.read(cx).is_running() {
            self.error = Some("Script Runner is busy — wait for it to finish first.".into());
            cx.notify();
            return;
        }

        self.running_action = Some(action.to_string());
        self.error = None;
        cx.notify();

        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.open_panel::<ScriptRunnerPanel>(window, cx);
            });
        }
        runner.update(cx, |runner, cx| runner.run_external(command, workspace_root, cx));

        // The runner notifies on every output line and on completion.
        self.run_subscription = Some(cx.observe(&runner, |this, runner, cx| {
            if !runner.read(cx).is_running() {
                this.running_action = None;
                cx.notify();
            }
        }));
        // A very fast run can finish before the observer is attached.
        if !runner.read(cx).is_running() {
            self.running_action = None;
            cx.notify();
        }
    }
}

// ── Section header helper ──────────────────────────────────────────────

fn section_container(
    cx: &Context<RustPanel>,
    title: &str,
    count: Option<usize>,
    open: bool,
    body: impl IntoElement,
) -> impl IntoElement {
    let key = title.to_string();

    let header = div()
        .id(format!("rust-section-{key}"))
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
        // No count is shown as nothing at all, not as a zero: an unscanned
        // section must not read as "0 problems".
        .when_some(count, |header, count| {
            header.child(Tag::secondary().xsmall().child(count.to_string()))
        })
        .on_click(cx.listener(move |this, _e, _w, cx| {
            this.toggle(&key);
            cx.notify();
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

fn muted_line(cx: &Context<RustPanel>, text: impl Into<String>) -> gpui::Div {
    div()
        .px_3()
        .py_2()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

fn busy_line(cx: &Context<RustPanel>, text: impl Into<String>) -> gpui::Div {
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
                .child(text.into()),
        )
}

fn severity_tag(finding: &Finding) -> Option<Tag> {
    let tag = match (finding.severity, finding.kind) {
        (Some(Severity::Critical), _) => Tag::danger().child("critical"),
        (Some(Severity::High), _) => Tag::warning().child("high"),
        (Some(Severity::Moderate), _) => Tag::info().child("moderate"),
        (Some(Severity::Low), _) => Tag::secondary().child("low"),
        // Unknown severity on a vulnerability is a warning, never neutral.
        (None, FindingKind::Vulnerability) => Tag::warning().child("unknown"),
        // A notice without a severity simply has none.
        (None, _) => return None,
    };
    // Outlined, like the Dashboard's: a filled tag's label is unreadable in
    // themes where the fill and the text are close in colour.
    Some(tag.xsmall().outline())
}

// ── Render ─────────────────────────────────────────────────────────────

impl Render for RustPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel_header = PanelHeader::new("Rust").icon(
            svg()
                .path("icons/file_rust.svg")
                .size(px(16.0))
                .text_color(cx.theme().foreground),
        );

        let message = if self.cargo_missing {
            Some("cargo not found in PATH".to_string())
        } else {
            match &self.load {
                LoadState::Loading => Some("Reading the workspace…".to_string()),
                LoadState::NoManifest => Some("Cargo.toml not found in this folder".to_string()),
                LoadState::Failed(error) => Some(error.clone()),
                LoadState::Ready => None,
            }
        };
        if let Some(message) = message {
            return div()
                .id("rust-panel-empty")
                .track_focus(&self.focus_handle(cx))
                .flex()
                .flex_col()
                .w_full()
                .h_full()
                .bg(cx.theme().background)
                .child(panel_header)
                .child(self.render_versions(cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .p_4()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(message),
                )
                .into_any_element();
        }

        let outdated_count = match self.outdated {
            OutdatedState::Done { .. } => Some(self.outdated_rows().len()),
            _ => None,
        };
        let advisory_count = match &self.advisories {
            AdvisoryState::Done { findings, .. } => {
                Some(FindingCounts::of(findings).vulnerabilities)
            }
            _ => None,
        };

        let mut body = div()
            .flex()
            .flex_col()
            .child(self.render_versions(cx))
            .child(section_container(
                cx,
                "Crates",
                Some(self.crate_count()),
                self.is_open("Crates"),
                self.render_crates(cx),
            ))
            .child(section_container(
                cx,
                "Quick Actions",
                None,
                self.is_open("Quick Actions"),
                self.render_quick_actions(cx),
            ))
            .child(section_container(
                cx,
                "Dependencies",
                Some(self.dependencies.listed.len()),
                self.is_open("Dependencies"),
                self.render_dependencies(cx),
            ))
            .child(section_container(
                cx,
                "Outdated",
                outdated_count,
                self.is_open("Outdated"),
                self.render_outdated(cx),
            ))
            .child(section_container(
                cx,
                "Advisories",
                advisory_count,
                self.is_open("Advisories"),
                self.render_advisories(cx),
            ));

        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }

        div()
            .id("rust-panel")
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
                    .id("rust-scroll")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .child(body),
            )
            .into_any_element()
    }
}

impl RustPanel {
    fn render_versions(&self, cx: &Context<Self>) -> impl IntoElement {
        let text = match (&self.rustc_version, &self.cargo_version) {
            (Some(rustc), Some(cargo)) => format!("rustc {rustc} · cargo {cargo}"),
            (Some(rustc), None) => format!("rustc {rustc}"),
            (None, Some(cargo)) => format!("cargo {cargo}"),
            (None, None) if self.cargo_missing => "Rust toolchain not detected".to_string(),
            (None, None) => "Detecting toolchain…".to_string(),
        };
        div()
            .flex()
            .items_center()
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

    fn render_crates(&self, cx: &Context<Self>) -> impl IntoElement {
        let crates: &[CrateInfo] = self
            .cargo_workspace
            .as_ref()
            .map_or(&[], |workspace| workspace.crates.as_slice());
        let query = self.crate_filter.read(cx).value().to_string();
        let visible = filter_crates(crates, &query);

        let mut list = div().flex().flex_col();
        if visible.is_empty() {
            list = list.child(muted_line(cx, "No crate matches the filter"));
        }
        for krate in visible {
            let selected = self.selected_crate.as_deref() == Some(krate.name.as_str());
            let name = krate.name.clone();
            list = list.child(
                div()
                    .id(format!("rust-crate-{}", krate.name))
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
                        this.select_crate(name.clone(), cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .font_family("Cascadia Mono")
                            .text_color(if selected {
                                cx.theme().primary
                            } else {
                                cx.theme().foreground
                            })
                            .child(krate.name.clone()),
                    )
                    .child(
                        div()
                            .font_family("Cascadia Mono")
                            .text_color(cx.theme().muted_foreground)
                            .child(krate.version.clone()),
                    ),
            );
        }

        let mut column = div().flex().flex_col();
        // A lone crate needs no filter; a 287-crate workspace does.
        if crates.len() > 8 {
            column = column.child(
                div()
                    .px_3()
                    .py_1()
                    .child(Input::new(&self.crate_filter).xsmall().w_full()),
            );
        }
        column.child(
            div()
                .id("rust-crates-scroll")
                .max_h(px(220.))
                .overflow_y_scroll()
                .child(list),
        )
    }

    fn render_quick_actions(&self, cx: &Context<Self>) -> impl IntoElement {
        let ready = self.selected_crate.is_some();
        let busy = self.running_action.is_some();

        let mut row = div().flex().flex_row().flex_wrap().gap_1().px_3().py_1();
        for action in QUICK_ACTIONS {
            let tooltip = match &self.selected_crate {
                Some(name) => format!("cargo {action} -p {name} — runs in the Script Runner"),
                None => format!("cargo {action}"),
            };
            row = row.child(
                Button::new(format!("rust-quick-{action}"))
                    .secondary()
                    .xsmall()
                    .label(action)
                    .disabled(!ready || busy)
                    .tooltip(tooltip)
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.run_quick_action(action, window, cx);
                    })),
            );
        }
        row = row.child(
            Button::new("rust-quick-package-manager")
                .secondary()
                .xsmall()
                .label("Package Manager")
                .disabled(!ready)
                .tooltip("Open the Cargo manager for the selected crate")
                .on_click(cx.listener(|this, _e, window, cx| {
                    let name = this.selected_crate.clone();
                    let root = this.root.clone();
                    if let Some(workspace) = this.workspace.upgrade() {
                        workspace.update(cx, |workspace, cx| {
                            cargo_manager_panel::open(root, name, workspace, window, cx);
                        });
                    }
                })),
        );
        if let Some(label) = &self.running_action {
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

    fn render_dependencies(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut column = div().flex().flex_col();
        if self.selected_crate.is_none() {
            return column.child(muted_line(cx, "Select a crate"));
        }
        if self.dependencies.listed.is_empty() {
            column = column.child(muted_line(cx, "No crates.io dependencies"));
        }
        if self.lockfile.is_none() && !self.dependencies.listed.is_empty() {
            column = column.child(muted_line(
                cx,
                "No Cargo.lock yet: installed versions are unknown",
            ));
        }

        for row in &self.dependencies.listed {
            let mut line = div()
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
                        .child(row.name.clone()),
                );
            if row.kind != DependencyKind::Normal {
                line = line.child(Tag::secondary().xsmall().child(row.kind.label()));
            }
            if row.optional {
                line = line.child(Tag::secondary().xsmall().outline().child("optional"));
            }
            line = line.child(
                div()
                    .font_family("Cascadia Mono")
                    .text_color(cx.theme().muted_foreground)
                    // Unknown stays unknown: never a guessed version.
                    .child(match &row.locked_version {
                        Some(version) => version.clone(),
                        None => "unknown".to_string(),
                    }),
            );
            column = column.child(line);
        }

        if self.dependencies.hidden > 0 {
            column = column.child(muted_line(
                cx,
                format!(
                    "{} local and git dependencies not shown",
                    self.dependencies.hidden
                ),
            ));
        }
        column
    }

    fn render_outdated(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut column = div().flex().flex_col();
        if self.selected_crate.is_none() {
            return column.child(muted_line(cx, "Select a crate"));
        }
        match self.outdated {
            OutdatedState::Idle => return column.child(muted_line(cx, "Nothing to check")),
            OutdatedState::Checking => {
                return column.child(busy_line(cx, "Checking crates.io\u{2026}"));
            }
            OutdatedState::Done { failed } => {
                if failed > 0 {
                    column = column.child(
                        div()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .text_color(cx.theme().warning)
                            .child(format!(
                                "{failed} lookup(s) failed — offline? Results below may be incomplete."
                            )),
                    );
                }
            }
        }

        let rows = self.outdated_rows();
        if rows.is_empty() {
            return column.child(muted_line(cx, "Everything checked is up to date"));
        }
        for row in rows {
            let mut block = div().flex().flex_col().px_3().py_0p5().text_xs().child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .font_family("Cascadia Mono")
                            .text_color(cx.theme().foreground)
                            .child(row.name.clone()),
                    )
                    .child(
                        div()
                            .font_family("Cascadia Mono")
                            .text_color(cx.theme().muted_foreground)
                            .child(row.locked.clone().unwrap_or_else(|| "unknown".into())),
                    ),
            );
            if row.locked_yanked {
                block = block.child(
                    div()
                        .text_color(cx.theme().warning)
                        .child("The locked version has been yanked"),
                );
            }
            // "Up to": the newest this crate's requirement allows. Another
            // package in the workspace can hold Cargo lower.
            if let Some(target) = &row.in_range {
                block = block.child(self.render_target(cx, "up to", target, "cargo update"));
            }
            if let Some(target) = &row.out_of_range {
                block = block.child(self.render_target(
                    cx,
                    "newer",
                    target,
                    "needs the requirement changed",
                ));
            }
            column = column.child(block);
        }
        column
    }

    fn render_target(
        &self,
        cx: &Context<Self>,
        prefix: &str,
        target: &UpdateTarget,
        how: &str,
    ) -> impl IntoElement {
        let mut line = h_flex()
            .gap_1()
            .items_center()
            .pl_2()
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{prefix} ")),
            )
            .child(
                div()
                    .font_family("Cascadia Mono")
                    .text_color(cx.theme().foreground)
                    .child(target.version.to_string()),
            )
            .child(Tag::secondary().xsmall().child(target.kind.label()))
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(how.to_string()),
            );
        // The three states of design decision 6: too new is an error with
        // the version needed; not declared is a neutral marker; compatible
        // says nothing.
        match &target.rust {
            RustCompat::Compatible => {}
            RustCompat::TooNew { needs } => {
                line = line.child(
                    Tag::danger()
                        .xsmall()
                        .child(format!("needs Rust {needs}")),
                );
            }
            RustCompat::NotDeclared => {
                line = line.child(
                    Tag::secondary()
                        .xsmall()
                        .outline()
                        .child("Rust version not declared"),
                );
            }
        }
        line
    }

    fn render_advisories(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut column = div().flex().flex_col();
        if self.selected_crate.is_none() {
            return column.child(muted_line(cx, "Select a crate"));
        }

        let scanning = matches!(self.advisories, AdvisoryState::Scanning);
        let scan_button = Button::new("rust-advisory-scan")
            .secondary()
            .xsmall()
            .label(match self.advisories {
                AdvisoryState::NotScanned => "Scan",
                _ => "Rescan",
            })
            .disabled(scanning)
            .tooltip("Ask OSV.dev about every crates.io package this crate pulls in")
            .on_click(cx.listener(|this, _e, _w, cx| this.scan_advisories(cx)));
        column = column.child(div().px_3().py_1().child(scan_button));

        match &self.advisories {
            AdvisoryState::NotScanned => column.child(muted_line(cx, "Not scanned yet")),
            AdvisoryState::Scanning => column.child(busy_line(cx, "Asking OSV.dev\u{2026}")),
            AdvisoryState::Failed(error) => column.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            ),
            AdvisoryState::Done {
                findings,
                scanned,
                truncated,
            } => {
                if findings.is_empty() {
                    column = column.child(muted_line(
                        cx,
                        format!("No advisories for the {scanned} packages checked"),
                    ));
                }
                let mut heading = None;
                for finding in findings {
                    if heading != Some(finding.kind) {
                        heading = Some(finding.kind);
                        column = column.child(
                            div()
                                .px_3()
                                .pt_1()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(cx.theme().muted_foreground)
                                .child(match finding.kind {
                                    FindingKind::Vulnerability => "Vulnerabilities",
                                    FindingKind::Unsound => "Unsound",
                                    FindingKind::Unmaintained => "Unmaintained",
                                    FindingKind::Notice => "Notices",
                                }),
                        );
                    }
                    column = column.child(self.render_finding(cx, finding));
                }
                if *truncated {
                    column = column.child(
                        div()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .text_color(cx.theme().warning)
                            .child("Some packages had more advisories than were returned; this list is incomplete."),
                    );
                }
                // The scan reads the lockfile, which covers every platform.
                column.child(muted_line(
                    cx,
                    format!(
                        "Checked {scanned} packages from Cargo.lock, for every platform. \
                         A finding may concern a package that is never built here."
                    ),
                ))
            }
        }
    }

    fn render_finding(&self, cx: &Context<Self>, finding: &Finding) -> impl IntoElement {
        let url = finding.url.clone();
        let mut head = h_flex().gap_2().items_center();
        if let Some(tag) = severity_tag(finding) {
            head = head.child(tag);
        }
        head = head
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .font_family("Cascadia Mono")
                    .text_color(cx.theme().foreground)
                    .child(format!("{} {}", finding.package, finding.version)),
            )
            .child(
                div()
                    .id(format!(
                        "rust-advisory-{}-{}-{}",
                        finding.id, finding.package, finding.version
                    ))
                    .font_family("Cascadia Mono")
                    .text_color(cx.theme().primary)
                    .cursor_pointer()
                    .on_click(move |_, _, cx| cx.open_url(&url))
                    .child(finding.id.clone()),
            );

        let mut block = div()
            .flex()
            .flex_col()
            .px_3()
            .py_0p5()
            .text_xs()
            .child(head);
        if let Some(summary) = &finding.summary {
            block = block.child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(summary.clone()),
            );
        }
        if finding.details_missing {
            block = block.child(
                div()
                    .text_color(cx.theme().warning)
                    .child("Details could not be fetched"),
            );
        }
        if !finding.fixed_in.is_empty() {
            block = block.child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("Fixed in {}", finding.fixed_in.join(", "))),
            );
        }
        block
    }
}

impl Focusable for RustPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for RustPanel {}

impl Panel for RustPanel {
    fn persistent_name() -> &'static str {
        "Rust Panel"
    }

    fn panel_key() -> &'static str {
        "RustPanel"
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        match RustPanelSettings::get_global(cx).dock {
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
            settings.rust_panel.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<ui::IconName> {
        RustPanelSettings::get_global(cx).button.then_some(ui::IconName::FileRust)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Rust")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        29
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.rust_panel.get_or_insert_default().button = Some(false);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargo_backend::parse_metadata;

    fn crates(names: &[&str]) -> Vec<CrateInfo> {
        let packages: Vec<String> = names
            .iter()
            .map(|name| {
                format!(
                    r#"{{"name":"{name}","version":"0.1.0","id":"{name}","manifest_path":"/w/{name}/Cargo.toml"}}"#
                )
            })
            .collect();
        parse_metadata(&format!(
            r#"{{"workspace_root":"/w","packages":[{}]}}"#,
            packages.join(",")
        ))
        .unwrap()
        .crates
    }

    #[test]
    fn cargo_commands_target_one_crate() {
        assert_eq!(cargo_command("check", "npm_backend").as_deref(), Ok("cargo check -p npm_backend"));
        assert_eq!(cargo_command("test", "gpui-component").as_deref(), Ok("cargo test -p gpui-component"));
        for action in QUICK_ACTIONS {
            assert!(cargo_command(action, "zed").unwrap().starts_with("cargo "));
        }
    }

    #[test]
    fn cargo_commands_refuse_shell_syntax_in_the_crate_name() {
        assert!(cargo_command("check", "zed && calc").is_err());
        assert!(cargo_command("check", "--manifest-path=/etc/x").is_err());
        assert!(cargo_command("check", "").is_err());
    }

    #[test]
    fn crate_filter_is_case_insensitive_and_keeps_order() {
        let all = crates(&["cargo_backend", "Dotnet_Panel", "node_panel", "zed"]);
        let names = |query: &str| -> Vec<String> {
            filter_crates(&all, query)
                .iter()
                .map(|krate| krate.name.clone())
                .collect()
        };
        assert_eq!(names(""), vec!["cargo_backend", "Dotnet_Panel", "node_panel", "zed"]);
        assert_eq!(names("  "), names(""));
        assert_eq!(names("PANEL"), vec!["Dotnet_Panel", "node_panel"]);
        assert_eq!(names("dotnet"), vec!["Dotnet_Panel"]);
        assert!(names("nothing-matches").is_empty());
    }

    #[test]
    fn only_cargo_files_trigger_a_reload() {
        assert!(is_manifest_file("Cargo.toml"));
        assert!(is_manifest_file("Cargo.lock"));
        assert!(!is_manifest_file("cargo.toml"));
        assert!(!is_manifest_file("Cargo.toml.bak"));
        assert!(!is_manifest_file("main.rs"));
        assert!(!is_manifest_file(""));
    }
}

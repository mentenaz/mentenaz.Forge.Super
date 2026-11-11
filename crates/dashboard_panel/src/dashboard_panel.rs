//! Cross-ecosystem Dashboard — a workspace tab (not a dock panel) opened
//! from the Cockpit panel's header button (`cockpit_panel::OpenDashboard`).
//! Surfaces, at a glance: detected runtime versions + project counts for
//! Node/Python/.NET, a per-ecosystem security rollup with one row per
//! finding (severity, package, fixed-in, advisory link; PyPI advisories
//! expand in place), the active project's git branch/changed-file count +
//! ahead/behind, and the same live system charts the Cockpit dock panel
//! shows.
//!
//! Deliberately lighter than the Forge original's dashboard:
//! - The security rollup reads each ecosystem panel's own findings
//!   directly via a held `Entity<T>` (`cx.observe`'d so the tab
//!   live-updates) instead of porting Forge's `AppState`/broadcast-channel
//!   plumbing. The rescan affordance dispatches each panel's own scan.
//! - No LSP update-server tracking — unrelated to this panel's runtime/
//!   package/git summary theme, dropped rather than ported.
//! - Git status is read straight from `project::git_store::Repository`'s
//!   snapshot (repo name, branch, changed-file count, ahead/behind) rather
//!   than porting Forge's own custom `GitPanel`, or embedding Zed's real
//!   (much heavier) `git_ui` panel — this only needs a compact summary.
//! - "System" mirrors the docked `CockpitPanel` (same live charts, its own
//!   collapse state) without running a second `sysinfo` poller, and drops the
//!   header's "Dashboard" button since the Dashboard is already open.

use std::collections::HashSet;

use anyhow::Result;
use cargo_backend::{Finding, FindingKind};
use dotnet_backend::VulnerablePackage;
use dotnet_panel::DotNetPanel;
use git::repository::Branch;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Task, WeakEntity, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    collapsible::Collapsible,
    h_flex,
    scroll::ScrollableElement as _,
    spinner::Spinner,
    tag::Tag,
    v_flex,
};
use node_panel::NodePanel;
use npm_backend::NpmAuditVuln;
use python_backend::PyPiVulnerability;
use python_panel::PythonPanel;
use rust_panel::RustPanel;
use workspace::{Item, ItemId, SerializableItem, Workspace, WorkspaceId};

/// Opens (or activates) the Dashboard tab. Looks up the sibling ecosystem
/// panels and the Cockpit panel by type via `workspace.panel::<T>` — they're
/// all already-loaded dock panels by the time this button is clickable, so
/// no pre-wired `WeakEntity` plumbing through `initialize_panels` is
/// needed. A panel that genuinely isn't loaded yet (e.g. it failed to
/// initialize) just renders as "not detected" rather than blocking the tab
/// from opening.
pub fn open(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<DashboardPanel> {
    let existing = workspace
        .active_pane()
        .read(cx)
        .items()
        .find_map(|item| item.downcast::<DashboardPanel>());

    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return existing;
    }

    let node_panel = workspace.panel::<NodePanel>(cx);
    let python_panel = workspace.panel::<PythonPanel>(cx);
    let dotnet_panel = workspace.panel::<DotNetPanel>(cx);
    let rust_panel = workspace.panel::<RustPanel>(cx);
    // Mirror the docked Cockpit panel for the "System" section: same live
    // charts, but no header "Dashboard" button (redundant inside the
    // Dashboard) and no second `sysinfo` poller — see `CockpitPanel::new_embedded`.
    let cockpit_panel = workspace
        .panel::<cockpit_panel::CockpitPanel>(cx)
        .map(|source| cx.new(|cx| cockpit_panel::CockpitPanel::new_embedded(source, cx)));

    let panel = cx.new(|cx| {
        DashboardPanel::new(
            workspace.weak_handle(),
            node_panel,
            python_panel,
            dotnet_panel,
            rust_panel,
            cockpit_panel,
            cx,
        )
    });
    workspace.add_item_to_active_pane(Box::new(panel.clone()), None, true, window, cx);
    panel
}

/// Registers the handler for `cockpit_panel::OpenDashboard` on every
/// workspace. `cockpit_panel` itself can't call `open` directly — this
/// crate embeds `CockpitPanel`, so the dependency has to run this
/// direction, not both ways.
pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<DashboardPanel>(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _cx| {
        workspace.register_action(|workspace, _: &cockpit_panel::OpenDashboard, window, cx| {
            open(workspace, window, cx);
        });
    })
    .detach();
}

pub struct DashboardPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    node_panel: Option<Entity<NodePanel>>,
    python_panel: Option<Entity<PythonPanel>>,
    dotnet_panel: Option<Entity<DotNetPanel>>,
    rust_panel: Option<Entity<RustPanel>>,
    cockpit_panel: Option<Entity<cockpit_panel::CockpitPanel>>,
    /// PyPI package ids currently expanded in the Security section.
    expanded: HashSet<String>,
}

impl DashboardPanel {
    fn new(
        workspace: WeakEntity<Workspace>,
        node_panel: Option<Entity<NodePanel>>,
        python_panel: Option<Entity<PythonPanel>>,
        dotnet_panel: Option<Entity<DotNetPanel>>,
        rust_panel: Option<Entity<RustPanel>>,
        cockpit_panel: Option<Entity<cockpit_panel::CockpitPanel>>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Relay each embedded panel's own `cx.notify()` so this tab
        // live-updates without polling anything itself.
        if let Some(panel) = &node_panel {
            cx.observe(panel, |_, _, cx| cx.notify()).detach();
        }
        if let Some(panel) = &python_panel {
            cx.observe(panel, |_, _, cx| cx.notify()).detach();
        }
        if let Some(panel) = &dotnet_panel {
            cx.observe(panel, |_, _, cx| cx.notify()).detach();
        }
        if let Some(panel) = &rust_panel {
            cx.observe(panel, |_, _, cx| cx.notify()).detach();
        }
        if let Some(panel) = &cockpit_panel {
            cx.observe(panel, |_, _, cx| cx.notify()).detach();
        }

        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            node_panel,
            python_panel,
            dotnet_panel,
            rust_panel,
            cockpit_panel,
            expanded: HashSet::new(),
        }
    }

    /// The active project's git state, read straight off
    /// `project::git_store::Repository`'s snapshot (`Repository` derefs to
    /// `RepositorySnapshot`). `None` when no worktree has a repository (not
    /// an error state — just an empty section).
    fn git_summary(&self, cx: &App) -> Option<GitSummary> {
        let workspace = self.workspace.upgrade()?;
        let repo = workspace
            .read(cx)
            .project()
            .read(cx)
            .active_repository(cx)?;
        let repo = repo.read(cx);
        let repo_name = repo
            .work_directory_abs_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo.work_directory_abs_path.display().to_string());
        let branch_name = repo
            .branch
            .as_ref()
            .map(Branch::name)
            .unwrap_or("(detached)")
            .to_string();
        let (ahead, behind) = repo
            .branch
            .as_ref()
            .and_then(|b| b.upstream.as_ref())
            .and_then(|upstream| upstream.tracking.status())
            .map(|s| (s.ahead as usize, s.behind as usize))
            .unwrap_or((0, 0));
        let changed = repo.status().count();
        Some(GitSummary {
            repo_name,
            branch: branch_name,
            ahead,
            behind,
            changed,
        })
    }
}

/// The compact git state the Git section renders — repo name, current
/// branch, unpushed/pulled commit counts and changed-file count.
struct GitSummary {
    repo_name: String,
    branch: String,
    ahead: usize,
    behind: usize,
    changed: usize,
}

impl EventEmitter<()> for DashboardPanel {}

impl Focusable for DashboardPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for DashboardPanel {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> gpui::SharedString {
        "Dashboard".into()
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<gpui::SharedString> {
        Some("Cross-ecosystem dashboard".into())
    }
}

impl SerializableItem for DashboardPanel {
    fn serialized_item_kind() -> &'static str {
        "dashboard_panel"
    }

    /// Persisting the tab just records its presence in the pane — there's
    /// no per-tab state to clean up.
    fn cleanup(
        _workspace_id: WorkspaceId,
        _alive_items: Vec<ItemId>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        _project: Entity<project::Project>,
        workspace: WeakEntity<Workspace>,
        _workspace_id: WorkspaceId,
        _item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            cx.update(|window, cx| {
                let Some(workspace_entity) = workspace.upgrade() else {
                    anyhow::bail!("workspace released before Dashboard could be restored");
                };
                Ok(workspace_entity.update(cx, |workspace, cx| open(workspace, window, cx)))
            })?
        })
    }

    /// Nothing is persisted beyond the item's presence in the pane.
    fn serialize(
        &mut self,
        _workspace: &mut Workspace,
        _item_id: ItemId,
        _closing: bool,
        _cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        None
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

/// A titled section card — the dashboard's one repeated shell, matching
/// `section_container`'s look in the ecosystem panels without pulling in
/// their exact (per-panel, collapsible-state-tracking) helper.
fn section(theme: &gpui_component::Theme, title: &str, body: impl IntoElement) -> impl IntoElement {
    v_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .p_3()
        .border_1()
        .rounded_md()
        .border_color(theme.border)
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_sm()
                .text_color(theme.foreground)
                .child(title.to_string()),
        )
        .child(body)
}

/// The text of a PyPI finding's advisory tag — "fixed in <v>" once a fix
/// exists, plain "advisory" otherwise.
fn advisory_label(vuln: &PyPiVulnerability) -> String {
    vuln.fixed_in
        .first()
        .map(|v| format!("fixed in {v}"))
        .unwrap_or_else(|| "advisory".to_string())
}

/// An advisory tag for a PyPI finding, green once a fix exists.
fn advisory_tag(vuln: &PyPiVulnerability) -> Tag {
    let tag = match vuln.fixed_in.first() {
        Some(_) => Tag::success(),
        None => Tag::info(),
    };
    tag.xsmall().outline().child(advisory_label(vuln))
}

/// The severity buckets the three ecosystems' findings are colored by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SeverityLevel {
    Critical,
    High,
    Moderate,
    /// "low", and anything unrecognized.
    Other,
}

/// Buckets a severity string, case-insensitively. npm says "moderate",
/// NuGet "Moderate", OSV "MEDIUM" — all the same bucket.
fn severity_level(severity: &str) -> SeverityLevel {
    match severity.to_ascii_lowercase().as_str() {
        "critical" => SeverityLevel::Critical,
        "high" => SeverityLevel::High,
        "moderate" | "medium" => SeverityLevel::Moderate,
        _ => SeverityLevel::Other,
    }
}

/// Severity tag with a per-severity accent. Strings that don't match a
/// known level just render secondary.
fn severity_tag(severity: &str) -> Tag {
    let tag = match severity_level(severity) {
        SeverityLevel::Critical => Tag::danger(),
        SeverityLevel::High => Tag::warning(),
        SeverityLevel::Moderate => Tag::info(),
        SeverityLevel::Other => Tag::secondary(),
    };
    tag.xsmall().outline().child(severity.to_string())
}

/// A compact per-finding row for an npm audit result: severity, package,
/// fixed-in, title and the advisory URL beneath.
fn node_finding_row(theme: &gpui_component::Theme, finding: &NpmAuditVuln) -> impl IntoElement {
    v_flex()
        .w_full()
        .gap_0p5()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(severity_tag(&finding.severity))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family("Cascadia Mono")
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(format!("{} {}", finding.package, finding.title.trim())),
                )
                .when(
                    finding.fixed_in.as_deref().is_some_and(|v| !v.is_empty()),
                    |row| {
                        row.child(Tag::success().xsmall().outline().child(format!(
                            "fixed in {}",
                            finding.fixed_in.as_deref().unwrap_or_default()
                        )))
                    },
                ),
        )
        .when(!finding.url.is_empty(), |body| {
            body.child(
                div()
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(finding.url.clone()),
            )
        })
}

/// A compact per-finding row for a NuGet advisory: severity, package id and
/// the advisory URL beneath.
fn dotnet_finding_row(
    theme: &gpui_component::Theme,
    finding: &VulnerablePackage,
) -> impl IntoElement {
    v_flex()
        .w_full()
        .gap_0p5()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(severity_tag(&finding.severity))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family("Cascadia Mono")
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(finding.id.clone()),
                ),
        )
        .when(!finding.advisory_url.is_empty(), |body| {
            body.child(
                div()
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(finding.advisory_url.clone()),
            )
        })
}

/// A "scanning…" status row (spinner + text).
/// The tag that leads a Rust finding's row. A vulnerability shows its
/// severity, and "unknown" when it has none: an advisory exists, only its
/// rating is missing. A notice shows its kind instead (unsound,
/// unmaintained), because for a notice a missing severity is not unknown,
/// there simply isn't one.
fn rust_finding_label(finding: &Finding) -> &'static str {
    match (finding.kind, finding.severity) {
        (_, Some(severity)) => severity.label(),
        (FindingKind::Vulnerability, None) => "unknown",
        (kind, None) => kind.label(),
    }
}

fn rust_finding_tag(finding: &Finding) -> Tag {
    let label = rust_finding_label(finding);
    match (finding.kind, finding.severity) {
        (_, Some(_)) => severity_tag(label),
        // Warning colour, never neutral: unknown must not rank below low.
        (FindingKind::Vulnerability, None) => Tag::warning().xsmall().outline().child(label),
        (_, None) => Tag::secondary().xsmall().outline().child(label),
    }
}

/// A compact per-finding row for a Rust advisory: severity or kind, the
/// package and locked version, the advisory id, fixed-in, and the summary
/// beneath.
fn rust_finding_row(theme: &gpui_component::Theme, finding: &Finding) -> impl IntoElement {
    v_flex()
        .w_full()
        .gap_0p5()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(rust_finding_tag(finding))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family("Cascadia Mono")
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(format!(
                            "{} {}  {}",
                            finding.package, finding.version, finding.id
                        )),
                )
                .when_some(finding.fixed_in.first(), |row, fixed| {
                    row.child(
                        Tag::success()
                            .xsmall()
                            .outline()
                            .child(format!("fixed in {fixed}")),
                    )
                }),
        )
        .when_some(finding.summary.clone(), |body, summary| {
            body.child(
                div()
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(summary),
            )
        })
}

fn scanning_row(theme: &gpui_component::Theme, message: impl Into<String>) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_center()
        .text_xs()
        .child(Spinner::new().xsmall())
        .child(
            div()
                .text_color(theme.muted_foreground)
                .child(message.into()),
        )
}

/// A scan-error notice.
fn scan_error_row(theme: &gpui_component::Theme, message: impl Into<String>) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(theme.danger)
        .child(message.into())
}

/// Whether a Runtimes row is for an ecosystem that was found but has not been
/// scanned for vulnerabilities, so its row says "not scanned" instead of
/// showing no vulnerable count. `data` is as `runtime_row` takes it:
/// `(version, projects, outdated, vulnerable)`, with `vulnerable` `None` until
/// a scan has finished.
fn row_not_scanned(data: &Option<(Option<String>, usize, usize, Option<usize>)>) -> bool {
    matches!(data, Some((Some(_), _, _, None)))
}

/// A muted one-line notice.
fn muted_row(theme: &gpui_component::Theme, message: impl Into<String>) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(message.into())
}

impl Render for DashboardPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let header = h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(Icon::new(IconName::LayoutDashboard).text_color(theme.foreground))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_lg()
                    .text_color(theme.foreground)
                    .child("Dashboard"),
            );

        let body = v_flex()
            .w_full()
            .min_w_0()
            .gap_3()
            .p_3()
            .child(self.render_runtimes(&theme, cx))
            .child(self.render_security(&theme, cx))
            .child(self.render_git(&theme, cx))
            .child(self.render_system(&theme));

        v_flex()
            .id("dashboard-panel")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .bg(theme.background)
            .child(header)
            .child(
                div()
                    .id("dashboard-scroll")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .w_full()
                    .overflow_y_scrollbar()
                    .child(body),
            )
    }
}

impl DashboardPanel {
    /// One row per ecosystem: version (or "not detected"), project count,
    /// outdated count, vulnerable count.
    fn render_runtimes(
        &self,
        theme: &gpui_component::Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let mut rows = v_flex().w_full().gap_2();

        rows = rows.child(self.runtime_row(
            theme,
            "Node",
            "project",
            self.node_panel.as_ref().map(|p| p.read(cx)).map(|p| {
                (
                    p.node_version().map(str::to_string),
                    p.detected_projects().len(),
                    p.outdated_count(),
                    // `None` until the audit has run: "not scanned" must not
                    // read as zero.
                    p.vulnerable_findings().map(|findings| findings.len()),
                )
            }),
        ));
        rows = rows.child(self.runtime_row(
            theme,
            "Python",
            "project",
            self.python_panel.as_ref().map(|p| p.read(cx)).map(|p| {
                (
                    p.python_version().map(str::to_string),
                    p.detected_projects().len(),
                    p.outdated_count(),
                    // Python's scan is on demand, so nothing has been found
                    // until one has run.
                    p.vulnerabilities_scanned().then(|| p.vulnerable_count()),
                )
            }),
        ));
        rows = rows.child(self.runtime_row(
            theme,
            ".NET",
            "project",
            self.dotnet_panel.as_ref().map(|p| p.read(cx)).map(|p| {
                (
                    p.dotnet_version().map(str::to_string),
                    p.detected_projects().len(),
                    p.outdated_count(),
                    p.vulnerable_findings().map(|findings| findings.len()),
                )
            }),
        ));

        rows = rows.child(self.runtime_row(
            theme,
            "Rust",
            "crate",
            self.rust_panel.as_ref().map(|p| p.read(cx)).map(|p| {
                (
                    p.rustc_version().map(str::to_string),
                    p.crate_count(),
                    p.outdated_count(),
                    // `None` until a scan has finished: Rust's scan is on
                    // demand, and "not scanned" must not read as zero.
                    p.vulnerability_count(),
                )
            }),
        ));

        section(theme, "Runtimes", rows)
    }

    /// `data` is `None` when the panel hasn't loaded (yet, or at all) —
    /// distinct from `Some((None, ..))`, which means the panel loaded but
    /// hasn't detected a runtime.
    fn runtime_row(
        &self,
        theme: &gpui_component::Theme,
        label: &'static str,
        unit: &'static str,
        data: Option<(Option<String>, usize, usize, Option<usize>)>,
    ) -> impl IntoElement {
        // A vulnerable count of `None` means the ecosystem has not been
        // scanned, which is shown as such rather than as nothing to report.
        let not_scanned = row_not_scanned(&data);
        let (version, projects, outdated, vulnerable) = match data {
            Some((version, projects, outdated, vulnerable)) => {
                (version, projects, outdated, vulnerable.unwrap_or(0))
            }
            None => (None, 0, 0, 0),
        };

        h_flex()
            .w_full()
            .items_center()
            .gap_3()
            .text_xs()
            .child(
                div()
                    .w_16()
                    .flex_shrink_0()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family("Cascadia Mono")
                    .text_color(theme.foreground)
                    .child(version.unwrap_or_else(|| "not detected".to_string())),
            )
            .child(div().text_color(theme.muted_foreground).child(format!(
                "{projects} {unit}{}",
                if projects == 1 { "" } else { "s" }
            )))
            .when(outdated > 0, |row| {
                row.child(
                    Tag::warning()
                        .xsmall()
                        .text_color(theme.foreground)
                        .child(format!("{outdated} outdated")),
                )
            })
            .when(vulnerable > 0, |row| {
                row.child(
                    Tag::danger()
                        .xsmall()
                        .text_color(theme.foreground)
                        .child(format!("{vulnerable} vulnerable")),
                )
            })
            .when(not_scanned, |row| {
                row.child(Tag::secondary().xsmall().outline().child("not scanned"))
            })
    }

    /// Source-grade security rollup: one card per ecosystem with a row per
    /// finding (severity tag, package, fixed-in), Python's advisories
    /// expandable in place, and a "Scan all" rescan affordance. Reads the
    /// docked panels' own scan results through the accessors added for the
    /// dashboard — never rescans from here.
    fn render_security(
        &mut self,
        theme: &gpui_component::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let scan_all = Button::new("sec-scan-all")
            .secondary()
            .xsmall()
            .label("Scan all")
            .on_click(cx.listener(|this, _event: &gpui::ClickEvent, _window, cx| {
                if let Some(panel) = &this.node_panel {
                    panel.update(cx, |panel, cx| panel.rescan_vulnerabilities(cx));
                }
                if let Some(panel) = &this.python_panel {
                    panel.update(cx, |panel, cx| panel.rescan_vulnerabilities(cx));
                }
                if let Some(panel) = &this.dotnet_panel {
                    panel.update(cx, |panel, cx| panel.rescan_vulnerabilities(cx));
                }
                if let Some(panel) = &this.rust_panel {
                    panel.update(cx, |panel, cx| panel.rescan_advisories(cx));
                }
            }));

        let body = v_flex()
            .w_full()
            .gap_2()
            .child(h_flex().w_full().justify_end().child(scan_all))
            .child(self.security_node_card(theme, cx))
            .child(self.security_python_card(theme, cx))
            .child(self.security_dotnet_card(theme, cx))
            .child(self.security_rust_card(theme, cx));

        section(theme, "Security", body)
    }

    /// Shared shell for the per-ecosystem security cards: accent-colored
    /// label row (ecosystem + scan target) atop the finding rows.
    fn security_card(
        &self,
        theme: &gpui_component::Theme,
        label: impl Into<String>,
        target: impl Into<String>,
        body: impl IntoElement,
    ) -> impl IntoElement {
        v_flex()
            .w_full()
            .gap_1()
            .p_2()
            .border_1()
            .rounded_md()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_xs()
                            .text_color(theme.foreground)
                            .child(label.into()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(target.into()),
                    ),
            )
            .child(body)
    }

    fn security_node_card(
        &self,
        theme: &gpui_component::Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let mut rows = v_flex().w_full().gap_1();

        let target = match self.node_panel.as_ref().map(|p| p.read(cx)) {
            Some(panel) => {
                if panel.vulnerable_loading() {
                    rows = rows.child(scanning_row(theme, "Scanning the active project\u{2026}"));
                } else if let Some(error) = panel.vulnerable_error() {
                    rows = rows.child(scan_error_row(theme, error));
                } else {
                    match panel.vulnerable_findings() {
                        Some(findings) => {
                            if findings.is_empty() {
                                rows = rows.child(muted_row(theme, "No known vulnerabilities."));
                            } else {
                                for finding in findings {
                                    rows = rows.child(node_finding_row(theme, finding));
                                }
                            }
                        }
                        None => rows = rows.child(muted_row(theme, "Nothing to scan yet.")),
                    }
                }
                panel.active_project_label()
            }
            None => {
                rows = rows.child(muted_row(
                    theme,
                    "The Node panel isn't loaded in this workspace.",
                ));
                "n/a".to_string()
            }
        };

        self.security_card(theme, "Node", target, rows)
    }

    fn security_python_card(
        &mut self,
        theme: &gpui_component::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut rows = v_flex().w_full().gap_1();

        // Snapshot the panel's scan state (owned) before building rows — the
        // per-package block below re-borrows `self` mutably (for the expand
        // listener), so nothing may remain borrowed from the panel.
        let (loaded, scanning, scanned, scan_error, by_package) =
            match self.python_panel.as_ref().map(|p| p.read(cx)) {
                Some(panel) => (
                    true,
                    panel.vulnerabilities_scanning(),
                    panel.vulnerabilities_scanned(),
                    panel.vulnerabilities_scan_error().map(str::to_string),
                    panel
                        .vulnerabilities()
                        .iter()
                        .map(|(package, vulns)| (package.clone(), vulns.clone()))
                        .collect::<Vec<_>>(),
                ),
                None => (false, false, false, None, Vec::new()),
            };

        let target = if loaded {
            self.python_panel
                .as_ref()
                .map(|p| p.read(cx).active_project_label())
                .unwrap_or_default()
        } else {
            "n/a".to_string()
        };

        if scanning {
            rows = rows.child(scanning_row(theme, "Scanning the active project\u{2026}"));
        } else if let Some(error) = &scan_error {
            rows = rows.child(scan_error_row(theme, error.as_str()));
        } else if !loaded {
            rows = rows.child(muted_row(
                theme,
                "The Python panel isn't loaded in this workspace.",
            ));
        } else if !scanned {
            rows = rows.child(muted_row(
                theme,
                "Not scanned yet — press Scan all (Python's scan is on-demand).",
            ));
        } else if by_package.is_empty() {
            rows = rows.child(muted_row(theme, "No known vulnerabilities."));
        } else {
            for (package, vulns) in by_package {
                rows = rows.child(self.python_package_block(theme, cx, &package, &vulns));
            }
        }

        self.security_card(theme, "Python", target, rows)
    }

    /// One package's advisory findings (PyPI); each advisory expands in
    /// place to show its summary/details/aliases.
    fn python_package_block(
        &mut self,
        theme: &gpui_component::Theme,
        cx: &mut Context<Self>,
        package: &str,
        vulns: &[PyPiVulnerability],
    ) -> impl IntoElement {
        let mut col = v_flex().w_full().gap_0p5();

        for vuln in vulns {
            let key = format!("{package}::{}", vuln.id);
            let open = self.expanded.contains(&key);

            let header = h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .id(format!("sec-py-{key}"))
                .child(
                    Icon::new(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall()
                    .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family("Cascadia Mono")
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(format!("{package} · {}", vuln.id)),
                )
                .child(advisory_tag(vuln))
                .on_click(
                    cx.listener(move |this, _event: &gpui::ClickEvent, _window, _cx| {
                        if this.expanded.contains(&key) {
                            this.expanded.remove(&key);
                        } else {
                            this.expanded.insert(key.clone());
                        }
                        _cx.notify();
                    }),
                );

            let detail = v_flex()
                .w_full()
                .gap_0p5()
                .pl_3()
                .child(match &vuln.summary {
                    Some(summary) if !summary.is_empty() => div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(summary.clone())
                        .into_any_element(),
                    _ => div().into_any_element(),
                })
                .child(match &vuln.details {
                    Some(details) if !details.is_empty() => div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(details.clone())
                        .into_any_element(),
                    _ => div().into_any_element(),
                })
                .when(!vuln.aliases.is_empty(), |body| {
                    body.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("aliases: {}", vuln.aliases.join(", "))),
                    )
                })
                .when(!vuln.fixed_in.is_empty(), |body| {
                    body.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("fixed in: {}", vuln.fixed_in.join(", "))),
                    )
                });

            col = col.child(Collapsible::new().open(open).child(header).content(detail));
        }

        col
    }

    fn security_dotnet_card(
        &self,
        theme: &gpui_component::Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let mut rows = v_flex().w_full().gap_1();

        let target = match self.dotnet_panel.as_ref().map(|p| p.read(cx)) {
            Some(panel) => {
                if panel.vulnerable_loading() {
                    rows = rows.child(scanning_row(theme, "Scanning the active project\u{2026}"));
                } else if let Some(error) = panel.vulnerable_error() {
                    rows = rows.child(scan_error_row(theme, error));
                } else {
                    match panel.vulnerable_findings() {
                        Some(findings) => {
                            if findings.is_empty() {
                                rows = rows.child(muted_row(theme, "No known vulnerabilities."));
                            } else {
                                for finding in findings {
                                    rows = rows.child(dotnet_finding_row(theme, finding));
                                }
                            }
                        }
                        None => rows = rows.child(muted_row(theme, "Nothing to scan yet.")),
                    }
                }
                panel.active_project_label()
            }
            None => {
                rows = rows.child(muted_row(
                    theme,
                    "The .NET panel isn't loaded in this workspace.",
                ));
                "n/a".to_string()
            }
        };

        self.security_card(theme, ".NET", target, rows)
    }

    /// The selected crate's advisories, as the Rust panel last scanned them.
    /// Vulnerabilities come first; unsound and unmaintained notices follow
    /// under their own kind, so an abandoned crate is not shown as a
    /// security hole.
    fn security_rust_card(
        &self,
        theme: &gpui_component::Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let mut rows = v_flex().w_full().gap_1();

        let target = match self.rust_panel.as_ref().map(|p| p.read(cx)) {
            Some(panel) => {
                if panel.advisories_scanning() {
                    rows = rows.child(scanning_row(theme, "Scanning the selected crate\u{2026}"));
                } else if let Some(error) = panel.advisory_scan_error() {
                    rows = rows.child(scan_error_row(theme, error));
                } else {
                    match panel.advisory_findings() {
                        Some([]) => {
                            rows = rows.child(muted_row(theme, "No known advisories."));
                        }
                        Some(findings) => {
                            for finding in findings {
                                rows = rows.child(rust_finding_row(theme, finding));
                            }
                        }
                        None => {
                            rows = rows.child(muted_row(
                                theme,
                                "Not scanned yet \u{2014} press Scan all (Rust's scan is on-demand).",
                            ));
                        }
                    }
                }
                match panel.selected_crate_name() {
                    Some(name) => name.to_string(),
                    None => "no crate selected".to_string(),
                }
            }
            None => {
                rows = rows.child(muted_row(
                    theme,
                    "The Rust panel isn't loaded in this workspace.",
                ));
                "n/a".to_string()
            }
        };

        self.security_card(theme, "Rust", target, rows)
    }

    fn render_git(&self, theme: &gpui_component::Theme, cx: &Context<Self>) -> impl IntoElement {
        let body = match self.git_summary(cx) {
            Some(git) => v_flex()
                .w_full()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child(git.repo_name),
                        )
                        .child(
                            div()
                                .font_family("Cascadia Mono")
                                .text_color(theme.muted_foreground)
                                .child(git.branch),
                        )
                        .when(git.ahead > 0, |row| {
                            row.child(
                                div()
                                    .text_color(theme.success)
                                    .child(format!("{} ahead", git.ahead)),
                            )
                        })
                        .when(git.behind > 0, |row| {
                            row.child(
                                div()
                                    .text_color(theme.warning)
                                    .child(format!("{} behind", git.behind)),
                            )
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "{} changed file{}",
                            git.changed,
                            if git.changed == 1 { "" } else { "s" }
                        )),
                )
                .into_any_element(),
            None => div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("No git repository detected.")
                .into_any_element(),
        };

        section(theme, "Git", body)
    }

    /// Embeds a header-button-less mirror of the docked `CockpitPanel` — no
    /// second `sysinfo` poller.
    fn render_system(&self, theme: &gpui_component::Theme) -> impl IntoElement {
        let body = match &self.cockpit_panel {
            Some(panel) => div()
                .h(gpui::px(420.))
                .w_full()
                .overflow_hidden()
                .child(panel.clone())
                .into_any_element(),
            None => div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Cockpit panel not loaded.")
                .into_any_element(),
        };
        section(theme, "System", body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_strings_are_bucketed_case_insensitively() {
        assert_eq!(severity_level("critical"), SeverityLevel::Critical);
        assert_eq!(severity_level("CRITICAL"), SeverityLevel::Critical);
        assert_eq!(severity_level("High"), SeverityLevel::High);
        assert_eq!(severity_level("moderate"), SeverityLevel::Moderate);
        assert_eq!(severity_level("Moderate"), SeverityLevel::Moderate);
        assert_eq!(severity_level("MEDIUM"), SeverityLevel::Moderate);
    }

    #[test]
    fn low_and_unknown_severities_share_the_neutral_bucket() {
        assert_eq!(severity_level("low"), SeverityLevel::Other);
        assert_eq!(severity_level("info"), SeverityLevel::Other);
        assert_eq!(severity_level("unknown"), SeverityLevel::Other);
        assert_eq!(severity_level(""), SeverityLevel::Other);
        // Not trimmed: a padded value is not silently promoted.
        assert_eq!(severity_level(" high "), SeverityLevel::Other);
    }

    fn rust_finding(kind: FindingKind, severity: Option<cargo_backend::Severity>) -> Finding {
        Finding {
            package: "time".to_string(),
            version: "0.1.43".to_string(),
            id: "RUSTSEC-2020-0071".to_string(),
            other_ids: Vec::new(),
            kind,
            severity,
            summary: None,
            fixed_in: Vec::new(),
            url: String::new(),
            details_missing: false,
        }
    }

    #[test]
    fn a_rust_finding_leads_with_its_severity_or_its_kind() {
        use cargo_backend::Severity;
        assert_eq!(
            rust_finding_label(&rust_finding(FindingKind::Vulnerability, Some(Severity::High))),
            "high"
        );
        // A vulnerability with no rating is "unknown", not blank.
        assert_eq!(
            rust_finding_label(&rust_finding(FindingKind::Vulnerability, None)),
            "unknown"
        );
        // A notice with no rating has none: it shows what kind it is.
        assert_eq!(
            rust_finding_label(&rust_finding(FindingKind::Unmaintained, None)),
            FindingKind::Unmaintained.label()
        );
        assert_eq!(
            rust_finding_label(&rust_finding(FindingKind::Unsound, Some(Severity::Moderate))),
            "moderate"
        );
    }

    #[test]
    fn advisory_label_names_the_first_fixed_version() {
        let mut vuln = PyPiVulnerability::default();
        assert_eq!(advisory_label(&vuln), "advisory");

        vuln.fixed_in = vec!["2.31.0".to_string(), "3.0.0".to_string()];
        assert_eq!(advisory_label(&vuln), "fixed in 2.31.0");
    }

    #[test]
    fn a_found_ecosystem_with_no_scan_yet_is_marked_not_scanned() {
        let version = Some("22.1.0".to_string());
        assert!(row_not_scanned(&Some((version.clone(), 2, 0, None))));
        // A scan that found nothing is a result, not a missing one.
        assert!(!row_not_scanned(&Some((version.clone(), 2, 0, Some(0)))));
        assert!(!row_not_scanned(&Some((version, 2, 0, Some(3)))));
        // Nothing found, or the panel not loaded: there is nothing to scan.
        assert!(!row_not_scanned(&Some((None, 0, 0, None))));
        assert!(!row_not_scanned(&None));
    }
}

//! Offers, once per project directory, to run `npm install` when a workspace
//! is opened whose root has a `package.json` but no `node_modules` yet.
//! Applies no matter how the folder was opened (drag-and-drop, File → Open
//! Folder, a fresh git clone, a project just made by the Scaffold panel, …)
//! since the hook is the generic `Workspace` lifecycle, not any particular
//! open path.
//!
//! `npm install` runs the project's own `preinstall`/`postinstall` scripts, so
//! it is project tooling like any other: nothing is offered while the project
//! is in Restricted Mode (the offer is made when it is trusted), and the
//! install runs in the Script Runner, which asks `forge_trust_gate` and shows
//! the output as it goes (an install can take minutes).
//!
//! A project the Scaffold panel has just made can ask for more: with "Run
//! install afterwards" on, the install starts by itself once the project is
//! trusted, without the click (`prepare_install_offer`). The offer is made either
//! way.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use db::kvp::KeyValueStore;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Subscription, WeakEntity, Window,
};
use gpui_component::{Sizable as _, spinner::Spinner};
use project::{Event as ProjectEvent, Worktree};
use script_runner_panel::ScriptRunnerPanel;
use ui::prelude::*;
use workspace::{Toast, ToastAction, ToastView, Workspace, notifications::NotificationId};

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        maybe_offer_install(workspace, cx);
    })
    .detach();
}

/// The workspace's first worktree isn't always attached the moment the
/// `Workspace` entity is created (e.g. a project still being loaded), so we
/// fall back to waiting for the first `WorktreeAdded` event rather than
/// assuming `worktrees(cx)` is non-empty here.
fn maybe_offer_install(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    // Whether this window's offer has been decided (made, or nothing to make).
    // Every way in below shares it, so none of them can make the offer twice.
    let handled = Rc::new(Cell::new(false));

    // A project that is not trusted yet is offered nothing, so the offer is made
    // when its trust changes.
    {
        let handled = handled.clone();
        forge_trust::on_trust_changed(cx, move |workspace, cx| {
            try_offer(workspace, &handled, cx);
        })
        .detach();
    }

    // The first worktree isn't always attached the moment the `Workspace` is
    // created (a project still being loaded).
    {
        let handled = handled.clone();
        let project = workspace.project().clone();
        cx.subscribe(&project, move |workspace, _, event, cx| {
            if matches!(event, ProjectEvent::WorktreeAdded(_)) {
                try_offer(workspace, &handled, cx);
            }
        })
        .detach();
    }

    try_offer(workspace, &handled, cx);

    // The trust gate publishes what it knows about a project a moment after its
    // window opens, and that can happen before the change hook above exists, so
    // for a while the answer is also asked for on a timer. Repeating is safe:
    // `handled` stops it as soon as the offer has been decided.
    cx.spawn(async move |workspace, cx| {
        for _ in 0..40 {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let done = workspace
                .update(cx, |workspace, cx| {
                    try_offer(workspace, &handled, cx);
                    handled.get()
                })
                .unwrap_or(true);
            if done {
                return;
            }
        }
        // Not trusted by now: the hook above makes the offer when it is.
        workspace
            .update(cx, |workspace, cx| {
                if let Some(worktree) = workspace.worktrees(cx).next() {
                    let root = worktree.read(cx).abs_path();
                    log::info!(
                        "npm_bootstrap: no install offer for {} yet: {:?}",
                        root.display(),
                        forge_trust_gate::check(&root).err()
                    );
                }
            })
            .ok();
    })
    .detach();
}

/// Makes the offer for the window's first worktree, unless it has been decided.
fn try_offer(workspace: &mut Workspace, handled: &Rc<Cell<bool>>, cx: &mut Context<Workspace>) {
    if handled.get() {
        return;
    }
    let first_worktree = workspace.worktrees(cx).next();
    if let Some(worktree) = first_worktree
        && check_worktree(workspace, &worktree, cx)
    {
        handled.set(true);
    }
}

/// Decides the offer for `worktree`. `true` once it is decided (made, already
/// made, or nothing to offer); `false` while the gate does not allow the folder
/// yet, so it is asked again.
fn check_worktree(
    workspace: &mut Workspace,
    worktree: &Entity<Worktree>,
    cx: &mut Context<Workspace>,
) -> bool {
    // Only once the gate allows the folder: the install would run the project's own
    // scripts, so not in Restricted Mode, and not before the editor has decided
    // either. The gate is asked, not the trust store, because it is the gate the
    // Script Runner asks when the install runs: asking anything else can say yes
    // while the gate still says no (the install was then refused, after the offer
    // had been used up). The trust-change hook comes back here when the answer
    // changes.
    let root = worktree.read(cx).abs_path();
    if let Err(reason) = forge_trust_gate::check(&root) {
        log::debug!("npm_bootstrap: {} is not offered an install yet: {reason}", root.display());
        return false;
    }
    if !needs_install(&root) {
        return true;
    }

    let key = prompt_key(&root);
    if KeyValueStore::global(cx)
        .read_kvp(&key)
        .ok()
        .flatten()
        .is_some()
    {
        log::info!("npm_bootstrap: the install was already offered for {}", root.display());
        return true;
    }

    // A project the Scaffold panel made with "Run install afterwards" on asked for
    // the install to start by itself, now that it is trusted. The request is used
    // up by this one install.
    let automatic = KeyValueStore::global(cx)
        .read_kvp(&auto_key(&root))
        .ok()
        .flatten()
        .is_some();

    // "Ask once" means the marker goes down the moment the prompt is shown,
    // not only if the user accepts it — otherwise dismissing the toast would
    // just re-prompt on the next open.
    let kvp = KeyValueStore::global(cx);
    let auto = auto_key(&root);
    cx.background_spawn(async move {
        kvp.write_kvp(key, "1".to_string()).await.ok();
        if automatic {
            kvp.delete_kvp(auto).await.ok();
        }
    })
    .detach();

    log::info!(
        "npm_bootstrap: {} the install for {}",
        if automatic { "starting" } else { "offering" },
        root.display()
    );
    if automatic {
        start_install_when_ready(root.to_path_buf(), cx);
    } else {
        offer_install(workspace, root.to_path_buf(), cx);
    }
    true
}

enum NpmInstallPrompt {}

/// The toast that asks whether to install; it waits for a click.
fn offer_install(workspace: &mut Workspace, root: PathBuf, cx: &mut Context<Workspace>) {
    let workspace_handle = cx.weak_entity();
    workspace.show_toast(
        Toast::new(
            NotificationId::unique::<NpmInstallPrompt>(),
            "package.json found — install dependencies?",
        )
        .on_click("Run npm install", move |window, cx| {
            let (root, workspace) = (root.clone(), workspace_handle.clone());
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    start_install(workspace, &root, Some(window), cx);
                });
            }
        }),
        cx,
    );
}

/// Starts the install a project asked for, as soon as the Script Runner is
/// there. The runner is loaded a moment after the window is, and a project that
/// is already trusted gets here at once, so this waits for it for a few seconds;
/// if it never comes, the usual toast is shown instead and the install waits for
/// a click. Starts without a window, so the Script Runner is not revealed: the
/// install toast has a "Show output" button for that.
fn start_install_when_ready(root: PathBuf, cx: &mut Context<Workspace>) {
    cx.spawn(async move |workspace, cx| {
        for _ in 0..40 {
            let ready = workspace
                .update(cx, |workspace, cx| {
                    workspace.panel::<ScriptRunnerPanel>(cx).is_some()
                })
                .unwrap_or(false);
            if ready {
                // If it did not start after all, the usual toast is shown too,
                // so the request is not lost and a click can try again.
                workspace
                    .update(cx, |workspace, cx| {
                        if !start_install(workspace, &root, None, cx) {
                            offer_install(workspace, root.clone(), cx);
                        }
                    })
                    .ok();
                return;
            }
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
        }
        workspace
            .update(cx, |workspace, cx| offer_install(workspace, root, cx))
            .ok();
    })
    .detach();
}

/// What the Script Runner runs. Run with its output piped, as it is here, plain
/// `npm install` prints nothing until it finishes, so an install of a thousand
/// packages looks stuck for minutes. `--loglevel=http` makes npm report each
/// registry request as it goes (first the package metadata, then the `.tgz`
/// downloads), which scrolls in the panel and shows both that it is alive and
/// roughly which phase it is in. It changes nothing about what is installed or
/// which scripts run.
const INSTALL_COMMAND: &str = "npm install --loglevel=http";

/// Runs `npm install` in `root` in the Script Runner. With a `window` the
/// output panel is revealed so the install can be watched. The Script Runner
/// asks the trust gate itself, so a folder that is not trusted is refused there
/// with its message.
///
/// Called on the workspace itself, from inside its own update, so that it can be
/// reached both from a toast's click (which updates the workspace first) and from
/// the workspace's own hooks (which are already inside that update).
fn start_install(
    workspace: &mut Workspace,
    root: &Path,
    window: Option<&mut Window>,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(runner) = workspace.panel::<ScriptRunnerPanel>(cx) else {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<NpmInstallResult>(),
                "The Script Runner is not available, so npm install was not run.",
            ),
            cx,
        );
        return false;
    };
    // The runner streams one command at a time: a run already going would
    // swallow this one without a word.
    if runner.read(cx).is_running() {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<NpmInstallResult>(),
                "The Script Runner is busy. Stop the current run, then install.",
            ),
            cx,
        );
        return false;
    }
    if let Some(window) = window {
        workspace.reveal_panel::<ScriptRunnerPanel>(window, cx);
    }
    runner.update(cx, |runner, cx| {
        runner.run_external(INSTALL_COMMAND.to_string(), root.display().to_string(), cx);
    });
    // A run that was refused (the folder is not trusted, say) never started:
    // the toast then says so, instead of claiming an install is going.
    let started = runner.read(cx).is_running();
    let weak_workspace = cx.weak_entity();
    let toast = cx.new(|cx| InstallToast::new(&runner, weak_workspace, started, cx));
    workspace.toggle_status_toast(toast, cx);
    started
}

enum NpmInstallResult {}

/// Which stage the install is in.
enum InstallState {
    Running,
    Done,
    Failed(String),
}

/// The toast shown while the install runs: a spinner and "Installing
/// dependencies…", then what came of it. It watches the Script Runner, which
/// does the work, and stays until the install ends: a plain toast has no
/// spinner and cannot say when something finishes.
struct InstallToast {
    focus_handle: FocusHandle,
    state: InstallState,
    workspace: WeakEntity<Workspace>,
    _runner: Subscription,
}

impl InstallToast {
    fn new(
        runner: &Entity<ScriptRunnerPanel>,
        workspace: WeakEntity<Workspace>,
        started: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(runner, |this, runner, cx| this.runner_changed(&runner, cx));
        Self {
            focus_handle: cx.focus_handle(),
            state: if started {
                InstallState::Running
            } else {
                InstallState::Failed(
                    "npm install did not start. The Script Runner says why.".to_string(),
                )
            },
            workspace,
            _runner: subscription,
        }
    }

    /// Called on every change of the Script Runner (each output line included):
    /// cheap until the run has ended.
    fn runner_changed(&mut self, runner: &Entity<ScriptRunnerPanel>, cx: &mut Context<Self>) {
        if !matches!(self.state, InstallState::Running) {
            return;
        }
        let runner = runner.read(cx);
        if runner.is_running() {
            return;
        }
        self.state = match runner.last_exit() {
            Some((0, false)) => InstallState::Done,
            Some((_, true)) => InstallState::Failed("npm install was stopped.".to_string()),
            Some((code, false)) => {
                InstallState::Failed(format!("npm install failed (exit code {code})."))
            }
            None => InstallState::Failed("npm install did not finish.".to_string()),
        };
        if matches!(self.state, InstallState::Done) {
            // Success needs no further attention; a failure stays until dismissed.
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(8)).await;
                this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
            })
            .detach();
        }
        cx.notify();
    }
}

impl Render for InstallToast {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let finished = !matches!(self.state, InstallState::Running);
        let (indicator, text) = match &self.state {
            InstallState::Running => (
                Spinner::new().small().into_any_element(),
                "Installing dependencies…".to_string(),
            ),
            InstallState::Done => (
                Icon::new(IconName::Check)
                    .color(Color::Success)
                    .into_any_element(),
                "Dependencies installed".to_string(),
            ),
            InstallState::Failed(message) => (
                Icon::new(IconName::XCircle)
                    .color(Color::Error)
                    .into_any_element(),
                message.clone(),
            ),
        };
        let workspace = self.workspace.clone();
        let toast = cx.entity();
        h_flex()
            .id("npm-install-toast")
            .elevation_3(cx)
            .gap_2()
            .py_1p5()
            .px_2p5()
            .flex_none()
            .items_center()
            .bg(cx.theme().colors().surface_background)
            .shadow_lg()
            .child(indicator)
            .child(Label::new(text).color(Color::Default))
            // While it runs too: an install that started by itself did not reveal
            // the Script Runner, and this is the way to it.
            .child(
                Button::new("npm-install-output", "Show output")
                    .color(Color::Muted)
                    .on_click(move |_, window, cx| {
                        let workspace = workspace.clone();
                        // After this click has finished: revealing a panel looks
                        // at every docked panel, which must not happen while one
                        // of them is being updated.
                        window.defer(cx, move |window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    workspace.reveal_panel::<ScriptRunnerPanel>(window, cx);
                                })
                                .ok();
                        });
                    }),
            )
            .when(finished, |this| {
                this.child(
                    IconButton::new("npm-install-dismiss", IconName::Close)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .on_click(move |_, _, cx| {
                            toast.update(cx, |_, cx| cx.emit(DismissEvent));
                        }),
                )
            })
    }
}

impl Focusable for InstallToast {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for InstallToast {}

impl ToastView for InstallToast {
    fn action(&self) -> Option<ToastAction> {
        None
    }

    /// Stays until the install ends (it dismisses itself after a success).
    fn auto_dismiss(&self) -> bool {
        false
    }
}

/// Sets up the install offer for a project that has just been created in `root`.
/// The offer is always made when the project opens and is trusted. With
/// `automatic` the install also starts by itself then, without a click: for a
/// project made with "Run install afterwards" on.
///
/// It always forgets that the offer was made for `root` before: that marker is
/// kept per folder path for good, so a name used before (a test project made,
/// deleted and made again) would otherwise keep its old marker and never be
/// offered the install.
pub fn prepare_install_offer(root: &Path, automatic: bool, cx: &mut App) {
    let kvp = KeyValueStore::global(cx);
    let asked = prompt_key(root);
    let auto = auto_key(root);
    cx.background_spawn(async move {
        kvp.delete_kvp(asked).await.ok();
        if automatic {
            kvp.write_kvp(auto, "1".to_string()).await.ok();
        } else {
            kvp.delete_kvp(auto).await.ok();
        }
    })
    .detach();
}

/// Whether `root` looks like a JavaScript project whose dependencies were
/// never installed: it has a `package.json` file and no `node_modules`.
fn needs_install(root: &Path) -> bool {
    root.join("package.json").is_file() && !root.join("node_modules").exists()
}

/// The key-value store key recording that a project asked for its install to
/// start by itself once it is trusted. Set by [`prepare_install_offer`] and used
/// up by the one install it starts.
fn auto_key(root: &Path) -> String {
    format!("npm-bootstrap-auto:{}", root.display())
}

/// The key-value store key recording that the install prompt was already
/// shown for `root`, so it is offered once per project directory.
fn prompt_key(root: &Path) -> String {
    format!("npm-bootstrap-asked:{}", root.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_offered_only_for_uninstalled_projects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Not a JavaScript project at all.
        assert!(!needs_install(root));

        std::fs::write(root.join("package.json"), "{}").unwrap();
        assert!(needs_install(root));

        // Dependencies already installed.
        std::fs::create_dir(root.join("node_modules")).unwrap();
        assert!(!needs_install(root));
    }

    #[test]
    fn a_directory_named_package_json_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("package.json")).unwrap();
        assert!(!needs_install(dir.path()));
    }

    #[test]
    fn a_node_modules_file_still_counts_as_installed() {
        // `exists`, not `is_dir`: a symlink or stray file named
        // `node_modules` means something else is managing it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::write(dir.path().join("node_modules"), "").unwrap();
        assert!(!needs_install(dir.path()));
    }

    #[test]
    fn missing_directories_need_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!needs_install(&dir.path().join("does-not-exist")));
    }

    #[test]
    fn prompt_key_is_distinct_per_project() {
        let a = prompt_key(Path::new("/work/a"));
        let b = prompt_key(Path::new("/work/b"));
        assert!(a.starts_with("npm-bootstrap-asked:"));
        assert!(a.ends_with("a"));
        assert_ne!(a, b);
        assert_eq!(a, prompt_key(Path::new("/work/a")));
    }
}

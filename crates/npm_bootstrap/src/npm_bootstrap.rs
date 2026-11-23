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

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use db::kvp::KeyValueStore;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Subscription, WeakEntity, Window,
};
use gpui_component::spinner::Spinner;
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
    // A restricted project is offered nothing yet, so the offer is made again
    // when its trust changes (`check_worktree` asks once, by its marker).
    forge_trust::on_trust_changed(cx, |workspace, cx| {
        let first_worktree = workspace.worktrees(cx).next();
        if let Some(worktree) = first_worktree {
            check_worktree(workspace, &worktree, cx);
        }
    })
    .detach();

    let first_worktree = workspace.worktrees(cx).next();
    if let Some(worktree) = first_worktree {
        check_worktree(workspace, &worktree, cx);
        return;
    }

    let project = workspace.project().clone();
    let handled = Rc::new(Cell::new(false));
    cx.subscribe(&project, move |workspace, project, event, cx| {
        if handled.get() {
            return;
        }
        let ProjectEvent::WorktreeAdded(id) = event else {
            return;
        };
        let Some(worktree) = project.read(cx).worktree_for_id(*id, cx) else {
            return;
        };
        handled.set(true);
        check_worktree(workspace, &worktree, cx);
    })
    .detach();
}

fn check_worktree(
    workspace: &mut Workspace,
    worktree: &Entity<Worktree>,
    cx: &mut Context<Workspace>,
) {
    // Not while the project is in Restricted Mode: the install would run its own
    // scripts. The trust-change hook above comes back here once it is trusted.
    let project = workspace.project().clone();
    if forge_trust::project_restricted(&project, cx) {
        return;
    }
    let root = worktree.read(cx).abs_path();
    if !needs_install(&root) {
        return;
    }

    let key = prompt_key(&root);
    if KeyValueStore::global(cx)
        .read_kvp(&key)
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }

    // "Ask once" means the marker goes down the moment the prompt is shown,
    // not only if the user accepts it — otherwise dismissing the toast would
    // just re-prompt on the next open.
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move {
        kvp.write_kvp(key, "1".to_string()).await.ok();
    })
    .detach();

    let install_root = root.to_path_buf();
    let workspace_handle = cx.weak_entity();
    workspace.show_toast(
        Toast::new(
            NotificationId::unique::<NpmInstallPrompt>(),
            "package.json found — install dependencies?",
        )
        .on_click("Run npm install", move |window, cx| {
            run_npm_install(install_root.clone(), workspace_handle.clone(), window, cx);
        }),
        cx,
    );
}

enum NpmInstallPrompt {}

/// Runs `npm install` in `root` in the Script Runner, whose output panel is
/// revealed so the install can be watched. The Script Runner asks the trust gate
/// itself, so a folder that is not trusted is refused there with its message.
fn run_npm_install(
    root: PathBuf,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        let Some(runner) = workspace.panel::<ScriptRunnerPanel>(cx) else {
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<NpmInstallResult>(),
                    "The Script Runner is not available, so npm install was not run.",
                ),
                cx,
            );
            return;
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
            return;
        }
        workspace.reveal_panel::<ScriptRunnerPanel>(window, cx);
        runner.update(cx, |runner, cx| {
            runner.run_external("npm install".to_string(), root.display().to_string(), cx);
        });
        // A run that was refused (the folder is not trusted, say) never started:
        // the toast then says so, instead of claiming an install is going.
        let started = runner.read(cx).is_running();
        let weak_workspace = cx.weak_entity();
        let toast = cx.new(|cx| InstallToast::new(&runner, weak_workspace, started, cx));
        workspace.toggle_status_toast(toast, cx);
    });
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
            .when(finished, |this| {
                this.child(
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
                .child(
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

/// Records that the install offer for `root` has been dealt with, so it is not
/// made: for a project whose creator already said the dependencies are not
/// wanted. The same marker the offer itself leaves once it has been shown.
pub fn decline_install_offer(root: &Path, cx: &mut App) {
    let kvp = KeyValueStore::global(cx);
    let key = prompt_key(root);
    cx.background_spawn(async move {
        kvp.write_kvp(key, "1".to_string()).await.ok();
    })
    .detach();
}

/// Whether `root` looks like a JavaScript project whose dependencies were
/// never installed: it has a `package.json` file and no `node_modules`.
fn needs_install(root: &Path) -> bool {
    root.join("package.json").is_file() && !root.join("node_modules").exists()
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

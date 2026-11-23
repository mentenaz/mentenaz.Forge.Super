//! The local half of the Git panel's "Using Helm" create flow: wiring the
//! current workspace to a repository `gh_create_repo` just created on
//! GitHub. The API call can't do any of this itself — its `auto_init`
//! commit would make pushing the workspace's existing files fail — so it
//! runs here, after GitHub confirms the repository: README/.gitignore,
//! `git init`, the initial commit, the remote, and the first push.
//!
//! All of it runs as the account the repository was created with, not as
//! whatever the global git config and credential helpers say: the
//! workspace has no repository yet, so the usual per-repository wiring
//! (`commit_identity.rs`, `repo_access.rs`) has nothing to attach to. The
//! account's commit identity is written into the new repository before the
//! initial commit, and the push gets the account's token the way a clone
//! does, with credential helpers and prompts turned off, so no sign-in
//! window can appear.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use gpui::AsyncApp;
use helm_backend::github::{
    gh_get_current_user, gh_get_repo, gh_get_verified_emails, git_auth_config,
};

use super::accounts::set_commit_identity;
use super::commit_identity::git_config;
use super::*;

/// The commit used when the dialog's message field was left blank.
pub(super) const DEFAULT_COMMIT_MESSAGE: &str = "Initial commit";

/// What the Create dialog offers in [`HelmModalKind::CreateRepo`]'s
/// `attach_local` mode, parked on the panel while `gh_create_repo` runs and
/// handed to [`HelmPanel::attach_created_repo`] once GitHub confirms the
/// repository.
#[derive(Clone)]
pub(super) struct CreateRepoAttach {
    /// The initial commit's message; blank falls back to
    /// [`DEFAULT_COMMIT_MESSAGE`].
    pub(super) message: String,
    /// Write a `README.md` when the workspace has none.
    pub(super) readme: bool,
    /// Write a default `.gitignore` when the workspace has none.
    pub(super) gitignore: bool,
    /// The workspace already is a repository with commits of its own (the
    /// publish dialog's "Create new repo"): the new repository is connected
    /// and pushed to as it is, and the three fields above are not used.
    pub(super) existing: bool,
}

/// A failed step of the local wiring, tagged with whether the repository
/// had already been initialized — a failure before that point left the
/// workspace untouched, one after it left a usable local repository (and
/// possibly a remote) behind.
struct AttachFailure {
    local_repo: bool,
    message: String,
}

/// Who the initial commit is authored as.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    name: String,
    email: String,
}

/// The identity for an account with no commit email saved, by the same
/// rule as `commit_identity.rs`: the global identity when its email is one
/// of the account's verified addresses, otherwise the account's GitHub name
/// with GitHub's private noreply address. Never another account's email.
fn identity_for(
    login: &str,
    id: u64,
    github_name: Option<&str>,
    verified_emails: &[String],
    global_name: Option<String>,
    global_email: Option<String>,
) -> Identity {
    let github_name = github_name.unwrap_or(login).to_string();
    match global_email {
        Some(email) if verified_emails.iter().any(|own| own.eq_ignore_ascii_case(&email)) => {
            Identity {
                name: global_name.unwrap_or(github_name),
                email,
            }
        }
        _ => Identity {
            name: github_name,
            email: format!("{id}+{login}@users.noreply.github.com"),
        },
    }
}

impl HelmPanel {
    /// Opens the "Publish to GitHub" dialog the Git panel's Publish button
    /// uses when the repository has no remote. Without a GitHub login it
    /// reports a toast and sends the user to the Helm panel, as
    /// [`Self::open_create_repo_for_attach`] does.
    pub fn open_publish_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.account.is_empty() {
            self.notify(
                "Sign in to GitHub from the Helm panel to publish this repository. For another \
                 host, add a remote with \"Create Remote Repository\" in the branch picker.",
                cx,
            );
            if let Some(workspace) = self.workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    workspace.focus_panel::<HelmPanel>(window, cx);
                });
            }
            return;
        }
        if let Err(message) = self.check_publish_trust(cx) {
            self.notify(message, cx);
            return;
        }
        self.open_workspace_modal(HelmModalKind::PublishRepo, window, cx);
    }

    /// The publish dialog's Connect: adds the chosen repository as `origin`
    /// and pushes, without forcing.
    ///
    /// Unlike the Create flow this never runs `git init`, `git add` or a
    /// commit: the repository and its commits are the user's own. Nothing is
    /// forced, so a repository that already has history of its own makes the
    /// push fail with git's message instead of being overwritten.
    pub(super) fn handle_publish_connect(&mut self, repo: Repo, cx: &mut Context<Self>) {
        if let Err(message) = self.check_publish_trust(cx) {
            self.notify(message, cx);
            return;
        }
        let Some(root) = self.workspace_root(cx) else {
            self.notify("This window has no folder to publish.", cx);
            return;
        };
        let Some(login) = self.signed_in_login().map(str::to_string) else {
            self.notify("Helm is no longer signed in, so it cannot publish this workspace.", cx);
            return;
        };
        let saved = saved_identity(&login, cx);

        self.notify(format!("Connecting this workspace to {}…", repo.full_name), cx);
        let gh_state = self.gh_state.clone();
        let url = repo.clone_url;
        let full_name = repo.full_name;
        let retry_root = root.clone();
        cx.spawn(async move |this, cx| {
            let token = gh_state.token().await;
            let result = match account_identity(&gh_state, &login, &root, saved, cx).await {
                Ok(identity) => {
                    on_tokio(async move {
                        connect_workspace(&root, &url, &identity, token.as_deref()).await
                    })
                    .await
                }
                Err(error) => Err(AttachFailure {
                    local_repo: false,
                    message: format!("Could not read the account's commit identity: {error}"),
                }),
            };
            this.update(cx, |this, cx| match result {
                Ok(()) => this.notify(format!("Connected to {full_name} and pushed this workspace"), cx),
                Err(AttachFailure {
                    local_repo: true,
                    message,
                }) => this.notify_push_failed(
                    format!("{} — the remote was kept.", explain_push_error(message)),
                    retry_root,
                    cx,
                ),
                Err(AttachFailure {
                    local_repo: false,
                    message,
                }) => this.notify(message, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Whether the workspace's folder may have `git` run in it: refused for a
    /// folder in Restricted Mode, with the gate's message. Asked up front, so
    /// nothing is created on GitHub for a workspace that could not then be
    /// published; [`run_git`] asks again for every command. A window with no
    /// folder passes, and is reported by the callers.
    pub(super) fn check_publish_trust(&self, cx: &mut App) -> Result<(), String> {
        match self.workspace_root(cx) {
            Some(root) => forge_trust_gate::check_run(
                "helm_panel::check_publish_trust",
                "git",
                &[],
                Some(root.as_path()),
            ),
            None => Ok(()),
        }
    }

    /// A banner for the top of the panel while the workspace's project is in
    /// Restricted Mode. The panel stays usable (the profile, organisations and
    /// GitHub search have nothing to do with the folder); only the actions that
    /// run `git` in the folder are refused, and the banner says so.
    pub(super) fn render_trust_banner(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let project = self.workspace.upgrade()?.read(cx).project().clone();
        forge_trust::project_restricted(&project, cx).then(|| {
            forge_trust::restricted_banner(
                "This project is in Restricted Mode, so publishing it to GitHub is \
                 turned off until you trust it.",
                cx,
            )
            .into_any_element()
        })
    }

    /// The first directory worktree of this window's workspace: where a
    /// repository is created or connected.
    fn workspace_root(&self, cx: &mut App) -> Option<PathBuf> {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .worktrees(cx)
                    .find(|worktree| !worktree.read(cx).is_single_file())
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            })
            .ok()
            .flatten()
    }

    /// The publish dialog's "Create new repo": the Create dialog in its
    /// existing-repository mode.
    pub(super) fn handle_publish_create_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let folder = self
            .workspace_root(cx)
            .and_then(|root| root.file_name().map(|name| name.to_string_lossy().into_owned()))
            .unwrap_or_default();
        self.open_workspace_modal(
            HelmModalKind::CreateRepo {
                attach_local: true,
                existing: Some(folder),
            },
            window,
            cx,
        );
    }

    /// The follow-up to a `CreateRepo { attach_local: true }` run: creates
    /// the requested local files, initializes the workspace's first
    /// directory worktree as a Git repository, commits everything (the
    /// workspace was necessarily uninitialized — that's the state the Git
    /// panel's "Using Helm" button shows in), points `origin` at the new
    /// GitHub repository, and pushes. Each outcome is one toast; a failure
    /// keeps whatever succeeded so far, so the push can simply be retried
    /// from the Git panel.
    pub(super) fn attach_created_repo(
        &mut self,
        repo: Repo,
        attach: CreateRepoAttach,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.workspace_root(cx) else {
            self.notify(
                format!(
                    "Created {} on GitHub, but this window has no folder to attach it to.",
                    repo.name
                ),
                cx,
            );
            return;
        };

        let Some(login) = self.signed_in_login().map(str::to_string) else {
            self.notify(
                format!(
                    "Created {} on GitHub, but Helm is no longer signed in to push this \
                     workspace to it.",
                    repo.name
                ),
                cx,
            );
            return;
        };
        let saved = saved_identity(&login, cx);

        self.notify(format!("Publishing {} to GitHub…", repo.name), cx);
        let fs = <dyn fs::Fs>::global(cx);
        let gh_state = self.gh_state.clone();
        let owner = repo.owner.login;
        let retry_root = root.clone();
        let name = repo.name;
        let url = repo.clone_url;
        let description = repo.description.unwrap_or_default();
        // The task owns its own copy — the toast below names the repository
        // after the future has consumed the originals.
        let created = name.clone();
        cx.spawn(async move |this, cx| {
            let token = gh_state.token().await;
            let identity = account_identity(&gh_state, &login, &root, saved, cx).await;
            let result = match identity {
                Ok(identity) => {
                    wait_until_pushable(&gh_state, &owner, &name, cx).await;
                    on_tokio(async move {
                        if attach.existing {
                            connect_workspace(&root, &url, &identity, token.as_deref()).await
                        } else {
                            attach_workspace(
                                &*fs,
                                &root,
                                &name,
                                &description,
                                &url,
                                &attach,
                                &identity,
                                token.as_deref(),
                            )
                            .await
                        }
                    })
                    .await
                }
                Err(error) => Err(AttachFailure {
                    local_repo: false,
                    message: format!(
                        "Created {created} on GitHub, but could not read the account's \
                         commit identity: {error}"
                    ),
                }),
            };
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.notify(
                        format!("Created {created} and pushed this workspace to GitHub"),
                        cx,
                    );
                }
                Err(AttachFailure {
                    local_repo: true,
                    message,
                }) => this.notify_push_failed(
                    format!("{} — the local repository was kept.", explain_push_error(message)),
                    retry_root,
                    cx,
                ),
                Err(AttachFailure {
                    local_repo: false,
                    message,
                }) => this.notify(message, cx),
            })
            .ok();
        })
        .detach();
    }

    /// The failure toast for a publish that got as far as a local
    /// repository, with a "Retry push" that pushes it again.
    fn notify_push_failed(&self, message: String, root: PathBuf, cx: &mut Context<Self>) {
        let panel = cx.entity().downgrade();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<HelmPanel>(), message).on_click(
                        "Retry push",
                        move |_, cx| {
                            panel
                                .update(cx, |this, cx| this.retry_push(root.clone(), cx))
                                .ok();
                        },
                    ),
                    cx,
                );
            })
            .ok();
    }

    /// Pushes the repository at `root` again, as the panel's account, the
    /// same way the first push did.
    fn retry_push(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.notify("Pushing this workspace to GitHub…", cx);
        let gh_state = self.gh_state.clone();
        cx.spawn(async move |this, cx| {
            let token = gh_state.token().await;
            let result = {
                let root = root.clone();
                on_tokio(async move {
                    git_as_account(&["push", "-u", "origin", "HEAD"], &root, token.as_deref())
                        .await
                })
                .await
            };
            this.update(cx, |this, cx| match result {
                Ok(()) => this.notify("Pushed this workspace to GitHub", cx),
                Err(message) => this.notify_push_failed(explain_push_error(message), root, cx),
            })
            .ok();
        })
        .detach();
    }
}

/// A failed push's message, with GitHub's private-email rejection (GH007)
/// explained, since git only passes on GitHub's terse line.
fn explain_push_error(message: String) -> String {
    if message.contains("GH007") {
        format!(
            "GitHub refused the push because the commit uses an email address the account \
             keeps private (GH007). Set the account's commit email to its GitHub noreply \
             address in Helm, re-author the commit (git commit --amend --reset-author), then \
             retry. ({message})"
        )
    } else {
        message
    }
}

/// How many times, a second apart, to ask GitHub for a repository it has
/// just created before pushing to it anyway.
const PUSHABLE_ATTEMPTS: u32 = 10;

/// Waits until the API can see `owner/name`. Straight after creating a
/// repository GitHub can still answer "Repository not found" for a few
/// seconds, which made the first push fail. Gives up after about ten
/// seconds, or on any answer other than "not found", and lets the push
/// report whatever is wrong.
async fn wait_until_pushable(gh_state: &Arc<GhState>, owner: &str, name: &str, cx: &AsyncApp) {
    for attempt in 1..=PUSHABLE_ATTEMPTS {
        let (owner, name, state) = (owner.to_string(), name.to_string(), gh_state.clone());
        match on_tokio(async move { gh_get_repo(owner, name, &state).await }).await {
            Err(GhError::NotFound { .. }) if attempt < PUSHABLE_ATTEMPTS => {
                cx.background_executor()
                    .timer(Duration::from_secs(1))
                    .await;
            }
            _ => return,
        }
    }
}

/// Runs every local step, in order, stopping at the first failure. The
/// caller words the outcome based on [`AttachFailure::local_repo`]. Commits
/// are authored as `identity`, and the push logs in with `token`.
async fn attach_workspace(
    fs: &dyn fs::Fs,
    root: &Path,
    name: &str,
    description: &str,
    url: &str,
    attach: &CreateRepoAttach,
    identity: &Identity,
    token: Option<&str>,
) -> Result<(), AttachFailure> {
    // An explicit `main` so the GitHub default branch matches what Zed's
    // own `git init` produces; gits too old to know `--initial-branch`
    // get a plain init, whose branch the push below adopts either way. The
    // first error is deliberately dropped in favor of the fallback's —
    // on an old git it's just the unknown-flag usage text.
    if git(&["init", "--initial-branch=main"], root).await.is_err() {
        git(&["init"], root)
            .await
            .map_err(|message| AttachFailure {
                local_repo: false,
                message,
            })?;
    }

    // In the repository's own config, as Helm does for every repository of
    // the account, so the initial commit (and later ones from any tool) is
    // not authored as the global identity.
    for (key, value) in [
        ("user.name", identity.name.as_str()),
        ("user.email", identity.email.as_str()),
    ] {
        git(&["config", "--local", key, value], root)
            .await
            .map_err(local_failure)?;
    }

    if attach.readme {
        let path = root.join("README.md");
        if !fs.is_file(&path).await {
            fs.write(&path, readme_content(name, description).as_bytes())
                .await
                .map_err(|error| AttachFailure {
                    local_repo: true,
                    message: format!("Writing README.md failed: {error}"),
                })?;
        }
    }
    if attach.gitignore {
        let path = root.join(".gitignore");
        if !fs.is_file(&path).await {
            fs.write(&path, DEFAULT_GITIGNORE.as_bytes())
                .await
                .map_err(|error| AttachFailure {
                    local_repo: true,
                    message: format!("Writing .gitignore failed: {error}"),
                })?;
        }
    }

    git(&["add", "-A"], root).await.map_err(local_failure)?;
    // `--allow-empty` so a workspace whose files are all ignored still ends
    // up with the initial commit the push needs.
    git(
        &["commit", "--allow-empty", "-m", commit_message(&attach.message)],
        root,
    )
    .await
    .map_err(local_failure)?;
    // A re-run (a first attempt that failed at the push) may already have
    // the remote — move it to the just-created repository rather than
    // failing.
    if git(&["remote", "add", "origin", url], root).await.is_err() {
        git(&["remote", "set-url", "origin", url], root)
            .await
            .map_err(local_failure)?;
    }
    git_as_account(&["push", "-u", "origin", "HEAD"], root, token)
        .await
        .map_err(local_failure)
}

/// The account's saved commit identity, if it has one.
fn saved_identity(login: &str, cx: &App) -> Option<Identity> {
    let account = HelmPanelSettings::get_global(cx).account(login)?;
    let email = account.git_email.clone()?;
    let name = account
        .git_name
        .clone()
        .unwrap_or_else(|| account.display_name().to_string());
    Some(Identity { name, email })
}

/// The identity commits are authored as for `login`: the saved one, or else
/// worked out from the account (see [`identity_for`]) and remembered, so the
/// repository's later wiring finds the identity it was committed with.
async fn account_identity(
    gh_state: &Arc<GhState>,
    login: &str,
    root: &Path,
    saved: Option<Identity>,
    cx: &AsyncApp,
) -> Result<Identity, GhError> {
    if let Some(identity) = saved {
        return Ok(identity);
    }
    let (state, root) = (gh_state.clone(), root.to_path_buf());
    // Off the main thread: two API calls and two `git`s.
    let identity = on_tokio(async move {
        let user = gh_get_current_user(&state).await?;
        // A token without the `user` scope cannot read them.
        let emails = gh_get_verified_emails(&state).await.unwrap_or_default();
        Ok::<_, GhError>(identity_for(
            &user.login,
            user.id,
            user.name.as_deref(),
            &emails,
            git_config(&root, "--global", "user.name"),
            git_config(&root, "--global", "user.email"),
        ))
    })
    .await?;
    let (login, remembered) = (login.to_string(), identity.clone());
    cx.update(|cx| set_commit_identity(&login, &remembered.name, &remembered.email, cx));
    Ok(identity)
}

/// Connects the repository at `root` to `url` and pushes the current
/// branch, as the account whose `token` it is. The repository must already
/// have a commit; nothing is initialized, staged, committed or forced.
async fn connect_workspace(
    root: &Path,
    url: &str,
    identity: &Identity,
    token: Option<&str>,
) -> Result<(), AttachFailure> {
    if git(&["rev-parse", "--verify", "HEAD"], root).await.is_err() {
        return Err(AttachFailure {
            local_repo: false,
            message: "This repository has no commits yet. Commit your changes first, then \
                      publish."
                .to_string(),
        });
    }

    // A repository that already names a commit email keeps it: it was set by
    // hand or by another tool, and is not overwritten here.
    if git_config(root, "--local", "user.email").is_none() {
        for (key, value) in [
            ("user.name", identity.name.as_str()),
            ("user.email", identity.email.as_str()),
        ] {
            git(&["config", "--local", key, value], root)
                .await
                .map_err(local_failure)?;
        }
    }

    match git_config(root, "--local", "remote.origin.url") {
        Some(existing) if existing.trim() == url => {}
        Some(existing) => {
            return Err(AttachFailure {
                local_repo: false,
                message: format!(
                    "This repository already has an origin ({}). Remove or change it first.",
                    existing.trim()
                ),
            });
        }
        None => git(&["remote", "add", "origin", url], root)
            .await
            .map_err(local_failure)?,
    }

    git_as_account(&["push", "-u", "origin", "HEAD"], root, token)
        .await
        .map_err(local_failure)
}

fn local_failure(message: String) -> AttachFailure {
    AttachFailure {
        local_repo: true,
        message,
    }
}

/// The commit the initial commit uses — a blank field falls back to
/// [`DEFAULT_COMMIT_MESSAGE`], so `git commit` can't fail on an empty
/// message.
fn commit_message(entered: &str) -> &str {
    match entered.trim() {
        "" => DEFAULT_COMMIT_MESSAGE,
        trimmed => trimmed,
    }
}

/// The `README.md` written when the workspace has none: the repository's
/// name as its heading, with its description under it when one was given.
fn readme_content(name: &str, description: &str) -> String {
    if description.trim().is_empty() {
        format!("# {name}\n")
    } else {
        format!("# {name}\n\n{}\n", description.trim())
    }
}

/// What the gitignore switch writes when the workspace has none — the OS
/// and tooling junk that should never land in a fresh repository. A real
/// template picker (GitHub's `gitignore_template` list) can replace this
/// later without touching the flow around it.
const DEFAULT_GITIGNORE: &str = "\
# OS
.DS_Store
Thumbs.db
desktop.ini

# Editors
.vscode/
.idea/

# Dependencies and build output
node_modules/
target/
dist/
build/
__pycache__/
*.pyc
.venv/
venv/

# Logs
*.log

# Environment
.env
.env.local
";

/// Runs `git <args>` in `cwd` with no environment config of its own.
async fn git(args: &[&str], cwd: &Path) -> Result<(), String> {
    run_git(args, cwd, Vec::new()).await
}

/// Runs `git <args>` in `cwd` logged in to github.com as the account whose
/// `token` it is, the way a clone does (`git_auth_config`). Every credential
/// helper is turned off, so neither `gh`'s active account nor Git Credential
/// Manager is asked, and no sign-in window or prompt can appear: without a
/// token the step fails with git's own message instead.
async fn git_as_account(args: &[&str], cwd: &Path, token: Option<&str>) -> Result<(), String> {
    // An empty value clears the helpers configured so far.
    let mut config = vec![("credential.helper".to_string(), String::new())];
    config.extend(token.map(git_auth_config).unwrap_or_default());
    run_git(args, cwd, config).await
}

/// Runs `git <args>` in `cwd` with `config` as `GIT_CONFIG_*` environment
/// config, folding git's own stderr into the error, so a failed step's
/// toast carries git's actual explanation (missing author identity,
/// rejected push, …). Stdin is null, terminal prompts and Git Credential
/// Manager's windows are turned off, and on Windows no console window is
/// opened, so a step can never hang waiting for input or pop anything up.
///
/// The one place Helm starts `git` in a project folder, so it is also where
/// the trust gate is asked: a repository's own config and hooks can run
/// programs (`core.fsmonitor`, `core.sshCommand`, a pre-commit or pre-push
/// hook), which a folder in Restricted Mode must not be able to start.
async fn run_git(args: &[&str], cwd: &Path, config: Vec<(String, String)>) -> Result<(), String> {
    forge_trust_gate::check_run("helm_panel::attach::run_git", "git", args, Some(cwd))?;
    let mut command = tokio::process::Command::new("git");
    command
        // Never let a repository name a program to ask about changes, as
        // Zed's own git runner does.
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env("GIT_CONFIG_COUNT", config.len().to_string())
        .stdin(Stdio::null());
    for (index, (key, value)) in config.into_iter().enumerate() {
        command
            .env(format!("GIT_CONFIG_KEY_{index}"), key)
            .env(format!("GIT_CONFIG_VALUE_{index}"), value);
    }
    #[cfg(target_os = "windows")]
    command.creation_flags(0x0800_0000);
    let output = command
        .output()
        .await
        .map_err(|error| format!("Running `git {}` failed: {error}", args[0]))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = if stderr.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout)
    } else {
        stderr
    };
    let detail = detail.trim();
    if detail.is_empty() {
        Err(format!("`git {}` failed", args[0]))
    } else {
        Err(format!("`git {}` failed: {detail}", args[0]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_commit_message_falls_back() {
        assert_eq!(commit_message(""), DEFAULT_COMMIT_MESSAGE);
        assert_eq!(commit_message("   \t"), DEFAULT_COMMIT_MESSAGE);
        assert_eq!(commit_message(" first pass "), "first pass");
    }

    #[test]
    fn the_global_identity_is_used_only_when_its_email_is_the_accounts() {
        let sencorp = ("FrancoisHuyzers-Sencorp", 42, Some("Francois H"));
        let verified = vec!["francoish@sencorpdigital.com".to_string()];

        // The global email belongs to another account: GitHub's private
        // address for this one, never the other account's email.
        assert_eq!(
            identity_for(
                sencorp.0,
                sencorp.1,
                sencorp.2,
                &verified,
                Some("Mentenaz-Server (Francois)".into()),
                Some("mentenaz@outlook.de".into()),
            ),
            Identity {
                name: "Francois H".into(),
                email: "42+FrancoisHuyzers-Sencorp@users.noreply.github.com".into(),
            }
        );

        // The global email is one of this account's: use the global identity.
        assert_eq!(
            identity_for(
                sencorp.0,
                sencorp.1,
                sencorp.2,
                &verified,
                Some("Francois".into()),
                Some("FrancoisH@SencorpDigital.com".into()),
            ),
            Identity {
                name: "Francois".into(),
                email: "FrancoisH@SencorpDigital.com".into(),
            }
        );

        // No name on GitHub: the login.
        assert_eq!(
            identity_for("someone", 7, None, &[], None, None).name,
            "someone"
        );
    }

    #[test]
    fn readme_leads_with_the_name() {
        assert_eq!(readme_content("AniCraft", ""), "# AniCraft\n");
        assert_eq!(
            readme_content("AniCraft", "  A toy. "),
            "# AniCraft\n\nA toy.\n"
        );
    }

    #[test]
    fn default_gitignore_covers_the_usual_junk() {
        for pattern in [".DS_Store", "node_modules/", ".env", "target/"] {
            assert!(
                DEFAULT_GITIGNORE.contains(pattern),
                "{pattern} missing from the default .gitignore"
            );
        }
    }
}

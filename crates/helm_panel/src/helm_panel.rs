//! The "Helm" (GitHub) panel — Auth (sign-in) → Menu, backed by the
//! `helm_backend::github` module. Ported from
//! `Forge_Old/panels/github/{gate,auth,menu}.rs`, using
//! `helm_backend::on_tokio` to bridge `github`'s tokio-native calls onto
//! GPUI's own `cx.spawn`, and
//! `gpui_component` elements (`Button`, `list::ListItem`, `Icon`,
//! `Spinner`) instead of the old `forge_ui` crate.

mod account_switcher;
mod accounts;
mod activity;
mod attach;
mod auth;
mod branches;
mod changes;
mod clone;
mod collaborators;
mod commit_identity;
mod gh_cli_import;
mod helm_panel_settings;
mod insights;
mod invitations;
mod issues;
mod lists;
mod loading;
mod navigation;
mod orgs;
mod profile;
mod pulls;
mod releases_packages;
mod repo_access;
mod repos;
mod repository_modal;
mod state;
mod workflow_run_tab;

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Action, App, AppContext, AsyncWindowContext, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    PathPromptOptions, Render, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Task, TaskExt, WeakEntity, Window, actions, div, prelude::FluentBuilder as _, px, relative,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable as _, Sizable as _, StyledExt,
    alert::Alert,
    button::{Button, ButtonGroup, ButtonVariants as _},
    description_list::{DescriptionItem, DescriptionList},
    form::{field, v_form},
    group_box::GroupBox,
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    link::Link,
    list::ListItem,
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::ScrollableElement as _,
    separator::Separator,
    spinner::Spinner,
    switch::Switch,
    tag::Tag as Pill,
    text::markdown,
    v_flex,
};
use gpui_flow::{Controls, FlowGraph, FlowNode, FlowState, NodeId};
use serde_json::json;

use accounts::{HelmAccountEvent, HelmAccounts};
use attach::{CreateRepoAttach, DEFAULT_COMMIT_MESSAGE};
use changes::*;
use collaborators::*;
pub use commit_identity::CommitIdentity;
use helm_backend::github::{
    Branch, CheckRun, CloneCanceller, CloneEvent, CloneFailure, Collaborator, Comment,
    CommitSummary, Deployment, DeviceFlow, ErrorLink, GhCliAccount, GhCliSignInCanceller, GhCliSignInFailure,
    GhError, GhState, GitHubUser, GitHubUserDetail, HELM_OAUTH_CLIENT_ID, HELM_OAUTH_SCOPES, Issue,
    OrgDetail, OrgInvitation, OrgMember, Package, PackageVersion, Page, PollOutcome, Pull, Release,
    Repo, RepoInvitation, RepoTraffic, Tag, WorkflowJob, WorkflowRun, clone_canceller, clone_repo,
    fetch_page, fetch_page_under, gh_accept_org_invitation, gh_accept_repo_invitation,
    gh_add_collaborator, gh_cli_accounts, gh_cli_sign_in, gh_cli_sign_in_canceller, gh_cli_token,
    gh_create_issue_comment, gh_create_pull, gh_create_release, gh_create_repo,
    gh_decline_org_invitation, gh_decline_repo_invitation, gh_delete_branch, gh_get_current_user,
    gh_get_pull, gh_list_check_runs, gh_merge_pull, gh_update_pull, gh_get_org_detail, gh_get_org_logins,
    gh_get_org_role, gh_get_repo_invitations, gh_get_repos, gh_get_traffic_clones,
    gh_get_traffic_paths, gh_get_traffic_referrers, gh_get_traffic_views, gh_get_user,
    gh_get_workflow_run, gh_get_workflow_run_jobs, gh_list_dependabot_alerts,
    gh_list_issue_comments, gh_list_org_invitations, gh_list_package_versions, gh_list_packages,
    gh_list_secret_scanning_alerts, gh_remove_collaborator, gh_update_repo, gh_update_topics,
    gh_update_user, parse_device_prompt, peek_page, peek_page_under, poll_for_token,
    request_device_code, requests, search_repositories_page,
};
use helm_backend::{EventRecvError, on_tokio};
pub use helm_panel_settings::{HelmAccount, HelmAccountSource, HelmPanelSettings};
use helm_ui::*;
pub use repo_access::RepoAccess;
use pulls::*;
use repository_modal::*;
use settings::{DockSide, Settings as _};
use state::*;
use workflow_run_tab::*;
use workspace::{
    Item, ModalView, Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

actions!(
    helm_panel,
    [
        ToggleFocus,
        /// Opens the selected repository in the read-only Workspace tab.
        OpenRepositoryWorkspace
    ]
);

type ListView = helm_ui::ListView<HelmPanel>;

impl HelmView for HelmPanel {
    fn gh_state(&self) -> &Arc<GhState> {
        &self.gh_state
    }

    fn repo(&self) -> Option<&Repo> {
        self.selected_repo.as_ref()
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<HelmPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &OpenRepositoryWorkspace, window, cx| {
            if let Some(panel) = workspace.panel::<HelmPanel>(cx) {
                panel.update(cx, |panel, cx| {
                    panel.open_selected_repository_workspace(window, cx)
                });
            }
        });
        workspace.register_action(
            |workspace, _: &helm_workspace::OpenWorkspaceWorkflowRun, window, cx| {
                let active_tab = workspace
                    .active_pane()
                    .read(cx)
                    .active_item()
                    .and_then(|item| item.downcast::<helm_workspace::WorkspaceTab>());
                let Some(active_tab) = active_tab else {
                    return;
                };
                let context = active_tab.read(cx).workflow_run_context();
                if let Some((run, owner, repo, gh_state)) = context {
                    open_workflow_run_tab(run, owner, repo, gh_state, workspace, window, cx);
                }
            },
        );
        workspace.register_action(
            |workspace, _: &helm_workspace::CloneRepositoryForWorkspace, window, cx| {
                let active_tab = workspace
                    .active_pane()
                    .read(cx)
                    .active_item()
                    .and_then(|item| item.downcast::<helm_workspace::WorkspaceTab>());
                let Some(active_tab) = active_tab else {
                    return;
                };
                let (repo, file) = active_tab.read_with(cx, |tab, _| {
                    (
                        tab.repository().clone(),
                        tab.requested_clone_file().map(str::to_string),
                    )
                });
                if let Some(panel) = workspace.panel::<HelmPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.selected_repo = Some(repo);
                        panel.open_clone_modal_for_file(file, window, cx);
                    });
                }
            },
        );
        workspace.register_action(
            |workspace, _: &helm_workspace::AuthorizeRepositoryWorkspace, window, cx| {
                if let Some(panel) = workspace.panel::<HelmPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.prepare_workspace_scope_authorization(cx)
                    });
                    workspace.focus_panel::<HelmPanel>(window, cx);
                }
            },
        );
    })
    .detach();
}

pub struct HelmPanel {
    focus_handle: FocusHandle,
    gh_state: Arc<GhState>,
    screen: HelmScreen,
    /// Screens navigated away from via [`Self::navigate_to`], most recent
    /// last. Internal state-machine transitions (auth succeeding, signing
    /// out) assign `self.screen`/call `set_screen`
    /// directly and don't touch this — you can't usefully "go back" to the
    /// login screen.
    back_stack: Vec<HelmScreen>,
    /// Screens popped off `back_stack` by [`Self::go_back`], available to
    /// [`Self::go_forward`]. Cleared whenever [`Self::navigate_to`] is used.
    forward_stack: Vec<HelmScreen>,
    load_state: LoadState,
    error_msg: String,
    /// Where on GitHub the error in `error_msg` can be put right, if
    /// anywhere (single sign-on, an organisation's approval of Helm).
    error_link: Option<ErrorLink>,

    // Auth
    /// Whether the stored sign-in has been checked at least once.
    auth_initialized: bool,
    /// The GitHub login this panel uses. Until a sign-in sets it, the
    /// default account from settings.
    active_login: Option<String>,
    sign_in: SignIn,
    sign_in_for: SignInFor,
    /// The sign-in running for `sign_in`. Dropping it stops the polling.
    sign_in_task: Option<Task<()>>,
    /// The pasted-token field, and whether it is shown.
    token_input: Entity<InputState>,
    token_input_open: bool,
    /// Set while "Add account" from the account menu is signing in, so
    /// cancelling goes back to the account in use.
    adding_account: bool,
    /// Whether the GitHub CLI is installed and which accounts it has, for
    /// "Use GitHub CLI", and whether that list is open.
    gh_cli: GhCliState,
    gh_cli_picker_open: bool,
    /// Signing `gh` in to an account it does not have yet.
    gh_cli_login: Option<GhCliLogin>,
    /// Whether the workspace's account can push to the workspace's GitHub
    /// repository (see `repo_access.rs`), which account and repository that
    /// was checked for, and the checks already offered as a toast.
    repo_access: Option<RepoAccess>,
    repo_access_key: Option<String>,
    repo_access_offered: repo_access::OfferedChecks,
    _git_store_subscription: Option<Subscription>,
    /// Who commits in the workspace are authored as (`commit_identity.rs`),
    /// and which account and repository that was worked out for.
    commit_identity: CommitIdentity,
    commit_identity_key: Option<String>,
    /// The repository folder whose git push, pull and fetch were given this
    /// account's token (`repo_access.rs`), to take it back.
    git_credentials_dir: Option<std::path::PathBuf>,
    /// This panel's number in `ide_env`'s per-folder git settings, so that
    /// two windows on the same folder do not undo each other's.
    git_owner: u64,
    /// Takes this panel's git settings back when it goes away (its window
    /// closed), so they do not outlive it.
    _release_git_credentials: Subscription,
    /// Whether what `git_credentials_dir` was given is a refusal (the
    /// account is signed out) rather than the account's token.
    git_credentials_refused: bool,
    /// Whether the workspace's account is signed out: GitHub no longer
    /// accepts its token, or there is none on this machine. Not set when
    /// GitHub could not be reached, which says nothing about the sign-in.
    signed_out: bool,
    /// Set from the moment an account starts loading (startup, a switch)
    /// until its token is registered for the repository, or the load ends
    /// without a session. The git panel holds pushes back meanwhile, so they
    /// do not go out with git's own login.
    account_loading: bool,
    /// Whether the workspace's repository has a github.com HTTPS remote, as
    /// of the last check. Kept so the git panel can ask without reading the
    /// workspace, which it may not do while a push is being started.
    workspace_remote_https: bool,
    /// Set when an action was bounced to the auth gate for a missing scope;
    /// re-sent by `start_session` once the scope is granted.
    pending_action: Option<PendingAction>,
    /// The Git panel's "Using Helm" local follow-up, parked when its Create
    /// dialog confirms and consumed by [`Self::attach_created_repo`] once
    /// `gh_create_repo` returns the new repository. Cleared on a terminal
    /// failure so it can't leak into a later, unrelated create.
    pending_attach: Option<CreateRepoAttach>,
    /// The scope the auth gate's "Authorize" button requests. `repo` for
    /// the startup check; an action's own scope when it sent us there.
    scope_to_authorize: &'static str,
    /// True for 2s after the device code is copied, flips the copy icon to
    /// a checkmark.
    code_copied: bool,

    // Session (populated once authenticated)
    account: String,
    scopes: Vec<String>,
    org_logins: Section<String>,
    org_logins_list: ListView,
    user: Option<GitHubUser>,
    /// Pending repo invitations the signed-in user hasn't accepted yet —
    /// just a count, surfaced as the hint badge on the Profile screen's
    /// Invitations row (loaded by [`Self::load_repo_invitations`]).
    repo_invitation_count: usize,

    /// Which org's details the identity header should show instead of the
    /// user's own profile. Set by [`Self::select_org`], cleared whenever
    /// [`Self::set_screen`] lands on a screen that isn't `OrgDetail`.
    selected_org: Option<String>,
    org_detail: Option<OrgDetail>,
    /// The signed-in user's role in `selected_org` ("admin" or "member"),
    /// loaded with `org_detail`. `None` when they are not a member.
    org_role: Option<String>,
    /// Populated by [`Self::load_org_members_page`] for the `OrgMembers`
    /// screen.
    org_members: Section<OrgMember>,
    org_members_list: ListView,
    /// The main menu and the Profile screen's menu.
    menu_list: ListView,
    profile_menu_list: ListView,

    // Repos
    repos: Section<Repo>,
    /// Positions in `repos` of the rows on the page being shown, out of the
    /// ones the search box leaves. Recomputed at the top of every render of
    /// the repository list, so it always matches what `repos`, the search
    /// box and `repos_page` hold.
    repos_shown: Vec<usize>,
    /// The page of matching repositories on screen, counting from 1.
    repos_page: u32,
    repos_last_page: u32,
    repos_list: ListView,
    /// The sections of the repository that is open.
    repo_sections_list: ListView,
    repo_search: Entity<InputState>,
    github_search_input: Entity<InputState>,
    github_search_results: Section<Repo>,
    github_search_query: String,
    github_search_list: ListView,
    /// Row `up`/`down`/`enter` act on, within the filtered repo list
    /// `render_repo_list` computes from `repos` + `repo_search` — an index
    /// into that filtered order, not into `repos` itself, since the two can
    /// disagree once a search narrows the list.

    /// The repo the user drilled into from `RepoList`. Cleared whenever
    /// `set_screen` lands anywhere but `RepoDetail`.
    selected_repo: Option<Repo>,
    /// True for 2s after the clone URL is copied, flips the copy icon to a
    /// checkmark — separate from `code_copied` (device-flow code) since both
    /// concepts, while never shown together, are otherwise unrelated.
    clone_url_copied: bool,
    cloning: bool,
    clone_lines: Vec<String>,
    /// git's progress line ("Receiving objects: 42%"), which it rewrites in
    /// place; shown under `clone_lines` and replaced by each update.
    clone_progress: Option<String>,
    /// Stops the running clone. `Some` while `cloning`.
    clone_canceller: Option<CloneCanceller>,
    clone_error: Option<String>,
    /// Set on a successful clone, to the path it landed at — drives the
    /// `HelmModalKind::CloneRepo` modal's "Would you like to open that
    /// workspace?" stage instead of opening it automatically. Reset whenever
    /// [`Self::open_clone_modal`] starts a fresh attempt.
    clone_succeeded_path: Option<String>,
    /// File to open in the new workspace when a clone was started from Code.
    clone_file_to_open: Option<String>,
    /// Parent directory the Clone action targets — `None` means "the current
    /// workspace root", the pre-picker default. Chosen via [`Self::pick_clone_dir`]
    /// and consumed by [`Self::handle_clone`].
    clone_target_dir: Option<String>,
    /// The `AppState` this panel needs for the workspace-root reads/writes
    /// around cloning — read to compute where a clone lands, written on a
    /// successful clone so the freshly-cloned repo becomes the open folder.
    workspace: WeakEntity<Workspace>,

    /// Populated by [`Self::load_branches`] for the `Branches` screen.
    branches: Section<Branch>,
    branches_list: ListView,
    /// Populated by [`Self::load_collaborators`] for the `Collaborators`
    /// screen.
    collaborators: Section<Collaborator>,
    collaborators_list: ListView,

    // Repo-detail tab caches (Issues/Pulls/Releases/Packages/Traffic) —
    // loaded on screen entry and kept while drilling; `set_screen` clears
    // them along with `selected_repo` when leaving the repo-drilled screens.
    issues: Section<Issue>,
    issues_list: ListView,
    issues_filter: String,
    /// Row `up`/`down`/`enter` act on, within `issues`.
    pulls: Section<Pull>,
    pulls_list: ListView,
    pulls_filter: String,
    /// The issue/PR drilled into from `Issues`/`Pulls` — mutually exclusive
    /// (only one of the two is ever `Some` at a time), cleared whenever
    /// `set_screen` lands anywhere but `IssueDetail`/`PrDetail`. Same
    /// pattern as `selected_repo`.
    selected_issue: Option<Issue>,
    selected_pr: Option<Pull>,
    /// The open item's comment thread, loaded by
    /// [`Self::load_detail_comments`] — shared between `IssueDetail` and
    /// `PrDetail` since GitHub serves both from the same endpoint (see
    /// `Comment`'s doc comment).
    detail_comments: Vec<Comment>,
    detail_comments_state: LoadState,
    /// The new-comment box under the thread, shared by both detail screens.
    comment_input: Entity<TextareaState>,
    /// A comment is on its way, so the button does not send it twice.
    comment_sending: bool,
    /// The box is emptied by the next render, which has the window.
    clear_comment_input: bool,
    /// The background check of the open issue or pull request, with the number
    /// it is for; see [`Self::sync_detail_poll`].
    detail_poll: Option<(u64, Task<()>)>,
    /// The open pull request's checks, loaded by [`Self::refresh_pr`].
    pr_checks: Vec<CheckRun>,
    pr_checks_state: LoadState,
    releases: Section<Release>,
    releases_list: ListView,
    packages: Section<Package>,
    packages_list: ListView,
    package_versions: Vec<PackageVersion>,
    package_versions_error: Option<String>,
    expanded_package: Option<String>,
    traffic: Option<RepoTraffic>,
    commits: Section<CommitSummary>,
    commits_list: ListView,
    workflow_runs: Section<WorkflowRun>,
    workflow_runs_list: ListView,
    deployments: Section<Deployment>,
    deployments_list: ListView,
    tags: Section<Tag>,
    tags_list: ListView,
    dependabot_alerts: Section<serde_json::Value>,
    secret_scanning_alerts: Section<serde_json::Value>,
    /// Both kinds of alert in one list, as two headed sections.
    security_list: ListView,

    // Pending repo invitations shown on the Profile screen's Invitations
    // screen — `repo_invitation_count` above is just the badge for that row.
    invitations: Section<RepoInvitation>,
    /// Organisation and repository invitations in one list, as two headed
    /// sections.
    invitations_list: ListView,
    /// Pending org invitations, shown on the same Invitations screen as
    /// `invitations`, as the first of its two sections.
    org_invitations: Vec<OrgInvitation>,

    /// Another user's public profile, viewed via [`Self::open_user_profile`]
    /// (e.g. clicking a collaborator) — cleared whenever `set_screen` lands
    /// anywhere but `UserProfile`. Distinct from `user`, which is always the
    /// signed-in account.
    viewed_user: Option<GitHubUserDetail>,
}

impl HelmPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |_, window, cx| {
            Self::view(workspace.clone(), window, cx)
        })
    }

    pub fn view(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let repo_search =
                cx.new(|cx| InputState::new(window, cx).placeholder("Search repositories…"));
            cx.subscribe(
                &repo_search,
                |this: &mut Self, _, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        // A new search starts from its first page.
                        this.repos_page = 1;
                        cx.notify();
                    }
                },
            )
            .detach();
            let github_search_input =
                cx.new(|cx| InputState::new(window, cx).placeholder("Search GitHub repositories…"));
            cx.subscribe(
                &github_search_input,
                |this: &mut Self, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.load_github_search(1, cx);
                    }
                },
            )
            .detach();

            let token_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("ghp_… or github_pat_…")
            });
            let accounts = HelmAccounts::global(cx);
            cx.subscribe(
                &accounts,
                |this: &mut Self, _, event: &HelmAccountEvent, cx| {
                    this.handle_account_event(event, cx)
                },
            )
            .detach();

            let mut this = Self {
                focus_handle: cx.focus_handle(),
                gh_state: Arc::new(GhState::new(cx.http_client())),
                screen: HelmScreen::Auth,
                back_stack: Vec::new(),
                forward_stack: Vec::new(),
                load_state: LoadState::Idle,
                error_msg: String::new(),
                error_link: None,
                auth_initialized: false,
                active_login: None,
                sign_in: SignIn::Idle,
                sign_in_for: SignInFor::AnyAccount,
                sign_in_task: None,
                token_input,
                token_input_open: false,
                adding_account: false,
                gh_cli: GhCliState::Unknown,
                gh_cli_picker_open: false,
                gh_cli_login: None,
                repo_access: None,
                repo_access_key: None,
                repo_access_offered: Default::default(),
                _git_store_subscription: None,
                commit_identity: CommitIdentity::Unknown,
                commit_identity_key: None,
                git_credentials_dir: None,
                git_owner: cx.entity_id().as_u64(),
                _release_git_credentials: cx
                    .on_release(|this: &mut HelmPanel, _| this.unregister_git_credentials()),
                git_credentials_refused: false,
                signed_out: false,
                account_loading: false,
                workspace_remote_https: false,
                pending_action: None,
                pending_attach: None,
                scope_to_authorize: "repo",
                code_copied: false,
                account: String::new(),
                scopes: Vec::new(),
                org_logins: Section::default(),
                org_logins_list: lists::org_logins_list(window, cx),
                user: None,
                repo_invitation_count: 0,
                selected_org: None,
                org_detail: None,
                org_role: None,
                org_members: Section::default(),
                org_members_list: lists::org_members_list(window, cx),
                menu_list: lists::menu_list(window, cx),
                profile_menu_list: lists::profile_menu_list(window, cx),
                repos: Section::default(),
                repos_shown: Vec::new(),
                repos_page: 1,
                repos_last_page: 1,
                repos_list: lists::repos_list(window, cx),
                repo_sections_list: lists::repo_sections_list(window, cx),
                repo_search,
                github_search_input,
                github_search_results: Section::default(),
                github_search_query: String::new(),
                github_search_list: lists::github_search_list(window, cx),
                selected_repo: None,
                clone_url_copied: false,
                cloning: false,
                clone_lines: Vec::new(),
                clone_progress: None,
                clone_canceller: None,
                clone_error: None,
                clone_succeeded_path: None,
                clone_file_to_open: None,
                clone_target_dir: None,
                workspace,
                branches: Section::default(),
                branches_list: lists::branches_list(window, cx),
                collaborators: Section::default(),
                collaborators_list: lists::collaborators_list(window, cx),
                issues: Section::default(),
                issues_list: lists::issues_list(window, cx),
                issues_filter: "open".into(),
                pulls: Section::default(),
                pulls_list: lists::pulls_list(window, cx),
                pulls_filter: "open".into(),
                selected_issue: None,
                selected_pr: None,
                detail_comments: Vec::new(),
                detail_comments_state: LoadState::Idle,
                comment_input: cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(2, 8)
                        .placeholder("Leave a comment")
                }),
                comment_sending: false,
                clear_comment_input: false,
                detail_poll: None,
                pr_checks: Vec::new(),
                pr_checks_state: LoadState::Idle,
                releases: Section::default(),
                releases_list: lists::releases_list(window, cx),
                packages: Section::default(),
                packages_list: lists::packages_list(window, cx),
                package_versions: Vec::new(),
                package_versions_error: None,
                expanded_package: None,
                traffic: None,
                commits: Section::default(),
                commits_list: lists::commits_list(window, cx),
                workflow_runs: Section::default(),
                workflow_runs_list: lists::workflow_runs_list(window, cx),
                deployments: Section::default(),
                deployments_list: lists::deployments_list(window, cx),
                tags: Section::default(),
                tags_list: lists::tags_list(window, cx),
                dependabot_alerts: Section::default(),
                secret_scanning_alerts: Section::default(),
                security_list: lists::security_list(window, cx),
                invitations: Section::default(),
                invitations_list: lists::invitations_list(window, cx),
                org_invitations: Vec::new(),
                viewed_user: None,
            };
            this.restore_account_and_start(cx);
            this
        })
    }

    fn open_workspace_modal(
        &mut self,
        kind: HelmModalKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            // No workspace to open a modal *in*, so there's nowhere to show
            // a toast either — this is a "should never happen" guard, not a
            // user-facing error state.
            log::warn!("helm_panel: open_workspace_modal called with no attached workspace");
            return;
        };
        let parent = cx.entity();
        // Read while this panel's lease is still held: the modal's build
        // closure runs synchronously inside `toggle_modal`, where `parent`
        // cannot be read (it would double-lease the panel and panic).
        let org_logins = self.org_logins.items.clone();
        let login = self.signed_in_login().map(str::to_string);
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                HelmRepositoryModal::new(kind, parent, login, org_logins, window, cx)
            });
        });
    }

    /// Opens the "Create repository" dialog in the mode the Git panel's
    /// "Using Helm" button uses: on success the new repository is wired to
    /// this workspace by [`Self::attach_created_repo`] instead of only
    /// navigating to its detail screen. Without a GitHub login it reports a
    /// toast and sends the user to the Helm panel, where signing in happens.
    pub fn open_create_repo_for_attach(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.account.is_empty() {
            self.notify(
                "Sign in to GitHub from the Helm panel to create a repository.",
                cx,
            );
            if let Some(workspace) = self.workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    workspace.focus_panel::<HelmPanel>(window, cx);
                });
            }
            return;
        }
        self.open_workspace_modal(HelmModalKind::CreateRepo { attach_local: true }, window, cx);
    }

    /// Shows `message` as a toast on the workspace this panel is attached
    /// to. `gpui_component`'s own `window.push_notification` (this crate's
    /// earlier approach) requires the window to be wrapped in a
    /// `gpui_component::Root`, which the main Zed workspace window never is
    /// — calling it here panics with "window first layer should be a
    /// gpui_component::Root". `workspace::Toast` is the mechanism Zed's own
    /// windows actually support.
    /// [`Self::notify`] for a request GitHub refused: when the refusal can
    /// be put right on GitHub (single sign-on, an organisation's approval of
    /// Helm), the toast's button opens that page.
    fn notify_error(
        &self,
        message: impl Into<std::borrow::Cow<'static, str>>,
        error: &GhError,
        cx: &mut App,
    ) {
        let message = message.into();
        let link = error.link();
        self.workspace
            .update(cx, |workspace, cx| {
                let mut toast = Toast::new(NotificationId::unique::<HelmPanel>(), message);
                if let Some(link) = link {
                    toast = toast.on_click(link.label, move |_, cx| cx.open_url(&link.url));
                }
                workspace.show_toast(toast, cx);
            })
            .ok();
    }

    fn notify(&self, message: impl Into<std::borrow::Cow<'static, str>>, cx: &mut App) {
        let message = message.into();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<HelmPanel>(), message),
                    cx,
                );
            })
            .ok();
    }
}

impl Focusable for HelmPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for HelmPanel {}

impl Panel for HelmPanel {
    fn persistent_name() -> &'static str {
        "Helm Panel"
    }

    fn panel_key() -> &'static str {
        "HelmPanel"
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        match HelmPanelSettings::get_global(cx).dock {
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
            settings.helm_panel.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> gpui::Pixels {
        px(320.)
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<ui::IconName> {
        HelmPanelSettings::get_global(cx)
            .button
            .then_some(ui::IconName::Github)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Helm")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        21
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.helm_panel.get_or_insert_default().button = Some(false);
        }))
    }
}

impl Render for HelmPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.clear_comment_input) {
            self.comment_input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        let show_nav = !matches!(self.screen, HelmScreen::Auth);
        if self.screen == HelmScreen::RepoList {
            let query = self.repo_search.read(cx).value().to_string();
            let matching = repos::matching_repos(&self.repos.items, &query);
            let (page, last_page, rows) = page_slice(matching.len(), self.repos_page, PAGE_SIZE);
            self.repos_page = page;
            self.repos_last_page = last_page;
            self.repos_shown = matching[rows].to_vec();
        }

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(IconName::Helm).text_color(cx.theme().foreground))
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_sm()
                                    .text_color(cx.theme().foreground)
                                    .child("Helm"),
                            )
                            .children(self.render_account_menu(cx)),
                    )
                    .when(show_nav, |row| row.child(self.render_nav_bar(cx))),
            )
            .child(Separator::horizontal())
            .children(self.render_identity_header(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(match self.screen {
                        HelmScreen::Auth => self.render_auth(cx).into_any_element(),
                        HelmScreen::Menu => self.render_menu(cx).into_any_element(),
                        HelmScreen::Profile => self.render_profile(cx).into_any_element(),
                        HelmScreen::OrgList => self.render_org_list(cx).into_any_element(),
                        HelmScreen::OrgDetail => self.render_org_detail(cx).into_any_element(),
                        HelmScreen::OrgMembers => self.render_org_members(cx).into_any_element(),
                        HelmScreen::RepoList => self.render_repo_list(cx).into_any_element(),
                        HelmScreen::GitHubSearch => {
                            self.render_github_search(cx).into_any_element()
                        }
                        HelmScreen::RepoDetail => self.render_repo_detail(cx).into_any_element(),
                        HelmScreen::Branches => self.render_branches(cx).into_any_element(),
                        HelmScreen::Collaborators => {
                            self.render_collaborators(cx).into_any_element()
                        }
                        HelmScreen::Issues => self.render_issues(cx).into_any_element(),
                        HelmScreen::IssueDetail => self.render_issue_detail(cx).into_any_element(),
                        HelmScreen::Pulls => self.render_pulls(cx).into_any_element(),
                        HelmScreen::PrDetail => self.render_pr_detail(cx).into_any_element(),
                        HelmScreen::Releases => self.render_releases(cx).into_any_element(),
                        HelmScreen::Packages => self.render_packages(cx).into_any_element(),
                        HelmScreen::Traffic => self.render_traffic(cx).into_any_element(),
                        HelmScreen::Invitations => self.render_invitations(cx).into_any_element(),
                        HelmScreen::UserProfile => self.render_user_profile(cx).into_any_element(),
                        HelmScreen::Commits => self.render_commits(cx).into_any_element(),
                        HelmScreen::WorkflowRuns => {
                            self.render_workflow_runs(cx).into_any_element()
                        }
                        HelmScreen::Deployments => self.render_deployments(cx).into_any_element(),
                        HelmScreen::Tags => self.render_tags(cx).into_any_element(),
                        HelmScreen::Security => self.render_security(cx).into_any_element(),
                    }),
            )
            .children(show_nav.then(|| self.render_rate_limit(cx)).flatten())
    }
}

impl HelmPanel {
    /// The rate-limit bar at the foot of the panel (`rate_limit_bar`).
    fn render_rate_limit(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let bar = rate_limit_bar("helm-rate-limit", self.gh_state.rate_limit(), cx)?;
        Some(
            div()
                .flex_none()
                .border_t_1()
                .border_color(cx.theme().border)
                .child(bar),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn token_scope_matching() {
        let granted = scopes(&["gist", "read:org", "repo"]);
        assert!(token_has_scope(&granted, "repo"));
        assert!(!token_has_scope(&granted, "user"));
        // `read:org` is not enough to change an org membership.
        assert!(!token_has_scope(&granted, "write:org"));
        assert!(token_has_scope(&scopes(&["write:org"]), "write:org"));
        assert!(token_has_scope(&scopes(&["admin:org"]), "write:org"));
        // A narrower repo scope is not the full one.
        assert!(!token_has_scope(&scopes(&["public_repo"]), "repo"));
        assert!(!token_has_scope(&[], "repo"));
    }

    #[test]
    fn actions_ask_for_the_scope_they_need() {
        let json = serde_json::Value::Null;
        assert_eq!(
            HelmAction::CreateRepo { opts: json.clone() }.required_scope(),
            "repo"
        );
        assert_eq!(
            HelmAction::EditRepo {
                changes: json.clone(),
                topics: Vec::new()
            }
            .required_scope(),
            "repo"
        );
        assert_eq!(
            HelmAction::RemoveCollaborator("x".into()).required_scope(),
            "repo"
        );
        assert_eq!(HelmAction::AcceptRepoInvitation(1).required_scope(), "repo");
        assert_eq!(
            HelmAction::UpdateProfile { changes: json }.required_scope(),
            "user"
        );
        assert_eq!(
            HelmAction::AcceptOrgInvitation("o".into()).required_scope(),
            "write:org"
        );
        assert_eq!(
            HelmAction::DeclineOrgInvitation("o".into()).required_scope(),
            "write:org"
        );
    }

    #[test]
    fn only_repository_actions_need_a_selected_repo() {
        let json = serde_json::Value::Null;
        assert!(
            HelmAction::EditRepo {
                changes: json.clone(),
                topics: Vec::new()
            }
            .needs_repo()
        );
        assert!(HelmAction::RemoveCollaborator("x".into()).needs_repo());
        assert!(!HelmAction::CreateRepo { opts: json.clone() }.needs_repo());
        assert!(!HelmAction::UpdateProfile { changes: json }.needs_repo());
        assert!(!HelmAction::AcceptRepoInvitation(1).needs_repo());
    }

    #[test]
    fn missing_scope_message_names_the_scope() {
        assert_eq!(missing_scope_message("repo"), "Missing 'repo' scope.");
        assert_ne!(missing_scope_message("repo"), missing_scope_message("user"));
    }
}

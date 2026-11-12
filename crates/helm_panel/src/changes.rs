//! The changes Helm sends to GitHub. Every one is a `HelmAction` run through
//! `HelmPanel::run_action`, so they share one failure path: a change rejected
//! for a missing token scope sends the user to authorize it and is sent again.

use super::*;

/// A change Helm sends to GitHub on the user's behalf. Every mutating
/// handler builds one of these and hands it to [`HelmPanel::run_action`], so
/// they all share one failure path — including being sent through the auth
/// gate and re-sent when the token turns out to lack a scope.
#[derive(Clone)]
pub(super) enum HelmAction {
    CreateRepo {
        opts: serde_json::Value,
    },
    EditRepo {
        changes: serde_json::Value,
        topics: Vec<String>,
    },
    CreatePull {
        title: String,
        body: String,
        head: String,
        base: String,
    },
    /// A comment on an issue or a pull request's conversation.
    CommentOn {
        number: u64,
        body: String,
    },
    /// Closes, reopens or edits a pull request; `changes` holds any of
    /// `title`, `body` and `state`.
    UpdatePull {
        number: u64,
        changes: serde_json::Value,
    },
    /// Merges a pull request. `sha` is the head the user saw, so GitHub
    /// refuses the merge if the branch moved in the meantime.
    MergePull {
        number: u64,
        method: String,
        commit_title: String,
        sha: String,
        /// The head branch to delete once merged, when asked for.
        delete_branch: Option<String>,
    },
    CreateRelease {
        tag_name: String,
        title: String,
        body: String,
        draft: bool,
        prerelease: bool,
    },
    UpdateProfile {
        changes: serde_json::Value,
    },
    AcceptRepoInvitation(u64),
    DeclineRepoInvitation(u64),
    AcceptOrgInvitation(String),
    DeclineOrgInvitation(String),
    SetCollaboratorPermission {
        login: String,
        permission: String,
    },
    RemoveCollaborator(String),
}

impl HelmAction {
    /// The OAuth scope GitHub wants for this action — what the auth gate
    /// asks for when the action is rejected and the token doesn't have it.
    pub(super) fn required_scope(&self) -> &'static str {
        match self {
            HelmAction::UpdateProfile { .. } => "user",
            HelmAction::AcceptOrgInvitation(_) | HelmAction::DeclineOrgInvitation(_) => "write:org",
            _ => "repo",
        }
    }

    /// Completes "Failed to …" in the failure notification.
    pub(super) fn failure_label(&self) -> &'static str {
        match self {
            HelmAction::CreateRepo { .. } => "create repository",
            HelmAction::EditRepo { .. } => "update repository",
            HelmAction::CreatePull { .. } => "create pull request",
            HelmAction::CommentOn { .. } => "post the comment",
            HelmAction::UpdatePull { .. } => "update pull request",
            HelmAction::MergePull { .. } => "merge pull request",
            HelmAction::CreateRelease { .. } => "create release",
            HelmAction::UpdateProfile { .. } => "update profile",
            HelmAction::AcceptRepoInvitation(_) | HelmAction::AcceptOrgInvitation(_) => {
                "accept invitation"
            }
            HelmAction::DeclineRepoInvitation(_) | HelmAction::DeclineOrgInvitation(_) => {
                "decline invitation"
            }
            HelmAction::SetCollaboratorPermission { .. } => "update collaborator",
            HelmAction::RemoveCollaborator(_) => "remove collaborator",
        }
    }

    /// Whether this acts on `HelmPanel::selected_repo`.
    pub(super) fn needs_repo(&self) -> bool {
        matches!(
            self,
            HelmAction::EditRepo { .. }
                | HelmAction::CreatePull { .. }
                | HelmAction::CommentOn { .. }
                | HelmAction::UpdatePull { .. }
                | HelmAction::MergePull { .. }
                | HelmAction::CreateRelease { .. }
                | HelmAction::SetCollaboratorPermission { .. }
                | HelmAction::RemoveCollaborator(_)
        )
    }

    /// Makes the API call(s). Returns the resulting repository for the two
    /// actions that produce one (create and edit), `None` otherwise.
    pub(super) async fn perform(
        self,
        repo: Option<Repo>,
        gh_state: &GhState,
    ) -> Result<Option<Repo>, GhError> {
        let selected = || {
            repo.clone()
                .ok_or_else(|| GhError::Other("No repository selected".to_string()))
        };
        match self {
            HelmAction::CreateRepo { opts } => gh_create_repo(opts, gh_state).await.map(Some),
            HelmAction::EditRepo { changes, topics } => {
                let repo = selected()?;
                let owner = repo.owner.login;
                let updated = gh_update_repo(owner.clone(), repo.name, changes, gh_state).await?;
                // The topics endpoint 404s on a pre-rename name, so it has to
                // use the name from the PATCH response.
                gh_update_topics(owner, updated.name.clone(), topics, gh_state).await?;
                Ok(Some(updated))
            }
            HelmAction::CreatePull {
                title,
                body,
                head,
                base,
            } => {
                let repo = selected()?;
                gh_create_pull(
                    repo.owner.login,
                    repo.name,
                    head,
                    base,
                    title,
                    (!body.is_empty()).then_some(body),
                    gh_state,
                )
                .await
                .map(|_| None)
            }
            HelmAction::CommentOn { number, body } => {
                let repo = selected()?;
                gh_create_issue_comment(repo.owner.login, repo.name, number, body, gh_state)
                    .await
                    .map(|_| None)
            }
            HelmAction::UpdatePull { number, changes } => {
                let repo = selected()?;
                gh_update_pull(repo.owner.login, repo.name, number, changes, gh_state)
                    .await
                    .map(|_| None)
            }
            HelmAction::MergePull {
                number,
                method,
                commit_title,
                sha,
                delete_branch,
            } => {
                let repo = selected()?;
                let merged = gh_merge_pull(
                    repo.owner.login.clone(),
                    repo.name.clone(),
                    number,
                    method,
                    (!commit_title.is_empty()).then_some(commit_title),
                    (!sha.is_empty()).then_some(sha),
                    gh_state,
                )
                .await?;
                if !merged.merged {
                    return Err(GhError::Other(merged.message));
                }
                // The merge is done, so a branch that cannot be deleted (a
                // protected one, or one GitHub already removed because the
                // repository deletes merged branches) is not a failure.
                if let Some(branch) = delete_branch {
                    gh_delete_branch(repo.owner.login, repo.name, branch, gh_state)
                        .await
                        .ok();
                }
                Ok(None)
            }
            HelmAction::CreateRelease {
                tag_name,
                title,
                body,
                draft,
                prerelease,
            } => {
                let repo = selected()?;
                gh_create_release(
                    repo.owner.login,
                    repo.name,
                    tag_name,
                    (!title.is_empty()).then_some(title),
                    (!body.is_empty()).then_some(body),
                    Some(draft),
                    Some(prerelease),
                    gh_state,
                )
                .await
                .map(|_| None)
            }
            HelmAction::UpdateProfile { changes } => {
                gh_update_user(changes, gh_state).await.map(|_| None)
            }
            HelmAction::AcceptRepoInvitation(id) => {
                gh_accept_repo_invitation(id, gh_state).await.map(|_| None)
            }
            HelmAction::DeclineRepoInvitation(id) => {
                gh_decline_repo_invitation(id, gh_state).await.map(|_| None)
            }
            HelmAction::AcceptOrgInvitation(org) => {
                gh_accept_org_invitation(org, gh_state).await.map(|_| None)
            }
            HelmAction::DeclineOrgInvitation(org) => {
                gh_decline_org_invitation(org, gh_state).await.map(|_| None)
            }
            HelmAction::SetCollaboratorPermission { login, permission } => {
                let repo = selected()?;
                gh_add_collaborator(repo.owner.login, repo.name, login, permission, gh_state)
                    .await
                    .map(|_| None)
            }
            HelmAction::RemoveCollaborator(login) => {
                let repo = selected()?;
                gh_remove_collaborator(repo.owner.login, repo.name, login, gh_state)
                    .await
                    .map(|_| None)
            }
        }
    }
}

/// An action GitHub rejected because the token lacked a scope, kept so it
/// can be re-sent once the auth gate has granted it.
pub(super) struct PendingAction {
    pub(super) action: HelmAction,
    /// Where the user was when the action failed, restored after re-auth
    /// (which otherwise always lands on the menu).
    pub(super) resume_screen: HelmScreen,
}

/// What `gh auth status` says about the token after a permission-shaped API
/// failure — decides whether re-authorizing could fix it at all.
pub(super) enum TokenScopeCheck {
    /// The token has the scope; the failure is about the account's rights
    /// (or org policy), which re-auth can't change.
    HasScope,
    MissingScope,
    NotLoggedIn,
}

/// The `error_msg` that puts the auth screen into its "authorize this scope"
/// state. One function so the code that sets it and `render_auth`'s check
/// for it can't drift apart.
pub(super) fn missing_scope_message(scope: &str) -> String {
    format!("Missing '{scope}' scope.")
}

/// Whether a token carrying `granted` satisfies `needed`, counting the one
/// parent scope that implies it (`admin:org` includes `write:org`).
pub(super) fn token_has_scope(granted: &[String], needed: &str) -> bool {
    granted
        .iter()
        .any(|scope| scope == needed || (needed == "write:org" && scope == "admin:org"))
}

impl HelmPanel {
    /// Sends `action` to GitHub and applies its result to the panel.
    ///
    /// When GitHub rejects it with a permission-shaped error and
    /// `may_reauthorize` is set, the token's scopes (as GitHub reports them)
    /// tell a token that lacks the action's scope apart from an account that
    /// simply lacks the rights. Only the former is sent to the auth gate
    /// (with the action parked in `pending_action` for `start_session` to
    /// re-send) —
    /// re-authorizing can't fix the latter, so that just reports the
    /// failure.
    pub(super) fn run_action(
        &mut self,
        action: HelmAction,
        may_reauthorize: bool,
        cx: &mut Context<Self>,
    ) {
        let repo = self.selected_repo.clone();
        if action.needs_repo() && repo.is_none() {
            return;
        }
        let scope = action.required_scope();
        let performed = action.clone();

        let gh_state = self.gh_state.clone();
        let started = gh_state.clone();
        cx.spawn(async move |this, cx| {
            let (result, scope_check) = on_tokio(async move {
                let result = performed.perform(repo, &gh_state).await;
                let scope_check = match &result {
                    Err(GhError::Unauthorized { .. }) if may_reauthorize => {
                        TokenScopeCheck::NotLoggedIn
                    }
                    Err(error) if may_reauthorize && error.is_permission() => {
                        match gh_state.token_scopes() {
                            Some(scopes) if !token_has_scope(&scopes, scope) => {
                                TokenScopeCheck::MissingScope
                            }
                            // The scope is there, or the token does not
                            // report scopes: report the original failure
                            // rather than guessing.
                            _ => TokenScopeCheck::HasScope,
                        }
                    }
                    _ => TokenScopeCheck::HasScope,
                };
                (result, scope_check)
            })
            .await;
            this.update(cx, |this, cx| match (result, scope_check) {
                // The panel switched account while this was on its way: the
                // change was sent as the previous account, so say how it went
                // but leave the new account's screens alone.
                (result, _) if !this.is_current_state(&started) => {
                    this.pending_attach = None;
                    match result {
                        Ok(_) => this.notify(
                            "A change finished after the account switch; it was made with the previous account.",
                            cx,
                        ),
                        Err(error) => this.notify_error(
                            format!(
                                "Failed to {} with the previous account: {error}",
                                action.failure_label()
                            ),
                            &error,
                            cx,
                        ),
                    }
                }
                (Ok(repo), _) => {
                    this.action_succeeded(&action, repo, cx);
                    this.action_settled(&action, cx);
                }
                (Err(error), TokenScopeCheck::HasScope) => {
                    // Terminal failure — the parked local follow-up (if any)
                    // will never run, so don't let it attach a later,
                    // unrelated create.
                    this.pending_attach = None;
                    // Maybe the workspace's repository wants another account.
                    if error.is_permission() {
                        this.check_repo_access(true, cx);
                    }
                    this.notify_error(
                        format!("Failed to {}: {error}", action.failure_label()),
                        &error,
                        cx,
                    );
                    this.action_settled(&action, cx);
                }
                (Err(_), TokenScopeCheck::MissingScope) => {
                    this.notify(
                        format!(
                            "GitHub rejected the request: your login is missing the '{scope}' \
                             scope. Authorize it and Helm will try again."
                        ),
                        cx,
                    );
                    this.pending_action = Some(PendingAction {
                        action,
                        resume_screen: this.screen,
                    });
                    // The same state `start_session` leaves behind for a
                    // token without `repo`, so `render_auth` shows its
                    // "Authorize" prompt — for this action's scope.
                    this.screen = HelmScreen::Auth;
                    this.cancel_sign_in(cx);
                    this.load_state = LoadState::Idle;
                    this.scope_to_authorize = scope;
                    this.error_msg = missing_scope_message(scope);
                    cx.notify();
                }
                (Err(_), TokenScopeCheck::NotLoggedIn) => {
                    this.notify(
                        "GitHub rejected the request: you are no longer signed in. \
                         Sign in and Helm will try again.",
                        cx,
                    );
                    this.pending_action = Some(PendingAction {
                        action,
                        resume_screen: this.screen,
                    });
                    this.cancel_sign_in(cx);
                    // Re-checks the stored sign-in, which lands on the
                    // sign-in prompt.
                    this.start_session(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Applies a successful action to panel state. `repo` is the repository
    /// GitHub returned, for the actions that return one.
    pub(super) fn action_succeeded(
        &mut self,
        action: &HelmAction,
        repo: Option<Repo>,
        cx: &mut Context<Self>,
    ) {
        match action {
            HelmAction::CreateRepo { .. } => {
                if let Some(repo) = repo {
                    self.repos.items.push(repo.clone());
                    // A create parked the Git panel's local follow-up
                    // (`handle_create_repo`): wire the workspace up instead
                    // of — not as well as — navigating to the detail screen.
                    match self.pending_attach.take() {
                        Some(attach) => self.attach_created_repo(repo, attach, cx),
                        None => self.select_repo(repo, cx),
                    }
                }
            }
            HelmAction::EditRepo { .. } => {
                if let Some(updated) = repo {
                    if let Some(existing) = self.repos.items.iter_mut().find(|r| r.id == updated.id)
                    {
                        *existing = updated.clone();
                    }
                    self.selected_repo = Some(updated);
                }
            }
            HelmAction::UpdateProfile { .. } => {
                self.notify("Profile updated", cx);
                let gh_state = self.gh_state.clone();
                let started = gh_state.clone();
                cx.spawn(async move |this, cx| {
                    let updated =
                        on_tokio(async move { gh_get_current_user(&gh_state).await }).await;
                    this.update(cx, |this, cx| {
                        if !this.is_current_state(&started) {
                            return;
                        }
                        if let Ok(user) = updated {
                            this.user = Some(user);
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
            HelmAction::AcceptRepoInvitation(id) => {
                self.invitations.items.retain(|inv| inv.id != *id);
                self.notify("Invitation accepted", cx);
            }
            HelmAction::DeclineRepoInvitation(id) => {
                self.invitations.items.retain(|inv| inv.id != *id);
                self.notify("Invitation declined", cx);
            }
            HelmAction::AcceptOrgInvitation(org) => {
                self.org_invitations
                    .retain(|inv| &inv.organization.login != org);
                self.notify("Invitation accepted", cx);
            }
            HelmAction::DeclineOrgInvitation(org) => {
                self.org_invitations
                    .retain(|inv| &inv.organization.login != org);
                self.notify("Invitation declined", cx);
            }
            HelmAction::CommentOn { .. } => {
                // The box is cleared by the next render, which has the window.
                self.clear_comment_input = true;
            }
            HelmAction::UpdatePull { changes, .. } => {
                let closed = changes.get("state").and_then(|state| state.as_str());
                self.notify(
                    match closed {
                        Some("closed") => "Pull request closed",
                        Some("open") => "Pull request reopened",
                        _ => "Pull request updated",
                    },
                    cx,
                );
            }
            HelmAction::MergePull { number, .. } => {
                self.notify(format!("Pull request #{number} merged"), cx);
            }
            HelmAction::CreatePull { .. }
            | HelmAction::CreateRelease { .. }
            | HelmAction::SetCollaboratorPermission { .. }
            | HelmAction::RemoveCollaborator(_) => {}
        }
        self.repo_invitation_count = self.invitations.items.len() + self.org_invitations.len();
        cx.notify();
    }

    /// Runs once an action has finished either way (but not when it was
    /// parked for re-auth): reloads the list the action edits, so the UI
    /// reflects what GitHub actually has rather than what was attempted.
    pub(super) fn action_settled(&mut self, action: &HelmAction, cx: &mut Context<Self>) {
        match action {
            HelmAction::CreatePull { .. } => self.load_pulls(cx),
            HelmAction::CommentOn { number, .. } => {
                self.comment_sending = false;
                self.load_detail_comments(*number, cx);
            }
            // Reload the open screen from GitHub, so it shows what GitHub
            // has (including a merge it refused) and not what was attempted.
            HelmAction::UpdatePull { number, .. } | HelmAction::MergePull { number, .. } => {
                self.refresh_pr(*number, false, cx);
                self.load_pulls(cx);
            }
            HelmAction::CreateRelease { .. } => self.load_releases(cx),
            HelmAction::SetCollaboratorPermission { .. } | HelmAction::RemoveCollaborator(_) => {
                self.load_collaborators(cx)
            }
            _ => {}
        }
    }
}

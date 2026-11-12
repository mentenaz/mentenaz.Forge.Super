//! Helm's pull request screens: the list, the detail screen, and creating,
//! editing, closing and merging a pull request. The state filter and the
//! comment thread are in `issues.rs`.

use super::*;

/// What the Merge button may do, from what GitHub says about the pull
/// request (`mergeable_state`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MergeGate {
    /// It can be merged. The text, when there is one, is a caution.
    Ready(Option<&'static str>),
    /// It cannot, for this reason.
    Blocked(&'static str),
    /// GitHub has not worked it out yet.
    Checking,
}

/// Whether an open pull request can be merged. GitHub's own verdict is
/// used as it is: Helm does not try to work out branch protection itself.
pub(super) fn merge_gate(pr: &Pull) -> MergeGate {
    if pr.draft {
        return MergeGate::Blocked("This is a draft. Mark it ready for review on GitHub first.");
    }
    match pr.mergeable_state.as_deref() {
        Some("clean" | "has_hooks") => MergeGate::Ready(None),
        Some("unstable") => MergeGate::Ready(Some(
            "Some checks are not passing, though none is required.",
        )),
        Some("dirty") => MergeGate::Blocked("It has merge conflicts. Resolve them first."),
        Some("behind") => {
            MergeGate::Blocked("The branch is behind its base. Bring it up to date first.")
        }
        Some("blocked") => MergeGate::Blocked(
            "Blocked by the branch's rules: a required review or check is missing.",
        ),
        Some("draft") => MergeGate::Blocked("This is a draft. Mark it ready for review on GitHub first."),
        _ if pr.mergeable == Some(false) => MergeGate::Blocked("GitHub says it cannot be merged."),
        _ => MergeGate::Checking,
    }
}

/// The ways of merging, as GitHub names them and as the dialog words them.
const MERGE_METHODS: [(&str, &str); 3] = [
    ("merge", "Create a merge commit"),
    ("squash", "Squash and merge"),
    ("rebase", "Rebase and merge"),
];

/// The merge methods `repo` allows. A repository that did not say is taken
/// to allow all of them, and GitHub refuses the one it does not.
pub(super) fn merge_methods(repo: &Repo) -> Vec<(&'static str, &'static str)> {
    let allowed = [
        repo.allow_merge_commit,
        repo.allow_squash_merge,
        repo.allow_rebase_merge,
    ];
    let methods: Vec<_> = MERGE_METHODS
        .iter()
        .zip(allowed)
        .filter(|(_, allowed)| allowed.unwrap_or(true))
        .map(|(method, _)| *method)
        .collect();
    if methods.is_empty() {
        MERGE_METHODS.to_vec()
    } else {
        methods
    }
}

/// The head branch to offer to delete after a merge: only one in the
/// repository itself (not a fork's), and never its default branch.
pub(super) fn deletable_head(pr: &Pull, repo: &Repo) -> Option<String> {
    let owner = |label: &str| label.split(':').next().map(str::to_string);
    let same_repository = !pr.head.label.is_empty()
        && !pr.base.label.is_empty()
        && owner(&pr.head.label) == owner(&pr.base.label);
    (same_repository && !pr.head.r#ref.is_empty() && pr.head.r#ref != repo.default_branch)
        .then(|| pr.head.r#ref.clone())
}

/// What the merge dialog needs, captured when it opens so it still describes
/// the pull request the user was looking at.
#[derive(Clone)]
pub(super) struct MergePlan {
    pub(super) number: u64,
    pub(super) title: String,
    pub(super) base: String,
    /// The head the user saw; GitHub refuses the merge if it has moved.
    pub(super) head_sha: String,
    /// The head branch, when offering to delete it is sensible.
    pub(super) deletable_branch: Option<String>,
    pub(super) methods: Vec<(&'static str, &'static str)>,
    pub(super) caution: Option<&'static str>,
}

/// A status badge in the tinted form, which stays readable in every theme.
fn tinted(label: impl Into<SharedString>, color: gpui::Hsla) -> Pill {
    Pill::custom(color.opacity(0.2), color, color.opacity(0.3))
        .xsmall()
        .rounded_full()
        .child(label.into())
}

impl HelmPanel {
    /// Loads `self.selected_repo`'s pull requests for the current
    /// `pulls_filter`.
    pub(super) fn load_pulls(&mut self, cx: &mut Context<Self>) {
        self.load_pulls_page(1, cx);
    }

    pub(super) fn load_pulls_page(&mut self, page: u32, cx: &mut Context<Self>) {
        let filter = self.pulls_filter.clone();
        self.load_repo_page(
            cx,
            |this| &mut this.pulls,
            page,
            move |repo| requests::pulls(&repo.owner.login, &repo.name, &filter),
        );
    }

    /// Same as [`Self::open_issue_detail`], for a PR row.
    pub(super) fn open_pr_detail(&mut self, pr: Pull, cx: &mut Context<Self>) {
        let number = pr.number;
        self.selected_pr = Some(pr);
        self.navigate_to(HelmScreen::PrDetail, cx);
        self.reset_comment_box();
        self.pr_checks.clear();
        self.load_detail_comments(number, cx);
        self.refresh_pr(number, false, cx);
    }

    /// Fetches pull request `number` on its own, which the list does not
    /// do: it carries whether the pull request merged and whether it can be
    /// merged, and the head commit whose checks are then loaded. GitHub works
    /// mergeability out when first asked, so while it has not, this asks
    /// again every two seconds, a few times. `quiet` is for the background
    /// check: it shows no loading state, and a failure leaves the screen as it
    /// is.
    pub(super) fn refresh_pr(&mut self, number: u64, quiet: bool, cx: &mut Context<Self>) {
        let Some(repo) = self.selected_repo.clone() else {
            return;
        };
        if !quiet {
            self.pr_checks_state = LoadState::Loading;
            cx.notify();
        }
        let gh_state = self.gh_state.clone();
        let started = gh_state.clone();
        cx.spawn(async move |this, cx| {
            let mut attempt = 0;
            loop {
                let (owner, name, state) = (repo.owner.login.clone(), repo.name.clone(), gh_state.clone());
                let pull = on_tokio(async move { gh_get_pull(owner, name, number, &state).await }).await;
                let pull = match pull {
                    Ok(pull) => pull,
                    Err(error) => {
                        this.update(cx, |this, cx| {
                            if !quiet && this.is_current_state(&started) {
                                this.pr_checks_state = LoadState::Error;
                                this.error_msg = error.to_string();
                                this.error_link = error.link();
                                cx.notify();
                            }
                        })
                        .ok();
                        return;
                    }
                };
                let unsettled = pull.state == "open" && pull.mergeable.is_none();

                // The checks are those of the head commit, so they are only
                // fetched on the first pass, and again if the head moved.
                let head = pull.head.sha.clone();
                let checks = if attempt == 0 && !head.is_empty() {
                    let (owner, name, state) =
                        (repo.owner.login.clone(), repo.name.clone(), gh_state.clone());
                    Some(on_tokio(async move { gh_list_check_runs(owner, name, head, &state).await }).await)
                } else {
                    None
                };

                let stop = this
                    .update(cx, |this, cx| {
                        if !this.is_current_state(&started)
                            || this.selected_pr.as_ref().map(|pr| pr.number) != Some(number)
                        {
                            return true;
                        }
                        this.selected_pr = Some(pull);
                        match checks {
                            Some(Ok(runs)) => {
                                this.pr_checks = runs;
                                this.pr_checks_state = LoadState::Idle;
                            }
                            // A background check that failed keeps what is shown.
                            Some(Err(_)) if quiet => {}
                            Some(Err(_)) => this.pr_checks_state = LoadState::Error,
                            None => this.pr_checks_state = LoadState::Idle,
                        }
                        cx.notify();
                        false
                    })
                    .unwrap_or(true);
                if stop || !unsettled || attempt >= 4 {
                    return;
                }
                attempt += 1;
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
        .detach();
    }

    /// Opens the "Create pull request" modal, prefilling the base branch
    /// with `self.selected_repo`'s default branch.
    pub(super) fn open_create_pull_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let default_base = self
            .selected_repo
            .as_ref()
            .map(|r| r.default_branch.clone())
            .unwrap_or_default();
        self.open_workspace_modal(HelmModalKind::CreatePull(default_base), window, cx);
    }

    pub(super) fn handle_create_pull(
        &mut self,
        title: String,
        body: String,
        head: String,
        base: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_action(
            HelmAction::CreatePull {
                title,
                body,
                head,
                base,
            },
            true,
            cx,
        );
    }

    /// Opens the dialog that edits the open pull request's title and body.
    pub(super) fn open_edit_pull_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pr) = self.selected_pr.clone() else {
            return;
        };
        self.open_workspace_modal(
            HelmModalKind::EditPull {
                number: pr.number,
                title: pr.title,
                body: pr.body.unwrap_or_default(),
            },
            window,
            cx,
        );
    }

    pub(super) fn handle_edit_pull(
        &mut self,
        number: u64,
        title: String,
        body: String,
        cx: &mut Context<Self>,
    ) {
        self.run_action(
            HelmAction::UpdatePull {
                number,
                changes: json!({ "title": title, "body": body }),
            },
            true,
            cx,
        );
    }

    /// Closes the open pull request, or reopens a closed one.
    pub(super) fn set_pull_open(&mut self, number: u64, open: bool, cx: &mut Context<Self>) {
        self.run_action(
            HelmAction::UpdatePull {
                number,
                changes: json!({ "state": if open { "open" } else { "closed" } }),
            },
            true,
            cx,
        );
    }

    /// Opens the merge dialog for the open pull request, when GitHub says it
    /// can be merged.
    pub(super) fn open_merge_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(pr), Some(repo)) = (self.selected_pr.clone(), self.selected_repo.clone()) else {
            return;
        };
        let MergeGate::Ready(caution) = merge_gate(&pr) else {
            return;
        };
        let plan = MergePlan {
            number: pr.number,
            title: pr.title.clone(),
            base: pr.base.r#ref.clone(),
            head_sha: pr.head.sha.clone(),
            deletable_branch: deletable_head(&pr, &repo),
            methods: merge_methods(&repo),
            caution,
        };
        self.open_workspace_modal(HelmModalKind::MergePull(plan), window, cx);
    }

    pub(super) fn handle_merge_pull(
        &mut self,
        plan: &MergePlan,
        method: &str,
        commit_title: String,
        delete_branch: bool,
        cx: &mut Context<Self>,
    ) {
        self.run_action(
            HelmAction::MergePull {
                number: plan.number,
                method: method.to_string(),
                commit_title,
                sha: plan.head_sha.clone(),
                delete_branch: delete_branch.then(|| plan.deletable_branch.clone()).flatten(),
            },
            true,
            cx,
        );
    }

    /// The Pull requests tab — filterable list showing head→base branches,
    /// each row opening its page in the browser.
    pub(super) fn render_pulls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filter_row = h_flex()
            .items_center()
            .justify_between()
            .gap_1()
            .px_3()
            .py_2()
            .child(self.state_filter(
                "helm-pulls-filter",
                &self.pulls_filter,
                HelmScreen::Pulls,
                cx,
            ))
            .child(
                Button::new("pulls-create")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .label("New pull request")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_create_pull_dialog(window, cx)),
                    ),
            );

        self.list_screen(
            self.pulls
                .paged_status(|this, page, cx| this.load_pulls_page(page, cx)),
            &self.pulls_list,
            Some(filter_row.into_any_element()),
            ListLabels {
                loading: "Loading pull requests…",
                error: "Failed to load pull requests",
                empty: "No pull requests found",
            },
            |this, cx| this.load_pulls_page(this.pulls.page, cx),
            cx,
        )
    }

    /// The line under the buttons that says whether the pull request can be
    /// merged, and why not.
    fn render_merge_status(&self, pr: &Pull, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let (label, color) = match merge_gate(pr) {
            MergeGate::Ready(None) => ("Ready to merge".to_string(), theme.success),
            MergeGate::Ready(Some(caution)) => (format!("Can be merged. {caution}"), theme.warning),
            MergeGate::Blocked(reason) => (reason.to_string(), theme.danger),
            MergeGate::Checking => (
                "GitHub is still working out whether this can be merged.".to_string(),
                theme.muted_foreground,
            ),
        };
        h_flex()
            .gap_2()
            .items_center()
            .child(tinted("Merge", color))
            .child(div().text_xs().text_color(theme.muted_foreground).child(label))
            .into_any_element()
    }

    /// The checks that ran on the pull request's latest commit.
    fn render_checks(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (success, danger, warning) = (theme.success, theme.danger, theme.warning);
        match self.pr_checks_state {
            LoadState::Loading if self.pr_checks.is_empty() => {
                return h_flex()
                    .gap_2()
                    .items_center()
                    .child(Spinner::new().small())
                    .child(div().text_xs().text_color(muted).child("Loading checks…"))
                    .into_any_element();
            }
            LoadState::Error if self.pr_checks.is_empty() => {
                return div()
                    .text_xs()
                    .text_color(muted)
                    .child("The checks could not be loaded.")
                    .into_any_element();
            }
            _ => {}
        }
        if self.pr_checks.is_empty() {
            return div()
                .text_xs()
                .text_color(muted)
                .child("No checks ran on this pull request.")
                .into_any_element();
        }

        const SHOWN: usize = 12;
        let failed = self
            .pr_checks
            .iter()
            .filter(|run| {
                matches!(
                    run.conclusion.as_deref(),
                    Some("failure" | "timed_out" | "action_required")
                )
            })
            .count();
        let running = self
            .pr_checks
            .iter()
            .filter(|run| run.status != "completed")
            .count();
        let summary = format!(
            "{} checks: {} failed, {} still running",
            self.pr_checks.len(),
            failed,
            running
        );
        v_flex()
            .gap_1()
            .child(div().text_xs().font_semibold().child(summary))
            .children(self.pr_checks.iter().take(SHOWN).enumerate().map(|(ix, run)| {
                let (label, color) = if run.status != "completed" {
                    (if run.status == "queued" { "Queued" } else { "Running" }, warning)
                } else {
                    match run.conclusion.as_deref() {
                        Some("success") => ("Passed", success),
                        Some("failure" | "timed_out" | "action_required") => ("Failed", danger),
                        Some("cancelled") => ("Cancelled", muted),
                        Some("skipped") => ("Skipped", muted),
                        _ => ("Neutral", muted),
                    }
                };
                let url = run.html_url.clone();
                h_flex()
                    .id(("helm-pr-check", ix))
                    .gap_2()
                    .items_center()
                    .child(tinted(label, color))
                    .child(div().text_xs().child(run.name.clone()))
                    .when_some(url, |row, url| {
                        row.cursor_pointer()
                            .on_click(move |_, _, cx| cx.open_url(&url))
                    })
            }))
            .when(self.pr_checks.len() > SHOWN, |col| {
                col.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("and {} more", self.pr_checks.len() - SHOWN)),
                )
            })
            .into_any_element()
    }

    /// Same as [`Self::render_issue_detail`], for a PR — head→base branch
    /// info, a status, the checks and the actions (edit, close or reopen,
    /// merge) take the place of labels; the body, the comment thread and
    /// the browser link are the same.
    pub(super) fn render_pr_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted_foreground = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;

        let Some(pr) = self.selected_pr.clone() else {
            return v_flex()
                .p_4()
                .child(
                    div()
                        .text_sm()
                        .text_color(muted_foreground)
                        .child("No pull request selected"),
                )
                .into_any_element();
        };

        // The list does not say whether a pull request merged, only
        // `merged_at` (and the single-pull fetch fills `merged` in).
        let merged = pr.merged || pr.merged_at.is_some();
        let (status_label, status_tag) = if merged {
            ("Merged", Pill::info())
        } else if pr.state == "closed" {
            ("Closed", Pill::danger())
        } else {
            ("Open", Pill::success())
        };
        let open = !merged && pr.state != "closed";
        let gate = merge_gate(&pr);
        let author = pr
            .user
            .as_ref()
            .map(|u| u.login.clone())
            .unwrap_or_default();
        let head_label = if pr.head.label.is_empty() {
            pr.head.r#ref.clone()
        } else {
            pr.head.label.clone()
        };
        let base_label = if pr.base.label.is_empty() {
            pr.base.r#ref.clone()
        } else {
            pr.base.label.clone()
        };
        let url = pr.html_url.clone();
        let number = pr.number;

        v_flex()
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(status_tag.xsmall().rounded_full().child(status_label))
                                    .when(pr.draft, |row| row.child(chip("Draft")))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_foreground)
                                            .child(format!("#{} opened by {author}", pr.number)),
                                    ),
                            )
                            .child(
                                Button::new("helm-pr-open-browser")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::ExternalLink)
                                    .tooltip("Open in Browser")
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            ),
                    )
                    .child(
                        div()
                            .text_base()
                            .font_semibold()
                            .text_color(foreground)
                            .child(pr.title.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_family("Cascadia Mono")
                            .text_color(muted_foreground)
                            .child(format!("{head_label} → {base_label}")),
                    )
                    .when(!merged, |col| {
                        col.child(
                            h_flex()
                                .flex_wrap()
                                .gap_1()
                                .when(open, |row| {
                                    row.child(
                                        Button::new("helm-pr-edit")
                                            .outline()
                                            .xsmall()
                                            .label("Edit")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.open_edit_pull_dialog(window, cx)
                                            })),
                                    )
                                    .child(
                                        Button::new("helm-pr-close")
                                            .outline()
                                            .xsmall()
                                            .label("Close")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.set_pull_open(number, false, cx)
                                            })),
                                    )
                                    .child(
                                        Button::new("helm-pr-merge")
                                            .primary()
                                            .xsmall()
                                            .label("Merge…")
                                            .disabled(!matches!(gate, MergeGate::Ready(_)))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.open_merge_dialog(window, cx)
                                            })),
                                    )
                                })
                                .when(!open, |row| {
                                    row.child(
                                        Button::new("helm-pr-reopen")
                                            .outline()
                                            .xsmall()
                                            .label("Reopen")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.set_pull_open(number, true, cx)
                                            })),
                                    )
                                })
                                .child(
                                    Button::new("helm-pr-refresh")
                                        .ghost()
                                        .xsmall()
                                        .label("Refresh")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.refresh_pr(number, false, cx);
                                            this.fetch_detail_comments(number, false, cx);
                                        })),
                                ),
                        )
                    })
                    .when(open, |col| col.child(self.render_merge_status(&pr, cx))),
            )
            .child(Separator::horizontal())
            .child(div().p_3().child(self.render_checks(cx)))
            .child(Separator::horizontal())
            .child(
                div().p_3().text_sm().child(markdown(
                    pr.body
                        .clone()
                        .filter(|b| !b.trim().is_empty())
                        .unwrap_or_else(|| "_No description provided._".to_string()),
                )),
            )
            .child(Separator::horizontal())
            .child(self.render_comment_thread(cx))
            .child(self.render_comment_box(cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull(state: Option<&str>, mergeable: Option<bool>) -> Pull {
        serde_json::from_value(json!({
            "number": 7,
            "title": "Add it",
            "state": "open",
            "mergeable": mergeable,
            "mergeable_state": state,
            "head": { "label": "me:feature", "ref": "feature", "sha": "abc" },
            "base": { "label": "me:main", "ref": "main", "sha": "def" },
        }))
        .expect("a pull request the backend's type accepts")
    }

    fn repo(allow: [Option<bool>; 3]) -> Repo {
        serde_json::from_value(json!({
            "id": 1,
            "name": "r",
            "full_name": "me/r",
            "owner": { "login": "me", "id": 1, "avatar_url": "", "type": "User" },
            "private": false,
            "description": null,
            "clone_url": "",
            "visibility": "public",
            "archived": false,
            "pushed_at": "",
            "has_issues": true,
            "has_wiki": true,
            "has_projects": true,
            "default_branch": "main",
            "allow_merge_commit": allow[0],
            "allow_squash_merge": allow[1],
            "allow_rebase_merge": allow[2],
        }))
        .expect("a repository the backend's type accepts")
    }

    #[test]
    fn a_clean_pull_request_can_be_merged() {
        assert_eq!(merge_gate(&pull(Some("clean"), Some(true))), MergeGate::Ready(None));
        assert_eq!(merge_gate(&pull(Some("has_hooks"), Some(true))), MergeGate::Ready(None));
    }

    #[test]
    fn failing_checks_that_are_not_required_only_warn() {
        assert!(matches!(
            merge_gate(&pull(Some("unstable"), Some(true))),
            MergeGate::Ready(Some(_))
        ));
    }

    #[test]
    fn conflicts_branch_rules_and_a_stale_branch_block_the_merge() {
        for state in ["dirty", "behind", "blocked", "draft"] {
            assert!(
                matches!(merge_gate(&pull(Some(state), Some(false))), MergeGate::Blocked(_)),
                "{state} should block"
            );
        }
    }

    #[test]
    fn a_draft_is_blocked_whatever_else_github_says() {
        let mut draft = pull(Some("clean"), Some(true));
        draft.draft = true;
        assert!(matches!(merge_gate(&draft), MergeGate::Blocked(_)));
    }

    #[test]
    fn an_answer_github_has_not_worked_out_yet_is_not_a_yes() {
        assert_eq!(merge_gate(&pull(None, None)), MergeGate::Checking);
        assert_eq!(merge_gate(&pull(Some("unknown"), None)), MergeGate::Checking);
    }

    #[test]
    fn only_the_merge_methods_the_repository_allows_are_offered() {
        let methods = |allow| {
            merge_methods(&repo(allow))
                .iter()
                .map(|(method, _)| *method)
                .collect::<Vec<_>>()
        };
        assert_eq!(methods([None, None, None]), ["merge", "squash", "rebase"]);
        assert_eq!(methods([Some(false), Some(true), Some(false)]), ["squash"]);
        // A repository that allows none is read as not having said.
        assert_eq!(
            methods([Some(false), Some(false), Some(false)]),
            ["merge", "squash", "rebase"]
        );
    }

    #[test]
    fn only_a_branch_of_the_repository_itself_is_offered_for_deletion() {
        let repo = repo([None, None, None]);
        assert_eq!(
            deletable_head(&pull(Some("clean"), Some(true)), &repo),
            Some("feature".to_string())
        );

        // From a fork: the head's owner differs from the base's.
        let mut from_fork = pull(Some("clean"), Some(true));
        from_fork.head.label = "someone:feature".to_string();
        assert_eq!(deletable_head(&from_fork, &repo), None);

        // Never the default branch.
        let mut from_default = pull(Some("clean"), Some(true));
        from_default.head.r#ref = "main".to_string();
        assert_eq!(deletable_head(&from_default, &repo), None);
    }
}

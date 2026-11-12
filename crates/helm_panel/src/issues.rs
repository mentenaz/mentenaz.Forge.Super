//! Helm's issue screens: the list with its state filter and the detail
//! screen. The state filter and the comment thread are shared with the pull
//! request screens in `pulls.rs`.

use super::*;

/// How often the open issue or pull request is checked for news.
const DETAIL_POLL: Duration = Duration::from_secs(30);
/// The same while a pull request's checks are still running.
const DETAIL_POLL_CHECKS_RUNNING: Duration = Duration::from_secs(15);

impl HelmPanel {
    /// Loads `self.selected_repo`'s issue list for the current
    /// `issues_filter`. The Issues tab drops PR-shaped items (the `/issues`
    /// endpoint mixes issues and PRs).
    pub(super) fn load_issues(&mut self, cx: &mut Context<Self>) {
        self.load_issues_page(1, cx);
    }

    pub(super) fn load_issues_page(&mut self, page: u32, cx: &mut Context<Self>) {
        let filter = self.issues_filter.clone();
        let remembered_filter = filter.clone();
        // GitHub's issues endpoint returns pull requests too, and counts
        // them in its pages. Leaving them out means a page can show fewer
        // than ten rows while more pages follow.
        fn without_pulls(mut issues: Page<Issue>) -> Page<Issue> {
            issues.items.retain(|issue| issue.pull_request.is_none());
            issues
        }
        self.load_section_page(
            cx,
            |this| &mut this.issues,
            page,
            move |repo, gh_state| {
                let request = requests::issues(&repo.owner.login, &repo.name, &remembered_filter);
                peek_page(gh_state, request, page, PAGE_SIZE).map(without_pulls)
            },
            move |repo, gh_state| async move {
                let request = requests::issues(&repo.owner.login, &repo.name, &filter);
                let issues = fetch_page(&gh_state, request, page, PAGE_SIZE).await?;
                Ok::<_, GhError>(without_pulls(issues))
            },
        );
    }

    /// User clicked an issue row: shows it in-panel instead of opening
    /// GitHub in the browser (`Issues`'s list `Issue` already has the full
    /// body — the list endpoint returns it — so this needs no fetch of its
    /// own beyond the comment thread).
    pub(super) fn open_issue_detail(&mut self, issue: Issue, cx: &mut Context<Self>) {
        let number = issue.number;
        self.selected_issue = Some(issue);
        self.navigate_to(HelmScreen::IssueDetail, cx);
        self.reset_comment_box();
        self.load_detail_comments(number, cx);
    }

    /// Starts the new-comment box empty for the item just opened. A draft
    /// written for the previous item is not carried over to this one.
    pub(super) fn reset_comment_box(&mut self) {
        self.comment_sending = false;
        self.clear_comment_input = true;
    }

    /// The number of the issue or pull request on screen.
    fn open_item_number(&self) -> Option<u64> {
        match self.screen {
            HelmScreen::IssueDetail => self.selected_issue.as_ref().map(|issue| issue.number),
            HelmScreen::PrDetail => self.selected_pr.as_ref().map(|pr| pr.number),
            _ => None,
        }
    }

    /// Posts what is in the comment box on the open issue or pull request.
    /// The box is emptied once GitHub has accepted it, so a comment that
    /// failed to send is still there to try again.
    pub(super) fn post_comment(&mut self, cx: &mut Context<Self>) {
        let body = self.comment_input.read(cx).value().trim().to_string();
        let Some(number) = self.open_item_number() else {
            return;
        };
        if body.is_empty() || self.comment_sending {
            return;
        }
        self.comment_sending = true;
        self.run_action(HelmAction::CommentOn { number, body }, true, cx);
        cx.notify();
    }

    /// The box under the thread for writing a comment.
    pub(super) fn render_comment_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.comment_input.read(cx).value().trim().is_empty();
        v_flex()
            .gap_2()
            .p_3()
            .child(Textarea::new(&self.comment_input))
            .child(
                h_flex().justify_end().child(
                    Button::new("helm-comment-send")
                        .primary()
                        .xsmall()
                        .label(if self.comment_sending { "Sending…" } else { "Comment" })
                        .disabled(empty || self.comment_sending)
                        .on_click(cx.listener(|this, _, _, cx| this.post_comment(cx))),
                ),
            )
    }

    /// Loads the comment thread for whichever issue/PR is now open —
    /// shared between both, since GitHub serves PR "conversation" comments
    /// from the same `/issues/{number}/comments` endpoint (see `Comment`'s
    /// doc comment).
    pub(super) fn load_detail_comments(&mut self, number: u64, cx: &mut Context<Self>) {
        if self.selected_repo.is_none() {
            return;
        }
        self.detail_comments_state = LoadState::Loading;
        self.detail_comments.clear();
        cx.notify();
        self.fetch_detail_comments(number, false, cx);
    }

    /// Asks GitHub for the thread again and swaps it in when it arrives,
    /// leaving the comments already on screen until then. This is what
    /// Refresh does, so the thread does not blink away and come back.
    /// `quiet` is for the background check: a failure then leaves the screen
    /// as it is instead of replacing the thread with an error.
    pub(super) fn fetch_detail_comments(
        &mut self,
        number: u64,
        quiet: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.selected_repo.clone() else {
            return;
        };
        let gh_state = self.gh_state.clone();
        let started = gh_state.clone();
        cx.spawn(async move |this, cx| {
            let result = on_tokio(async move {
                gh_list_issue_comments(repo.owner.login, repo.name, number, &gh_state).await
            })
            .await;
            this.update(cx, |this, cx| {
                // A late answer for an item the user has since left would
                // otherwise replace the thread of the one now open.
                if !this.is_current_state(&started) || this.open_item_number() != Some(number) {
                    return;
                }
                match result {
                    Ok(comments) => {
                        this.detail_comments = comments;
                        this.detail_comments_state = LoadState::Idle;
                    }
                    Err(_) if quiet => {}
                    Err(e) => {
                        this.detail_comments_state = LoadState::Error;
                        this.error_msg = e.to_string();
                        this.error_link = e.link();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Keeps the background check running for the issue or pull request on
    /// screen, and for no other: starts it on arriving at one, drops it on
    /// leaving. Called whenever the screen changes.
    pub(super) fn sync_detail_poll(&mut self, cx: &mut Context<Self>) {
        let running = self.detail_poll.as_ref().map(|(number, _)| *number);
        match self.open_item_number() {
            Some(number) if running == Some(number) => {}
            Some(number) => self.start_detail_poll(number, cx),
            None => self.detail_poll = None,
        }
    }

    /// How long to wait before the next background check: sooner while a pull
    /// request's checks are still running, since that is what is being
    /// watched.
    fn detail_poll_interval(&self) -> Duration {
        let checks_running = self.screen == HelmScreen::PrDetail
            && self.pr_checks.iter().any(|run| run.status != "completed");
        if checks_running {
            DETAIL_POLL_CHECKS_RUNNING
        } else {
            DETAIL_POLL
        }
    }

    /// Checks the open item for news every so often, so a reply or a finished
    /// check shows up without pressing Refresh. It uses the same conditional
    /// requests as everything else, which GitHub answers with "unchanged" at no
    /// cost to the rate limit, and it is silent when it fails. Replacing
    /// `detail_poll` drops the previous loop, which ends it.
    fn start_detail_poll(&mut self, number: u64, cx: &mut Context<Self>) {
        let started = self.gh_state.clone();
        let task = cx.spawn(async move |this, cx| {
            loop {
                let Ok(wait) = this.update(cx, |this, _| this.detail_poll_interval()) else {
                    return;
                };
                cx.background_executor().timer(wait).await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        if !this.is_current_state(&started)
                            || this.open_item_number() != Some(number)
                        {
                            return false;
                        }
                        this.fetch_detail_comments(number, true, cx);
                        if this.screen == HelmScreen::PrDetail {
                            this.refresh_pr(number, true, cx);
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    return;
                }
            }
        });
        self.detail_poll = Some((number, task));
    }

    /// The Open / Closed / All switch of the Issues and Pulls screens.
    /// `current` is the filter in force.
    pub(super) fn state_filter(
        &self,
        id: &'static str,
        current: &str,
        screen: HelmScreen,
        cx: &mut Context<Self>,
    ) -> ButtonGroup {
        const FILTERS: [(&str, &str); 3] = [("Open", "open"), ("Closed", "closed"), ("All", "all")];
        ButtonGroup::new(id)
            .outline()
            .compact()
            .small()
            .children(FILTERS.iter().enumerate().map(|(ix, (label, filter))| {
                Button::new(("helm-state-filter", ix))
                    .label(*label)
                    .selected(current == *filter)
            }))
            .on_click(cx.listener(move |this, clicked: &Vec<usize>, _, cx| {
                if let Some((_, filter)) = clicked.first().and_then(|ix| FILTERS.get(*ix)) {
                    this.set_tab_filter(screen, filter, cx);
                }
            }))
    }

    /// Applies a filter to whichever of Issues/Pulls owns it and reloads the
    /// list.
    pub(super) fn set_tab_filter(
        &mut self,
        screen: HelmScreen,
        filter: &str,
        cx: &mut Context<Self>,
    ) {
        match screen {
            HelmScreen::Issues => {
                self.issues_filter = filter.to_string();
                self.load_issues(cx);
            }
            HelmScreen::Pulls => {
                self.pulls_filter = filter.to_string();
                self.load_pulls(cx);
            }
            _ => {}
        }
    }

    /// The Issues tab — filterable list of `self.selected_repo`'s issues
    /// (PRs filtered out), each row opening its page in the browser.
    pub(super) fn render_issues(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filter_row = h_flex()
            .items_center()
            .px_3()
            .py_2()
            .child(self.state_filter(
                "helm-issues-filter",
                &self.issues_filter,
                HelmScreen::Issues,
                cx,
            ));

        self.list_screen(
            self.issues
                .paged_status(|this, page, cx| this.load_issues_page(page, cx)),
            &self.issues_list,
            Some(filter_row.into_any_element()),
            ListLabels {
                loading: "Loading issues…",
                error: "Failed to load issues",
                empty: "No issues found",
            },
            |this, cx| this.load_issues_page(this.issues.page, cx),
            cx,
        )
    }

    /// The open issue's own view: title/number/state/author/labels, full
    /// body (rendered as markdown — already present on `selected_issue`,
    /// since the `/issues` list endpoint returns it, so no extra fetch was
    /// needed just for this), and the comment thread
    /// `load_detail_comments` loaded on entry. Back/forward navigation
    /// comes for free from the shared nav bar (`render`'s `show_nav`).
    pub(super) fn render_issue_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted_foreground = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;

        let Some(issue) = self.selected_issue.clone() else {
            return v_flex()
                .p_4()
                .child(
                    div()
                        .text_sm()
                        .text_color(muted_foreground)
                        .child("No issue selected"),
                )
                .into_any_element();
        };

        let state_open = issue.state != "closed";
        let author = issue
            .user
            .as_ref()
            .map(|u| u.login.clone())
            .unwrap_or_default();
        let url = issue.html_url.clone();
        let number = issue.number;

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
                                    .child(
                                        if state_open {
                                            Pill::success()
                                        } else {
                                            Pill::danger()
                                        }
                                        .xsmall()
                                        .rounded_full()
                                        .child(if state_open { "Open" } else { "Closed" }),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_foreground)
                                            .child(format!("#{} opened by {author}", issue.number)),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("helm-issue-refresh")
                                            .ghost()
                                            .xsmall()
                                            .label("Refresh")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.fetch_detail_comments(number, false, cx)
                                            })),
                                    )
                                    .child(
                                        Button::new("helm-issue-open-browser")
                                            .ghost()
                                            .xsmall()
                                            .icon(IconName::ExternalLink)
                                            .tooltip("Open in Browser")
                                            .on_click(move |_, _, cx| cx.open_url(&url)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_base()
                            .font_semibold()
                            .text_color(foreground)
                            .child(issue.title.clone()),
                    )
                    .when(!issue.labels.is_empty(), |col| {
                        col.child(
                            h_flex().flex_wrap().gap_1().children(
                                issue.labels.iter().map(|label| chip(label.name.clone())),
                            ),
                        )
                    }),
            )
            .child(Separator::horizontal())
            .child(
                div().p_3().text_sm().child(markdown(
                    issue
                        .body
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

    /// The comment thread for whichever issue/PR is open — shared by both
    /// detail views since they're backed by the same `detail_comments`
    /// (see that field's doc comment). Each comment's body renders as
    /// markdown too, same as the issue/PR body above it.
    pub(super) fn render_comment_thread(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let foreground = cx.theme().foreground;
        let border = cx.theme().border;

        if self.detail_comments_state == LoadState::Loading {
            return loading_screen("Loading comments…", cx);
        }

        if self.detail_comments_state == LoadState::Error {
            return failed_screen(
                "Failed to load comments",
                &self.error_msg,
                self.error_link.clone(),
                None,
                cx,
            );
        }

        if self.detail_comments.is_empty() {
            return note_screen("No comments yet", cx);
        }

        v_flex()
            .gap_3()
            .p_3()
            .children(self.detail_comments.iter().map(|comment| {
                let author = comment
                    .user
                    .as_ref()
                    .map(|u| u.login.clone())
                    .unwrap_or_default();
                v_flex()
                    .gap_1()
                    .pb_3()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .text_color(foreground)
                            .child(author),
                    )
                    .child(
                        div()
                            .text_sm()
                            .child(markdown(comment.body.clone().unwrap_or_default())),
                    )
            }))
            .into_any_element()
    }
}

//! Helm's repository screens: the list with its filter, the repository detail
//! screen, and the create and edit dialogs.

use super::*;

impl HelmPanel {
    /// Navigate to the repo list, loading either the selected org's repos
    /// or the user's own (`"self"`) — mirrors the old TS
    /// `loadRepos(selectedOrg || "self")`.
    pub(super) fn open_repo_list(&mut self, cx: &mut Context<Self>) {
        let owner = self
            .selected_org
            .clone()
            .unwrap_or_else(|| "self".to_string());
        self.navigate_to(HelmScreen::RepoList, cx);
        self.load_repos(owner, cx);
    }

    pub(super) fn load_repos(&mut self, owner: String, cx: &mut Context<Self>) {
        self.load_section_with(
            cx,
            |this| &mut this.repos,
            |gh_state| async move { gh_get_repos(owner, &gh_state).await },
        );
    }

    pub(super) fn open_github_search(&mut self, cx: &mut Context<Self>) {
        self.github_search_results.clear();
        self.github_search_query.clear();
        self.navigate_to(HelmScreen::GitHubSearch, cx);
    }

    pub(super) fn load_github_search(&mut self, page: u32, cx: &mut Context<Self>) {
        const SEARCH_PAGE_SIZE: u32 = 20;
        let query = if page == 1 {
            self.github_search_input.read(cx).value().trim().to_string()
        } else {
            self.github_search_query.clone()
        };
        if query.is_empty() {
            self.github_search_results.clear();
            self.github_search_query.clear();
            self.error_msg = "Enter a search query.".into();
            cx.notify();
            return;
        }
        self.error_msg.clear();
        self.github_search_query = query.clone();
        let gh_state = self.gh_state.clone();
        let load = self.github_search_results.begin_page(page);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = on_tokio(async move {
                search_repositories_page(&gh_state, &query, page, SEARCH_PAGE_SIZE).await
            })
            .await
            .map_err(|error| error.to_string());
            this.update(cx, |this, cx| {
                if this.github_search_results.is_current(load) {
                    this.github_search_results.finish_page(result);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn render_github_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let input = self.github_search_input.clone();
        let rate_line = self.gh_state.rate_limit_for("search").and_then(|rate| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs());
            (now < rate.reset_at).then(|| {
                div()
                    .text_xs()
                    .text_color(if rate.is_low() {
                        cx.theme().warning
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(format!("Search allowance: {}", rate.summary(now)))
            })
        });
        let searched = !self.github_search_query.is_empty();
        let header = v_flex()
            .gap_2()
            .px_3()
            .py_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(Input::new(&input).flex_1())
                    .child(
                        Button::new("helm-search-github-submit")
                            .primary()
                            .label(if self.github_search_results.state == LoadState::Loading {
                                "Searching…"
                            } else {
                                "Search"
                            })
                            .disabled(self.github_search_results.state == LoadState::Loading)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.load_github_search(1, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Search GitHub repositories. Submit only when ready to use the search allowance."),
            )
            .children(rate_line)
            .when(!self.error_msg.is_empty(), |this| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(self.error_msg.clone()))
            });

        // The same list screen as Repositories: the rows, the spinner, the
        // error with Retry and the pager all come from `list_screen`.
        self.list_screen(
            self.github_search_results
                .paged_status(|this, page, cx| this.load_github_search(page, cx)),
            &self.github_search_list,
            Some(header.into_any_element()),
            ListLabels {
                loading: "Searching GitHub…",
                error: "Repository search failed",
                empty: if searched {
                    "No repositories found"
                } else {
                    "Type a search and press Enter"
                },
            },
            |this, cx| this.load_github_search(this.github_search_results.page, cx),
            cx,
        )
    }

    /// User picked a repo from `RepoList` — mirrors the old TS
    /// `setSelectedRepo` + `setScreen("repo-detail")` pair. No extra fetch
    /// needed, the list response already has everything the detail screen
    /// shows.
    pub(super) fn select_repo(&mut self, repo: Repo, cx: &mut Context<Self>) {
        self.selected_repo = Some(repo);
        self.navigate_to(HelmScreen::RepoDetail, cx);
    }

    pub(super) fn open_selected_repository_workspace(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.selected_repo.clone() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let gh_state = self.gh_state.clone();
        let account_color = self
            .signed_in_login()
            .map(|login| HelmPanelSettings::get_global(cx).account_color(login));
        workspace.update(cx, |workspace, cx| {
            helm_workspace::open_workspace_tab(
                repo,
                gh_state,
                account_color,
                workspace,
                window,
                cx,
            );
        });
    }

    /// Creates a repo under the authenticated user's own account, or under the
    /// given org (`Some(owner)`) via `gh_create_repo`'s `"owner"` key — the
    /// key is stripped from the request body and rerouted to
    /// `POST /orgs/{org}/repos` under the hood. On success, jumps straight to
    /// the new repo's detail screen; on failure, surfaces the error via a
    /// notification since the Menu screen (where this is triggered from) has
    /// no dedicated place to show it.
    ///
    /// `attach` is the Git panel's "Using Helm" local follow-up: parked in
    /// `pending_attach` until GitHub confirms the repository, then run by
    /// [`Self::attach_created_repo`] instead of the navigation above. The
    /// plain Helm dialog passes `None`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_create_repo(
        &mut self,
        name: String,
        description: String,
        private: bool,
        owner: Option<String>,
        attach: Option<CreateRepoAttach>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A publish creates the repository on GitHub first and runs `git`
        // afterwards: refuse before creating anything when the folder is not
        // trusted (the dialog was open before the answer changed, say).
        if attach.is_some()
            && let Err(message) = self.check_publish_trust(cx)
        {
            self.notify(message, cx);
            return;
        }
        self.pending_attach = attach;
        let mut opts = json!({
            "name": name,
            "description": description,
            "private": private,
        });
        if let Some(owner) = owner {
            opts["owner"] = json!(owner);
        }
        self.run_action(HelmAction::CreateRepo { opts }, true, cx);
    }

    /// Renames/updates description+homepage, toggles feature switches
    /// (issues/wiki/projects/discussions), optionally changes visibility,
    /// and updates topics for `self.selected_repo`.
    pub(super) fn handle_edit_repo(
        &mut self,
        name: String,
        description: String,
        homepage: String,
        topics: Vec<String>,
        private: bool,
        has_issues: bool,
        has_wiki: bool,
        has_projects: bool,
        has_discussions: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.selected_repo.as_ref() else {
            return;
        };

        let mut changes = json!({
            "name": name,
            "description": description,
            "homepage": homepage,
            "has_issues": has_issues,
            "has_wiki": has_wiki,
            "has_projects": has_projects,
            "has_discussions": has_discussions,
        });
        // Only sent when the switch was actually flipped: a visibility change
        // is the one consequential field here, so an unrelated edit (or an
        // `internal` repo, which also reports `private: true`) must not
        // restate it.
        if private != repo.private {
            changes["private"] = json!(private);
        }

        self.run_action(HelmAction::EditRepo { changes, topics }, true, cx);
    }

    /// Opens the "Create repository" dialog — targets the authenticated
    /// user's own account by default; typing an org login in the
    /// "Organization" field creates it org-scoped instead (`gh_create_repo`
    /// reroutes the `"owner"` key to `POST /orgs/{org}/repos`).
    pub(super) fn open_create_repo_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_workspace_modal(
            HelmModalKind::CreateRepo {
                attach_local: false,
                existing: None,
            },
            window,
            cx,
        );
    }

    /// The repo list — mirrors the old `RepoListScreen`. Loads either the
    /// selected org's repos or the user's own, filterable by name. Rows
    /// route into `RepoDetail` via [`Self::select_repo`].
    pub(super) fn render_repo_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let search_row = div().px_3().py_2().child(Input::new(&self.repo_search));
        let owner = self
            .selected_org
            .clone()
            .unwrap_or_else(|| "self".to_string());
        self.list_screen(
            ListStatus {
                state: self.repos.state,
                error: self.repos.error.clone(),
                error_link: self.repos.error_link.clone(),
                // `repos_shown` is the page of what the search box leaves,
                // worked out at the top of `render`.
                is_empty: self.repos_shown.is_empty(),
                refreshing: false,
                pager: Some(Pager {
                    page: self.repos_page,
                    last_page: self.repos_last_page,
                    // Every repository is already here; another page is
                    // just another slice.
                    go: |this, page, cx| {
                        this.repos_page = page;
                        cx.notify();
                    },
                }),
            },
            &self.repos_list,
            Some(search_row.into_any_element()),
            ListLabels {
                loading: "Loading repositories…",
                error: "Failed to load repositories",
                empty: if self.repos.items.is_empty() {
                    "No repositories found"
                } else {
                    "No repositories match your search"
                },
            },
            move |this, cx| this.load_repos(owner.clone(), cx),
            cx,
        )
    }

    /// Repo detail: name/visibility, description, homepage, topics, clone
    /// URL + a Clone action that streams `git clone` output and, on
    /// success, opens the clone as a new workspace. The old app's
    /// inline-editable description/homepage is now the Settings/Edit dialog,
    /// there's no feature-toggle port, and Branches/Collaborators drill into
    /// real screens — so the read side plus Clone (with an optional custom
    /// target folder via [`Self::pick_clone_dir`]) is fully covered.
    pub(super) fn render_repo_detail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted_foreground = cx.theme().muted_foreground;
        let foreground = cx.theme().foreground;

        let Some(repo) = self.selected_repo.clone() else {
            return v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p_4()
                .child(
                    div()
                        .text_sm()
                        .text_color(muted_foreground)
                        .child("No repo selected"),
                )
                .into_any_element();
        };

        let header_row = h_flex()
            .items_center()
            .justify_between()
            .gap_2()
            .px_3()
            .py_2()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .font_semibold()
                            .text_color(foreground)
                            .child(repo.name.clone()),
                    )
                    .child(
                        Pill::secondary()
                            .outline()
                            .xsmall()
                            .child(repo_vis_label(&repo)),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("helm-open-repository-workspace")
                            .primary()
                            .xsmall()
                            .icon(IconName::ExternalLink)
                            .label("Open Workspace")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_selected_repository_workspace(window, cx)
                            })),
                    )
                    .child(
                        Button::new("helm-repo-edit")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Settings)
                            .tooltip("Edit repository")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_edit_repo_dialog(window, cx)
                            })),
                    ),
            );

        let topics_row = (!repo.topics.is_empty()).then(|| {
            h_flex()
                .flex_wrap()
                .gap_1()
                .px_3()
                .py_2()
                .children(repo.topics.iter().map(|topic| chip(topic.clone())))
        });

        let copied = self.clone_url_copied;
        let clone_url = h_flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(div().min_w_0().truncate().child(repo.clone_url.clone()))
            .child(
                Button::new("helm-repo-copy-clone-url")
                    .ghost()
                    .xsmall()
                    .icon(if copied {
                        IconName::Check
                    } else {
                        IconName::Copy
                    })
                    .tooltip(if copied { "Copied" } else { "Copy clone URL" })
                    .on_click(cx.listener(|this, _, _, cx| this.handle_copy_clone_url(cx))),
            )
            .into_any_element();
        let details = div().px_3().py_2().child(
            DescriptionList::vertical()
                .bordered(false)
                .columns(1)
                .item(
                    "Description",
                    repo.description.clone().unwrap_or_else(|| "—".to_string()),
                    1,
                )
                .item(
                    "Homepage",
                    repo.homepage.clone().unwrap_or_else(|| "—".to_string()),
                    1,
                )
                .item("Clone URL", clone_url, 1),
        );

        let workspace_root = self
            .workspace
            .update(cx, |workspace, cx| {
                workspace
                    .worktrees(cx)
                    .next()
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            })
            .ok()
            .flatten();

        let clone_section = if workspace_root.is_some() {
            v_flex()
                .gap_2()
                .px_3()
                .py_2()
                .child(
                    Button::new("helm-repo-clone")
                        .primary()
                        .icon(IconName::Github)
                        .label("Clone repository")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_clone_modal(window, cx)),
                        ),
                )
                .into_any_element()
        } else {
            v_flex()
                .gap_1()
                .px_3()
                .py_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(muted_foreground)
                        .child("Open a folder first (File → Open Folder) to clone into it."),
                )
                .child(
                    Button::new("helm-repo-clone")
                        .primary()
                        .icon(IconName::Github)
                        .label("Clone repository")
                        .disabled(true),
                )
                .into_any_element()
        };

        v_flex()
            .size_full()
            .child(
                v_flex()
                    .flex_none()
                    .child(header_row)
                    .child(Separator::horizontal())
                    .children(topics_row)
                    .child(details)
                    .child(clone_section)
                    .child(Separator::horizontal()),
            )
            // The sections take the height that is left. In a panel too
            // short for that they keep a few rows' worth, and the screen
            // scrolls as a whole.
            .child(
                v_flex()
                    .flex_1()
                    .min_h(px(200.))
                    .child(self.repo_sections_list.element()),
            )
            .into_any_element()
    }

    /// Opens one of the repository's sections and starts loading it.
    pub(super) fn open_repo_section(&mut self, screen: HelmScreen, cx: &mut Context<Self>) {
        self.navigate_to(screen, cx);
        match screen {
            HelmScreen::Branches => self.load_branches(cx),
            HelmScreen::Collaborators => self.load_collaborators(cx),
            HelmScreen::Issues => self.load_issues(cx),
            HelmScreen::Pulls => self.load_pulls(cx),
            HelmScreen::Releases => self.load_releases(cx),
            HelmScreen::Packages => self.load_packages(cx),
            HelmScreen::Traffic => self.load_traffic(cx),
            HelmScreen::Commits => self.load_commits(cx),
            HelmScreen::WorkflowRuns => self.load_workflow_runs(cx),
            HelmScreen::Deployments => self.load_deployments(cx),
            HelmScreen::Tags => self.load_tags(cx),
            HelmScreen::Security => self.load_security(cx),
            _ => {}
        }
    }

    /// Opens the "Edit repository" dialog, prefilled from `self.selected_repo`.
    pub(super) fn open_edit_repo_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.selected_repo.clone() else {
            return;
        };

        self.open_workspace_modal(HelmModalKind::EditRepo(repo), window, cx);
    }
}

/// The sections of a repository, in the order its screen lists them.
pub(super) fn repo_sections() -> [(HelmScreen, IconName, &'static str); 12] {
    [
        (HelmScreen::Branches, IconName::Git, "Branches"),
        (HelmScreen::Collaborators, IconName::User, "Collaborators"),
        (HelmScreen::Issues, IconName::Inbox, "Issues"),
        (HelmScreen::Pulls, IconName::Redo, "Pull requests"),
        (
            HelmScreen::Releases,
            IconName::GalleryVerticalEnd,
            "Releases",
        ),
        (HelmScreen::Packages, IconName::HardDrive, "Packages"),
        (HelmScreen::Traffic, IconName::ChartPie, "Traffic"),
        (HelmScreen::Commits, IconName::Git, "Commits"),
        (
            HelmScreen::WorkflowRuns,
            IconName::SquareTerminal,
            "Actions",
        ),
        (HelmScreen::Deployments, IconName::Globe, "Deployments"),
        (HelmScreen::Tags, IconName::Asterisk, "Tags"),
        (HelmScreen::Security, IconName::TriangleAlert, "Security"),
    ]
}

/// One row of a repository's sections.
pub(super) fn repo_section_row(
    ix: usize,
    icon: IconName,
    label: &'static str,
    cx: &App,
) -> ListItem {
    let foreground = cx.theme().foreground;
    let muted_foreground = cx.theme().muted_foreground;
    ListItem::new(("helm-repo-section", ix))
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .child(Icon::new(icon).xsmall().text_color(muted_foreground))
                .child(div().text_color(foreground).child(label)),
        )
        .suffix(move |_, _| {
            Icon::new(IconName::ChevronRight)
                .xsmall()
                .text_color(muted_foreground)
        })
}

/// Which repositories the search box leaves, as positions in `repos`, in
/// their original order. An empty or blank query keeps them all. Matching
/// is on the repository's name, ignoring case.
pub(super) fn matching_repos(repos: &[Repo], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    repos
        .iter()
        .enumerate()
        .filter(|(_, repo)| query.is_empty() || repo.name.to_lowercase().contains(&query))
        .map(|(ix, _)| ix)
        .collect()
}

/// One row of the Repositories screen: the name and whether it is public,
/// private, internal or archived.
pub(super) fn repo_row(ix: usize, repo: &Repo, cx: &App) -> ListItem {
    let foreground = cx.theme().foreground;
    let muted_foreground = cx.theme().muted_foreground;
    let vis = repo_vis_label(repo);
    ListItem::new(("helm-repo", ix))
        .child(div().text_color(foreground).child(repo.name.clone()))
        .suffix(move |_, _| {
            h_flex()
                .items_center()
                .gap_2()
                .child(div().text_color(muted_foreground).child(vis))
                .child(
                    Icon::new(IconName::ChevronRight)
                        .xsmall()
                        .text_color(muted_foreground),
                )
        })
}

/// One row of the Search GitHub screen: owner and name, then the
/// description, with the star count on the right. Every row has the second
/// line, because the list needs its rows to be the same height.
pub(super) fn search_repo_row(ix: usize, repo: &Repo, cx: &App) -> ListItem {
    let foreground = cx.theme().foreground;
    let muted_foreground = cx.theme().muted_foreground;
    let description = repo
        .description
        .clone()
        .filter(|description| !description.trim().is_empty())
        .unwrap_or_else(|| "No description".to_string());
    let stars = fmt_num(repo.stargazers_count);
    ListItem::new(("helm-search-repo", ix))
        .child(
            v_flex()
                .gap_0p5()
                .min_w_0()
                .child(
                    div()
                        .truncate()
                        .text_sm()
                        .font_semibold()
                        .text_color(foreground)
                        .child(repo.full_name.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .text_xs()
                        .text_color(muted_foreground)
                        .child(description),
                ),
        )
        .suffix(move |_, _| {
            h_flex()
                .flex_none()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(muted_foreground)
                        .child(format!("★ {stars}")),
                )
                .child(
                    Icon::new(IconName::ChevronRight)
                        .xsmall()
                        .text_color(muted_foreground),
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> Repo {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "name": name,
            "full_name": format!("me/{name}"),
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
        }))
        .expect("a repository the backend's type accepts")
    }

    #[test]
    fn the_search_box_filters_by_name_and_keeps_order() {
        let repos = [
            repo("zed_Source"),
            repo("Forge.Scaffold.SDK"),
            repo("forge-templates"),
        ];
        assert_eq!(matching_repos(&repos, ""), vec![0, 1, 2]);
        assert_eq!(matching_repos(&repos, "   "), vec![0, 1, 2]);
        assert_eq!(matching_repos(&repos, "FORGE"), vec![1, 2]);
        assert_eq!(matching_repos(&repos, " zed "), vec![0]);
        assert!(matching_repos(&repos, "nothing").is_empty());
        assert!(matching_repos(&[], "zed").is_empty());
    }
}

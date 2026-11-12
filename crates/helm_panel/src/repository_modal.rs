//! The modal Helm uses for its forms and confirmations: create and edit
//! repository, create pull request and release, edit profile, collaborators,
//! and the clone dialog.

use gpui_component::IndexPath;
use gpui_component::select::{Select, SelectEvent, SelectState};

use super::auth::app_settings_url;
use super::*;

#[derive(Clone)]
pub(super) enum HelmModalKind {
    /// `attach_local` is set by the Git panel's "Using Helm" button: the
    /// form then also offers the local follow-up (README/.gitignore and an
    /// initial commit message) and, on success, wires the workspace to the
    /// new repository instead of only navigating to its detail screen —
    /// see [`HelmPanel::open_create_repo_for_attach`] and
    /// [`HelmPanel::attach_created_repo`].
    CreateRepo {
        attach_local: bool,
    },
    EditRepo(Repo),
    AddCollaborator,
    RemoveCollaborator(String),
    /// Carries the repo's default branch, prefilled as the base.
    CreatePull(String),
    /// Edits the open pull request's title and body, prefilled.
    EditPull {
        number: u64,
        title: String,
        body: String,
    },
    /// Confirms merging a pull request: the method, the commit title and
    /// whether to delete the branch.
    MergePull(MergePlan),
    CreateRelease,
    /// Carries the signed-in user's current profile, prefilled into the form.
    EditProfile(GitHubUser),
    /// The commit name and email for workspaces that use `login`, prefilled
    /// with the current ones. Uses the `name` field for the name and the
    /// `homepage` field for the email.
    CommitIdentity {
        login: String,
        name: String,
        email: String,
    },
    /// No form fields of its own — reads `parent`'s `cloning`/`clone_lines`/
    /// `clone_error`/`clone_succeeded_path` directly and walks through
    /// picking a folder, showing progress, and offering to open the result,
    /// rather than a single submit like every other kind.
    CloneRepo,
}

pub(super) struct HelmRepositoryModal {
    pub(super) kind: HelmModalKind,
    pub(super) parent: Entity<HelmPanel>,
    /// Re-renders this modal whenever `parent` does — needed only for
    /// `CloneRepo`, which reads `parent`'s clone-progress fields live rather
    /// than owning its own form state. `None` for every other kind.
    pub(super) _clone_progress_sub: Option<Subscription>,
    pub(super) focus_handle: FocusHandle,
    pub(super) name: Entity<InputState>,
    pub(super) description: Entity<InputState>,
    /// The Edit pull request dialog's multi-line description.
    pub(super) body: Entity<TextareaState>,
    pub(super) homepage: Entity<InputState>,
    pub(super) topics: Entity<InputState>,
    pub(super) organization: Entity<InputState>,
    /// The signed-in account, listed first as the Owner and named in the
    /// "Creating as" line.
    pub(super) account_login: Option<String>,
    /// The CreateRepo form's "Owner" dropdown: the signed-in account first,
    /// then `parent`'s already-loaded organisations. Keeps `organization` in
    /// sync on confirm (blank for the account itself), so
    /// `handle_create_repo` reads a plain string either way.
    pub(super) organization_select: Entity<SelectState<Vec<String>>>,
    pub(super) _organization_sub: Subscription,
    pub(super) username: Entity<InputState>,
    pub(super) tag_name: Entity<InputState>,
    pub(super) head_branch: Entity<InputState>,
    pub(super) base_branch: Entity<InputState>,
    pub(super) company: Entity<InputState>,
    pub(super) location: Entity<InputState>,
    pub(super) private: bool,
    /// Local-follow-up switches, only rendered (and only read on confirm)
    /// in the `CreateRepo { attach_local: true }` mode — see
    /// [`HelmPanel::attach_created_repo`].
    pub(super) gitignore: bool,
    pub(super) readme: bool,
    pub(super) commit_message: Entity<InputState>,
    pub(super) has_issues: bool,
    pub(super) has_wiki: bool,
    pub(super) has_projects: bool,
    pub(super) has_discussions: bool,
    pub(super) draft: bool,
    pub(super) prerelease: bool,
    pub(super) permission: usize,
    /// The merge dialog's chosen method, as an index into its plan's methods.
    pub(super) merge_method: usize,
    /// Whether the merge dialog also deletes the head branch.
    pub(super) delete_branch: bool,
}

impl HelmRepositoryModal {
    pub(super) fn new(
        kind: HelmModalKind,
        parent: Entity<HelmPanel>,
        account_login: Option<String>,
        // Read by `open_workspace_modal` while the panel's lease is held —
        // the build closure that calls this runs inside an update on
        // `HelmPanel`, so `parent.read(cx)` here would panic.
        org_logins: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (name_value, description_value, homepage_value, topics_value) = match &kind {
            HelmModalKind::EditRepo(repo) => (
                repo.name.clone(),
                repo.description.clone().unwrap_or_default(),
                repo.homepage.clone().unwrap_or_default(),
                repo.topics.join(", "),
            ),
            HelmModalKind::EditProfile(user) => (
                user.name.clone().unwrap_or_default(),
                user.bio.clone().unwrap_or_default(),
                user.blog.clone().unwrap_or_default(),
                String::new(),
            ),
            HelmModalKind::CommitIdentity { name, email, .. } => {
                (name.clone(), String::new(), email.clone(), String::new())
            }
            HelmModalKind::EditPull { title, .. } => {
                (title.clone(), String::new(), String::new(), String::new())
            }
            // GitHub's own default for a squash or merge commit's title.
            HelmModalKind::MergePull(plan) => (
                format!("{} (#{})", plan.title, plan.number),
                String::new(),
                String::new(),
                String::new(),
            ),
            _ => (String::new(), String::new(), String::new(), String::new()),
        };
        // A pull request's description runs to several lines, so it has a
        // text area of its own rather than the single-line `description`.
        let body_value = match &kind {
            HelmModalKind::EditPull { body, .. } => body.clone(),
            _ => String::new(),
        };
        let (has_issues, has_wiki, has_projects, has_discussions) = match &kind {
            HelmModalKind::EditRepo(repo) => (
                repo.has_issues,
                repo.has_wiki,
                repo.has_projects,
                repo.has_discussions,
            ),
            _ => (true, true, true, true),
        };
        let (company_value, location_value) = match &kind {
            HelmModalKind::EditProfile(user) => (
                user.company.clone().unwrap_or_default(),
                user.location.clone().unwrap_or_default(),
            ),
            _ => (String::new(), String::new()),
        };
        let base_branch_value = match &kind {
            HelmModalKind::CreatePull(default_base) => default_base.clone(),
            _ => String::new(),
        };
        // Create defaults to private; Edit starts from the repo's current
        // visibility so saving without touching the switch changes nothing.
        let private = match &kind {
            HelmModalKind::EditRepo(repo) => repo.private,
            _ => true,
        };

        let clone_progress_sub = matches!(kind, HelmModalKind::CloneRepo)
            .then(|| cx.observe(&parent, |_, _, cx| cx.notify()));

        // The Owner dropdown lists the signed-in account first, then the
        // orgs already fetched at login (`HelmPanel::org_logins`), so no
        // extra network work happens here. The backing `organization`
        // InputState stays the source of truth: blank means the account
        // itself, which is what `handle_create_repo` expects.
        let organization = cx.new(|cx| InputState::new(window, cx));
        let owners: Vec<String> = account_login
            .iter()
            .cloned()
            .chain(org_logins.into_iter().filter(|org| {
                !account_login
                    .as_deref()
                    .is_some_and(|login| login.eq_ignore_ascii_case(org))
            }))
            .collect();
        let selected = account_login.is_some().then(|| IndexPath::new(0));
        let organization_select =
            cx.new(|cx| SelectState::new(owners, selected, window, cx).searchable(true));
        let organization_for_sub = organization.clone();
        let account_for_sub = account_login.clone();
        let organization_sub = cx.subscribe_in(
            &organization_select,
            window,
            move |_, _, event: &SelectEvent<Vec<String>>, window, cx| {
                let SelectEvent::Confirm(choice) = event;
                // The account itself is a blank organisation.
                let organization = choice
                    .clone()
                    .filter(|owner| {
                        !account_for_sub
                            .as_deref()
                            .is_some_and(|login| login.eq_ignore_ascii_case(owner))
                    })
                    .unwrap_or_default();
                organization_for_sub.update(cx, |input, cx| {
                    input.set_value(organization, window, cx);
                });
                cx.notify();
            },
        );

        Self {
            kind,
            parent,
            _clone_progress_sub: clone_progress_sub,
            _organization_sub: organization_sub,
            focus_handle: cx.focus_handle(),
            name: cx.new(|cx| InputState::new(window, cx).default_value(name_value)),
            description: cx.new(|cx| InputState::new(window, cx).default_value(description_value)),
            body: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(4, 12)
                    .default_value(body_value)
            }),
            homepage: cx.new(|cx| InputState::new(window, cx).default_value(homepage_value)),
            topics: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(topics_value)
                    .placeholder("comma, separated, topics")
            }),
            organization,
            account_login,
            organization_select,
            username: cx.new(|cx| InputState::new(window, cx).placeholder("GitHub username")),
            tag_name: cx.new(|cx| InputState::new(window, cx).placeholder("v1.0.0")),
            head_branch: cx.new(|cx| InputState::new(window, cx).placeholder("feature-branch")),
            base_branch: cx.new(|cx| InputState::new(window, cx).default_value(base_branch_value)),
            company: cx.new(|cx| InputState::new(window, cx).default_value(company_value)),
            location: cx.new(|cx| InputState::new(window, cx).default_value(location_value)),
            private,
            gitignore: true,
            readme: true,
            commit_message: cx.new(|cx| {
                InputState::new(window, cx).default_value(DEFAULT_COMMIT_MESSAGE)
            }),
            has_issues,
            has_wiki,
            has_projects,
            has_discussions,
            draft: false,
            prerelease: false,
            permission: 0,
            merge_method: 0,
            delete_branch: false,
        }
    }

    pub(super) fn title(&self) -> &'static str {
        match self.kind {
            HelmModalKind::CreateRepo { .. } => "Create repository",
            HelmModalKind::EditRepo(_) => "Edit repository",
            HelmModalKind::AddCollaborator => "Add collaborator",
            HelmModalKind::RemoveCollaborator(_) => "Remove collaborator?",
            HelmModalKind::CreatePull(_) => "Create pull request",
            HelmModalKind::EditPull { .. } => "Edit pull request",
            HelmModalKind::MergePull(_) => "Merge pull request?",
            HelmModalKind::CreateRelease => "Create release",
            HelmModalKind::EditProfile(_) => "Edit profile",
            HelmModalKind::CommitIdentity { .. } => "Commit name and email",
            HelmModalKind::CloneRepo => "Clone repository",
        }
    }
}

impl Focusable for HelmRepositoryModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for HelmRepositoryModal {}

impl ModalView for HelmRepositoryModal {}

impl Render for HelmRepositoryModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let parent = self.parent.clone();
        let title = self.title();
        let kind = self.kind.clone();
        let confirm_kind = kind.clone();
        let show_footer = !matches!(confirm_kind, HelmModalKind::CloneRepo);

        let content = match &kind {
            HelmModalKind::CreateRepo { attach_local } => v_form()
                .child(field().label("Name").child(Input::new(&self.name)))
                .child(field().label("Description").child(Input::new(&self.description)))
                .child(
                    field().label("Owner").child(
                        Select::new(&self.organization_select)
                            .placeholder("Choose an owner")
                            .search_placeholder("Search…"),
                    ),
                )
                .children(self.account_login.clone().map(|login| {
                    let organization = self.organization.read(cx).value().trim().to_string();
                    let line = if organization.is_empty() {
                        format!("Creating as @{login}, in your own account.")
                    } else {
                        format!("Creating as @{login}, in the {organization} organization.")
                    };
                    field().child(
                        v_flex()
                            .gap_0p5()
                            .child(div().text_xs().text_color(muted).child(line))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("An organization missing? It may not have approved Helm:")
                                    .child(
                                        Link::new("helm-owner-org-access")
                                            .href(app_settings_url())
                                            .child("request access"),
                                    ),
                            ),
                    )
                }))
                .child(
                    field().child(
                        Switch::new("helm-modal-private")
                        .label("Private")
                        .checked(self.private)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.private = *checked;
                            cx.notify();
                        })),
                    ),
                )
                .when(*attach_local, |form| {
                    form.child(
                        field().child(
                            Switch::new("helm-modal-gitignore")
                            .label("Add .gitignore")
                            .checked(self.gitignore)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.gitignore = *checked;
                                cx.notify();
                            })),
                        ),
                    )
                    .child(
                        field().child(
                            Switch::new("helm-modal-readme")
                            .label("Add README.md")
                            .checked(self.readme)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.readme = *checked;
                                cx.notify();
                            })),
                        ),
                    )
                    .child(
                        field()
                            .label("Initial commit message")
                            .child(Input::new(&self.commit_message)),
                    )
                })
                .into_any_element(),
            HelmModalKind::EditRepo(repo) => v_form()
                .child(field().label("Name").child(Input::new(&self.name)))
                .child(field().label("Description").child(Input::new(&self.description)))
                .child(field().label("Homepage").child(Input::new(&self.homepage)))
                .child(field().label("Topics").child(Input::new(&self.topics)))
                .child(
                    field().child(
                        Switch::new("helm-modal-edit-private")
                        .label("Private")
                        .checked(self.private)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.private = *checked;
                            cx.notify();
                        })),
                    ),
                )
                .children((self.private != repo.private).then(|| {
                    field().child(Alert::warning(
                        "helm-modal-visibility",
                        if self.private {
                            "Saving will make this repository private."
                        } else {
                            "Saving will make this repository public — anyone will be able to see it."
                        },
                    ))
                }))
                .child(
                    field().child(
                        Switch::new("helm-modal-issues")
                        .label("Issues")
                        .checked(self.has_issues)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.has_issues = *checked;
                            cx.notify();
                        })),
                    ),
                )
                .child(
                    field().child(
                        Switch::new("helm-modal-projects")
                        .label("Projects")
                        .checked(self.has_projects)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.has_projects = *checked;
                            cx.notify();
                        })),
                    ),
                )
                .child(
                    field().child(
                        Switch::new("helm-modal-wiki")
                        .label("Wiki")
                        .checked(self.has_wiki)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.has_wiki = *checked;
                            cx.notify();
                        })),
                    ),
                )
                .child(
                    field().child(
                        Switch::new("helm-modal-discussions")
                        .label("Discussions")
                        .checked(self.has_discussions)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.has_discussions = *checked;
                            cx.notify();
                        })),
                    ),
                )
                .into_any_element(),
            HelmModalKind::AddCollaborator => v_form()
                .child(field().label("Username").child(Input::new(&self.username)))
                .child(field().label("Permission").child(Button::new("helm-modal-permission")
                        .outline()
                        .label(COLLABORATOR_PERMISSIONS[self.permission])
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.permission =
                                (this.permission + 1) % COLLABORATOR_PERMISSIONS.len();
                            cx.notify();
                        }))))
                .into_any_element(),
            HelmModalKind::RemoveCollaborator(login) => v_flex()
                .gap_2()
                .child(div().text_color(foreground).child(format!(
                    "{login} will lose access to this repository immediately."
                )))
                .into_any_element(),
            HelmModalKind::CreatePull(_) => v_form()
                .child(field().label("Title").child(Input::new(&self.name)))
                .child(field().label("Description").child(Input::new(&self.description)))
                .child(field().label("Head branch").child(Input::new(&self.head_branch)))
                .child(field().label("Base branch").child(Input::new(&self.base_branch)))
                .into_any_element(),
            HelmModalKind::EditPull { .. } => v_form()
                .child(field().label("Title").child(Input::new(&self.name)))
                .child(field().label("Description").child(Textarea::new(&self.body)))
                .into_any_element(),
            HelmModalKind::MergePull(plan) => {
                let (method, method_label) = plan.methods[self.merge_method.min(plan.methods.len() - 1)];
                v_form()
                    .child(field().child(div().text_sm().text_color(foreground).child(format!(
                        "Merge pull request #{} into {}? A merge cannot be undone from here.",
                        plan.number, plan.base
                    ))))
                    .children(plan.caution.map(|caution| {
                        field().child(Alert::warning("helm-merge-caution", caution))
                    }))
                    .child(
                        field().label("Method").child(
                            Button::new("helm-merge-method")
                                .outline()
                                .label(method_label)
                                .disabled(plan.methods.len() < 2)
                                .on_click(cx.listener({
                                    let count = plan.methods.len();
                                    move |this, _, _, cx| {
                                        this.merge_method = (this.merge_method + 1) % count;
                                        cx.notify();
                                    }
                                })),
                        ),
                    )
                    // A rebase replays the commits as they are, so there is
                    // no message to write.
                    .when(method != "rebase", |form| {
                        form.child(field().label("Commit title").child(Input::new(&self.name)))
                    })
                    .children(plan.deletable_branch.clone().map(|branch| {
                        field().child(
                            Switch::new("helm-merge-delete-branch")
                                .label(format!("Delete the branch {branch} afterwards"))
                                .checked(self.delete_branch)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.delete_branch = *checked;
                                    cx.notify();
                                })),
                        )
                    }))
                    .into_any_element()
            }
            HelmModalKind::CreateRelease => v_form()
                .child(field().label("Tag").child(Input::new(&self.tag_name)))
                .child(field().label("Title").child(Input::new(&self.name)))
                .child(field().label("Description").child(Input::new(&self.description)))
                .child(
                    field().child(
                        Switch::new("helm-modal-draft")
                        .label("Draft")
                        .checked(self.draft)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.draft = *checked;
                            cx.notify();
                        })),
                    ),
                )
                .child(
                    field().child(
                        Switch::new("helm-modal-prerelease")
                        .label("Pre-release")
                        .checked(self.prerelease)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.prerelease = *checked;
                            cx.notify();
                        })),
                    ),
                )
                .into_any_element(),
            HelmModalKind::EditProfile(_) => v_form()
                .child(field().label("Name").child(Input::new(&self.name)))
                .child(field().label("Bio").child(Input::new(&self.description)))
                .child(field().label("Company").child(Input::new(&self.company)))
                .child(field().label("Location").child(Input::new(&self.location)))
                .child(field().label("Blog / website").child(Input::new(&self.homepage)))
                .into_any_element(),
            HelmModalKind::CommitIdentity { login, .. } => v_form()
                .child(field().child(div().text_sm().text_color(muted).child(format!(
                    "Commits in workspaces that use {login} are authored with this name and \
                     email. It is written into each repository's own git config; your global \
                     git config is not changed."
                ))))
                .child(field().label("Name").child(Input::new(&self.name)))
                .child(field().label("Email").child(Input::new(&self.homepage)))
                .into_any_element(),
            HelmModalKind::CloneRepo => {
                let panel = parent.read(cx);
                if let Some(succeeded_path) = panel.clone_succeeded_path.clone() {
                    let repo_name = panel
                        .selected_repo
                        .as_ref()
                        .map(|repo| repo.name.clone())
                        .unwrap_or_default();
                    v_flex()
                        .gap_3()
                        .items_center()
                        .child(
                            Icon::new(IconName::CircleCheck)
                                .size(px(32.))
                                .text_color(cx.theme().success),
                        )
                        .child(
                            div()
                                .text_color(foreground)
                                .child(format!("You have successfully cloned {repo_name}.")),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child("Would you like to open that workspace?"),
                        )
                        .child(
                            h_flex()
                                .justify_end()
                                .gap_2()
                                .child(
                                    Button::new("helm-clone-open-no")
                                        .outline()
                                        .label("No")
                                        .on_click(cx.listener(|_, _, _, cx| {
                                            cx.emit(DismissEvent);
                                        })),
                                )
                                .child(Button::new("helm-clone-open-yes").primary().label("Yes").on_click({
                                    let parent = parent.clone();
                                    cx.listener(move |_, _, window, cx| {
                                        parent.update(cx, |panel, cx| {
                                            panel.handle_clone_open_workspace(
                                                succeeded_path.clone(),
                                                window,
                                                cx,
                                            );
                                        });
                                        cx.emit(DismissEvent);
                                    })
                                })),
                        )
                        .into_any_element()
                } else if panel.cloning {
                    let recent_lines: Vec<String> = panel
                        .clone_lines
                        .iter()
                        .rev()
                        .take(20)
                        .rev()
                        .cloned()
                        .chain(panel.clone_progress.clone())
                        .collect();
                    let can_cancel = panel.clone_canceller.is_some();
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(Spinner::new().small())
                                .child(div().text_sm().text_color(muted).child("Cloning…")),
                        )
                        .children(recent_lines.into_iter().map(|line| {
                            div()
                                .font_family("Cascadia Mono")
                                .text_xs()
                                .text_color(muted)
                                .child(line)
                        }))
                        .child(
                            h_flex().justify_end().child(
                                Button::new("helm-clone-cancel")
                                    .outline()
                                    .label("Cancel")
                                    .disabled(!can_cancel)
                                    .on_click({
                                        let parent = parent.clone();
                                        cx.listener(move |_, _, _, cx| {
                                            parent.update(cx, |panel, cx| panel.cancel_clone(cx));
                                        })
                                    }),
                            ),
                        )
                        .into_any_element()
                } else if let Some(err) = panel.clone_error.clone() {
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().text_color(cx.theme().danger).child(format!("✗ {err}")))
                        .child(
                            h_flex().justify_end().gap_2().child(
                                Button::new("helm-clone-retry").outline().label("Retry").on_click({
                                    let parent = parent.clone();
                                    cx.listener(move |_, _, window, cx| {
                                        parent.update(cx, |panel, cx| panel.handle_clone(window, cx));
                                    })
                                }),
                            ),
                        )
                        .into_any_element()
                } else {
                    let target = panel.clone_target_path(cx).unwrap_or_default();
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(format!("Clones into {target}")),
                                )
                                .child(
                                    Button::new("helm-clone-choose-dir")
                                        .ghost()
                                        .xsmall()
                                        .label("Choose folder…")
                                        .on_click({
                                            let parent = parent.clone();
                                            cx.listener(move |_, _, _, cx| {
                                                parent.update(cx, |panel, cx| panel.pick_clone_dir(cx));
                                            })
                                        }),
                                ),
                        )
                        .child(
                            Button::new("helm-clone-start")
                                .primary()
                                .icon(IconName::Github)
                                .label("Clone repository")
                                .on_click({
                                    let parent = parent.clone();
                                    cx.listener(move |_, _, window, cx| {
                                        parent.update(cx, |panel, cx| panel.handle_clone(window, cx));
                                    })
                                }),
                        )
                        .into_any_element()
                }
            }
        };

        let confirm = cx.listener(move |this, _, window, cx| {
            let parent = parent.clone();
            let kind = kind.clone();
            let name = this.name.read(cx).value().to_string();
            let description = this.description.read(cx).value().to_string();
            let body = this.body.read(cx).value().to_string();
            let homepage = this.homepage.read(cx).value().to_string();
            let topics = this
                .topics
                .read(cx)
                .value()
                .split(',')
                .map(|topic| topic.trim().to_string())
                .filter(|topic| !topic.is_empty())
                .collect::<Vec<_>>();
            let organization = this.organization.read(cx).value().trim().to_string();
            let username = this.username.read(cx).value().trim().to_string();
            let permission = COLLABORATOR_PERMISSIONS[this.permission].to_string();
            let private = this.private;
            let has_issues = this.has_issues;
            let has_wiki = this.has_wiki;
            let has_projects = this.has_projects;
            let has_discussions = this.has_discussions;
            let tag_name = this.tag_name.read(cx).value().to_string();
            let head_branch = this.head_branch.read(cx).value().trim().to_string();
            let base_branch = this.base_branch.read(cx).value().trim().to_string();
            let company = this.company.read(cx).value().to_string();
            let location = this.location.read(cx).value().to_string();
            let draft = this.draft;
            let prerelease = this.prerelease;
            let merge_method = this.merge_method;
            let delete_branch = this.delete_branch;
            let gitignore = this.gitignore;
            let readme = this.readme;
            let commit_message = this.commit_message.read(cx).value().trim().to_string();

            match kind {
                HelmModalKind::CreateRepo { attach_local } => {
                    let attach = attach_local.then_some(CreateRepoAttach {
                        message: commit_message,
                        readme,
                        gitignore,
                    });
                    parent.update(cx, |this, cx| {
                        this.handle_create_repo(
                            name,
                            description,
                            private,
                            (!organization.is_empty()).then_some(organization),
                            attach,
                            window,
                            cx,
                        );
                    });
                }
                HelmModalKind::EditRepo(_) => {
                    parent.update(cx, |this, cx| {
                        this.handle_edit_repo(
                            name,
                            description,
                            homepage,
                            topics,
                            private,
                            has_issues,
                            has_wiki,
                            has_projects,
                            has_discussions,
                            window,
                            cx,
                        );
                    });
                }
                HelmModalKind::AddCollaborator => {
                    if username.is_empty() {
                        return;
                    }
                    parent.update(cx, |this, cx| {
                        this.handle_set_collaborator_permission(username, permission, window, cx);
                    });
                }
                HelmModalKind::RemoveCollaborator(login) => {
                    parent.update(cx, |this, cx| {
                        this.handle_remove_collaborator(login, window, cx);
                    });
                }
                HelmModalKind::CreatePull(_) => {
                    if head_branch.is_empty() || base_branch.is_empty() || name.is_empty() {
                        return;
                    }
                    parent.update(cx, |this, cx| {
                        this.handle_create_pull(
                            name,
                            description,
                            head_branch,
                            base_branch,
                            window,
                            cx,
                        );
                    });
                }
                HelmModalKind::EditPull { number, .. } => {
                    if name.trim().is_empty() {
                        return;
                    }
                    parent.update(cx, |this, cx| {
                        this.handle_edit_pull(number, name.trim().to_string(), body, cx);
                    });
                }
                HelmModalKind::MergePull(plan) => {
                    let (method, _) = plan.methods[merge_method.min(plan.methods.len() - 1)];
                    parent.update(cx, |this, cx| {
                        this.handle_merge_pull(&plan, method, name.trim().to_string(), delete_branch, cx);
                    });
                }
                HelmModalKind::CreateRelease => {
                    if tag_name.is_empty() {
                        return;
                    }
                    parent.update(cx, |this, cx| {
                        this.handle_create_release(
                            tag_name,
                            name,
                            description,
                            draft,
                            prerelease,
                            window,
                            cx,
                        );
                    });
                }
                HelmModalKind::EditProfile(_) => {
                    parent.update(cx, |this, cx| {
                        this.handle_update_profile(
                            name,
                            description,
                            company,
                            location,
                            homepage,
                            window,
                            cx,
                        );
                    });
                }
                HelmModalKind::CommitIdentity { login, .. } => {
                    let (name, email) = (name.trim().to_string(), homepage.trim().to_string());
                    if name.is_empty() || !email.contains('@') {
                        return;
                    }
                    parent.update(cx, |this, cx| {
                        this.save_commit_identity(login, name, email, cx);
                    });
                }
                // The footer's generic Confirm button is hidden for this kind
                // (see below) — its own content has its own buttons/handlers,
                // each dismissing explicitly where appropriate.
                HelmModalKind::CloneRepo => return,
            }
            cx.emit(DismissEvent);
        });

        v_flex()
            .w(px(520.))
            .max_h(px(720.))
            .gap_3()
            .p_4()
            .bg(cx.theme().background)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_lg()
            .child(div().font_semibold().text_color(foreground).child(title))
            .child(div().text_sm().text_color(muted).child(content))
            .when(show_footer, |el| {
                el.child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("helm-modal-cancel")
                                .outline()
                                .label("Cancel")
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                        )
                        .child(
                            Button::new("helm-modal-confirm")
                                .primary()
                                .label(match confirm_kind {
                                    HelmModalKind::CreateRepo { .. } => "Create",
                                    HelmModalKind::EditRepo(_) => "Save",
                                    HelmModalKind::AddCollaborator => "Add",
                                    HelmModalKind::RemoveCollaborator(_) => "Remove",
                                    HelmModalKind::CreatePull(_) => "Create",
                                    HelmModalKind::EditPull { .. } => "Save",
                                    HelmModalKind::MergePull(_) => "Merge",
                                    HelmModalKind::CreateRelease => "Create",
                                    HelmModalKind::EditProfile(_) => "Save",
                                    HelmModalKind::CommitIdentity { .. } => "Save",
                                    HelmModalKind::CloneRepo => "Clone",
                                })
                                .on_click(confirm),
                        ),
                )
            })
    }
}

//! The Packages page of the Edit solution tab: importing a package from a folder
//! into the solution, with the engine's `inspect_import`, `plan_import` and
//! `apply_import` (engine 0.4.0-alpha.1, `docs/import-packages.md` there).
//!
//! A package is a folder with a `manifest.json` (its `import` section names the
//! shared core and the optional modules), a `package.json` of the packages it
//! needs, and source. The page reads the folder and shows, without writing, what
//! the solution already has of it (a switch per module: off, or on and locked),
//! what each need would do (add, skip, clash...), and then the plan: the files,
//! the lines `package.json` gains, what is kept, and the warnings. Importing
//! writes through the engine's safe writer and keeps one undo record, the same
//! as an edit or an add. The engine never deletes, so a switch that is on cannot
//! be turned off here.
//!
//! Everything that reads the disk runs in `cx.background_spawn`. A result that
//! arrives for an older choice is dropped (`generation`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use db::kvp::KeyValueStore;
use forge_scaffold::{
    AddOutcome, DependencyOutcome, DirSource, HaveFrom, ImportOptions, ImportPlan, ImportState,
    Manifest, ModuleState, ModuleStatus, SpfxSolution, apply_import, inspect_import, plan_import,
};
use gpui::{
    AppContext as _, Context, IntoElement, ParentElement as _, PathPromptOptions, Styled as _,
    Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
    alert::Alert,
    button::{Button, ButtonVariants as _},
    h_flex,
    spinner::Spinner,
    switch::Switch,
    v_flex,
};

use super::{EditSolutionView, undo_key};

/// What was read from the chosen folder.
#[derive(Default)]
pub(super) enum Loaded {
    #[default]
    None,
    Reading,
    Failed(String),
    Ready(Box<Package>),
}

/// A package folder, read.
pub(super) struct Package {
    id: String,
    version: String,
    state: ImportState,
}

/// What the engine says importing the chosen modules would do.
#[derive(Default)]
pub(super) enum Preview {
    #[default]
    Idle,
    Planning,
    Failed(String),
    Ready(Box<ImportPlan>),
}

/// The page's state.
#[derive(Default)]
pub(super) struct PackagesState {
    folder: Option<PathBuf>,
    loaded: Loaded,
    /// The modules switched on in this session. A module that is already in the
    /// solution is shown on and locked, and is not in here.
    chosen: BTreeSet<String>,
    preview: Preview,
    /// Counts the plans asked for; a plan that arrives for an older one is dropped.
    generation: u64,
    /// Show every file of the plan, not the groups.
    show_files: bool,
}

impl EditSolutionView {
    /// Asks for the package folder, then reads it.
    pub(super) fn choose_package(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose Package Folder".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(folder) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| this.open_package(folder, cx)).ok();
            }
        })
        .detach();
    }

    /// Reads the package at `folder` and what the solution already has of it.
    fn open_package(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let Some(solution) = self.solution.clone() else {
            return;
        };
        self.packages.folder = Some(folder.clone());
        self.packages.loaded = Loaded::Reading;
        self.packages.chosen.clear();
        self.packages.preview = Preview::Idle;
        self.packages.generation += 1;
        self.message = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { read_package(&solution, &folder) })
                .await;
            this.update(cx, |this, cx| {
                this.packages.loaded = match result {
                    Ok(package) => Loaded::Ready(Box::new(package)),
                    Err(error) => Loaded::Failed(error),
                };
                this.plan_package(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Switches a module on or off for this import.
    fn set_module(&mut self, name: String, on: bool, cx: &mut Context<Self>) {
        if on {
            self.packages.chosen.insert(name);
        } else {
            self.packages.chosen.remove(&name);
        }
        self.plan_package(cx);
    }

    /// Shows or hides the whole list of files.
    fn toggle_files(&mut self, cx: &mut Context<Self>) {
        self.packages.show_files = !self.packages.show_files;
        cx.notify();
    }

    /// Switches every module that is not in the solution yet on, or all off.
    fn set_all_modules(&mut self, on: bool, cx: &mut Context<Self>) {
        if on {
            if let Loaded::Ready(package) = &self.packages.loaded {
                for module in &package.state.modules {
                    if !matches!(module.state, ModuleStatus::Present | ModuleStatus::Modified) {
                        self.packages.chosen.insert(module.name.clone());
                    }
                }
            }
        } else {
            self.packages.chosen.clear();
        }
        self.plan_package(cx);
    }

    /// The `npm install` that would supply what the solution lacks, and the names
    /// that cannot be installed from here. An SPFx package (`@microsoft/...`) is
    /// asked for at the solution's own SPFx version, exactly, since they all move
    /// together; a missing peer at the range the package asks for. A name or a
    /// version that is not plain is never put in a command: the package comes from
    /// a folder the user chose, and what it says is not run as it stands.
    fn missing_packages_command(&self, package: &Package) -> (Option<String>, Vec<String>) {
        let version = self
            .solution
            .as_ref()
            .and_then(|solution| solution.version.clone())
            .filter(|version| safe_range(version));
        let (mut spfx, mut peers, mut by_hand) = (Vec::new(), Vec::new(), Vec::new());
        for outcome in &package.state.dependencies {
            match outcome {
                DependencyOutcome::MissingRequired { name, category, .. } => {
                    match (&version, category.as_str()) {
                        (Some(version), "spfx")
                            if name.starts_with("@microsoft/") && safe_package_name(name) =>
                        {
                            spfx.push(format!("\"{name}@{version}\""))
                        }
                        _ => by_hand.push(name.clone()),
                    }
                }
                DependencyOutcome::MissingPeer { name, range } => {
                    if safe_package_name(name) && safe_range(range) {
                        peers.push(format!("\"{name}@{range}\""));
                    } else {
                        by_hand.push(name.clone());
                    }
                }
                _ => {}
            }
        }
        let mut parts = Vec::new();
        if !spfx.is_empty() {
            parts.push(format!(
                "npm install {} --save-exact --loglevel=http",
                spfx.join(" ")
            ));
        }
        if !peers.is_empty() {
            parts.push(format!("npm install {} --loglevel=http", peers.join(" ")));
        }
        ((!parts.is_empty()).then(|| parts.join(" && ")), by_hand)
    }

    /// Installs what the solution lacks, in the Script Runner behind the trust gate.
    fn install_missing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Loaded::Ready(package) = &self.packages.loaded else {
            return;
        };
        let (Some(command), _) = self.missing_packages_command(package) else {
            return;
        };
        let root = self
            .solution
            .as_ref()
            .map(|solution| solution.root.clone())
            .unwrap_or_else(|| self.root.clone());
        npm_bootstrap::run_install_command(&self.workspace, root, command, window, cx);
    }

    /// Asks the engine what importing the chosen modules would do, off the main
    /// thread. Not while a need blocks the import: there is nothing to plan.
    fn plan_package(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let (Some(solution), Some(folder)) =
            (self.solution.clone(), self.packages.folder.clone())
        else {
            return;
        };
        let Loaded::Ready(package) = &self.packages.loaded else {
            return;
        };
        self.packages.generation += 1;
        if package.state.dependencies.iter().any(DependencyOutcome::blocks) {
            self.packages.preview = Preview::Idle;
            cx.notify();
            return;
        }
        // The shared core comes with the first module: with none chosen there is
        // nothing to import, and a list of the core's files would look like all of it.
        if !package.state.modules.is_empty() && self.packages.chosen.is_empty() {
            self.packages.preview = Preview::Idle;
            cx.notify();
            return;
        }
        let generation = self.packages.generation;
        let modules: Vec<String> = self.packages.chosen.iter().cloned().collect();
        self.packages.preview = Preview::Planning;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let manifest = read_manifest(&folder)?;
                    let mut options = ImportOptions::new();
                    options.modules = modules;
                    plan_import(&solution, &DirSource::new(&folder), &manifest, options)
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                if this.packages.generation != generation {
                    return;
                }
                this.packages.preview = match result {
                    Ok(plan) => Preview::Ready(Box::new(plan)),
                    Err(error) => Preview::Failed(error),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Writes the plan, and keeps the undo record once the files are on disk.
    pub(super) fn apply_package(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Preview::Ready(plan) = &self.packages.preview else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        let plan = (**plan).clone();
        let name = match &self.packages.loaded {
            Loaded::Ready(package) => package.id.clone(),
            _ => "package".to_string(),
        };
        // Made before the write: which folders an import creates can only be told
        // while they are not there yet.
        let record = plan.undo_record(&format!("Imported {name}"));
        let packages_changed = plan.add.files.iter().any(|file| file.path == "package.json");
        let notes = plan.notes.clone();
        let clashes = plan.has_clashes();
        let folder = self.packages.folder.clone();
        self.busy = Some("Importing the package…");
        self.message = None;
        cx.notify();
        let key = undo_key(&self.root);
        let kvp = KeyValueStore::global(cx);
        let stored = record.to_json();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    apply_import(&plan, |_| {}).map_err(|error| error.to_string())?;
                    kvp.write_kvp(key, stored).await.ok();
                    Ok::<_, String>(())
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(()) => {
                        this.undo = Some(record);
                        this.add.needs_install = packages_changed;
                        let mut text = format!("Imported {name}.");
                        if clashes {
                            text.push_str(
                                " Some packages differ from the ones the solution has: see the list, and settle them by hand.",
                            );
                        }
                        if packages_changed {
                            text.push_str(" package.json changed: run npm install before you build.");
                        }
                        if !notes.is_empty() {
                            text.push(' ');
                            text.push_str(&notes.join(" "));
                        }
                        this.message = Some(Ok(text));
                        // Shows what the solution has now, and the new components.
                        this.load(false, window, cx);
                        if let Some(folder) = folder {
                            this.open_package(folder, cx);
                        }
                    }
                    Err(error) => this.message = Some(Err(error)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The Packages page.
    pub(super) fn render_packages(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let (muted, danger, success, warning, border) = (
            theme.muted_foreground,
            theme.danger,
            theme.success,
            theme.warning,
            theme.border,
        );

        let picker = h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new("scaffold-pkg-choose")
                    .outline()
                    .small()
                    .label("Choose package folder…")
                    .disabled(self.busy.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.choose_package(cx))),
            )
            .children(self.packages.folder.as_ref().map(|folder| {
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .child(folder.display().to_string())
            }));

        let intro = div().text_xs().text_color(muted).child(
            "A package is a folder with a manifest.json, a package.json of what it needs, and its \
             source. Importing copies its files into this solution and adds what it needs to \
             package.json. Nothing is changed or deleted that the solution already has.",
        );

        let body = match &self.packages.loaded {
            Loaded::None => div()
                .text_sm()
                .text_color(muted)
                .child("Choose a package folder to see what importing it would do.")
                .into_any_element(),
            Loaded::Reading => h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small())
                .child(div().text_sm().text_color(muted).child("Reading the package…"))
                .into_any_element(),
            Loaded::Failed(error) => {
                Alert::error("scaffold-pkg-failed", error.clone()).into_any_element()
            }
            Loaded::Ready(package) => {
                self.render_package(package, muted, danger, success, warning, border, cx)
            }
        };

        v_flex()
            .gap_3()
            .child(intro)
            .child(picker)
            .child(body)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_package(
        &self,
        package: &Package,
        muted: gpui::Hsla,
        danger: gpui::Hsla,
        success: gpui::Hsla,
        warning: gpui::Hsla,
        border: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // The needs: from the plan once there is one, else as the package stands.
        let planned = match &self.packages.preview {
            Preview::Ready(plan) => Some(plan.as_ref()),
            _ => None,
        };
        let dependencies: &[DependencyOutcome] = planned
            .map(|plan| plan.dependencies.as_slice())
            .unwrap_or(package.state.dependencies.as_slice());
        let blockers: Vec<String> = package
            .state
            .dependencies
            .iter()
            .filter(|outcome| outcome.blocks())
            .map(|outcome| outcome.name().to_string())
            .collect();

        let chosen_count = self.packages.chosen.len();
        let selectable = package.state.modules.iter().any(|module| {
            !matches!(module.state, ModuleStatus::Present | ModuleStatus::Modified)
        });
        let modules = v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().font_semibold().child(format!(
                        "Modules ({chosen_count} of {} chosen)",
                        package.state.modules.len()
                    )))
                    .child(
                        Button::new("scaffold-pkg-all")
                            .ghost()
                            .xsmall()
                            .label("Select all")
                            .disabled(self.busy.is_some() || !selectable)
                            .on_click(cx.listener(|this, _, _, cx| this.set_all_modules(true, cx))),
                    )
                    .child(
                        Button::new("scaffold-pkg-none")
                            .ghost()
                            .xsmall()
                            .label("Clear")
                            .disabled(self.busy.is_some() || chosen_count == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.set_all_modules(false, cx))),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("Shared core: {}", status_text(&package.state.core))),
            )
            // A grid of switches that wraps, a few to a row, so a package with many
            // modules does not make a long column.
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_3()
                    .children(package.state.modules.iter().enumerate().map(|(index, module)| {
                        let locked = matches!(
                            module.state,
                            ModuleStatus::Present | ModuleStatus::Modified
                        );
                        let on = locked || self.packages.chosen.contains(&module.name);
                        let name = module.name.clone();
                        // Only a module that is not simply absent gets a word beside it.
                        let badge = match module.state {
                            ModuleStatus::Present => Some("imported".to_string()),
                            ModuleStatus::Modified => Some("changed".to_string()),
                            ModuleStatus::Partial => {
                                Some(format!("{} files missing", module.missing.len()))
                            }
                            _ => None,
                        };
                        h_flex()
                            .w(px(240.))
                            .gap_1()
                            .items_center()
                            .child(
                                Switch::new(("scaffold-pkg-module", index))
                                    .label(module.title.clone())
                                    .checked(on)
                                    .disabled(locked || self.busy.is_some())
                                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                        this.set_module(name.clone(), *checked, cx)
                                    })),
                            )
                            .children(
                                badge.map(|text| div().text_xs().text_color(muted).child(text)),
                            )
                    })),
            );

        let needs = v_flex()
            .gap_0p5()
            .child(div().font_semibold().child("What it needs"))
            .children(if dependencies.is_empty() {
                vec![
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("It needs no packages.")
                        .into_any_element(),
                ]
            } else {
                dependencies
                    .iter()
                    .map(|outcome| describe(outcome, muted, danger, success, warning))
                    .collect()
            });

        let (install_command, by_hand) = self.missing_packages_command(package);
        let blocked = (!blockers.is_empty()).then(|| {
            v_flex()
                .gap_2()
                .child(Alert::error(
                    "scaffold-pkg-blocked",
                    format!(
                        "The solution must have {} before this can be imported. Nothing was changed.",
                        blockers.join(", ")
                    ),
                ))
                .children((!by_hand.is_empty()).then(|| {
                    div().text_xs().text_color(muted).child(format!(
                        "These cannot be installed from here, add them yourself: {}.",
                        by_hand.join(", ")
                    ))
                }))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .children(install_command.is_some().then(|| {
                            Button::new("scaffold-pkg-install-missing")
                                .primary()
                                .small()
                                .label("Install the missing packages")
                                .disabled(self.busy.is_some())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.install_missing(window, cx)
                                }))
                        }))
                        .child(
                            Button::new("scaffold-pkg-recheck")
                                .outline()
                                .small()
                                .label("Check again")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(folder) = this.packages.folder.clone() {
                                        this.open_package(folder, cx);
                                    }
                                })),
                        ),
                )
        });

        let preview = match &self.packages.preview {
            Preview::Idle if !blockers.is_empty() => div().into_any_element(),
            Preview::Idle => div()
                .text_sm()
                .text_color(muted)
                .child("Choose the modules you want to import. The shared core comes with them.")
                .into_any_element(),
            Preview::Planning => h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small())
                .child(div().text_sm().text_color(muted).child("Checking…"))
                .into_any_element(),
            Preview::Failed(error) => {
                Alert::error("scaffold-pkg-plan-failed", error.clone()).into_any_element()
            }
            Preview::Ready(plan) => {
                self.render_plan(plan, muted, danger, success, warning, border, cx)
            }
        };

        let can_import = matches!(&self.packages.preview, Preview::Ready(plan) if !plan.add.is_empty())
            && self.busy.is_none();

        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .child(format!("{} {}", package.id, package.version)),
            )
            .child(modules)
            .child(needs)
            .children(blocked)
            .child(div().font_semibold().child("Changes"))
            .child(preview)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(
                        Button::new("scaffold-pkg-import")
                            .primary()
                            .label("Import package")
                            .disabled(!can_import)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.apply_package(window, cx)),
                            ),
                    )
                    .children(self.add.needs_install.then(|| {
                        Button::new("scaffold-pkg-install")
                            .outline()
                            .label("Run npm install")
                            .disabled(self.busy.is_some())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.run_install(window, cx)),
                            )
                    }))
                    .children(self.undo_button(cx))
                    .children(self.busy_indicator(muted)),
            )
            .into_any_element()
    }

    /// What an import would write, in groups (the files of one folder as one line),
    /// the lines `package.json` gains, what is kept, and the warnings. The whole list
    /// of files is one click away.
    #[allow(clippy::too_many_arguments)]
    fn render_plan(
        &self,
        plan: &ImportPlan,
        muted: gpui::Hsla,
        danger: gpui::Hsla,
        success: gpui::Hsla,
        warning: gpui::Hsla,
        border: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if plan.add.is_empty() {
            return div()
                .text_sm()
                .text_color(muted)
                .child("Nothing to add: everything the chosen modules need is already there.")
                .into_any_element();
        }
        let new_paths: Vec<&str> = plan
            .add
            .files
            .iter()
            .filter(|file| file.outcome == AddOutcome::New)
            .map(|file| file.path.as_str())
            .collect();
        let merged: Vec<&forge_scaffold::AddFile> = plan
            .add
            .files
            .iter()
            .filter(|file| file.outcome != AddOutcome::New)
            .collect();
        let prefix = common_dir(&new_paths);
        let groups = group_paths(&new_paths, &prefix);
        let show_files = self.packages.show_files;

        let summary = if prefix.is_empty() {
            format!("{} new files, {} changed", new_paths.len(), merged.len())
        } else {
            format!(
                "{} new files into {prefix}, {} changed",
                new_paths.len(),
                merged.len()
            )
        };

        // The new files: a line per group, or every file relative to the folder.
        let new_files = if show_files {
            v_flex()
                .gap_0p5()
                .children(new_paths.iter().map(|path| {
                    let relative = path.strip_prefix(prefix.as_str()).unwrap_or(*path);
                    h_flex()
                        .min_w_0()
                        .gap_2()
                        .child(div().text_sm().text_color(success).child("+"))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .font_family("Cascadia Mono")
                                .child(relative.to_string()),
                        )
                }))
                .into_any_element()
        } else {
            v_flex()
                .gap_0p5()
                .children(groups.iter().map(|(name, count)| {
                    h_flex()
                        .min_w_0()
                        .gap_2()
                        .child(div().text_sm().text_color(success).child("+"))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .font_family("Cascadia Mono")
                                .child(name.clone()),
                        )
                        .children((*count > 1).then(|| {
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("{count} files"))
                        }))
                }))
                .into_any_element()
        };

        v_flex()
            .gap_2()
            .p_2()
            .min_w_0()
            .rounded_md()
            .border_1()
            .border_color(border)
            .child(div().text_xs().text_color(muted).child(summary))
            // The files that already existed and gain lines (package.json).
            .children(merged.iter().map(|file| {
                let added = added_lines(file.previous.as_deref(), &file.content);
                v_flex()
                    .gap_0p5()
                    .min_w_0()
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .child(div().text_sm().text_color(warning).child("~"))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_family("Cascadia Mono")
                                    .child(file.path.clone()),
                            ),
                    )
                    .children(added.into_iter().take(12).map(|line| {
                        div()
                            .pl_4()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .font_family("Cascadia Mono")
                            .text_color(success)
                            .child(format!("+ {line}"))
                    }))
            }))
            .child(new_files)
            .children((new_paths.len() > groups.len()).then(|| {
                Button::new("scaffold-pkg-files")
                    .ghost()
                    .xsmall()
                    .label(if show_files {
                        "Show the groups".to_string()
                    } else {
                        format!("Show all {} files", new_paths.len())
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_files(cx)))
            }))
            .children((!plan.add.kept.is_empty()).then(|| {
                div().text_xs().text_color(muted).child(format!(
                    "Left as they are (the solution's own differ): {}",
                    plan.add.kept.join(", ")
                ))
            }))
            .children(
                plan.add
                    .warnings
                    .iter()
                    .map(|text| div().text_xs().text_color(warning).child(text.clone())),
            )
            .children(plan.has_clashes().then(|| {
                div().text_xs().text_color(danger).child(
                    "Some packages differ from the versions the solution has. Their files are \
                     imported and your versions are not changed: settle them by hand.",
                )
            }))
            .into_any_element()
    }
}

/// The directory every path shares, with a trailing slash (`src/services/x/`), or
/// nothing when they share none.
fn common_dir(paths: &[&str]) -> String {
    let mut iter = paths.iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    let mut prefix: Vec<&str> = first.split('/').collect();
    prefix.pop(); // the file name
    for path in iter {
        let parts: Vec<&str> = path.split('/').collect();
        let dirs = &parts[..parts.len().saturating_sub(1)];
        let common = prefix
            .iter()
            .zip(dirs.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(common);
    }
    if prefix.is_empty() {
        String::new()
    } else {
        format!("{}/", prefix.join("/"))
    }
}

/// The paths in groups, `(name, files)`: the first folder below `prefix`, or the
/// file itself when it is directly in it. In the order they first appear.
fn group_paths(paths: &[&str], prefix: &str) -> Vec<(String, usize)> {
    let mut groups: Vec<(String, usize)> = Vec::new();
    for &path in paths {
        let rest = path.strip_prefix(prefix).unwrap_or(path);
        let name = match rest.split_once('/') {
            Some((first, _)) => first.to_string(),
            None => rest.to_string(),
        };
        match groups.iter_mut().find(|(known, _)| *known == name) {
            Some((_, count)) => *count += 1,
            None => groups.push((name, 1)),
        }
    }
    groups
}

/// Whether `name` is a plain npm package name (`lodash`, `@scope/name`): lower
/// case letters, digits and `- . _ ~`, with an optional scope. Nothing else is put
/// in a command.
fn safe_package_name(name: &str) -> bool {
    fn part(text: &str) -> bool {
        !text.is_empty()
            && !text.starts_with('.')
            && text.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_' | '~')
            })
    }
    match name.strip_prefix('@') {
        Some(scoped) => match scoped.split_once('/') {
            Some((scope, package)) => part(scope) && part(package),
            None => false,
        },
        None => part(name),
    }
}

/// Whether `range` is a plain version or range (`1.24.1`, `^4.23.0`, `~2`): letters,
/// digits and `^ ~ . - + *` only. A range with a space, a comparison or an `||` is
/// not plain, so it is not put in a command.
fn safe_range(range: &str) -> bool {
    !range.is_empty()
        && range.len() <= 40
        && range
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '^' | '~' | '.' | '-' | '+' | '*'))
}

/// Reads `manifest.json` of a package folder. It must have an `import` section.
fn read_manifest(folder: &Path) -> Result<Manifest, String> {
    let text = std::fs::read_to_string(folder.join("manifest.json"))
        .map_err(|error| format!("Could not read manifest.json in this folder: {error}"))?;
    let manifest = Manifest::from_json(&text).map_err(|error| error.to_string())?;
    if manifest.import.is_none() {
        return Err(
            "This folder's manifest.json has no \"import\" section, so it is not a package."
                .to_string(),
        );
    }
    Ok(manifest)
}

/// Reads a package folder and what the solution has of it. Blocks on the disk:
/// runs off the main thread.
fn read_package(solution: &SpfxSolution, folder: &Path) -> Result<Package, String> {
    let manifest = read_manifest(folder)?;
    let state = inspect_import(solution, &DirSource::new(folder), &manifest)
        .map_err(|error| error.to_string())?;
    Ok(Package {
        id: manifest.id.clone(),
        version: manifest.version.clone(),
        state,
    })
}

/// What the solution has of a module or of the shared core, in a few words.
fn status_text(module: &ModuleState) -> String {
    match module.state {
        ModuleStatus::Absent => "not in the solution yet".to_string(),
        ModuleStatus::Present => "already imported".to_string(),
        ModuleStatus::Modified => "imported, and changed since".to_string(),
        ModuleStatus::Partial => format!(
            "{} of its files are missing; switching it on adds them",
            module.missing.len()
        ),
        _ => String::new(),
    }
}

/// One need of the package, in a sentence and the colour that says how it is.
fn describe(
    outcome: &DependencyOutcome,
    muted: gpui::Hsla,
    danger: gpui::Hsla,
    success: gpui::Hsla,
    warning: gpui::Hsla,
) -> gpui::AnyElement {
    let (text, color) = match outcome {
        DependencyOutcome::Add { name, range } => (
            format!("+ {name} {range}: will be added to package.json"),
            success,
        ),
        DependencyOutcome::Skip {
            name,
            have,
            want,
            have_from,
        } => {
            let from = match have_from {
                HaveFrom::NodeModules => "installed",
                HaveFrom::Lock => "from the lock file",
                HaveFrom::Declared => "from the declared range",
                _ => "",
            };
            (
                format!("{name}: you have {have} ({from}), the package wants {want}: nothing to do"),
                muted,
            )
        }
        DependencyOutcome::UpgradeAvailable { name, from, to } => (
            format!(
                "{name}: you have {from}, the package wants {to}. Not changed: upgrade it by hand if you need it"
            ),
            warning,
        ),
        DependencyOutcome::Clash {
            name,
            have,
            want,
            reason,
        } => (
            format!("{name}: you have {have}, the package wants {want}: {reason}"),
            warning,
        ),
        DependencyOutcome::Untouched { name, category } => (
            format!("{name}: supplied by the solution ({category})"),
            muted,
        ),
        DependencyOutcome::MissingRequired {
            name,
            range,
            category,
        } => (
            format!("{name} {range} is required and the solution has no {name} ({category})"),
            danger,
        ),
        DependencyOutcome::MissingPeer { name, range } => (
            format!("{name} {range} is a peer the solution does not have"),
            danger,
        ),
        other => (other.name().to_string(), muted),
    };
    div()
        .text_xs()
        .text_color(color)
        .child(text)
        .into_any_element()
}

/// The lines `content` has that `previous` did not: what a merge into an existing
/// file added. Compared by trimmed text and counted, so a repeated line counts
/// once for each time it is new.
fn added_lines(previous: Option<&[u8]>, content: &[u8]) -> Vec<String> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    // A trailing comma is ignored: the line above a new entry gains one, and is not new.
    let normal = |line: &str| line.trim().trim_end_matches(',').to_string();
    let mut remaining: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(previous).lines() {
        *remaining.entry(normal(line)).or_default() += 1;
    }
    let mut added = Vec::new();
    for line in String::from_utf8_lossy(content).lines() {
        let line = normal(line);
        match remaining.get_mut(&line) {
            Some(count) if *count > 0 => *count -= 1,
            _ => {
                if !line.is_empty() {
                    added.push(line);
                }
            }
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_merge_shows_only_the_lines_it_added() {
        let before = b"{\n  \"dependencies\": {\n    \"a\": \"^1.0.0\"\n  }\n}\n";
        let after = b"{\n  \"dependencies\": {\n    \"a\": \"^1.0.0\",\n    \"b\": \"^2.0.0\"\n  }\n}\n";
        // The comma the line above gains is not a new line: only `b` is.
        let added = added_lines(Some(before), after);
        assert_eq!(added, vec!["\"b\": \"^2.0.0\"".to_string()]);
    }

    #[test]
    fn only_plain_package_names_and_ranges_go_into_a_command() {
        assert!(safe_package_name("lodash"));
        assert!(safe_package_name("@pnp/sp"));
        assert!(safe_package_name("@microsoft/sp-http"));
        assert!(!safe_package_name("@pnp"));
        assert!(!safe_package_name("a b"));
        assert!(!safe_package_name("x\" && calc && \""));
        assert!(!safe_package_name("Upper"));
        assert!(!safe_package_name(""));
        assert!(safe_range("1.24.1"));
        assert!(safe_range("^4.23.0"));
        assert!(safe_range("~2"));
        assert!(!safe_range(">=4 <6"));
        assert!(!safe_range("1.0.0; calc"));
        assert!(!safe_range("$(x)"));
        assert!(!safe_range("%PATH%"));
        assert!(!safe_range(""));
    }

    #[test]
    fn the_files_share_a_folder_and_group_by_the_next_one() {
        let paths = [
            "src/services/x/Types/A/a.ts",
            "src/services/x/Types/A/index.ts",
            "src/services/x/Types/B/b.ts",
            "src/services/x/ctx.ts",
            "src/services/x/Items/get.ts",
        ];
        let prefix = common_dir(&paths);
        assert_eq!(prefix, "src/services/x/");
        assert_eq!(
            group_paths(&paths, &prefix),
            vec![
                ("Types".to_string(), 3),
                ("ctx.ts".to_string(), 1),
                ("Items".to_string(), 1)
            ]
        );
        assert_eq!(common_dir(&[]), "");
        assert_eq!(common_dir(&["a.ts", "b/c.ts"]), "");
    }

    #[test]
    fn a_file_that_was_not_there_has_no_added_lines() {
        assert!(added_lines(None, b"line\n").is_empty());
    }

    #[test]
    fn a_repeated_line_counts_each_time_it_is_new() {
        let added = added_lines(Some(b"x\n"), b"x\nx\nx\n");
        assert_eq!(added, vec!["x".to_string(), "x".to_string()]);
    }
}

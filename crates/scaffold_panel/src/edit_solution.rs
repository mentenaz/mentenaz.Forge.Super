//! Editing an existing SPFx solution, in a workspace tab: phase 7 of
//! `docs/scaffold-panel-plan.md`. The Node panel's gear on an SPFx solution opens
//! it (`zed_actions::scaffold::EditSolution`). Two pages:
//!
//! * **Details**: the name, description and version in
//!   `config/package-solution.json`.
//! * **Components**: what is in the solution, and adding web parts to it.
//!
//! The engine does the work: `plan_edit` / `plan_add` check the values and build
//! the result as data, `apply_edit` / `apply_add` write it safely, and
//! `revert_edit` puts it back. The tab plans again after every keystroke (off the
//! main thread), so the checks and the preview are always for what is typed. The
//! undo record is kept in the key-value store, keyed by the solution folder, so
//! Undo still works after a restart for as long as the files are as the change
//! left them. Only the last change of a solution is kept, an edit or an add.
//!
//! Nothing is ever deleted by an add, and removing a component is not something
//! the engine does: Undo takes the last add back, nothing more.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use db::kvp::KeyValueStore;
use forge_scaffold::{
    AddMode, AddOptions, AddOutcome, AddPlan, Answers, ComponentInfo, ComponentKind, DirSource,
    ScaffoldError, SolutionEdit, SolutionInfo, SpfxSolution, TemplateStore, UndoRecord,
    UpstreamInputs, ValidationIssue, apply_add, apply_edit, detect_spfx, plan_add, plan_edit,
    revert_edit, solution_info,
};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, TaskExt as _, WeakEntity, Window,
    div, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, StyledExt as _,
    alert::Alert,
    button::{Button, ButtonVariants as _},
    form::field,
    h_flex,
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    tab::{Tab, TabBar, TabVariant},
    v_flex,
};
use http_client::HttpClient;
use serde_json::{Value, json};
use workspace::{OpenOptions, Workspace, item::Item};

use crate::{HttpFetch, templates_dir};

/// The solution file that holds the three values.
const SOLUTION_FILE: &str = "config/package-solution.json";

/// The templates that add a web part, in the order they are offered. Others
/// (extensions, cards, libraries) come later.
const WEB_PART_TEMPLATES: [&str; 4] = [
    "spfx-webpart",
    "webpart-react",
    "webpart-minimal",
    "webpart-noframework",
];

/// The values as they are in the file.
#[derive(Clone, Default)]
struct Original {
    name: String,
    /// `None` when the file's description could not be read (the file has
    /// comments, say): the box then starts empty, and is only sent when typed in.
    description: Option<String>,
    version: String,
}

enum Status {
    Loading,
    Failed(String),
    Ready,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Details,
    Components,
}

/// What the engine says about the values as typed.
enum Preview {
    /// Nothing is different from the file.
    Unchanged,
    Planning,
    /// The engine refused a value: one message per field.
    Issues(Vec<ValidationIssue>),
    Changes(AddPlan, Vec<Change>),
    Failed(String),
}

/// One line that changes, for the preview.
struct Change {
    file: String,
    from: String,
    to: String,
}

/// A template that can add a web part to this solution.
#[derive(Clone)]
struct Candidate {
    generator: String,
    title: String,
    version: String,
    installed: bool,
}

enum Candidates {
    Loading,
    /// Why there is none: the version is too old, or could not be read.
    None(String),
    Ready(Vec<Candidate>),
}

/// What the engine says about the names typed on the Components page.
enum AddPreview {
    /// No name typed yet.
    Idle,
    Planning,
    Issues(Vec<String>),
    Failed(String),
    Ready(Box<AddPlan>),
}

/// The Components page.
struct AddState {
    existing: Vec<ComponentInfo>,
    candidates: Candidates,
    chosen: usize,
    rows: Vec<Entity<InputState>>,
    preview: AddPreview,
    /// Counts the plans asked for; a plan that arrives for an older one is dropped.
    generation: u64,
    downloading: bool,
    /// The web part's main file after an add, for "Open web part".
    added_file: Option<PathBuf>,
    /// The last add changed `package.json`, so "Run npm install" is offered.
    needs_install: bool,
}

pub struct EditSolutionView {
    root: PathBuf,
    workspace: WeakEntity<Workspace>,
    http: Arc<dyn HttpClient>,
    focus_handle: FocusHandle,
    status: Status,
    page: Page,
    /// What the engine detected; the plans are made against it.
    solution: Option<SpfxSolution>,
    /// The name SharePoint shows, for the tab and the undo label.
    title: String,
    original: Original,
    name: Entity<InputState>,
    description: Entity<InputState>,
    version: Entity<InputState>,
    preview: Preview,
    /// Counts the plans asked for; a plan that arrives for an older one is dropped.
    generation: u64,
    add: AddState,
    /// What is running ("Applying the changes…"), while something is.
    busy: Option<&'static str>,
    /// The result of the last apply or undo.
    message: Option<Result<String, String>>,
    /// The last change of this solution, which Undo reverts.
    undo: Option<UndoRecord>,
    _subscriptions: Vec<Subscription>,
}

impl EditSolutionView {
    pub fn new(
        root: PathBuf,
        workspace: WeakEntity<Workspace>,
        http: Arc<dyn HttpClient>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Solution name"));
        let description = cx.new(|cx| InputState::new(window, cx).placeholder("Description"));
        let version = cx.new(|cx| InputState::new(window, cx).placeholder("1.0.0.0"));
        let first_row = cx.new(|cx| InputState::new(window, cx).placeholder("Web part name"));

        let mut subscriptions = Vec::new();
        for input in [&name, &description, &version] {
            subscriptions.push(cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.plan_changes(cx);
                }
            }));
        }
        subscriptions.push(cx.subscribe(&first_row, |this, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                this.plan_components(cx);
            }
        }));

        let mut view = Self {
            root,
            workspace,
            http,
            focus_handle: cx.focus_handle(),
            status: Status::Loading,
            page: Page::Details,
            solution: None,
            title: String::new(),
            original: Original::default(),
            name,
            description,
            version,
            preview: Preview::Unchanged,
            generation: 0,
            add: AddState {
                existing: Vec::new(),
                candidates: Candidates::Loading,
                chosen: 0,
                rows: vec![first_row],
                preview: AddPreview::Idle,
                generation: 0,
                downloading: false,
                added_file: None,
                needs_install: false,
            },
            busy: None,
            message: None,
            undo: None,
            _subscriptions: subscriptions,
        };
        view.load(true, window, cx);
        view
    }

    /// Reads the solution, the stored undo record and the templates that fit it,
    /// off the main thread. `refill` also fills the three boxes of the Details
    /// page from the file (when the tab opens and after an undo); an add leaves
    /// them alone, so values typed there and not yet applied are not lost.
    fn load(&mut self, refill: bool, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let key = undo_key(&root);
        let kvp = KeyValueStore::global(cx);
        cx.spawn_in(window, async move |this, cx| {
            let solution = cx
                .background_spawn(async move { read_solution(&root) })
                .await;
            let stored = cx
                .background_spawn(async move { kvp.read_kvp(&key) })
                .await
                .ok()
                .flatten()
                .and_then(|text| UndoRecord::from_json(&text).ok());
            // The templates that fit the solution's SPFx version.
            let candidates = match &solution {
                Ok((solution, _, _)) => {
                    let version = solution.version.clone();
                    cx.background_spawn(async move { find_candidates(version) })
                        .await
                }
                Err(_) => Candidates::Loading,
            };
            this.update_in(cx, |this, window, cx| {
                match solution {
                    Ok((solution, info, description)) => {
                        this.title = info
                            .solution_name
                            .clone()
                            .or(info.package_name.clone())
                            .unwrap_or_default();
                        this.add.existing = info.components.clone();
                        this.add.chosen = best_candidate(&candidates, solution.framework.as_deref());
                        this.add.candidates = candidates;
                        this.solution = Some(solution);
                        this.undo = stored;
                        this.status = Status::Ready;
                        if refill {
                            this.original = Original {
                                name: info.solution_name.unwrap_or_default(),
                                description,
                                version: info.solution_version.unwrap_or_default(),
                            };
                            this.preview = Preview::Unchanged;
                            // The original is in place before the boxes change, so
                            // the change events they send find nothing to plan.
                            let original = this.original.clone();
                            this.name.update(cx, |input, cx| {
                                input.set_value(original.name.clone(), window, cx);
                            });
                            this.description.update(cx, |input, cx| {
                                input.set_value(
                                    original.description.clone().unwrap_or_default(),
                                    window,
                                    cx,
                                );
                            });
                            this.version.update(cx, |input, cx| {
                                input.set_value(original.version.clone(), window, cx);
                            });
                        }
                    }
                    Err(error) => this.status = Status::Failed(error),
                }
                this.busy = None;
                this.plan_components(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ── Details ────────────────────────────────────────────────────────────

    /// What the three boxes hold now.
    fn current_values(&self, cx: &App) -> Original {
        Original {
            name: self.name.read(cx).value().to_string(),
            description: Some(self.description.read(cx).value().to_string()),
            version: self.version.read(cx).value().to_string(),
        }
    }

    /// The values that differ from the file: a box left as it was is not sent.
    fn current_edit(&self, cx: &App) -> SolutionEdit {
        let now = self.current_values(cx);
        let description = now.description.unwrap_or_default();
        SolutionEdit {
            name: (now.name != self.original.name).then_some(now.name),
            description: (description != self.original.description.clone().unwrap_or_default())
                .then_some(description),
            version: (now.version != self.original.version).then_some(now.version),
        }
    }

    /// Asks the engine what the typed values would do, off the main thread.
    fn plan_changes(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.status, Status::Ready) || self.busy.is_some() {
            return;
        }
        let Some(solution) = self.solution.clone() else {
            return;
        };
        let edit = self.current_edit(cx);
        self.generation += 1;
        if edit == SolutionEdit::default() {
            self.preview = Preview::Unchanged;
            cx.notify();
            return;
        }
        let generation = self.generation;
        self.preview = Preview::Planning;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { plan_edit(&solution, &edit) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.preview = match result {
                    Ok(plan) if plan.is_empty() => Preview::Unchanged,
                    Ok(plan) => {
                        let changes = changed_lines(&plan);
                        Preview::Changes(plan, changes)
                    }
                    Err(ScaffoldError::Validation(issues)) => Preview::Issues(issues),
                    Err(error) => Preview::Failed(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Writes the planned changes, and keeps the undo record once they are on disk.
    fn apply(&mut self, cx: &mut Context<Self>) {
        let Preview::Changes(plan, _) = &self.preview else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        let plan = plan.clone();
        // Made before the write: which folders a change creates can only be told
        // while they are not there yet.
        let record = plan.undo_record_as("edit", &format!("Edited solution {}", self.title));
        self.busy = Some("Applying the changes…");
        self.message = None;
        cx.notify();
        let key = undo_key(&self.root);
        let kvp = KeyValueStore::global(cx);
        let stored = record.to_json();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    apply_edit(&plan, |_| {}).map_err(|error| error.to_string())?;
                    // Only for a write that worked.
                    kvp.write_kvp(key, stored).await.ok();
                    Ok::<_, String>(())
                })
                .await;
            this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(()) => {
                        this.undo = Some(record);
                        // The file now holds what is typed.
                        this.original = this.current_values(cx);
                        this.preview = Preview::Unchanged;
                        this.message = Some(Ok(
                            "Saved. SharePoint will show the new values after the next \
                             package-solution."
                                .to_string(),
                        ));
                    }
                    Err(error) => this.message = Some(Err(error)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ── Components ─────────────────────────────────────────────────────────

    fn candidate(&self) -> Option<&Candidate> {
        match &self.add.candidates {
            Candidates::Ready(list) => list.get(self.add.chosen),
            _ => None,
        }
    }

    /// Adds an empty name row directly below row `after`.
    fn add_row(&mut self, after: usize, window: &mut Window, cx: &mut Context<Self>) {
        let row = cx.new(|cx| InputState::new(window, cx).placeholder("Web part name"));
        self._subscriptions
            .push(cx.subscribe(&row, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.plan_components(cx);
                }
            }));
        let at = (after + 1).min(self.add.rows.len());
        self.add.rows.insert(at, row);
        cx.notify();
    }

    /// Takes a name row away; the last one stays.
    fn remove_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.add.rows.len() > 1 && index < self.add.rows.len() {
            self.add.rows.remove(index);
            self.plan_components(cx);
        }
        cx.notify();
    }

    /// The names typed, trimmed, without the empty rows.
    fn typed_names(&self, cx: &App) -> Vec<String> {
        self.add
            .rows
            .iter()
            .map(|row| row.read(cx).value().trim().to_string())
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// Asks the engine what adding the typed names would do, off the main thread.
    fn plan_components(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.status, Status::Ready) || self.busy.is_some() {
            return;
        }
        let Some(solution) = self.solution.clone() else {
            return;
        };
        let Some(candidate) = self.candidate().cloned() else {
            return;
        };
        self.add.generation += 1;
        let names = self.typed_names(cx);
        if names.is_empty() || !candidate.installed {
            self.add.preview = AddPreview::Idle;
            cx.notify();
            return;
        }
        let generation = self.add.generation;
        self.add.preview = AddPreview::Planning;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let store = TemplateStore::open(templates_dir())?;
                    let template = store.resolve(&candidate.generator, &candidate.version)?;
                    // The official templates add one component at a time and are
                    // asked for its name; ours take a list of web parts.
                    let answers = match template.manifest.add.as_ref().map(|add| add.mode) {
                        Some(AddMode::Render) => UpstreamInputs {
                            component_name: names[0].clone(),
                            ..Default::default()
                        }
                        .add_answers(),
                        _ => {
                            let mut answers = Answers::new();
                            answers.insert("webparts".into(), json!(names));
                            answers
                        }
                    };
                    plan_add(
                        &solution,
                        &template.manifest,
                        &DirSource::new(&template.dir),
                        &answers,
                        AddOptions::new(),
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                if this.add.generation != generation {
                    return;
                }
                this.add.preview = match result {
                    Ok(plan) => AddPreview::Ready(Box::new(plan)),
                    Err(ScaffoldError::Validation(issues)) => AddPreview::Issues(
                        issues.into_iter().map(|issue| issue.message).collect(),
                    ),
                    Err(error) => AddPreview::Failed(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Downloads the chosen template when it is only in the index.
    fn download_template(&mut self, cx: &mut Context<Self>) {
        let Some(candidate) = self.candidate().cloned() else {
            return;
        };
        if self.add.downloading {
            return;
        }
        self.add.downloading = true;
        cx.notify();
        let fetch = HttpFetch {
            client: self.http.clone(),
        };
        cx.spawn(async move |this, cx| {
            let generator = candidate.generator.clone();
            let version = candidate.version.clone();
            let result = cx
                .background_spawn(async move {
                    let store = TemplateStore::open(templates_dir())
                        .map_err(|error| error.to_string())?;
                    store
                        .update(&fetch, &generator, &version)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                })
                .await;
            this.update(cx, |this, cx| {
                this.add.downloading = false;
                match result {
                    Ok(()) => {
                        if let Candidates::Ready(list) = &mut this.add.candidates
                            && let Some(found) = list.get_mut(this.add.chosen)
                        {
                            found.installed = true;
                        }
                        this.plan_components(cx);
                    }
                    Err(error) => {
                        this.add.preview =
                            AddPreview::Failed(format!("The download failed: {error}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Writes the planned components, and keeps the undo record once they are on disk.
    fn apply_components(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let AddPreview::Ready(plan) = &self.add.preview else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        let plan = (**plan).clone();
        let names = self.typed_names(cx);
        let record = plan.undo_record(&format!("Added {}", names.join(", ")));
        // The web part's main file, for "Open web part", and whether new packages
        // were added.
        let main_file = plan
            .files
            .iter()
            .find(|file| {
                file.path.contains("webparts/")
                    && (file.path.ends_with("WebPart.ts") || file.path.ends_with("WebPart.tsx"))
            })
            .map(|file| plan.root.join(&file.path));
        let packages_changed = plan.files.iter().any(|file| file.path == "package.json");
        self.busy = Some("Adding the components…");
        self.message = None;
        cx.notify();
        let key = undo_key(&self.root);
        let kvp = KeyValueStore::global(cx);
        let stored = record.to_json();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    apply_add(&plan, |_| {}).map_err(|error| error.to_string())?;
                    kvp.write_kvp(key, stored).await.ok();
                    Ok::<_, String>(())
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(()) => {
                        this.undo = Some(record);
                        this.add.added_file = main_file;
                        this.add.needs_install = packages_changed;
                        this.add.preview = AddPreview::Idle;
                        // One empty row again for the next time.
                        for row in &this.add.rows {
                            row.update(cx, |input, cx| input.set_value("", window, cx));
                        }
                        this.add.rows.truncate(1);
                        let mut text = format!("Added {}.", names.join(", "));
                        if packages_changed {
                            text.push_str(
                                " package.json changed: run npm install before you build.",
                            );
                        }
                        this.message = Some(Ok(text));
                        // Shows the new components; the Details boxes are left alone.
                        this.load(false, window, cx);
                    }
                    Err(error) => this.message = Some(Err(error)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens the added web part's main file in the editor.
    fn open_added_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(path), Some(workspace)) =
            (self.add.added_file.clone(), self.workspace.upgrade())
        else {
            return;
        };
        // Not inside this click: the workspace reads every item, this one
        // included, and this view is borrowed right now.
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(path, OpenOptions::default(), window, cx)
                    .detach_and_log_err(cx);
            });
        });
    }

    /// Runs `npm install` in the solution, in the Script Runner behind the trust
    /// gate. It is offered once: the Script Runner shows the rest.
    fn run_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let root = self
            .solution
            .as_ref()
            .map(|solution| solution.root.clone())
            .unwrap_or_else(|| self.root.clone());
        npm_bootstrap::install_now(&self.workspace, root, window, cx);
        self.add.needs_install = false;
        cx.notify();
    }

    // ── Undo ───────────────────────────────────────────────────────────────

    /// Puts the files back as they were before the last change. The engine
    /// refuses, and names the file, when one has been changed since.
    fn revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(record) = self.undo.clone() else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("Undoing the last change…");
        self.message = None;
        cx.notify();
        let root = self.root.clone();
        let key = undo_key(&root);
        let kvp = KeyValueStore::global(cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    revert_edit(&root, &record).map_err(|error| error.to_string())?;
                    kvp.delete_kvp(key).await.ok();
                    Ok::<_, String>(())
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => {
                        this.undo = None;
                        this.add.added_file = None;
                        this.add.needs_install = false;
                        this.message = Some(Ok("The last change is undone.".to_string()));
                        // Shows the files as they are now.
                        this.load(true, window, cx);
                    }
                    Err(error) => {
                        this.busy = None;
                        this.message = Some(Err(format!("Could not undo: {error}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ── Drawing ────────────────────────────────────────────────────────────

    /// The Undo button, while there is a change to undo.
    fn undo_button(&self, cx: &mut Context<Self>) -> Option<Button> {
        self.undo.as_ref().map(|record| {
            Button::new("scaffold-edit-undo")
                .outline()
                .label(format!("Undo: {}", record.label))
                .disabled(self.busy.is_some())
                .on_click(cx.listener(|this, _, window, cx| this.revert(window, cx)))
        })
    }

    /// The spinner and the words while something runs.
    fn busy_indicator(&self, muted: gpui::Hsla) -> Option<impl IntoElement + use<>> {
        self.busy.map(|label| {
            h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small())
                .child(div().text_sm().text_color(muted).child(label))
        })
    }

    fn render_details(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let (muted, danger, border) = (theme.muted_foreground, theme.danger, theme.border);

        let issues: &[ValidationIssue] = match &self.preview {
            Preview::Issues(issues) => issues,
            _ => &[],
        };
        // What the engine said about one box, under that box.
        let problems = |name: &str| -> Vec<gpui::AnyElement> {
            issues
                .iter()
                .filter(|issue| issue.field == name)
                .map(|issue| {
                    div()
                        .text_xs()
                        .text_color(danger)
                        .child(issue.message.clone())
                        .into_any_element()
                })
                .collect()
        };

        let preview = match &self.preview {
            Preview::Unchanged => div()
                .text_sm()
                .text_color(muted)
                .child("Nothing has changed yet.")
                .into_any_element(),
            Preview::Planning => h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small())
                .child(div().text_sm().text_color(muted).child("Checking…"))
                .into_any_element(),
            Preview::Issues(_) => div()
                .text_sm()
                .text_color(muted)
                .child("Fix the values above to see what would change.")
                .into_any_element(),
            Preview::Failed(error) => {
                Alert::error("scaffold-edit-plan-failed", error.clone()).into_any_element()
            }
            Preview::Changes(_, changes) => v_flex()
                .gap_2()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(border)
                .children(changes.iter().map(|change| {
                    v_flex()
                        .gap_0p5()
                        .child(div().text_xs().text_color(muted).child(change.file.clone()))
                        .child(
                            div()
                                .text_sm()
                                .font_family("Cascadia Mono")
                                .text_color(theme.danger)
                                .child(format!("- {}", change.from)),
                        )
                        .child(
                            div()
                                .text_sm()
                                .font_family("Cascadia Mono")
                                .text_color(theme.success)
                                .child(format!("+ {}", change.to)),
                        )
                }))
                .into_any_element(),
        };

        let can_apply = matches!(self.preview, Preview::Changes(..)) && self.busy.is_none();

        v_flex()
            .gap_3()
            .child(
                field().label("Name").child(
                    v_flex()
                        .gap_1()
                        .child(Input::new(&self.name))
                        .children(problems("name")),
                ),
            )
            .child(
                field().label("Description").child(
                    v_flex()
                        .gap_1()
                        .child(Input::new(&self.description))
                        .children(problems("description")),
                ),
            )
            .child(
                field().label("Version").child(
                    v_flex()
                        .gap_1()
                        .child(Input::new(&self.version))
                        .children(problems("version")),
                ),
            )
            .child(div().text_xs().text_color(muted).child(
                "Four numbers, like 1.0.0.0. The solution id and the SPFx version are not \
                 changed here, and package.json is left as it is.",
            ))
            .child(div().font_semibold().child("Changes"))
            .child(preview)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("scaffold-edit-apply")
                            .primary()
                            .label("Apply changes")
                            .disabled(!can_apply)
                            .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                    )
                    .children(self.undo_button(cx))
                    .children(self.busy_indicator(muted)),
            )
            .into_any_element()
    }

    fn render_components(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let (muted, border, success, warning) =
            (theme.muted_foreground, theme.border, theme.success, theme.warning);

        // What is in the solution already.
        let existing = v_flex()
            .gap_0p5()
            .child(div().font_semibold().child("In this solution"))
            .children(if self.add.existing.is_empty() {
                vec![
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child("No components yet.")
                        .into_any_element(),
                ]
            } else {
                self.add
                    .existing
                    .iter()
                    .map(|component| {
                        let kind = match component.kind {
                            ComponentKind::WebPart => "web part".to_string(),
                            ComponentKind::Extension => component
                                .extension_type
                                .clone()
                                .unwrap_or_else(|| "extension".to_string()),
                            ComponentKind::AdaptiveCardExtension => {
                                "Adaptive Card extension".to_string()
                            }
                            ComponentKind::Library => "library".to_string(),
                            _ => "component".to_string(),
                        };
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().child(component.name.clone()))
                            .child(div().text_sm().text_color(muted).child(kind))
                            .into_any_element()
                    })
                    .collect()
            });

        let add_section = match &self.add.candidates {
            Candidates::Loading => h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small())
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child("Looking for templates…"),
                )
                .into_any_element(),
            Candidates::None(reason) => {
                Alert::info("scaffold-add-none", reason.clone()).into_any_element()
            }
            Candidates::Ready(list) => self.render_add(list, muted, border, success, warning, cx),
        };

        v_flex()
            .gap_3()
            .child(existing)
            .child(add_section)
            .into_any_element()
    }

    fn render_add(
        &self,
        list: &[Candidate],
        muted: gpui::Hsla,
        border: gpui::Hsla,
        success: gpui::Hsla,
        warning: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(candidate) = list.get(self.add.chosen) else {
            return div().into_any_element();
        };
        let one_at_a_time = candidate.generator != "spfx-webpart";
        let solution_framework = framework_of_solution(
            self.solution
                .as_ref()
                .and_then(|solution| solution.framework.as_deref()),
        );
        let template_framework = framework_of_template(&candidate.generator);

        // The template, when there is a choice.
        let choice = (list.len() > 1).then(|| {
            h_flex()
                .gap_1()
                .flex_wrap()
                .children(list.iter().enumerate().map(|(index, other)| {
                    let button = Button::new(("scaffold-add-template", index))
                        .small()
                        .label(other.title.clone());
                    let button = if index == self.add.chosen {
                        button.primary()
                    } else {
                        button.outline()
                    };
                    button.on_click(cx.listener(move |this, _, _, cx| {
                        this.add.chosen = index;
                        this.plan_components(cx);
                        cx.notify();
                    }))
                }))
        });

        // A framework that differs is allowed (the engine adds the packages and
        // leaves the solution's own files), so this says so and does not block.
        let framework_note = (solution_framework != template_framework).then(|| {
            let mut text = format!(
                "This solution uses {solution_framework} and this template makes a \
                 {template_framework} web part. Its packages are added to package.json; the \
                 ones already there are not changed."
            );
            if candidate.generator == "spfx-webpart" {
                text.push_str(" This combination has not been tested.");
            }
            Alert::info("scaffold-add-framework", text)
        });

        let rows = self.add.rows.len();
        let any_empty = self
            .add
            .rows
            .iter()
            .any(|row| row.read(cx).value().trim().is_empty());
        let name_rows = v_flex()
            .gap_1()
            .children(self.add.rows.iter().enumerate().map(|(row, state)| {
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1().child(Input::new(state)))
                    .child(
                        Button::new(("scaffold-add-remove", row))
                            .ghost()
                            .small()
                            .icon(IconName::Minus)
                            .tooltip("Remove")
                            .disabled(rows < 2)
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_row(row, cx))),
                    )
                    .child(
                        Button::new(("scaffold-add-add", row))
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip(if one_at_a_time {
                                "This template adds one at a time"
                            } else {
                                "Add another below"
                            })
                            // Not while a row is empty, which would only pile up
                            // blank rows, and not for a template that adds one
                            // component at a time.
                            .disabled(any_empty || one_at_a_time)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.add_row(row, window, cx)
                            })),
                    )
            }));

        let preview = match &self.add.preview {
            AddPreview::Idle => div()
                .text_sm()
                .text_color(muted)
                .child("Type a name to see what would be added.")
                .into_any_element(),
            AddPreview::Planning => h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small())
                .child(div().text_sm().text_color(muted).child("Checking…"))
                .into_any_element(),
            AddPreview::Issues(messages) => v_flex()
                .gap_0p5()
                .children(messages.iter().map(|message| {
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(message.clone())
                }))
                .into_any_element(),
            AddPreview::Failed(error) => {
                Alert::error("scaffold-add-failed", error.clone()).into_any_element()
            }
            AddPreview::Ready(plan) => {
                let new = plan
                    .files
                    .iter()
                    .filter(|file| file.outcome == AddOutcome::New)
                    .count();
                let merged = plan.files.len() - new;
                v_flex()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .child(div().text_xs().text_color(muted).child(format!(
                        "{new} new files, {merged} changed"
                    )))
                    .children(plan.files.iter().map(|file| {
                        let (mark, color) = match file.outcome {
                            AddOutcome::New => ("+", success),
                            _ => ("~", warning),
                        };
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().text_color(color).child(mark))
                            .child(
                                div()
                                    .text_sm()
                                    .font_family("Cascadia Mono")
                                    .child(file.path.clone()),
                            )
                    }))
                    .children((!plan.kept.is_empty()).then(|| {
                        div().text_xs().text_color(muted).child(format!(
                            "Left as they are: {}",
                            plan.kept.join(", ")
                        ))
                    }))
                    .children(plan.warnings.iter().map(|warning_text| {
                        div()
                            .text_xs()
                            .text_color(warning)
                            .child(warning_text.clone())
                    }))
                    .into_any_element()
            }
        };

        let can_add = matches!(self.add.preview, AddPreview::Ready(_)) && self.busy.is_none();
        let not_downloaded = (!candidate.installed).then(|| {
            v_flex()
                .gap_2()
                .child(Alert::info(
                    "scaffold-add-download",
                    "This template is not on this machine yet.",
                ))
                .child(
                    h_flex().child(
                        Button::new("scaffold-add-download-button")
                            .primary()
                            .label("Download this template")
                            .loading(self.add.downloading)
                            .on_click(cx.listener(|this, _, _, cx| this.download_template(cx))),
                    ),
                )
        });

        v_flex()
            .gap_3()
            .child(div().font_semibold().child("Add"))
            .children(choice)
            .children(framework_note)
            .children(not_downloaded)
            .child(field().label("Name").child(name_rows))
            .children(one_at_a_time.then(|| {
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("This template adds one component at a time.")
            }))
            .child(div().font_semibold().child("Changes"))
            .child(preview)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(
                        Button::new("scaffold-add-apply")
                            .primary()
                            .label("Add web part")
                            .disabled(!can_add)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.apply_components(window, cx)),
                            ),
                    )
                    .children(self.add.added_file.as_ref().map(|_| {
                        Button::new("scaffold-open-web-part")
                            .outline()
                            .label("Open web part")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_added_file(window, cx)),
                            )
                    }))
                    .children(self.add.needs_install.then(|| {
                        Button::new("scaffold-run-install")
                            .outline()
                            .label("Run npm install")
                            .disabled(self.busy.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.run_install(window, cx)))
                    }))
                    .children(self.undo_button(cx))
                    .children(self.busy_indicator(muted)),
            )
            .into_any_element()
    }
}

/// The solution at `root`, what the engine reads of it, and the description of
/// its first feature (the engine's info has no description). Blocks on the disk:
/// runs off the main thread.
fn read_solution(root: &Path) -> Result<(SpfxSolution, SolutionInfo, Option<String>), String> {
    let solution = detect_spfx(root)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "No SharePoint solution was found in this folder.".to_string())?;
    let info = solution_info(&solution).map_err(|error| error.to_string())?;
    let description = std::fs::read_to_string(solution.root.join(SOLUTION_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|json| {
            json.pointer("/solution/features/0/description")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    Ok((solution, info, description))
}

/// `1.23` for `1.23.2`: the engine only adds a template of the solution's minor
/// version.
fn major_minor(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}

/// The web part templates that fit the solution's SPFx version, newest version
/// of each. Reads the templates folder: runs off the main thread.
fn find_candidates(solution_version: Option<String>) -> Candidates {
    let Some(solution_version) = solution_version else {
        return Candidates::None("The SPFx version of this solution could not be read.".into());
    };
    let minor = major_minor(&solution_version);
    let Ok(store) = TemplateStore::open(templates_dir()) else {
        return Candidates::None("The templates folder could not be read.".into());
    };
    let titles = store.list_generators().ok();
    let mut found = Vec::new();
    for id in WEB_PART_TEMPLATES {
        let Ok(versions) = store.list_versions(id) else {
            continue;
        };
        // Newest first: the first of this minor version that is on disk or can be
        // downloaded.
        let Some(version) = versions
            .versions
            .iter()
            .find(|v| major_minor(&v.version) == minor && (v.installed || v.downloadable))
        else {
            continue;
        };
        let title = titles
            .as_ref()
            .and_then(|list| list.generators.iter().find(|g| g.id == id))
            .map(|g| g.title.clone())
            .unwrap_or_else(|| id.to_string());
        found.push(Candidate {
            generator: id.to_string(),
            title,
            version: version.version.clone(),
            installed: version.installed,
        });
    }
    if found.is_empty() {
        Candidates::None(format!(
            "There is no web part template for SPFx {solution_version} yet. Templates exist for \
             1.20 and newer; check for template updates in the Scaffold panel."
        ))
    } else {
        Candidates::Ready(found)
    }
}

/// The candidate whose framework is the solution's, else the first.
fn best_candidate(candidates: &Candidates, framework: Option<&str>) -> usize {
    let Candidates::Ready(list) = candidates else {
        return 0;
    };
    let wanted = framework_of_solution(framework);
    list.iter()
        .position(|candidate| framework_of_template(&candidate.generator) == wanted)
        .unwrap_or(0)
}

/// The solution's framework in the words the templates use.
fn framework_of_solution(framework: Option<&str>) -> &'static str {
    match framework {
        Some("react") => "React",
        Some("minimal") => "minimal",
        _ => "no framework",
    }
}

/// The framework a web part template makes.
fn framework_of_template(generator: &str) -> &'static str {
    match generator {
        "spfx-webpart" | "webpart-react" => "React",
        "webpart-minimal" => "minimal",
        _ => "no framework",
    }
}

/// Where a solution's last undo record is kept.
fn undo_key(root: &Path) -> String {
    format!(
        "scaffold_panel.undo.{}",
        root.to_string_lossy().to_lowercase()
    )
}

/// The lines a plan changes, as before and after, for the preview. A plan of this
/// kind changes values in place, so the lines of the two files pair up.
fn changed_lines(plan: &AddPlan) -> Vec<Change> {
    let mut changes = Vec::new();
    for file in &plan.files {
        let before = String::from_utf8_lossy(file.previous.as_deref().unwrap_or_default());
        let after = String::from_utf8_lossy(&file.content);
        for (old, new) in before.lines().zip(after.lines()) {
            if old != new {
                changes.push(Change {
                    file: file.path.clone(),
                    from: old.trim().to_string(),
                    to: new.trim().to_string(),
                });
            }
        }
    }
    changes.truncate(8);
    changes
}

impl Focusable for EditSolutionView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for EditSolutionView {}

impl Item for EditSolutionView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        if self.title.is_empty() {
            "Edit solution".into()
        } else {
            format!("Edit {}", self.title).into()
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        Some(ui::Icon::new(ui::IconName::Box))
    }
}

impl Render for EditSolutionView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, background) = (theme.muted_foreground, theme.background);

        let body = match &self.status {
            Status::Loading => h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small())
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child("Reading the solution…"),
                )
                .into_any_element(),
            Status::Failed(error) => {
                Alert::error("scaffold-edit-load-failed", error.clone()).into_any_element()
            }
            Status::Ready => v_flex()
                .gap_3()
                .child(
                    TabBar::new("scaffold-edit-pages")
                        .with_variant(TabVariant::Underline)
                        .children(
                            ["Details", "Components"]
                                .iter()
                                .map(|label| Tab::new().label(*label)),
                        )
                        .selected_index(match self.page {
                            Page::Details => 0,
                            Page::Components => 1,
                        })
                        .on_click(cx.listener(|this, index: &usize, _, cx| {
                            this.page = if *index == 1 {
                                Page::Components
                            } else {
                                Page::Details
                            };
                            cx.notify();
                        })),
                )
                .child(match self.page {
                    Page::Details => self.render_details(cx),
                    Page::Components => self.render_components(cx),
                })
                .children(self.message.as_ref().map(|message| match message {
                    Ok(text) => Alert::success("scaffold-edit-done", text.clone()),
                    Err(text) => Alert::error("scaffold-edit-error", text.clone()),
                }))
                .into_any_element(),
        };

        // Every root fills its area, loading and error states included.
        v_flex()
            .id("scaffold-edit-solution")
            .track_focus(&self.focus_handle)
            .size_full()
            .items_center()
            .bg(background)
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w_full()
                    .flex_shrink_0()
                    .gap_3()
                    .p_4()
                    .max_w(px(640.))
                    .child(div().text_lg().font_semibold().child("Edit solution"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .truncate()
                            .child(self.root.display().to_string()),
                    )
                    .child(body),
            )
    }
}

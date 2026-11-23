//! Editing the name, description and version of an existing SPFx solution, in a
//! workspace tab: phase 7 of `docs/scaffold-panel-plan.md`. The Node panel's
//! gear on an SPFx solution opens it (`zed_actions::scaffold::EditSolution`).
//!
//! The engine does the work: `plan_edit` checks the values and builds the new
//! `config/package-solution.json` as data, `apply_edit` writes it safely, and
//! `revert_edit` puts it back. The tab plans again after every keystroke (off the
//! main thread), so the checks and the preview are always for what is typed.
//! The undo record is kept in the key-value store, keyed by the solution folder,
//! so Undo still works after a restart for as long as the file is as the edit
//! left it. Only the last edit of a solution is kept.

use std::path::{Path, PathBuf};

use db::kvp::KeyValueStore;
use forge_scaffold::{
    AddPlan, ScaffoldError, SolutionEdit, SolutionInfo, SpfxSolution, UndoRecord, ValidationIssue,
    apply_edit, detect_spfx, plan_edit, revert_edit, solution_info,
};
use gpui::{
    App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
    alert::Alert,
    button::{Button, ButtonVariants as _},
    form::field,
    h_flex,
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    v_flex,
};
use serde_json::Value;
use workspace::item::Item;

/// The solution file that holds the three values.
const SOLUTION_FILE: &str = "config/package-solution.json";

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

pub struct EditSolutionView {
    root: PathBuf,
    focus_handle: FocusHandle,
    status: Status,
    /// What the engine detected; the plan is made against it.
    solution: Option<SpfxSolution>,
    /// The name SharePoint shows, for the tab and the undo label.
    title: String,
    original: Original,
    name: gpui::Entity<InputState>,
    description: gpui::Entity<InputState>,
    version: gpui::Entity<InputState>,
    preview: Preview,
    /// Counts the plans asked for; a plan that arrives for an older one is dropped.
    generation: u64,
    /// What is running ("Applying the changes…"), while something is.
    busy: Option<&'static str>,
    /// The result of the last apply or undo.
    message: Option<Result<String, String>>,
    /// The last edit of this solution, which Undo reverts.
    undo: Option<UndoRecord>,
    _subscriptions: Vec<Subscription>,
}

impl EditSolutionView {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Solution name"));
        let description = cx.new(|cx| InputState::new(window, cx).placeholder("Description"));
        let version = cx.new(|cx| InputState::new(window, cx).placeholder("1.0.0.0"));

        let mut subscriptions = Vec::new();
        for input in [&name, &description, &version] {
            subscriptions.push(cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.plan_changes(cx);
                }
            }));
        }

        let mut view = Self {
            root,
            focus_handle: cx.focus_handle(),
            status: Status::Loading,
            solution: None,
            title: String::new(),
            original: Original::default(),
            name,
            description,
            version,
            preview: Preview::Unchanged,
            generation: 0,
            busy: None,
            message: None,
            undo: None,
            _subscriptions: subscriptions,
        };
        view.load(window, cx);
        view
    }

    /// Reads the solution and the stored undo record off the main thread, then
    /// fills the three boxes. Also used after an undo, to show the file as it is.
    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            this.update_in(cx, |this, window, cx| {
                match solution {
                    Ok((solution, info, description)) => {
                        this.title = info
                            .solution_name
                            .clone()
                            .or(info.package_name.clone())
                            .unwrap_or_default();
                        this.original = Original {
                            name: info.solution_name.unwrap_or_default(),
                            description,
                            version: info.solution_version.unwrap_or_default(),
                        };
                        this.solution = Some(solution);
                        this.undo = stored;
                        this.status = Status::Ready;
                        this.preview = Preview::Unchanged;
                        // The original is in place before the boxes change, so the
                        // change events they send find nothing to plan.
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
                    Err(error) => this.status = Status::Failed(error),
                }
                this.busy = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

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
        // Made before the write: which folders an edit creates can only be told
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

    /// Puts the file back as it was before the last edit. The engine refuses, and
    /// names the file, when it has been changed since.
    fn revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(record) = self.undo.clone() else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        self.busy = Some("Undoing the last edit…");
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
                        this.message = Some(Ok("The last edit is undone.".to_string()));
                        // Shows the file as it is now.
                        this.load(window, cx);
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

    fn render_form(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
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
        let undo_label = self
            .undo
            .as_ref()
            .map(|record| format!("Undo: {}", record.label));

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
                    .children(undo_label.map(|label| {
                        Button::new("scaffold-edit-undo")
                            .outline()
                            .label(label)
                            .disabled(self.busy.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.revert(window, cx)))
                    }))
                    .children(self.busy.map(|label| {
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Spinner::new().small())
                            .child(div().text_sm().text_color(muted).child(label))
                    })),
            )
            .children(self.message.as_ref().map(|message| match message {
                Ok(text) => Alert::success("scaffold-edit-done", text.clone()),
                Err(text) => Alert::error("scaffold-edit-error", text.clone()),
            }))
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
            Status::Ready => self.render_form(cx),
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

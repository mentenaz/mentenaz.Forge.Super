//! The new-project form, in a workspace tab: phase 2 of
//! `docs/scaffold-panel-plan.md`. For one generator it lets the user pick a
//! version (downloading it first when it is only in the index), shows whether
//! the installed Node fits, builds one input per prompt of the generator's
//! manifest, and checks the answers with the engine's own validation.
//!
//! Nothing is written to disk here. Showing the plan, choosing a location and
//! creating the project are the next phases; "Check answers" runs `plan()` into
//! a scratch location only to get the engine's verdict and its warnings.
//!
//! Everything slow (reading the folder, the download, the Node query, `plan()`)
//! runs in `cx.background_spawn`. A result that arrives after the user has
//! picked another version is dropped (`generation`).

use std::path::PathBuf;
use std::sync::Arc;

use forge_scaffold::{
    Answers, DirSource, Manifest, NodeCheck, PlanOptions, Prompt, PromptKind, ScaffoldError,
    TemplateStore, VersionInfo, plan,
};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IndexPath, Sizable as _, StyledExt as _,
    alert::Alert,
    button::{Button, ButtonVariants as _},
    form::{field, v_form},
    h_flex,
    input::{Input, InputState},
    select::{Select, SelectEvent, SelectState},
    spinner::Spinner,
    switch::Switch,
    v_flex,
};
use http_client::HttpClient;
use serde_json::{Value, json};
use workspace::item::Item;

use crate::{HttpFetch, templates_dir};

/// What the tab shows instead of the form.
enum Status {
    LoadingVersions,
    Loading(&'static str),
    /// The chosen version is in the index but not on this machine.
    NeedsDownload,
    Downloading,
    Ready,
    Failed(String),
}

/// One prompt's input.
enum Control {
    Text(Entity<InputState>),
    Bool(bool),
    Choice(Entity<SelectState<Vec<String>>>),
    /// A comma separated list.
    List(Entity<InputState>),
}

struct Field {
    prompt: Prompt,
    control: Control,
}

/// A template that has been read: its manifest, where it is, and its form.
struct Loaded {
    manifest: Manifest,
    dir: PathBuf,
    fields: Vec<Field>,
}

enum NodeNotice {
    /// The template needs no particular Node, or nothing has been asked yet.
    Idle,
    Checking,
    Fine(String),
    Mismatch { required: String, installed: String },
    Missing(String),
}

enum Check {
    Idle,
    Running,
    Valid {
        folder: String,
        files: usize,
        warnings: Vec<String>,
    },
    /// One message per field, in the engine's words.
    Issues(Vec<(String, String)>),
    Failed(String),
}

pub struct NewProjectView {
    focus_handle: FocusHandle,
    http: Arc<dyn HttpClient>,
    generator: String,
    title: SharedString,
    status: Status,
    versions: Vec<VersionInfo>,
    version_select: Entity<SelectState<Vec<String>>>,
    chosen: Option<String>,
    loaded: Option<Loaded>,
    node: NodeNotice,
    check: Check,
    /// Bumped whenever another version is chosen, so a slow answer for the
    /// old one is ignored.
    generation: u64,
    _subscriptions: Vec<Subscription>,
}

/// A prompt's default shown as text: a placeholder, since an empty field means
/// "use the default" to the engine.
fn default_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// Whether this machine's Node fits the template, as a notice.
/// Blocks on a process: runs off the main thread. The program is started
/// through `node_backend`, which asks the trust gate.
fn node_notice(manifest: &Manifest) -> NodeNotice {
    match node_backend::query_runtime("node".to_string()) {
        Err(reason) => NodeNotice::Missing(reason),
        Ok(installed) => {
            let installed = installed.trim().to_string();
            match manifest.node_check(&installed) {
                Ok(NodeCheck::Fine) => NodeNotice::Fine(installed),
                Ok(NodeCheck::Mismatch {
                    required,
                    installed,
                }) => NodeNotice::Mismatch {
                    required: required.to_string(),
                    installed: installed.to_string(),
                },
                Ok(_) => NodeNotice::Idle,
                Err(error) => NodeNotice::Missing(error.to_string()),
            }
        }
    }
}

/// The engine's verdict on `answers`: `plan()` into a scratch location, which
/// writes nothing. Blocks on the disk: runs off the main thread.
fn run_plan(manifest: &Manifest, dir: &PathBuf, answers: &Answers) -> Check {
    let location = std::env::temp_dir();
    let source = DirSource::new(dir);
    match plan(manifest, &source, answers, PlanOptions::new(&location)) {
        Ok(planned) => Check::Valid {
            folder: planned.folder_name.clone(),
            files: planned.files.len(),
            warnings: planned.warnings.iter().map(|w| w.to_string()).collect(),
        },
        Err(ScaffoldError::Validation(issues)) => Check::Issues(
            issues
                .into_iter()
                .map(|issue| (issue.field, issue.message))
                .collect(),
        ),
        Err(other) => Check::Failed(other.to_string()),
    }
}

impl NewProjectView {
    pub fn new(
        generator: String,
        title: SharedString,
        http: Arc<dyn HttpClient>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let version_select = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let subscription = cx.subscribe_in(
            &version_select,
            window,
            |this, _, event: &SelectEvent<Vec<String>>, window, cx| {
                let SelectEvent::Confirm(choice) = event;
                if let Some(version) = choice.clone() {
                    this.choose_version(version, window, cx);
                }
            },
        );
        let mut view = Self {
            focus_handle: cx.focus_handle(),
            http,
            generator,
            title,
            status: Status::LoadingVersions,
            versions: Vec::new(),
            version_select,
            chosen: None,
            loaded: None,
            node: NodeNotice::Idle,
            check: Check::Idle,
            generation: 0,
            _subscriptions: vec![subscription],
        };
        view.load_versions(window, cx);
        view
    }

    fn load_versions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let generator = self.generator.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let store = TemplateStore::open(templates_dir()).map_err(|e| e.to_string())?;
                    store
                        .list_versions(&generator)
                        .map(|list| list.versions)
                        .map_err(|e| e.to_string())
                })
                .await;
            this.update_in(cx, |this, window, cx| this.versions_loaded(result, window, cx))
                .ok();
        })
        .detach();
    }

    fn versions_loaded(
        &mut self,
        result: Result<Vec<VersionInfo>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let versions = match result {
            Ok(versions) if !versions.is_empty() => versions,
            Ok(_) => {
                self.status = Status::Failed("This generator has no versions.".to_string());
                cx.notify();
                return;
            }
            Err(error) => {
                self.status = Status::Failed(error);
                cx.notify();
                return;
            }
        };
        // Newest first: start on the newest one that is already here, else the
        // newest there is.
        let start = versions.iter().position(|v| v.installed).unwrap_or(0);
        let names: Vec<String> = versions.iter().map(|v| v.version.clone()).collect();
        let chosen = names[start].clone();
        self.versions = versions;
        self.version_select.update(cx, |select, cx| {
            select.set_items(names, window, cx);
            select.set_selected_index(Some(IndexPath::new(start)), window, cx);
        });
        self.choose_version(chosen, window, cx);
    }

    fn choose_version(&mut self, version: String, window: &mut Window, cx: &mut Context<Self>) {
        self.generation += 1;
        self.chosen = Some(version.clone());
        self.loaded = None;
        self.node = NodeNotice::Idle;
        self.check = Check::Idle;
        let installed = self
            .versions
            .iter()
            .any(|v| v.version == version && v.installed);
        if installed {
            self.load_template(version, window, cx);
        } else {
            self.status = Status::NeedsDownload;
            cx.notify();
        }
    }

    /// Downloads the chosen version: only on the user's click.
    fn download(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(version) = self.chosen.clone() else {
            return;
        };
        self.status = Status::Downloading;
        cx.notify();
        let generation = self.generation;
        let generator = self.generator.clone();
        let fetch = HttpFetch {
            client: self.http.clone(),
        };
        cx.spawn_in(window, async move |this, cx| {
            let downloading = version.clone();
            let result = cx
                .background_spawn(async move {
                    let store = TemplateStore::open(templates_dir()).map_err(|e| e.to_string())?;
                    store
                        .update(&fetch, &generator, &downloading)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(()) => {
                        for known in this.versions.iter_mut().filter(|v| v.version == version) {
                            known.installed = true;
                        }
                        this.load_template(version, window, cx);
                    }
                    Err(error) => {
                        this.status = Status::Failed(format!("The download failed: {error}"));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn load_template(&mut self, version: String, window: &mut Window, cx: &mut Context<Self>) {
        self.status = Status::Loading("Reading the template…");
        cx.notify();
        let generation = self.generation;
        let generator = self.generator.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let store = TemplateStore::open(templates_dir()).map_err(|e| e.to_string())?;
                    let template = store
                        .resolve(&generator, &version)
                        .map_err(|e| e.to_string())?;
                    Ok::<_, String>((template.manifest, template.dir))
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.generation == generation {
                    this.template_loaded(result, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn template_loaded(
        &mut self,
        result: Result<(Manifest, PathBuf), String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (manifest, dir) = match result {
            Ok(loaded) => loaded,
            Err(error) => {
                self.status = Status::Failed(error);
                cx.notify();
                return;
            }
        };
        let fields = Self::build_fields(&manifest, window, cx);
        self.start_node_check(manifest.clone(), cx);
        self.loaded = Some(Loaded {
            manifest,
            dir,
            fields,
        });
        self.status = Status::Ready;
        cx.notify();
    }

    /// One input per prompt, from the manifest: the form is data, so a new
    /// generator needs no code here.
    fn build_fields(
        manifest: &Manifest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Field> {
        manifest
            .prompts
            .iter()
            .map(|prompt| {
                let control = match prompt.kind {
                    PromptKind::Bool => Control::Bool(
                        prompt
                            .default
                            .as_ref()
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    ),
                    PromptKind::Choice => {
                        let options = prompt.options.clone();
                        let selected = prompt
                            .default
                            .as_ref()
                            .and_then(Value::as_str)
                            .and_then(|default| options.iter().position(|o| o == default))
                            .or((!options.is_empty()).then_some(0));
                        Control::Choice(cx.new(|cx| {
                            SelectState::new(options, selected.map(IndexPath::new), window, cx)
                        }))
                    }
                    _ => {
                        let placeholder = prompt
                            .default
                            .as_ref()
                            .map(default_text)
                            .unwrap_or_default();
                        let sensitive = prompt.sensitive;
                        let state = cx.new(|cx| {
                            let state = InputState::new(window, cx).placeholder(placeholder);
                            if sensitive { state.masked(true) } else { state }
                        });
                        if matches!(prompt.kind, PromptKind::List) {
                            Control::List(state)
                        } else {
                            Control::Text(state)
                        }
                    }
                };
                Field {
                    prompt: prompt.clone(),
                    control,
                }
            })
            .collect()
    }

    fn start_node_check(&mut self, manifest: Manifest, cx: &mut Context<Self>) {
        let needs_node = manifest
            .requires
            .as_ref()
            .and_then(|requires| requires.node.as_ref())
            .is_some();
        if !needs_node {
            self.node = NodeNotice::Idle;
            return;
        }
        self.node = NodeNotice::Checking;
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let notice = cx
                .background_spawn(async move { node_notice(&manifest) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.node = notice;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// The answers as the form stands: active prompts only, in order (a `when`
    /// may look at an earlier answer). An empty text field is left out, so the
    /// engine's default applies.
    fn collect_answers(&self, cx: &App) -> Answers {
        let mut answers = Answers::new();
        let Some(loaded) = &self.loaded else {
            return answers;
        };
        for field in &loaded.fields {
            if let Some(when) = &field.prompt.when
                && !when.matches(&answers)
            {
                continue;
            }
            let key = field.prompt.key.clone();
            match &field.control {
                Control::Text(state) => {
                    let text = state.read(cx).value().trim().to_string();
                    if !text.is_empty() {
                        answers.insert(key, json!(text));
                    }
                }
                Control::Bool(value) => {
                    answers.insert(key, json!(value));
                }
                Control::Choice(state) => {
                    if let Some(choice) = state.read(cx).selected_value() {
                        answers.insert(key, json!(choice));
                    }
                }
                Control::List(state) => {
                    let items: Vec<String> = state
                        .read(cx)
                        .value()
                        .split(',')
                        .map(|item| item.trim().to_string())
                        .filter(|item| !item.is_empty())
                        .collect();
                    if !items.is_empty() {
                        answers.insert(key, json!(items));
                    }
                }
            }
        }
        answers
    }

    fn check_answers(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let (manifest, dir) = (loaded.manifest.clone(), loaded.dir.clone());
        let answers = self.collect_answers(cx);
        self.check = Check::Running;
        cx.notify();
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let verdict = cx
                .background_spawn(async move { run_plan(&manifest, &dir, &answers) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.check = verdict;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn set_bool(&mut self, index: usize, value: bool) {
        if let Some(Loaded { fields, .. }) = &mut self.loaded
            && let Some(Field {
                control: Control::Bool(current),
                ..
            }) = fields.get_mut(index)
        {
            *current = value;
        }
    }

    fn render_node_notice(&self, muted: gpui::Hsla) -> Option<gpui::AnyElement> {
        match &self.node {
            NodeNotice::Idle => None,
            NodeNotice::Checking => Some(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Checking Node…")
                    .into_any_element(),
            ),
            NodeNotice::Fine(installed) => Some(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("Node {installed} fits this template."))
                    .into_any_element(),
            ),
            NodeNotice::Mismatch {
                required,
                installed,
            } => Some(
                Alert::warning(
                    "scaffold-node",
                    format!(
                        "This template needs Node {required}, and {installed} is installed. \
                         Switch Node before creating the project."
                    ),
                )
                .into_any_element(),
            ),
            NodeNotice::Missing(reason) => Some(
                Alert::warning(
                    "scaffold-node",
                    format!("Node was not found ({reason}). Installing the project needs it."),
                )
                .into_any_element(),
            ),
        }
    }

    fn render_form(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(loaded) = &self.loaded else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let danger = theme.danger;
        let muted = theme.muted_foreground;
        let answers = self.collect_answers(cx);
        let issues: &[(String, String)] = match &self.check {
            Check::Issues(issues) => issues,
            _ => &[],
        };

        let mut form = v_form();
        for (index, field_state) in loaded.fields.iter().enumerate() {
            let prompt = &field_state.prompt;
            if let Some(when) = &prompt.when
                && !when.matches(&answers)
            {
                continue;
            }
            let mut label = prompt.label.clone().unwrap_or_else(|| prompt.key.clone());
            if matches!(field_state.control, Control::List(_)) {
                label.push_str(" (comma separated)");
            }
            let message = issues
                .iter()
                .find(|(key, _)| key == &prompt.key)
                .map(|(_, message)| message.clone());
            let control = match &field_state.control {
                Control::Text(state) | Control::List(state) => {
                    let input = Input::new(state);
                    if prompt.sensitive {
                        input.mask_toggle().into_any_element()
                    } else {
                        input.into_any_element()
                    }
                }
                Control::Choice(state) => Select::new(state).into_any_element(),
                Control::Bool(value) => Switch::new(("scaffold-bool", index))
                    .label(label.clone())
                    .checked(*value)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        this.set_bool(index, *checked);
                        cx.notify();
                    }))
                    .into_any_element(),
            };
            let body = v_flex()
                .gap_1()
                .child(control)
                .children(message.map(|message| {
                    div()
                        .text_xs()
                        .text_color(danger)
                        .child(message)
                }));
            form = if matches!(field_state.control, Control::Bool(_)) {
                form.child(field().child(body))
            } else {
                form.child(field().label(label).child(body))
            };
        }

        let running = matches!(self.check, Check::Running);
        let result = match &self.check {
            Check::Idle | Check::Running => None,
            Check::Valid {
                folder,
                files,
                warnings,
            } => Some(
                v_flex()
                    .gap_2()
                    .child(Alert::success(
                        "scaffold-valid",
                        format!(
                            "These answers are valid. It would create \"{folder}\" with \
                             {files} files. Reviewing the files and choosing where to create \
                             them comes next."
                        ),
                    ))
                    .children(warnings.iter().enumerate().map(|(index, warning)| {
                        Alert::warning(("scaffold-warning", index), warning.clone())
                    }))
                    .into_any_element(),
            ),
            Check::Issues(_) => Some(
                Alert::error(
                    "scaffold-issues",
                    "Some answers need fixing: see the messages under the fields.",
                )
                .into_any_element(),
            ),
            Check::Failed(error) => {
                Some(Alert::error("scaffold-failed", error.clone()).into_any_element())
            }
        };

        v_flex()
            .gap_3()
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Nothing is written to disk yet."),
            )
            .child(form)
            .child(
                h_flex().gap_2().items_center().child(
                    Button::new("scaffold-check-answers")
                        .primary()
                        .label(if running {
                            "Checking…"
                        } else {
                            "Check answers"
                        })
                        .disabled(running)
                        .on_click(cx.listener(|this, _, _, cx| this.check_answers(cx))),
                ),
            )
            .children(result)
            .into_any_element()
    }
}

impl Focusable for NewProjectView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for NewProjectView {}

impl Item for NewProjectView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("New {}", self.title).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        Some(ui::Icon::new(ui::IconName::Box))
    }
}

impl Render for NewProjectView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, background) = (theme.muted_foreground, theme.background);

        let body = match &self.status {
            Status::LoadingVersions => loading_row("Looking for versions…", muted),
            Status::Loading(label) => loading_row(label, muted),
            Status::Downloading => loading_row("Downloading the template…", muted),
            Status::Failed(error) => Alert::error("scaffold-load-failed", error.clone())
                .into_any_element(),
            Status::NeedsDownload => v_flex()
                .gap_2()
                .child(Alert::info(
                    "scaffold-needs-download",
                    "This version is not on this machine yet. Download it to see its form.",
                ))
                .child(
                    h_flex().child(
                        Button::new("scaffold-download")
                            .primary()
                            .label("Download this version")
                            .on_click(cx.listener(|this, _, window, cx| this.download(window, cx))),
                    ),
                )
                .into_any_element(),
            Status::Ready => v_flex()
                .gap_3()
                .children(self.render_node_notice(muted))
                .child(self.render_form(cx))
                .into_any_element(),
        };

        // Every root fills its area, loading and error states included.
        v_flex()
            .id("scaffold-new-project")
            .track_focus(&self.focus_handle)
            .size_full()
            .items_center()
            .bg(background)
            .overflow_y_scroll()
            .child(
                // Centred in the tab, at most 640px wide, full width when the
                // tab is narrower.
                v_flex()
                    .w_full()
                    .gap_3()
                    .p_4()
                    .max_w(px(640.))
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child(format!("New {}", self.title)),
                    )
                    .child(
                        field()
                            .label("Version")
                            .child(Select::new(&self.version_select).placeholder("Choose a version")),
                    )
                    .child(body),
            )
    }
}

fn loading_row(label: &str, muted: gpui::Hsla) -> gpui::AnyElement {
    h_flex()
        .gap_2()
        .items_center()
        .child(Spinner::new().small())
        .child(div().text_sm().text_color(muted).child(label.to_string()))
        .into_any_element()
}

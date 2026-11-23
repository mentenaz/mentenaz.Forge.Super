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

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use db::kvp::KeyValueStore;
use forge_scaffold::{
    Answers, DirSource, Manifest, NameRule, NodeCheck, PlanOptions, PostAction,
    PostStepDescriptor, Prompt, PromptKind, ScaffoldError, TemplateStore, VersionInfo,
    WriteProgress, apply, plan, validate_package_name, validate_solution_name,
    validate_webpart_names,
};
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, TaskExt as _,
    WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, IndexPath, Sizable as _, StyledExt as _,
    alert::Alert,
    button::{Button, ButtonVariants as _},
    form::{field, v_form},
    h_flex,
    input::{Input, InputEvent, InputState},
    list::ListItem,
    progress::Progress,
    select::{Select, SelectEvent, SelectState},
    spinner::Spinner,
    stepper::{Stepper, StepperItem},
    switch::Switch,
    tree::{TreeItem, TreeState, tree},
    v_flex,
};
use http_client::HttpClient;
use serde_json::{Value, json};
use workspace::{Workspace, item::Item};

use crate::{HttpFetch, templates_dir};

/// Where the last chosen location is remembered, so the next project starts
/// from the same place.
const LAST_LOCATION_KEY: &str = "scaffold_panel.last_location";

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
    /// A list: one input per item, with a [+] to add another. The engine takes
    /// the items as an array, so no text has to be split or joined.
    List(Vec<Entity<InputState>>),
}

/// The hint in row `row` (counting from 0) of a list field: that row's default
/// when the prompt has one, else the prompt's label made singular, numbered:
/// "Web part names" gives "Web part 1", "Web part 2".
fn row_placeholder(prompt: &Prompt, row: usize) -> String {
    if let Some(Value::Array(items)) = &prompt.default
        && let Some(Value::String(item)) = items.get(row)
    {
        return item.clone();
    }
    let label = prompt.label.clone().unwrap_or_else(|| prompt.key.clone());
    let singular = label
        .trim_end_matches(" names")
        .trim_end_matches(" name");
    let singular = singular.strip_suffix('s').unwrap_or(singular);
    format!("{singular} {}", row + 1)
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
    /// The plan: what would be written, and what would be offered afterwards.
    Valid {
        folder: String,
        /// Every path that would be written, relative to the new folder.
        files: Vec<String>,
        bytes: u64,
        warnings: Vec<String>,
        /// What the host would run afterwards, in words.
        post: Vec<String>,
    },
    /// One message per field, in the engine's words.
    Issues(Vec<(String, String)>),
    Failed(String),
}

/// The wizard's pages.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Options,
    Review,
    Location,
    Create,
}

impl Step {
    const ALL: [(Step, &'static str); 4] = [
        (Step::Options, "Options"),
        (Step::Review, "Review"),
        (Step::Location, "Location"),
        (Step::Create, "Create"),
    ];

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|(step, _)| *step == self)
            .unwrap_or(0)
    }
}

/// What the Location page knows about the chosen folder: where the project
/// would go, whether that is taken, and the warnings that depend on the place
/// (a long path, a solution inside another).
enum LocationCheck {
    Idle,
    Checking,
    Ready {
        target: PathBuf,
        exists: bool,
        warnings: Vec<String>,
    },
    Failed(String),
}

/// A project that has been written.
struct Created {
    target: PathBuf,
    files: usize,
    bytes: u64,
    /// What the host would run afterwards, in words.
    post: Vec<String>,
    /// The plan asked for the dependencies to be installed ("Run install
    /// afterwards" was on).
    install_requested: bool,
}

enum Creation {
    Idle,
    Running {
        done: usize,
        total: usize,
        path: String,
    },
    Done(Created),
    Failed(String),
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

/// The plan for `answers` at the chosen `location`: where the project would
/// go and what depends on the place. Writes nothing; runs off the main thread.
fn run_location_plan(
    manifest: &Manifest,
    dir: &PathBuf,
    answers: &Answers,
    location: &Path,
) -> LocationCheck {
    let source = DirSource::new(dir);
    match plan(manifest, &source, answers, PlanOptions::new(location)) {
        Ok(planned) => LocationCheck::Ready {
            exists: planned.target.exists(),
            warnings: planned.warnings.iter().map(|w| w.to_string()).collect(),
            target: planned.target.clone(),
        },
        Err(error) => LocationCheck::Failed(error.to_string()),
    }
}

/// Plans and writes the project at `location`. The engine refuses an existing
/// folder and removes the one it made when a write fails. Runs off the main
/// thread; `on_progress` is called once per file.
fn run_create(
    manifest: &Manifest,
    dir: &PathBuf,
    answers: &Answers,
    location: &Path,
    on_progress: impl FnMut(WriteProgress),
) -> Result<Created, String> {
    let source = DirSource::new(dir);
    let planned = plan(manifest, &source, answers, PlanOptions::new(location))
        .map_err(|error| error.to_string())?;
    let post = planned
        .post
        .iter()
        .map(|step| describe_post(step, &planned.target))
        .collect();
    let install_requested = planned
        .post
        .iter()
        .any(|step| step.action == PostAction::Install);
    let outcome = apply(&planned, on_progress).map_err(|error| error.to_string())?;
    Ok(Created {
        target: outcome.root,
        files: outcome.files_written,
        bytes: outcome.bytes_written,
        post,
        install_requested,
    })
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
    step: Step,
    /// Where the project will be created (its parent folder). Starts at the
    /// last one used, else the home folder.
    location: Option<PathBuf>,
    location_check: LocationCheck,
    creation: Creation,
    /// To open the new project as a workspace.
    workspace: WeakEntity<Workspace>,
    /// The review's file tree, made on the main thread from `Check::Valid`.
    review_tree: Option<Entity<TreeState>>,
    /// Bumped whenever another version is chosen, so a slow answer for the
    /// old one is ignored.
    generation: u64,
    /// One per input of the form, so typing re-draws it and the validation under
    /// each field follows what is typed. Rebuilt with the form.
    field_subscriptions: Vec<Subscription>,
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

/// What to say when a downloaded template cannot be read. A manifest this
/// version of the engine does not understand is what a template made for a newer
/// Forge Super looks like; a damaged one looks the same, so both are named.
fn describe_resolve_error(error: ScaffoldError) -> String {
    match error {
        ScaffoldError::Manifest(detail) => format!(
            "This template needs a newer version of Forge Super than this one, or it is damaged \
             ({detail}). Check for template updates, or update Forge Super."
        ),
        other => other.to_string(),
    }
}

/// A post step in words. The engine only describes these; the panel never runs
/// them itself (an install goes through the Script Runner, behind the trust gate).
fn describe_post(step: &PostStepDescriptor, target: &Path) -> String {
    let place = match step.cwd.strip_prefix(target) {
        Ok(inside) if inside.as_os_str().is_empty() => "the new folder".to_string(),
        Ok(inside) => format!("{} in the new folder", inside.display()),
        Err(_) => step.cwd.display().to_string(),
    };
    match step.action {
        PostAction::Install => {
            let manager = step
                .package_manager
                .map(|manager| format!(" with {}", format!("{manager:?}").to_lowercase()))
                .unwrap_or_default();
            format!("Install the dependencies{manager}, in {place}")
        }
        PostAction::ComposeUp => format!("Start the containers (docker compose up -d), in {place}"),
        _ => format!("{:?}, in {place}", step.action),
    }
}

/// The planned paths as a folder tree: folders first, then files, each sorted.
/// The top two levels start open.
fn tree_items(paths: &[String]) -> Vec<TreeItem> {
    #[derive(Default)]
    struct Node {
        children: BTreeMap<String, Node>,
    }

    let mut root = Node::default();
    for path in paths {
        let mut node = &mut root;
        for part in path.split('/').filter(|part| !part.is_empty()) {
            node = node.children.entry(part.to_string()).or_default();
        }
    }

    fn convert(prefix: &str, node: &Node, depth: usize) -> Vec<TreeItem> {
        let mut entries: Vec<(&String, &Node)> = node.children.iter().collect();
        // Folders (nodes with children) before files.
        entries.sort_by_key(|(name, child)| (child.children.is_empty(), name.to_lowercase()));
        entries
            .into_iter()
            .map(|(name, child)| {
                let id = format!("{prefix}{name}");
                let item = TreeItem::new(id.clone(), name.clone());
                if child.children.is_empty() {
                    item
                } else {
                    item.children(convert(&format!("{id}/"), child, depth + 1))
                        .expanded(depth < 2)
                }
            })
            .collect()
    }
    convert("", &root, 0)
}

/// The engine's verdict on `answers`: `plan()` into a scratch location, which
/// writes nothing. Blocks on the disk: runs off the main thread.
fn run_plan(manifest: &Manifest, dir: &PathBuf, answers: &Answers) -> Check {
    let location = std::env::temp_dir();
    let source = DirSource::new(dir);
    match plan(manifest, &source, answers, PlanOptions::new(&location)) {
        Ok(planned) => {
            let mut files: Vec<String> = planned.files.paths().map(str::to_string).collect();
            files.sort();
            Check::Valid {
                folder: planned.folder_name.clone(),
                files,
                bytes: planned.files.total_bytes(),
                warnings: planned.warnings.iter().map(|w| w.to_string()).collect(),
                post: planned
                    .post
                    .iter()
                    .map(|step| describe_post(step, &planned.target))
                    .collect(),
            }
        }
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
        workspace: WeakEntity<Workspace>,
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
            step: Step::Options,
            location: None,
            location_check: LocationCheck::Idle,
            creation: Creation::Idle,
            workspace,
            review_tree: None,
            generation: 0,
            field_subscriptions: Vec::new(),
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
        self.review_tree = None;
        self.step = Step::Options;
        self.location_check = LocationCheck::Idle;
        self.creation = Creation::Idle;
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
                        // What was downloaded is the index's revision: no update is
                        // pending for it any more.
                        for known in this.versions.iter_mut().filter(|v| v.version == version) {
                            known.installed = true;
                            known.update_available = false;
                            if known.available_revision.is_some() {
                                known.installed_revision = known.available_revision;
                            }
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
                        .map_err(describe_resolve_error)?;
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
        // Typing in any input re-draws the form, which is what makes the
        // validation live.
        let inputs: Vec<Entity<InputState>> = fields
            .iter()
            .flat_map(|field| match &field.control {
                Control::Text(state) => vec![state.clone()],
                Control::List(rows) => rows.clone(),
                _ => Vec::new(),
            })
            .collect();
        self.field_subscriptions.clear();
        for input in &inputs {
            self.watch_input(input, cx);
        }
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
                    // One empty row to start with; an empty list leaves the engine's
                    // default in place.
                    PromptKind::List => {
                        let placeholder = row_placeholder(prompt, 0);
                        Control::List(vec![cx.new(|cx| {
                            InputState::new(window, cx).placeholder(placeholder)
                        })])
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
                        Control::Text(state)
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
                Control::List(rows) => {
                    // Each row is one item, exactly as the validation saw it.
                    let items: Vec<String> = rows
                        .iter()
                        .map(|row| row.read(cx).value().trim().to_string())
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
        self.review_tree = None;
        cx.notify();
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let verdict = cx
                .background_spawn(async move { run_plan(&manifest, &dir, &answers) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    // The tree is an entity, so it is made here, not on the
                    // background thread that planned.
                    this.review_tree = match &verdict {
                        Check::Valid { files, .. } => {
                            let items = tree_items(files);
                            Some(cx.new(|cx| TreeState::new(cx).items(items)))
                        }
                        _ => None,
                    };
                    // Valid answers move on to the review; anything else stays
                    // on the options, with the messages under the fields.
                    if matches!(verdict, Check::Valid { .. }) {
                        this.step = Step::Review;
                    }
                    this.check = verdict;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Re-draws the form whenever `input` changes, so the messages under the
    /// fields follow what is typed.
    fn watch_input(&mut self, input: &Entity<InputState>, cx: &mut Context<Self>) {
        let subscription = cx.subscribe(input, |_, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        });
        self.field_subscriptions.push(subscription);
    }

    /// Adds an empty row to the list field at `index`, directly below row `after`
    /// (that row's [+] button).
    fn add_list_row(
        &mut self,
        index: usize,
        after: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = {
            let Some(Loaded { fields, .. }) = &self.loaded else {
                return;
            };
            let Some(Field {
                prompt,
                control: Control::List(rows),
            }) = fields.get(index)
            else {
                return;
            };
            row_placeholder(prompt, rows.len())
        };
        let row = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        if let Some(Loaded { fields, .. }) = &mut self.loaded
            && let Some(Field {
                control: Control::List(rows),
                ..
            }) = fields.get_mut(index)
        {
            rows.insert((after + 1).min(rows.len()), row.clone());
        }
        self.watch_input(&row, cx);
        cx.notify();
    }

    /// Whether any row of a list is still empty: another row is not added then.
    fn has_empty_row(rows: &[Entity<InputState>], cx: &App) -> bool {
        rows.iter()
            .any(|row| row.read(cx).value().trim().is_empty())
    }

    /// What is wrong with `field` as it stands, by the engine's own rules and
    /// nothing else: `(row, message)`, with `row` set for a list item. Empty
    /// fields are left alone (an empty text uses the default; "at least one" is
    /// the engine's to say at the end). Duplicates in a list are caught
    /// ignoring case, as the engine does, since the names become files.
    fn field_issues(field: &Field, cx: &App) -> Vec<(Option<usize>, String)> {
        match (&field.control, &field.prompt.rule) {
            (Control::Text(state), Some(rule)) => {
                let text = state.read(cx).value().trim().to_string();
                if text.is_empty() {
                    return Vec::new();
                }
                let issues = match rule {
                    NameRule::SolutionName => validate_solution_name(&text),
                    NameRule::PackageName => validate_package_name(&text),
                    _ => Vec::new(),
                };
                issues
                    .into_iter()
                    .map(|issue| (None, issue.message))
                    .collect()
            }
            (Control::List(rows), rule) => {
                // The rows that have something in them, with their row number.
                let filled: Vec<(usize, String)> = rows
                    .iter()
                    .enumerate()
                    .filter_map(|(row, state)| {
                        let text = state.read(cx).value().trim().to_string();
                        (!text.is_empty()).then_some((row, text))
                    })
                    .collect();
                if let Some(NameRule::WebpartNames) = rule {
                    let names: Vec<String> = filled.iter().map(|(_, name)| name.clone()).collect();
                    validate_webpart_names(&names)
                        .into_iter()
                        .filter_map(|issue| {
                            // `webparts[2]`: the third filled row. An issue about the
                            // list as a whole has no number and is left to the end.
                            let position = issue
                                .field
                                .split_once('[')?
                                .1
                                .strip_suffix(']')?
                                .parse::<usize>()
                                .ok()?;
                            Some((Some(filled.get(position)?.0), issue.message))
                        })
                        .collect()
                } else {
                    let mut seen = HashSet::new();
                    filled
                        .into_iter()
                        .filter_map(|(row, text)| {
                            (!seen.insert(text.to_lowercase())).then(|| {
                                (Some(row), format!("'{text}' is already in the list"))
                            })
                        })
                        .collect()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Takes row `row` out of the list field at `index` (a row's [-] button).
    /// The last remaining row stays: a list always has one to type in.
    fn remove_list_row(&mut self, index: usize, row: usize, cx: &mut Context<Self>) {
        let Some(Loaded { fields, .. }) = &mut self.loaded else {
            return;
        };
        if let Some(Field {
            control: Control::List(rows),
            ..
        }) = fields.get_mut(index)
            && rows.len() > 1
            && row < rows.len()
        {
            rows.remove(row);
            cx.notify();
        }
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

    /// Moves to the Location page. The first time it starts from the last
    /// location used, else the home folder.
    fn go_to_location(&mut self, cx: &mut Context<Self>) {
        self.step = Step::Location;
        self.location_check = LocationCheck::Idle;
        cx.notify();
        if self.location.is_some() {
            self.check_location(cx);
            return;
        }
        let kvp = KeyValueStore::global(cx);
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_spawn(async move {
                    kvp.read_kvp(LAST_LOCATION_KEY)
                        .ok()
                        .flatten()
                        .map(PathBuf::from)
                        .filter(|path| path.is_dir())
                })
                .await;
            this.update(cx, |this, cx| {
                if this.location.is_none() {
                    this.location =
                        Some(saved.unwrap_or_else(|| util::paths::home_dir().clone()));
                }
                this.check_location(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Plans at the chosen location to learn where the project would go, whether
    /// that is taken, and the warnings that depend on the place.
    fn check_location(&mut self, cx: &mut Context<Self>) {
        let (Some(loaded), Some(location)) = (&self.loaded, self.location.clone()) else {
            return;
        };
        let (manifest, dir) = (loaded.manifest.clone(), loaded.dir.clone());
        let answers = self.collect_answers(cx);
        self.location_check = LocationCheck::Checking;
        cx.notify();
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let asked = location.clone();
            let result = cx
                .background_spawn(async move {
                    run_location_plan(&manifest, &dir, &answers, &location)
                })
                .await;
            this.update(cx, |this, cx| {
                // Dropped if another version or another folder was chosen meanwhile.
                if this.generation == generation && this.location.as_ref() == Some(&asked) {
                    this.location_check = result;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn pick_location(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose Location".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| {
                    this.location = Some(path);
                    this.check_location(cx);
                })
                .ok();
            }
        })
        .detach();
    }

    fn remember_location(location: &Path, cx: &mut Context<Self>) {
        let kvp = KeyValueStore::global(cx);
        let value = location.to_string_lossy().into_owned();
        cx.background_spawn(async move {
            kvp.write_kvp(LAST_LOCATION_KEY.to_string(), value).await.ok();
        })
        .detach();
    }

    /// Writes the project. The engine reports each file as it goes; the page
    /// shows it, and a failed write leaves no half-made folder behind.
    fn create_project(&mut self, cx: &mut Context<Self>) {
        let (Some(loaded), Some(location)) = (&self.loaded, self.location.clone()) else {
            return;
        };
        if matches!(self.creation, Creation::Running { .. }) {
            return;
        }
        let (manifest, dir) = (loaded.manifest.clone(), loaded.dir.clone());
        let answers = self.collect_answers(cx);
        self.step = Step::Create;
        self.creation = Creation::Running {
            done: 0,
            total: 0,
            path: String::new(),
        };
        cx.notify();

        let (sender, mut receiver) = futures::channel::mpsc::unbounded::<WriteProgress>();
        let chosen = location.clone();
        let work = cx.background_spawn(async move {
            run_create(&manifest, &dir, &answers, &location, move |progress| {
                sender.unbounded_send(progress).ok();
            })
        });
        cx.spawn(async move |this, cx| {
            // Ends when the writer is done and has dropped its sender.
            while let Some(progress) = receiver.next().await {
                this.update(cx, |this, cx| {
                    this.creation = Creation::Running {
                        done: progress.index,
                        total: progress.total,
                        path: progress.path,
                    };
                    cx.notify();
                })
                .ok();
            }
            let result = work.await;
            this.update(cx, |this, cx| {
                this.creation = match result {
                    Ok(created) => {
                        Self::remember_location(&chosen, cx);
                        // A new window always offers to install a project that has a
                        // package.json, once it is trusted, even if this folder name
                        // was used (and offered) before. "Run install afterwards"
                        // decides whether it also starts by itself, without a click.
                        npm_bootstrap::prepare_install_offer(
                            &created.target,
                            created.install_requested,
                            cx,
                        );
                        Creation::Done(created)
                    }
                    Err(error) => Creation::Failed(error),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens the new project in a window of its own, so the work in this one
    /// stays where it is. A new folder opens in Restricted Mode like any other.
    fn open_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Creation::Done(created) = &self.creation else {
            return;
        };
        let target = created.target.clone();
        let workspace = self.workspace.clone();
        // Deferred until this click has finished. Opening a workspace looks at
        // every item in the window to see if any is dirty, and this view is one
        // of them: asked from inside this view's own update, that reads it while
        // it is already borrowed, and gpui panics.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace
                        .open_workspace_for_paths(
                            workspace::OpenMode::NewWindow,
                            vec![target],
                            window,
                            cx,
                        )
                        .detach_and_log_err(cx);
                })
                .ok();
        });
    }

    /// A click on an earlier step of the stepper goes back to it. Not while the
    /// project is being written.
    fn go_to_step(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.step.index() || matches!(self.creation, Creation::Running { .. }) {
            return;
        }
        let Some((step, _)) = Step::ALL.get(index) else {
            return;
        };
        self.step = *step;
        self.creation = Creation::Idle;
        if self.step == Step::Location {
            self.check_location(cx);
        }
        cx.notify();
    }

    /// The wizard: a stepper over the page for the current step.
    fn render_wizard(&self, muted: gpui::Hsla, cx: &mut Context<Self>) -> gpui::AnyElement {
        let current = self.step.index();
        // Later steps are reached only with "Next", which plans again, so a page
        // never shows answers that have since been edited. An earlier step can
        // be clicked to go back.
        let stepper = Stepper::new("scaffold-steps")
            .selected_index(current)
            .items(Step::ALL.iter().enumerate().map(|(index, (_, label))| {
                StepperItem::new().disabled(index > current).child(*label)
            }))
            .on_click(cx.listener(|this, index: &usize, _, cx| this.go_to_step(*index, cx)));
        let page = match self.step {
            Step::Options => v_flex()
                .gap_3()
                .children(self.render_template_notices(cx))
                .children(self.render_node_notice(muted))
                .child(self.render_form(cx))
                .into_any_element(),
            Step::Review => self.render_review_page(cx),
            Step::Location => self.render_location_page(muted, cx),
            Step::Create => self.render_create_page(muted, cx),
        };
        v_flex().gap_4().child(stepper).child(page).into_any_element()
    }

    /// What to know about this template before using it: that it is a legacy one,
    /// and that a newer revision of it is available.
    fn render_template_notices(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut notices = Vec::new();
        if self.generator == crate::LEGACY_SPFX_GENERATOR {
            notices.push(
                v_flex()
                    .gap_1()
                    .child(Alert::warning(
                        "scaffold-legacy",
                        "Legacy template. It was built from the output of Microsoft's yo \
                         generator and then edited. Whether such output may be redistributed \
                         has not been confirmed by Microsoft, and this project is not \
                         affiliated with or endorsed by Microsoft.",
                    ))
                    .child(
                        h_flex().child(
                            Button::new("scaffold-legacy-notice")
                                .ghost()
                                .small()
                                .label("Read the notice")
                                .on_click(|_, _, cx| {
                                    cx.open_url(
                                        "https://github.com/mentenaz/Forge.Scaffold.Templates#readme",
                                    );
                                }),
                        ),
                    )
                    .into_any_element(),
            );
        }
        // Only a downloaded template can be updated: a folder of the user's own
        // is never replaced.
        let update = self
            .versions
            .iter()
            .find(|known| Some(&known.version) == self.chosen.as_ref())
            .filter(|known| known.installed && !known.local && known.update_available);
        if let Some(known) = update {
            let message = match (known.installed_revision, known.available_revision) {
                (Some(from), Some(to)) => format!(
                    "A newer revision of this template is available (revision {from} to {to})."
                ),
                _ => "A newer revision of this template is available.".to_string(),
            };
            notices.push(
                v_flex()
                    .gap_1()
                    .child(Alert::info("scaffold-update", message))
                    .child(
                        h_flex().child(
                            Button::new("scaffold-update-template")
                                .outline()
                                .small()
                                .label("Update this template")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.download(window, cx)
                                })),
                        ),
                    )
                    .into_any_element(),
            );
        }
        notices
    }

    /// The Location page: where the project goes, and whether that works.
    fn render_location_page(&self, muted: gpui::Hsla, cx: &mut Context<Self>) -> gpui::AnyElement {
        let place = self
            .location
            .as_ref()
            .map_or_else(|| "…".to_string(), |path| path.display().to_string());
        let (status, can_create) = match &self.location_check {
            LocationCheck::Idle | LocationCheck::Checking => {
                (loading_row("Checking the location…", muted), false)
            }
            LocationCheck::Failed(error) => (
                Alert::error("scaffold-location-failed", error.clone()).into_any_element(),
                false,
            ),
            LocationCheck::Ready {
                target,
                exists,
                warnings,
            } => {
                let mut column = v_flex().gap_2().child(
                    div()
                        .text_sm()
                        .child(format!("The project will be created at {}", target.display())),
                );
                if *exists {
                    column = column.child(Alert::error(
                        "scaffold-exists",
                        "A folder with this name already exists there. Choose another \
                         location, or go back and change the name.",
                    ));
                }
                column = column.children(warnings.iter().enumerate().map(|(index, warning)| {
                    Alert::warning(("scaffold-location-warning", index), warning.clone())
                }));
                (column.into_any_element(), !*exists)
            }
        };
        v_flex()
            .gap_3()
            .child(
                field().label("Create in").child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().flex_1().min_w_0().truncate().text_sm().child(place))
                        .child(
                            Button::new("scaffold-choose-folder")
                                .outline()
                                .small()
                                .label("Choose folder…")
                                .on_click(cx.listener(|this, _, _, cx| this.pick_location(cx))),
                        ),
                ),
            )
            .child(status)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("scaffold-location-back")
                            .outline()
                            .label("Back")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step = Step::Review;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("scaffold-create")
                            .primary()
                            .label("Create project")
                            .disabled(!can_create)
                            .on_click(cx.listener(|this, _, _, cx| this.create_project(cx))),
                    ),
            )
            .into_any_element()
    }

    /// The Create page: progress while writing, then what was made.
    fn render_create_page(&self, muted: gpui::Hsla, cx: &mut Context<Self>) -> gpui::AnyElement {
        match &self.creation {
            Creation::Idle => div().into_any_element(),
            Creation::Running { done, total, path } => {
                let percent = if *total == 0 {
                    0.0
                } else {
                    *done as f32 / *total as f32 * 100.0
                };
                v_flex()
                    .gap_2()
                    .child(div().text_sm().child("Creating the project…"))
                    .child(Progress::new("scaffold-progress").value(percent))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .truncate()
                            .child(format!("{done} of {total}: {path}")),
                    )
                    .into_any_element()
            }
            Creation::Failed(error) => v_flex()
                .gap_3()
                .child(Alert::error("scaffold-create-failed", error.clone()))
                .child(
                    h_flex().child(
                        Button::new("scaffold-create-back")
                            .outline()
                            .label("Back")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step = Step::Location;
                                this.creation = Creation::Idle;
                                this.check_location(cx);
                                cx.notify();
                            })),
                    ),
                )
                .into_any_element(),
            Creation::Done(created) => {
                let target = created.target.clone();
                v_flex()
                    .gap_3()
                    .child(Alert::success(
                        "scaffold-created",
                        format!(
                            "Created {} ({} files, {}).",
                            created.target.display(),
                            created.files,
                            human_size(created.bytes)
                        ),
                    ))
                    .when(!created.post.is_empty(), |column| {
                        column.child(
                            v_flex()
                                .gap_1()
                                .child(div().text_sm().font_semibold().child("Afterwards"))
                                .children(created.post.iter().map(|step| {
                                    div().text_xs().text_color(muted).child(format!("• {step}"))
                                }))
                                .child(div().text_xs().text_color(muted).child(
                                    if created.install_requested {
                                        "Open the project: once you trust it, the install \
                                         starts by itself and its output appears in the \
                                         Script Runner. Anything else here you run yourself."
                                    } else {
                                        "Open the project: once you trust it, Forge offers to \
                                         install the dependencies. Anything else here you run \
                                         yourself."
                                    },
                                )),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("scaffold-open-project")
                                    .primary()
                                    .label("Open project")
                                    .on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.open_project(window, cx)
                                        }),
                                    ),
                            )
                            .child(
                                Button::new("scaffold-reveal")
                                    .outline()
                                    .label("Show in file explorer")
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.reveal_path(&target);
                                    })),
                            )
                            .child(
                                Button::new("scaffold-another")
                                    .ghost()
                                    .label("Create another")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.step = Step::Options;
                                        this.creation = Creation::Idle;
                                        this.check = Check::Idle;
                                        this.review_tree = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    /// The Review page: the review, and a way back to the options.
    fn render_review_page(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Check::Valid {
            folder,
            files,
            bytes,
            warnings,
            post,
        } = &self.check
        else {
            return div().into_any_element();
        };
        v_flex()
            .gap_3()
            .child(self.render_review(folder, files, *bytes, warnings, post, cx))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("scaffold-back")
                            .outline()
                            .label("Back")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.step = Step::Options;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("scaffold-next-location")
                            .primary()
                            .label("Next: choose location")
                            .on_click(cx.listener(|this, _, _, cx| this.go_to_location(cx))),
                    ),
            )
            .into_any_element()
    }

    /// The review: what would be created, the warnings, and what would be
    /// offered afterwards. Shown before anything is written.
    fn render_review(
        &self,
        folder: &str,
        files: &[String],
        bytes: u64,
        warnings: &[String],
        post: &[String],
        cx: &App,
    ) -> gpui::AnyElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let size = human_size(bytes);

        // The tree is a virtual list, so it needs a height of its own.
        let tree_view = match &self.review_tree {
            Some(state) => div()
                .h(px(320.))
                .border_1()
                .border_color(border)
                .rounded_md()
                .child(tree(state, |ix, entry, selected, _window, cx| {
                    let theme = cx.theme();
                    let is_folder = entry.is_folder();
                    let colour = if is_folder {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    };
                    ListItem::new(ix)
                        .selected(selected)
                        .px(px(12.0 * entry.depth() as f32 + 8.0))
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .when(is_folder, |row| row.child(Icon::new(IconName::Folder).small()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(colour)
                                        .child(entry.item().label.clone()),
                                ),
                        )
                }))
                .into_any_element(),
            None => div().into_any_element(),
        };

        v_flex()
            .gap_3()
            .child(Alert::success(
                "scaffold-valid",
                format!(
                    "These answers are valid. It would create \"{folder}\": {} files, {size}.",
                    files.len()
                ),
            ))
            .children(warnings.iter().enumerate().map(|(index, warning)| {
                Alert::warning(("scaffold-warning", index), warning.clone())
            }))
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().font_semibold().child("Files"))
                    .child(tree_view),
            )
            .when(!post.is_empty(), |column| {
                column.child(
                    v_flex()
                        .gap_1()
                        .child(div().text_sm().font_semibold().child("Afterwards"))
                        .children(post.iter().map(|step| {
                            div().text_xs().text_color(muted).child(format!("• {step}"))
                        }))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child("Nothing runs on its own: you decide when."),
                        ),
                )
            })
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Next you choose where to create it. Nothing is written yet."),
            )
            .into_any_element()
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
        // Set when any field that is showing has something wrong with it right
        // now: "Next" waits until it is put right.
        let mut blocked = false;
        for (index, field_state) in loaded.fields.iter().enumerate() {
            let prompt = &field_state.prompt;
            if let Some(when) = &prompt.when
                && !when.matches(&answers)
            {
                continue;
            }
            let label = prompt.label.clone().unwrap_or_else(|| prompt.key.clone());
            // What is wrong as it is typed, then (if nothing is) what the engine
            // said on the last "Next".
            let live = Self::field_issues(field_state, cx);
            blocked |= !live.is_empty();
            let message = match (&field_state.control, live.iter().find(|(row, _)| row.is_none())) {
                (_, Some((_, message))) => Some(message.clone()),
                (Control::List(_), None) if !live.is_empty() => None,
                _ => issues
                    .iter()
                    .find(|(key, _)| key == &prompt.key)
                    .map(|(_, message)| message.clone()),
            };
            let control = match &field_state.control {
                Control::Text(state) => {
                    let input = Input::new(state);
                    if prompt.sensitive {
                        input.mask_toggle().into_any_element()
                    } else {
                        input.into_any_element()
                    }
                }
                // [item 1][+]  then  [item 2][-]  and so on: the first row's [+]
                // adds another row, every later row has a [-] to take it away.
                Control::List(rows) => v_flex()
                    .gap_1()
                    .children(rows.iter().enumerate().map(|(row, state)| {
                        let row_message = live
                            .iter()
                            .find(|(at, _)| *at == Some(row))
                            .map(|(_, message)| message.clone());
                        // [-] takes this row away (the last one stays), [+] adds an
                        // empty row directly below it, so a list can grow from any row.
                        let remove = Button::new(("scaffold-list-remove", index * 1000 + row))
                            .ghost()
                            .small()
                            .icon(IconName::Minus)
                            .tooltip("Remove")
                            .disabled(rows.len() < 2)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_list_row(index, row, cx)
                            }));
                        let add = Button::new(("scaffold-list-add", index * 1000 + row))
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip("Add another below")
                            // Not while a row is still empty: that would only pile
                            // up blank rows.
                            .disabled(Self::has_empty_row(rows, cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.add_list_row(index, row, window, cx)
                            }));
                        v_flex()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(div().flex_1().child(Input::new(state)))
                                    .child(remove)
                                    .child(add),
                            )
                            .children(row_message.map(|message| {
                                div().text_xs().text_color(danger).child(message)
                            }))
                    }))
                    .into_any_element(),
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
            // Valid answers are shown on the Review page, not under the form.
            Check::Valid { .. } => None,
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
                            "Next: review"
                        })
                        // Not while a field is wrong: the engine would only refuse it.
                        .disabled(running || blocked)
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
            Status::Ready => self.render_wizard(muted, cx),
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
                    // Keeps its own height: without this the column shrinks to
                    // fit the tab and the content spills out instead of the
                    // tab scrolling.
                    .flex_shrink_0()
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

//! The SQL workbench: a schema tree (left, the same tree the explorer uses,
//! plus a right-click context menu that inserts ready-made statements) and
//! an `Editor` + Run button + results table (right).
//!
//! `WorkbenchTab` is a real **workspace `Item`**, opened via
//! `workbench::open` into the active pane — a genuine tab in the main editor
//! area, the same pattern `npm_manager_panel` uses (`workspace::Item` +
//! `workspace.add_item_to_active_pane`), not a view swapped inside the
//! bottom-dock `DatabasePanel`. `WorkbenchTab` itself is a thin shell: it
//! holds a strong `Entity<DatabasePanel>` + the `SchemaKey` it's for, and
//! delegates all rendering back to `DatabasePanel::render_workbench_body` —
//! the actual state (`WorkbenchState`: editor, results, history) still lives
//! on `DatabasePanel`, one per `(connection, database)`, so it isn't lost if
//! the tab is closed and reopened.
//!
//! Autocomplete is schema-driven, not LSP-backed: `SqlSchemaCompletionProvider`
//! implements the editor's plain `CompletionProvider` trait directly (no
//! spawned language server, no JSON-RPC) — it just matches the word at the
//! cursor against a `Rc<RefCell<Vec<String>>>` of the active schema's table
//! and column names, refreshed whenever the schema is (re)fetched.

use std::cell::RefCell;
use std::rc::Rc;

use database_backend::{ConnectionId, ConnectionRegistry, QueryResult};
use editor::{CompletionProvider, Editor};
use gpui::{
    App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Task, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName as GIconName, Sizable as _, Size,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::PopupMenuItem,
    resizable::{h_resizable, resizable_panel},
    spinner::Spinner,
    table::{Table, TableBody, TableCell, TableHead, TableHeader, TableRow},
    tree::{TreeEntry, TreeState, tree},
    v_flex,
};
use language::{self, ToOffset};
use project::{self, CompletionDisplayOptions};
use ui::{Color, Label, LabelCommon as _, LabelSize};
use workspace::{Item, Workspace};

use crate::{DatabasePanel, SchemaKey, SchemaState};

/// A row/statement cap on the results table — this crate doesn't virtualize
/// the results grid (see `render_results_table`'s doc comment), so an
/// unbounded result set would mean rendering every row's cells up front.
const MAX_DISPLAYED_ROWS: usize = 500;

fn measure_workbench_text_width(text: &str, window: &mut Window) -> Pixels {
    let text = SharedString::from(text.to_owned());
    let font_size = gpui::rems(0.875).to_pixels(window.rem_size());
    let run = window.text_style().to_run(text.len());
    window
        .text_system()
        .shape_line(text, font_size, &[run], None)
        .width()
}

fn workbench_column_widths(result: &QueryResult, window: &mut Window) -> Vec<Pixels> {
    let mut widths = result
        .columns
        .iter()
        .map(|name| measure_workbench_text_width(name, window))
        .collect::<Vec<_>>();

    for row in result.rows.iter().take(MAX_DISPLAYED_ROWS) {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                let value = cell.as_deref().unwrap_or("NULL");
                let measured = measure_workbench_text_width(value, window);
                if measured > *width {
                    *width = measured;
                }
            }
        }
    }

    widths
        .into_iter()
        .map(|width| width + gpui::px(16.))
        .collect()
}

fn sql_string_literal(value: &str) -> String {
    let mut literal = String::with_capacity(value.len() + 2);
    literal.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            literal.push('\'');
        }
        literal.push(ch);
    }
    literal.push('\'');
    literal
}

fn sql_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn serialize_query_result_as_sql(result: &QueryResult) -> String {
    if result.columns.is_empty() {
        return String::new();
    }

    let identifiers = result
        .columns
        .iter()
        .map(|column| sql_identifier(column))
        .collect::<Vec<_>>();

    if result.rows.is_empty() {
        let values = identifiers
            .iter()
            .map(|identifier| format!("NULL AS {identifier}"))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("SELECT {values} WHERE 1 = 0;\n");
    }

    let mut sql = String::new();
    for (row_index, row) in result.rows.iter().enumerate() {
        if row_index > 0 {
            sql.push_str("\nUNION ALL\n");
        }
        sql.push_str("SELECT ");
        for (column_index, identifier) in identifiers.iter().enumerate() {
            if column_index > 0 {
                sql.push_str(", ");
            }
            let value = row.get(column_index).and_then(|value| value.as_deref());
            match value {
                Some(value) => sql.push_str(&sql_string_literal(value)),
                None => sql.push_str("NULL"),
            }
            if row_index == 0 {
                sql.push_str(" AS ");
                sql.push_str(identifier);
            }
        }
    }
    sql.push_str(";\n");
    sql
}

fn serialize_query_result_as_json(result: &QueryResult) -> String {
    let rows = result
        .rows
        .iter()
        .map(|row| {
            result.columns.iter().enumerate().fold(
                serde_json::Map::new(),
                |mut object, (index, column)| {
                    object.insert(column.clone(), row.get(index).cloned().flatten().into());
                    object
                },
            )
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string())
}

fn export_text(content: String, suggested_name: &str, cx: &mut App) {
    let directory = std::path::PathBuf::default();
    let dialog = cx.prompt_for_new_path(&directory, Some(suggested_name));
    cx.spawn(async move |cx| {
        let path = match dialog.await {
            Ok(Ok(Some(path))) => path,
            Ok(Ok(None)) | Err(_) => return,
            Ok(Err(error)) => {
                log::error!("database_panel: failed to open export dialog: {error}");
                return;
            }
        };
        if let Err(error) = cx
            .background_spawn(async move { std::fs::write(path, content) })
            .await
        {
            log::error!("database_panel: failed to write exported query result: {error}");
        }
    })
    .detach();
}

fn render_query_result_actions(key: &SchemaKey, result: &QueryResult) -> impl IntoElement {
    let result = Rc::new(result.clone());
    let copy_sql_result = result.clone();
    let copy_json_result = result.clone();
    let export_sql_result = result.clone();
    let export_json_result = result.clone();

    h_flex()
        .gap_1()
        .flex_shrink_0()
        .child(
            Button::new(format!("workbench-copy-sql-{}-{}", key.0.0, key.1))
                .with_size(Size::Small)
                .compact()
                .icon(GIconName::Copy)
                .label("Copy SQL")
                .tooltip("Copy all result rows as SQL")
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        serialize_query_result_as_sql(copy_sql_result.as_ref()),
                    ));
                }),
        )
        .child(
            Button::new(format!("workbench-copy-json-{}-{}", key.0.0, key.1))
                .with_size(Size::Small)
                .compact()
                .icon(GIconName::Copy)
                .label("Copy JSON")
                .tooltip("Copy all result rows as JSON")
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        serialize_query_result_as_json(copy_json_result.as_ref()),
                    ));
                }),
        )
        .child(
            Button::new(format!("workbench-export-sql-{}-{}", key.0.0, key.1))
                .with_size(Size::Small)
                .compact()
                .icon(GIconName::File)
                .label("Export SQL")
                .tooltip("Export all result rows as SQL")
                .on_click(move |_, _, cx| {
                    export_text(
                        serialize_query_result_as_sql(export_sql_result.as_ref()),
                        "query-results.sql",
                        cx,
                    );
                }),
        )
        .child(
            Button::new(format!("workbench-export-json-{}-{}", key.0.0, key.1))
                .with_size(Size::Small)
                .compact()
                .icon(GIconName::File)
                .label("Export JSON")
                .tooltip("Export all result rows as JSON")
                .on_click(move |_, _, cx| {
                    export_text(
                        serialize_query_result_as_json(export_json_result.as_ref()),
                        "query-results.json",
                        cx,
                    );
                }),
        )
}

/// How many past statements `history` keeps, per `(connection, database)`.
const MAX_HISTORY_ENTRIES: usize = 50;

/// One `(connection, database)`'s live workbench: its editor, autocomplete
/// candidates, and last run's outcome. Created lazily the first time the
/// workbench is opened for that key (see `DatabasePanel::open_workbench`);
/// `history` is seeded from `DatabasePanel::workbench_history` at that point.
pub(crate) struct WorkbenchState {
    editor: Entity<Editor>,
    completion_candidates: Rc<RefCell<Vec<String>>>,
    running: bool,
    result: Option<Result<QueryResult, String>>,
    history: Vec<String>,
}

/// Feeds the editor's autocomplete popover from a shared, mutable list of
/// candidate names instead of a language server — see this module's doc
/// comment. Matching is a plain case-insensitive prefix match on the word
/// immediately behind the cursor.
struct SqlSchemaCompletionProvider {
    candidates: Rc<RefCell<Vec<String>>>,
}

impl CompletionProvider for SqlSchemaCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<language::Buffer>,
        buffer_position: language::Anchor,
        _trigger: editor::CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<project::CompletionResponse>>> {
        let buffer = buffer.read(cx);
        let mut count_back = 0;
        for ch in buffer.reversed_chars_at(buffer_position) {
            if ch.is_alphanumeric() || ch == '_' {
                count_back += 1;
            } else {
                break;
            }
        }

        let start_anchor =
            buffer.anchor_before(buffer_position.to_offset(buffer).saturating_sub(count_back));
        let replace_range = start_anchor..buffer_position;
        let snapshot = buffer.text_snapshot();
        let query: String = snapshot.text_for_range(replace_range.clone()).collect();
        let normalized_query = query.to_lowercase();
        let candidates = self.candidates.borrow().clone();

        cx.background_spawn(async move {
            let completions = candidates
                .into_iter()
                .filter(|name| name.to_lowercase().starts_with(&normalized_query))
                .map(|name| project::Completion {
                    replace_range: replace_range.clone(),
                    new_text: name.clone(),
                    label: language::CodeLabel::plain(name, None),
                    documentation: None,
                    source: project::CompletionSource::Custom,
                    icon_path: None,
                    icon_color: None,
                    match_start: None,
                    snippet_deduplication_key: None,
                    insert_text_mode: None,
                    confirm: None,
                    group: None,
                })
                .collect();

            Ok(vec![project::CompletionResponse {
                completions,
                display_options: CompletionDisplayOptions {
                    dynamic_width: true,
                },
                is_incomplete: false,
            }])
        })
    }

    fn is_completion_trigger(
        &self,
        _buffer: &Entity<language::Buffer>,
        _position: language::Anchor,
        text: &str,
        _trigger_in_words: bool,
        _cx: &mut Context<Editor>,
    ) -> bool {
        text.chars()
            .last()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
    }
}

/// The schema object a workbench tree row belongs to, decoded from its
/// `TreeItem` id the same way `DatabasePanel::selected_schema_item` decodes
/// selection — a `view::`-prefixed id is a view, otherwise a table (a
/// column/index child's id still resolves to its owning table/view, since
/// the context menu offers table-level actions regardless of which row
/// under it was right-clicked).
fn tree_row_object_name(id: &str) -> Option<&str> {
    if let Some(rest) = id.strip_prefix("view::") {
        return rest.split_once("::").map(|(view, _)| view).or(Some(rest));
    }
    if let Some((table, _)) = id.split_once("::idx::") {
        return Some(table);
    }
    if let Some((table, _)) = id.split_once("::") {
        return Some(table);
    }
    Some(id)
}

impl DatabasePanel {
    /// Ensures a `WorkbenchState` exists for `key`, creating one (seeded from
    /// any persisted history) if this is the first time it's been opened
    /// this session. Called from `workbench::open` before the workspace tab
    /// is created/activated, so the tab always has state to render.
    pub(crate) fn ensure_workbench(
        &mut self,
        key: &SchemaKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.workbench_tree_states.contains_key(key) {
            if let Some(SchemaState::Loaded { tables, views }) = self.schemas.get(key) {
                let items = crate::schema_tree_items(tables, views, "");
                let tree_state = cx.new(|cx| TreeState::new(cx).items(items));
                self.workbench_tree_states.insert(key.clone(), tree_state);
            }
        }
        if self.workbenches.contains_key(key) {
            return;
        }
        let candidates = Rc::new(RefCell::new(self.schema_candidate_names(key)));
        let provider_candidates = candidates.clone();
        let language_registry = self
            .workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).app_state().languages.clone());
        let buffer = cx.new(|cx| language::Buffer::local("", cx));
        if let Some(language_registry) = language_registry.clone() {
            buffer.update(cx, |buffer, _cx| {
                buffer.set_language_registry(language_registry);
            });
        }
        let language_buffer = buffer.clone();
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer, None, window, cx);
            editor.set_completion_provider(Some(Rc::new(SqlSchemaCompletionProvider {
                candidates: provider_candidates,
            })));
            editor.set_show_completions_on_input(Some(true));
            editor.set_placeholder_text("Write SQL...", window, cx);
            editor
        });
        if let Some(language_registry) = language_registry {
            cx.spawn({
                let buffer = language_buffer;
                async move |_, cx| {
                    if let Ok(language) = language_registry.language_for_name("SQL").await {
                        buffer.update(cx, |buffer, cx| {
                            buffer.set_language_async(Some(language), cx);
                        });
                    }
                }
            })
            .detach();
        }
        let history = self.workbench_history.get(key).cloned().unwrap_or_default();
        self.workbenches.insert(
            key.clone(),
            WorkbenchState {
                editor,
                completion_candidates: candidates,
                running: false,
                result: None,
                history,
            },
        );
        cx.notify();
    }

    /// The workbench tab's display title for `id`'s `database` — shared
    /// between `open_workbench_tab` (a live, already-connected click) and
    /// `WorkbenchTab::deserialize` (a tab restored from a previous session,
    /// where the connection is re-established as part of the same call).
    fn workbench_tab_title(&self, id: ConnectionId, database: &str) -> SharedString {
        self.connections
            .iter()
            .find(|connection| connection.id == id)
            .map(|connection| {
                if database.is_empty() {
                    format!("SQL: {}", connection.title)
                } else {
                    format!("SQL: {} / {database}", connection.title)
                }
            })
            .unwrap_or_else(|| "SQL Workbench".to_string())
            .into()
    }

    /// The "Workbench" button's entry point: ensures `id`'s active database
    /// has a `WorkbenchState`, then opens (or activates, if already open) its
    /// real workspace tab.
    pub(crate) fn open_workbench_tab(
        &mut self,
        id: ConnectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let database = self.registry.active_database(id).unwrap_or_default();
        let key: SchemaKey = (id, database.clone());
        self.ensure_workbench(&key, window, cx);

        let title = self.workbench_tab_title(id, &database);

        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(editor) = self
            .workbenches
            .get(&key)
            .map(|workbench| workbench.editor.clone())
        else {
            return;
        };
        let panel = cx.entity();
        workspace.update(cx, |workspace, cx| {
            open(panel, editor, key, title, workspace, window, cx);
        });
    }

    /// Every table/view name plus its columns, for `key`'s currently loaded
    /// schema — the completion provider's candidate list. Empty when the
    /// schema hasn't loaded (or failed), which just means no suggestions
    /// show yet, not a broken editor.
    fn schema_candidate_names(&self, key: &SchemaKey) -> Vec<String> {
        let Some(SchemaState::Loaded { tables, views }) = self.schemas.get(key) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        for table in tables.iter() {
            names.push(table.name.clone());
            names.extend(table.columns.iter().map(|column| column.name.clone()));
        }
        for view in views.iter() {
            names.push(view.name.clone());
            names.extend(view.columns.iter().map(|column| column.name.clone()));
        }
        names.sort();
        names.dedup();
        names
    }

    /// Refreshes `key`'s open workbench's completion candidates in place —
    /// called after a schema (re)fetch completes, so autocomplete never
    /// keeps serving names from a stale/previous schema.
    pub(crate) fn refresh_workbench_candidates(&mut self, key: &SchemaKey) {
        let names = self.schema_candidate_names(key);
        if let Some(workbench) = self.workbenches.get(key) {
            *workbench.completion_candidates.borrow_mut() = names;
        }
    }

    /// Replaces the workbench editor's content with `sql` — what the schema
    /// tree's context-menu actions do instead of running immediately.
    fn insert_workbench_sql(
        &mut self,
        key: &SchemaKey,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(workbench) = self.workbenches.get(key) {
            workbench.editor.update(cx, |editor, cx| {
                editor.set_text(sql, window, cx);
            });
        }
    }

    /// Drops `sql` into `id`'s workbench and runs it immediately, opening
    /// (or focusing) the workbench tab so the result is visible right away —
    /// the schema graph's node context menu's entry point (unlike the schema
    /// tree's equivalent menu, which only inserts and lets the user hit Run).
    /// `open_workbench_tab` ensures the `WorkbenchState` exists before the
    /// other two calls rely on it.
    pub(crate) fn run_canned_query(
        &mut self,
        id: ConnectionId,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_workbench_tab(id, window, cx);
        let database = self.registry.active_database(id).unwrap_or_default();
        let key: SchemaKey = (id, database);
        self.insert_workbench_sql(&key, sql, window, cx);
        self.run_workbench_query(key, cx);
    }

    /// Runs the workbench editor's current SQL against `key`'s own database,
    /// whichever database the connection has active: a tab belongs to one
    /// database, and with several open a run must not land on another. Does
    /// nothing unless that database is connected. Mirrors `fetch_schema`'s
    /// `mem::take`/`registry_busy` dance (see its doc comment) — the query is
    /// `&self`, but the task must still own the registry to stay `'static`.
    fn run_workbench_query(&mut self, key: SchemaKey, cx: &mut Context<Self>) {
        if self.registry_busy || !self.live_keys.contains(&key) {
            return;
        }
        let id = key.0;
        let database = key.1.clone();
        let Some(workbench) = self.workbenches.get(&key) else {
            return;
        };
        if workbench.running {
            return;
        }
        let sql = workbench.editor.read(cx).text(cx);
        if sql.trim().is_empty() {
            return;
        }

        if let Some(workbench) = self.workbenches.get_mut(&key) {
            workbench.running = true;
            workbench.result = None;
            workbench.history.retain(|entry| entry != &sql);
            workbench.history.insert(0, sql.clone());
            workbench.history.truncate(MAX_HISTORY_ENTRIES);
        }
        self.workbench_history.insert(
            key.clone(),
            self.workbenches
                .get(&key)
                .map(|workbench| workbench.history.clone())
                .unwrap_or_default(),
        );
        self.persist_workbench_history(cx);

        self.registry_busy = true;
        cx.notify();

        let refresh_schema = may_change_schema(&sql);
        let registry = std::mem::take(&mut self.registry);

        cx.spawn(async move |this, cx| {
            let result = gpui_tokio::Tokio::spawn_result(cx, async move {
                let outcome = registry.execute_query_database(id, &database, &sql).await;
                anyhow::Ok((registry, outcome))
            })
            .await;

            this.update(cx, |this, cx| {
                this.registry_busy = false;
                match result {
                    Ok((registry, outcome)) => {
                        this.registry = registry;
                        if let Some(workbench) = this.workbenches.get_mut(&key) {
                            workbench.running = false;
                            workbench.result = Some(outcome.map_err(|err| err.to_string()));
                        }
                        // Even a failed script may have applied its earlier
                        // statements, so refresh regardless of the outcome.
                        if refresh_schema {
                            this.fetch_schema_for(key.clone(), cx);
                        }
                    }
                    Err(err) => {
                        // Same reasoning as `fetch_schema`'s equivalent
                        // branch: the tokio task itself failed to join, so
                        // the registry it owned — every connection that was
                        // live in it — is gone.
                        this.registry = ConnectionRegistry::new();
                        this.statuses.clear();
                        this.refresh_live_keys(cx);
                        if let Some(workbench) = this.workbenches.get_mut(&key) {
                            workbench.running = false;
                            workbench.result = Some(Err(err.to_string()));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The workbench body: schema tree (left, with a context menu) and
    /// editor + Run + results (right) — `WorkbenchTab::render`'s content,
    /// via `WorkbenchTab::database_panel.update(cx, ...)`.
    pub(crate) fn render_workbench_body(
        &self,
        key: &SchemaKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = key.0;

        let Some(workbench) = self.workbenches.get(key) else {
            // Shouldn't normally happen — `workbench::open` calls
            // `ensure_workbench` before the tab is ever created — but a
            // fallback beats a blank tab if state was somehow lost (e.g. a
            // deserialized tab from a previous session, before its
            // `WorkbenchState` exists again this run).
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .child(
                    Icon::new(GIconName::SquareTerminal)
                        .with_size(Size::Large)
                        .text_color(cx.theme().muted_foreground),
                )
                .child(Label::new("Workbench state unavailable.").size(LabelSize::Default))
                .into_any_element();
        };

        let tree_state = self.workbench_tree_states.get(key).cloned();
        let running = workbench.running;
        let result = workbench.result.clone();
        let history = workbench.history.clone();

        h_resizable(("workbench-body", id.0))
            .child(
                resizable_panel()
                    .size(gpui::px(260.))
                    .size_range(gpui::px(200.)..gpui::px(420.))
                    .flex_none()
                    .child(self.render_workbench_sidebar(id, key, tree_state, &history, cx)),
            )
            .child(resizable_panel().child(
                self.render_workbench_editor_and_results(id, key, running, result, window, cx),
            ))
            .into_any_element()
    }

    /// Left pane: the same schema tree the explorer uses (so it stays in
    /// sync automatically — same `tree_state`), with a right-click menu that
    /// inserts a ready-made statement into the editor, plus a compact
    /// history list underneath.
    fn render_workbench_sidebar(
        &self,
        id: ConnectionId,
        key: &SchemaKey,
        tree_state: Option<gpui::Entity<gpui_component::tree::TreeState>>,
        history: &[String],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let key = key.clone();
        let panel = cx.entity();

        let tree_element: gpui::AnyElement = match tree_state {
            Some(tree_state) => tree(
                &tree_state,
                |ix, entry: &TreeEntry, is_selected, _window, cx| {
                    let is_folder = entry.is_folder();
                    let item = entry.item();
                    gpui_component::list::ListItem::new(ix)
                        .selected(is_selected)
                        .px(gpui::px(16.0 * entry.depth() as f32 + 8.0))
                        .child(
                            h_flex()
                                .w_full()
                                .items_center()
                                .gap_1_5()
                                .child(
                                    Icon::new(if is_folder {
                                        GIconName::Folder
                                    } else {
                                        GIconName::ChevronRight
                                    })
                                    .with_size(Size::XSmall)
                                    .text_color(
                                        if is_folder {
                                            cx.theme().foreground
                                        } else {
                                            cx.theme().muted_foreground
                                        },
                                    ),
                                )
                                .child(Label::new(item.label.clone()).size(LabelSize::XSmall)),
                        )
                },
            )
            .context_menu({
                let panel = panel.clone();
                let key = key.clone();
                move |_ix, entry, menu, _window, _cx| {
                    let Some(name) = tree_row_object_name(entry.item().id.as_str()) else {
                        return menu;
                    };
                    let name = name.to_string();
                    let panel_select = panel.clone();
                    let panel_where = panel.clone();
                    let name_select = name.clone();
                    let name_where = name.clone();
                    let key_select = key.clone();
                    let key_where = key.clone();
                    menu.item(
                        PopupMenuItem::new(format!("SELECT * FROM {name}")).on_click(
                            move |_, window, cx| {
                                let sql = format!("SELECT * FROM {name_select} LIMIT 100;");
                                panel_select.update(cx, |this, cx| {
                                    this.insert_workbench_sql(&key_select, sql, window, cx);
                                });
                            },
                        ),
                    )
                    .item(
                        PopupMenuItem::new(format!("SELECT * FROM {name} WHERE …")).on_click(
                            move |_, window, cx| {
                                let sql = format!("SELECT * FROM {name_where} WHERE ;");
                                panel_where.update(cx, |this, cx| {
                                    this.insert_workbench_sql(&key_where, sql, window, cx);
                                });
                            },
                        ),
                    )
                }
            })
            .into_any_element(),
            None => v_flex()
                .w_full()
                .items_center()
                .py_6()
                .child(
                    Label::new("No schema loaded")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        };

        v_flex()
            .id(("workbench-sidebar", id.0))
            .size_full()
            .min_w_0()
            .child(
                div()
                    .id(("workbench-tree-scroll", id.0))
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_1()
                    .child(tree_element),
            )
            .when(!history.is_empty(), |this| {
                this.child(
                    v_flex()
                        .w_full()
                        .flex_shrink_0()
                        .gap_1()
                        .px_2()
                        .py_2()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .child(
                            Label::new("History")
                                .size(LabelSize::XSmall)
                                .weight(FontWeight::BOLD)
                                .color(Color::Muted),
                        )
                        .child(
                            div()
                                .id(("workbench-history-scroll", id.0))
                                .w_full()
                                .max_h(gpui::px(160.))
                                .overflow_y_scroll()
                                .child(v_flex().gap_1().children(history.iter().enumerate().map(
                                    |(ix, entry)| {
                                        let sql = entry.clone();
                                        let key = key.clone();
                                        Button::new(("workbench-history-entry", ix))
                                            .ghost()
                                            .label(one_line_preview(entry))
                                            .on_click(cx.listener(
                                                move |this, _: &ClickEvent, window, cx| {
                                                    this.insert_workbench_sql(
                                                        &key,
                                                        sql.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            ))
                                    },
                                ))),
                        ),
                )
            })
    }

    /// Right pane: the editor on top (with its Run toolbar), the results
    /// table underneath.
    fn render_workbench_editor_and_results(
        &self,
        id: ConnectionId,
        key: &SchemaKey,
        running: bool,
        result: Option<Result<QueryResult, String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(workbench) = self.workbenches.get(key) else {
            return div().into_any_element();
        };
        let busy = self.registry_busy;
        let run_key = key.clone();

        v_flex()
            .id(("workbench-editor", id.0))
            .size_full()
            .min_w_0()
            .child(
                h_flex()
                    .w_full()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Label::new("SQL Workbench")
                            .size(LabelSize::Small)
                            .weight(FontWeight::BOLD),
                    )
                    .child(
                        h_flex().gap_2().items_center().child(
                            Button::new(("run-query", id.0))
                                .primary()
                                .disabled(running || busy)
                                .label(if running { "Running…" } else { "Run" })
                                .icon(GIconName::Play)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.run_workbench_query(run_key.clone(), cx);
                                })),
                        ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .h(gpui::px(220.))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(workbench.editor.clone()),
            )
            .child(
                div()
                    .id(("workbench-results-scroll", id.0))
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .items_start()
                    .overflow_scroll()
                    .restrict_scroll_to_axis()
                    .p_3()
                    .child(self.render_workbench_results(key, running, result, window, cx)),
            )
            .into_any_element()
    }

    /// The results area: a spinner while running, the error banner, the
    /// affected-row message, or the results table — whichever `result`
    /// (or `running`) says applies. Not virtualized (see `MAX_DISPLAYED_ROWS`'s
    /// doc comment) — a simple `gpui_component::table::Table`, same as every
    /// other tabular view in this panel (Foreign Keys, Indexes, ...), capped
    /// defensively rather than rendering an arbitrarily large result set.
    fn render_workbench_results(
        &self,
        key: &SchemaKey,
        running: bool,
        result: Option<Result<QueryResult, String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if running {
            return h_flex()
                .items_center()
                .gap_2()
                .child(Spinner::new().with_size(Size::Medium))
                .child(
                    Label::new("Running…")
                        .size(LabelSize::Default)
                        .color(Color::Muted),
                )
                .into_any_element();
        }

        match result {
            None => v_flex()
                .w_full()
                .items_center()
                .py_10()
                .gap_2()
                .child(
                    Label::new("No query run yet")
                        .size(LabelSize::Default)
                        .weight(FontWeight::MEDIUM),
                )
                .child(
                    Label::new(
                        "Write SQL above and click Run, or right-click a table on the left.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element(),
            Some(Err(message)) => v_flex()
                .w_full()
                .gap_2()
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::new(GIconName::TriangleAlert)
                                .with_size(Size::Small)
                                .text_color(cx.theme().danger_foreground),
                        )
                        .child(
                            Label::new("Query failed")
                                .size(LabelSize::Default)
                                .weight(FontWeight::BOLD),
                        ),
                )
                .child(
                    Label::new(message)
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
                .into_any_element(),
            Some(Ok(result)) => self
                .render_query_result_table(key, &result, window)
                .into_any_element(),
        }
    }

    fn render_query_result_table(
        &self,
        key: &SchemaKey,
        result: &QueryResult,
        window: &mut Window,
    ) -> impl IntoElement {
        if let Some(rows_affected) = result.rows_affected {
            return v_flex()
                .gap_1()
                .child(
                    Label::new(format!("{rows_affected} row(s) affected"))
                        .size(LabelSize::Default)
                        .weight(FontWeight::MEDIUM),
                )
                .child(
                    Label::new(format!("{} ms", result.exec_ms))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element();
        }

        if result.columns.is_empty() {
            return v_flex()
                .w_full()
                .items_center()
                .py_10()
                .child(
                    Label::new("Query returned no columns")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element();
        }

        let total_rows = result.rows.len();
        let shown = &result.rows[..total_rows.min(MAX_DISPLAYED_ROWS)];
        let column_widths = workbench_column_widths(result, window);

        v_flex()
            .w_auto()
            .items_start()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new(format!("{total_rows} row(s)"))
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM),
                    )
                    .child(
                        Label::new(format!("{} ms", result.exec_ms))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when(total_rows > MAX_DISPLAYED_ROWS, |this| {
                        this.child(
                            Label::new(format!("showing first {MAX_DISPLAYED_ROWS}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                    })
                    .child(render_query_result_actions(key, result)),
            )
            .child(
                Table::new()
                    .w_auto()
                    .child(
                        TableHeader::new()
                            .w_auto()
                            .child(TableRow::new().w_auto().children(
                                result.columns.iter().enumerate().map(|(index, name)| {
                                    let width =
                                        column_widths.get(index).copied().unwrap_or(gpui::px(16.));
                                    TableHead::new()
                                        .w(width)
                                        .min_w(width)
                                        .flex_none()
                                        .whitespace_nowrap()
                                        .child(Label::new(name.clone()))
                                }),
                            )),
                    )
                    .child(TableBody::new().w_auto().children(shown.iter().map(|row| {
                        TableRow::new()
                            .w_auto()
                            .children(row.iter().enumerate().map(|(index, cell)| {
                                let width =
                                    column_widths.get(index).copied().unwrap_or(gpui::px(16.));
                                TableCell::new()
                                    .w(width)
                                    .min_w(width)
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .child(match cell {
                                        Some(value) => Label::new(value.clone())
                                            .size(LabelSize::Small)
                                            .into_any_element(),
                                        None => Label::new("NULL")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .into_any_element(),
                                    })
                            }))
                    }))),
            )
            .into_any_element()
    }

    /// Persists `self.workbench_history` — every `(connection, database)`'s
    /// past statements, not just currently-open workbenches' — after a
    /// short debounce, mirroring `persist_schema_cache`.
    fn persist_workbench_history(&mut self, cx: &mut Context<Self>) {
        let entries: Vec<SerializedWorkbenchHistoryEntry> = self
            .workbench_history
            .iter()
            .filter(|(_, history)| !history.is_empty())
            .map(
                |((connection_id, database), history)| SerializedWorkbenchHistoryEntry {
                    connection_id: *connection_id,
                    database: database.clone(),
                    history: history.clone(),
                },
            )
            .collect();
        let kvp = db::kvp::KeyValueStore::global(cx);

        self._workbench_persist_task = cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(workspace::SERIALIZATION_THROTTLE_TIME)
                .await;
            cx.background_spawn(async move {
                let json = serde_json::to_string(&SerializedWorkbenchHistory { entries })?;
                kvp.write_kvp(DATABASE_WORKBENCH_HISTORY_KVP_KEY.to_string(), json)
                    .await?;
                anyhow::Ok(())
            })
            .await
            .ok();
        });
    }

    pub(crate) fn load_persisted_workbench_history(
        cx: &App,
    ) -> std::collections::HashMap<SchemaKey, Vec<String>> {
        db::kvp::KeyValueStore::global(cx)
            .read_kvp(DATABASE_WORKBENCH_HISTORY_KVP_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<SerializedWorkbenchHistory>(&json).ok())
            .map(|cache| {
                cache
                    .entries
                    .into_iter()
                    .map(|entry| ((entry.connection_id, entry.database), entry.history))
                    .collect()
            })
            .unwrap_or_default()
    }
}

const DATABASE_WORKBENCH_HISTORY_KVP_KEY: &str = "database-panel-workbench-history";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SerializedWorkbenchHistoryEntry {
    connection_id: ConnectionId,
    database: String,
    history: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct SerializedWorkbenchHistory {
    entries: Vec<SerializedWorkbenchHistoryEntry>,
}

/// A single-line preview of a (possibly multi-line) history entry, for the
/// sidebar's compact history buttons.
fn one_line_preview(sql: &str) -> String {
    let flattened = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview = flattened.chars().take(60).collect::<String>();
    if flattened.chars().count() > 60 {
        format!("{preview}…")
    } else {
        preview
    }
}

/// Opens (or activates, if already open in the active pane) the workspace
/// tab for `key` — the same "find existing, else create" shape
/// `npm_manager_panel::open` uses. Not registered as a global action (no
/// keybinding/command-palette entry) since the only entry point is
/// `DatabasePanel::open_workbench_tab`'s button.
fn open(
    database_panel: Entity<DatabasePanel>,
    editor: Entity<Editor>,
    key: SchemaKey,
    title: SharedString,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .active_pane()
        .read(cx)
        .items()
        .find_map(|item| item.downcast::<WorkbenchTab>())
        .filter(|tab| tab.read(cx).key == key);

    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
    } else {
        let tab = cx.new(|cx| WorkbenchTab::new(database_panel, key, title, editor, cx));
        workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
    }
}

/// A real workspace tab (an [`Item`]) hosting one `(connection, database)`'s
/// SQL workbench — see this module's doc comment for why it's a tab and not
/// a view inside `DatabasePanel`. A thin shell: all state and rendering
/// logic live on `DatabasePanel`, reached through `database_panel`.
pub struct WorkbenchTab {
    editor: Entity<Editor>,
    database_panel: Entity<DatabasePanel>,
    key: SchemaKey,
    title: SharedString,
    /// Set once this tab has asked for its database to be connected, which it
    /// does the first time it shows: a tab restored from last session opens
    /// at once and connects here, not while the app starts.
    connect_requested: bool,
}

impl WorkbenchTab {
    fn new(
        database_panel: Entity<DatabasePanel>,
        key: SchemaKey,
        title: SharedString,
        editor: Entity<Editor>,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            editor,
            database_panel,
            key,
            title,
            connect_requested: false,
        }
    }
}

impl EventEmitter<()> for WorkbenchTab {}

impl Focusable for WorkbenchTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl Render for WorkbenchTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let key = self.key.clone();
        if !self.connect_requested {
            self.connect_requested = true;
            let panel = self.database_panel.clone();
            let key = key.clone();
            window.defer(cx, move |_, cx| {
                panel.update(cx, |panel, cx| panel.request_database(key, cx));
            });
        }
        self.database_panel.update(cx, |panel, cx| {
            if panel.database_unavailable(&key) {
                panel.render_database_placeholder(&key, cx)
            } else {
                panel
                    .render_workbench_body(&key, window, cx)
                    .into_any_element()
            }
        })
    }
}

impl Item for WorkbenchTab {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.title.clone())
    }
}

impl workspace::SerializableItem for WorkbenchTab {
    fn serialized_item_kind() -> &'static str {
        "database_workbench"
    }

    /// Persisting the tab just records `(connection, database)`; the actual
    /// SQL text/results/history live in `DatabasePanel::workbench_history`'s
    /// own JSON persistence (see `persist_workbench_history`), not here.
    fn cleanup(
        workspace_id: workspace::WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = crate::persistence::DatabasePanelTabsDb::global(cx);
        cx.background_spawn(async move { db.delete_unloaded(workspace_id, alive_items).await })
    }

    /// Recreates the tab at once, without waiting for its database: the tab
    /// connects it the first time it shows (see `connect_requested`), and
    /// until then says it is not connected. Waiting here made every restored
    /// tab queue behind the others' connections and time out. Only a
    /// connection that has since been deleted fails the restore, with a toast
    /// naming it.
    fn deserialize(
        _project: Entity<project::Project>,
        workspace: gpui::WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let db = crate::persistence::DatabasePanelTabsDb::global(cx);
        window.spawn(cx, async move |cx| {
            let persisted = db
                .tab_for_item(item_id, workspace_id)?
                .filter(|tab| tab.kind == crate::persistence::TabKind::Workbench)
                .ok_or_else(|| anyhow::anyhow!("No workbench tab persisted for item"))?;
            let key: SchemaKey = (persisted.connection_id, persisted.database);

            let workspace_entity = workspace
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("Workspace released before workbench restore"))?;
            let panel = workspace_entity
                .read_with(cx, |workspace, cx| workspace.panel::<DatabasePanel>(cx))
                .ok_or_else(|| anyhow::anyhow!("Database panel not available"))?;

            let connection_exists = panel.read_with(cx, |panel, _| {
                panel
                    .connections
                    .iter()
                    .any(|connection| connection.id == key.0)
            });
            if !connection_exists {
                log::warn!("database_panel: workbench restore for {key:?} failed: connection no longer exists");
                let message = panel.read_with(cx, |panel, _| panel.reopen_failure_message(&key));
                workspace_entity.update(cx, |workspace, cx| {
                    workspace.show_toast(
                        workspace::Toast::new(
                            workspace::notifications::NotificationId::unique::<WorkbenchTab>(),
                            message,
                        ),
                        cx,
                    );
                });
                anyhow::bail!("database connection could not be reopened");
            }

            cx.update(|window, cx| {
                let (id, database) = key.clone();
                let (editor, title) = panel.update(cx, |panel, cx| {
                    panel.ensure_workbench(&key, window, cx);
                    let editor = panel
                        .workbenches
                        .get(&key)
                        .map(|workbench| workbench.editor.clone());
                    (editor, panel.workbench_tab_title(id, &database))
                });
                let editor = editor
                    .ok_or_else(|| anyhow::anyhow!("Workbench state missing after reconnect"))?;
                anyhow::Ok(cx.new(|cx| WorkbenchTab::new(panel.clone(), key, title, editor, cx)))
            })?
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let (connection_id, database) = self.key.clone();
        let db = crate::persistence::DatabasePanelTabsDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_tab(
                workspace_id,
                item_id,
                crate::persistence::TabKind::Workbench,
                connection_id,
                database,
            )
            .await
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

/// True when running `sql` may have changed the database's structure, so the
/// cached schema (tree, explorer, graph) is out of date. Looks at the first
/// word of each `;`-separated statement, skipping leading comments — a
/// deliberately cheap test: a false positive only costs one extra schema
/// fetch, and a plain `SELECT`/`INSERT`/`UPDATE`/`DELETE` never triggers one.
fn may_change_schema(sql: &str) -> bool {
    sql.split(';').any(|statement| {
        let mut rest = statement.trim_start();
        loop {
            if let Some(after) = rest.strip_prefix("--") {
                rest = after.split_once('\n').map_or("", |(_, tail)| tail).trim_start();
            } else if let Some(after) = rest.strip_prefix("/*") {
                rest = after.split_once("*/").map_or("", |(_, tail)| tail).trim_start();
            } else {
                break;
            }
        }
        let word: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect::<String>()
            .to_ascii_lowercase();
        matches!(word.as_str(), "create" | "alter" | "drop" | "rename")
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn ddl_statements_may_change_the_schema() {
        assert!(may_change_schema("CREATE TABLE t (id int)"));
        assert!(may_change_schema("  alter table t add x int;"));
        assert!(may_change_schema("drop view v"));
        assert!(may_change_schema("rename t to u"));
        assert!(may_change_schema("-- note\ncreate table t (id int)"));
        assert!(may_change_schema("/* hi */ DROP TABLE t"));
    }

    #[test]
    fn a_ddl_statement_later_in_a_script_counts() {
        assert!(may_change_schema("insert into t values (1);\ncreate index i on t (id);"));
        assert!(may_change_schema("select 1 from dual;; ; drop table t;"));
    }

    #[test]
    fn data_statements_do_not_change_the_schema() {
        assert!(!may_change_schema("SELECT * FROM t"));
        assert!(!may_change_schema("insert into t values (1)"));
        assert!(!may_change_schema("update t set x = 1; delete from t"));
        assert!(!may_change_schema("select 'create table' from dual"));
        assert!(!may_change_schema(""));
    }

    use super::*;

    fn sample_result() -> QueryResult {
        QueryResult {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec![Some("1".to_string()), Some("O'Reilly".to_string())],
                vec![Some("2".to_string()), None],
            ],
            rows_affected: None,
            exec_ms: 0,
        }
    }

    #[test]
    fn serializes_query_result_as_sql() {
        assert_eq!(
            serialize_query_result_as_sql(&sample_result()),
            "SELECT '1' AS \"id\", 'O''Reilly' AS \"name\"\nUNION ALL\nSELECT '2', NULL;\n"
        );
    }

    #[test]
    fn serializes_empty_query_result_as_sql() {
        let result = QueryResult {
            columns: vec!["id".to_string()],
            ..QueryResult::default()
        };
        assert_eq!(
            serialize_query_result_as_sql(&result),
            "SELECT NULL AS \"id\" WHERE 1 = 0;\n"
        );
    }

    #[test]
    fn serializes_query_result_as_json() {
        let json: serde_json::Value =
            serde_json::from_str(&serialize_query_result_as_json(&sample_result())).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                { "id": "1", "name": "O'Reilly" },
                { "id": "2", "name": null }
            ])
        );
    }
}

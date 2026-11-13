//! Database panel UI.
//!
//! See `crates/gpui_component/DATABASE_PANEL_SPEC.md` for the full
//! architecture and phased delivery plan. On top of the multi-driver
//! connect/disconnect + database discovery/create/register flow, the
//! connection body is a `gpui_component` three-pane explorer — a searchable
//! schema explorer (left), Overview/Tables/Views/Relationships content tabs
//! (center) and an object inspector (right) — all resizable via
//! `h_resizable`/`resizable_panel`. The SQL workbench (`workbench.rs`) and
//! the schema graph (`graph.rs`) open from here as workspace tabs rather
//! than living inside the dock. Passwords are never persisted on `ConnectionConfig` — they're
//! read/written exclusively through `zed_credentials_provider`, keyed by
//! `credential_url(db_type, host, port)`.

mod database_panel_settings;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use anyhow::Result;
use database_backend::{
    ConnectionConfig, ConnectionId, ConnectionRegistry, ConnectionStatus, DatabaseId, DbType,
    NetworkConnectParams, SavedDatabase, TableInfo, ViewInfo, credential_url,
};
pub use database_panel_settings::DatabasePanelSettings;
use db::kvp::KeyValueStore;
use gpui::{
    App, AppContext as _, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    PromptLevel, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Task, WeakEntity, Window, actions, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName as GIconName, Sizable as _, Size,
    button::{Button, ButtonVariants as _},
    form::{field, v_form},
    h_flex,
    input::{Input, InputEvent, InputState},
    panel_header::PanelHeader,
    resizable::{h_resizable, resizable_panel},
    setting::{SettingGroup, SettingItem, SettingPage, Settings},
    spinner::Spinner,
    switch::Switch,
    tab::{Tab, TabBar, TabVariant},
    tree::{TreeItem, TreeState},
    v_flex,
};
use settings::Settings as _;
use ui::{Color, Label, LabelCommon as _, LabelSize};
use workspace::{
    SERIALIZATION_THROTTLE_TIME, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

mod content;
mod explorer;
mod graph;
mod header;
mod inspector;
mod ir;
mod layout;
mod persistence;
mod workbench;

use workbench::WorkbenchState;

const DATABASE_CONNECTIONS_KVP_KEY: &str = "database-panel-connections";
const DATABASE_SCHEMA_CACHE_KVP_KEY: &str = "database-panel-schema-cache";

/// The state of a connection's schema discovery.
#[derive(Clone)]
enum SchemaState {
    Loading,
    Loaded {
        tables: Rc<[TableInfo]>,
        views: Rc<[ViewInfo]>,
    },
    Error(String),
}

/// A live schema/tree-state slot is keyed by connection *and* database, not
/// just connection — a connection can have several databases open at once
/// (see `ConnectionRegistry`'s tuple-keyed sessions), each with its own
/// independently cached schema. SQLite (a single file = a single database)
/// uses the empty string, mirroring `ConnectionRegistry`'s own convention.
type SchemaKey = (ConnectionId, String);

/// The panel's center-pane content tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ContentTab {
    #[default]
    Overview,
    Tables,
    Views,
    Relationships,
    SchemaGraph,
}

impl ContentTab {
    pub(crate) const ALL: [ContentTab; 5] = [
        ContentTab::Overview,
        ContentTab::Tables,
        ContentTab::Views,
        ContentTab::Relationships,
        ContentTab::SchemaGraph,
    ];

    pub(crate) fn index(self) -> usize {
        match self {
            ContentTab::Overview => 0,
            ContentTab::Tables => 1,
            ContentTab::Views => 2,
            ContentTab::Relationships => 3,
            ContentTab::SchemaGraph => 4,
        }
    }

    pub(crate) fn from_index(ix: usize) -> Self {
        match ix {
            1 => ContentTab::Tables,
            2 => ContentTab::Views,
            3 => ContentTab::Relationships,
            4 => ContentTab::SchemaGraph,
            _ => ContentTab::Overview,
        }
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            ContentTab::Overview => "Overview",
            ContentTab::Tables => "Tables",
            ContentTab::Views => "Views",
            ContentTab::Relationships => "Relationships",
            ContentTab::SchemaGraph => "Schema Graph",
        }
    }
}

/// What's currently selected in the schema explorer tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SchemaSelection {
    Table { name: String },
    Column { table: String, column: String },
    Index { table: String, index: String },
    View { name: String },
    ViewColumn { view: String, column: String },
}

/// The table behind the explorer tree's current selection, plus which single
/// column or index (if either) within it is selected — see `selected_table`.
pub(crate) struct TableSelection<'a> {
    pub(crate) selected_column: Option<String>,
    pub(crate) selected_index: Option<String>,
    pub(crate) table: &'a TableInfo,
}

/// The view behind the explorer tree's current selection, plus which single
/// column (if any) within it is selected — see `selected_view`.
pub(crate) struct ViewSelection<'a> {
    pub(crate) selected_column: Option<String>,
    pub(crate) view: &'a ViewInfo,
}

/// One `(connection, database)` slot's cached schema, as persisted. A flat
/// `Vec` rather than a `HashMap<SchemaKey, _>` — `serde_json` can't use a
/// tuple as an object key.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SerializedSchemaCacheEntry {
    connection_id: ConnectionId,
    database: String,
    tables: Vec<TableInfo>,
    #[serde(default)]
    views: Vec<ViewInfo>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct SerializedSchemaCache {
    entries: Vec<SerializedSchemaCacheEntry>,
}

/// Builds the tree items for one table: the table itself, with a child item
/// per column followed by a child item per index. Column/index labels bake
/// in their details, since `TreeItem` carries only an id and a display label
/// — there's no separate typed slot for a renderer to inspect, so
/// `render_item` differentiates tables/columns/indexes purely by
/// `TreeEntry::depth()` plus the id shape `selected_schema_item` decodes
/// (`table`, `table::column`, `table::idx::index`, checked in that order of
/// specificity). When `needle` is non-empty, only matching columns/indexes
/// are included as children.
fn table_tree_item(table: &TableInfo, needle: &str) -> TreeItem {
    let column_children = table
        .columns
        .iter()
        .filter(|column| needle.is_empty() || column.name.to_lowercase().contains(needle))
        .map(|column| {
            let pk_suffix = if column.primary_key { " · PK" } else { "" };
            let null_suffix = if column.nullable { "" } else { " · NOT NULL" };
            TreeItem::new(
                format!("{}::{}", table.name, column.name),
                format!(
                    "{}  {}{}{}",
                    column.name, column.type_name, pk_suffix, null_suffix
                ),
            )
        });
    let index_children = table
        .indexes
        .iter()
        .filter(|index| needle.is_empty() || index.name.to_lowercase().contains(needle))
        .map(|index| {
            let unique_suffix = if index.unique { " · UNIQUE" } else { "" };
            TreeItem::new(
                format!("{}::idx::{}", table.name, index.name),
                format!(
                    "{}  ({}){}",
                    index.name,
                    index.columns.join(", "),
                    unique_suffix
                ),
            )
        });
    TreeItem::new(
        table.name.clone(),
        format!("{}  ({})", table.name, table.columns.len()),
    )
    .expanded(true)
    .children(column_children.chain(index_children))
}

/// Whether `table` matches the explorer's filter `needle`: its own name, or
/// any of its columns'/indexes' names (case-insensitive). An empty needle
/// matches everything.
fn table_matches(table: &TableInfo, needle: &str) -> bool {
    needle.is_empty()
        || table.name.to_lowercase().contains(needle)
        || table
            .columns
            .iter()
            .any(|column| column.name.to_lowercase().contains(needle))
        || table
            .indexes
            .iter()
            .any(|index| index.name.to_lowercase().contains(needle))
}

/// Builds the tree item for one view: the view itself (id `view::{name}`, so
/// `selected_schema_item` can tell it apart from a table at a glance), with a
/// child item per column (id `view::{name}::{column}`). Mirrors
/// `table_tree_item`'s shape, minus indexes/foreign keys — a view has neither.
fn view_tree_item(view: &ViewInfo, needle: &str) -> TreeItem {
    let children = view
        .columns
        .iter()
        .filter(|column| needle.is_empty() || column.name.to_lowercase().contains(needle))
        .map(|column| {
            TreeItem::new(
                format!("view::{}::{}", view.name, column.name),
                format!("{}  {}", column.name, column.type_name),
            )
        });
    TreeItem::new(
        format!("view::{}", view.name),
        format!("{}  ({})", view.name, view.columns.len()),
    )
    .expanded(true)
    .children(children)
}

/// Whether `view` matches the explorer's filter `needle`: its own name, or
/// any of its columns' names (case-insensitive). An empty needle matches
/// everything.
fn view_matches(view: &ViewInfo, needle: &str) -> bool {
    needle.is_empty()
        || view.name.to_lowercase().contains(needle)
        || view
            .columns
            .iter()
            .any(|column| column.name.to_lowercase().contains(needle))
}

/// Builds the full explorer tree — every matching table, then every matching
/// view — for one schema fetch, applying the filter `needle` to both.
fn schema_tree_items(tables: &[TableInfo], views: &[ViewInfo], needle: &str) -> Vec<TreeItem> {
    let table_items = tables
        .iter()
        .filter(|table| table_matches(table, needle))
        .map(|table| table_tree_item(table, needle));
    let view_items = views
        .iter()
        .filter(|view| view_matches(view, needle))
        .map(|view| view_tree_item(view, needle));
    table_items.chain(view_items).collect()
}

/// The default port to pre-fill when a network type is first selected in the
/// "Add Connection" form. Meaningless for SQLite (never read for it).
fn default_port(db_type: DbType) -> u16 {
    match db_type {
        DbType::Sqlite => 0,
        DbType::Postgres => 5432,
        DbType::MySql => 3306,
        DbType::MsSql => 1433,
        DbType::Oracle => 1521,
    }
}

/// The panel-level type tabs, in display order — fixed regardless of how
/// many connections (if any) exist for each type.
const DB_TYPES: [DbType; 5] = [
    DbType::Sqlite,
    DbType::Postgres,
    DbType::MySql,
    DbType::MsSql,
    DbType::Oracle,
];

actions!(
    database_panel,
    [
        /// Toggles focus on the Database panel.
        ToggleFocus
    ]
);

/// Registers the Database panel's actions on every workspace. Call once at
/// app startup, alongside the other panels' `init` functions.
pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<workbench::WorkbenchTab>(cx);
    workspace::register_serializable_item::<graph::SchemaGraphTab>(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<DatabasePanel>(window, cx);
        });
    })
    .detach();
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct SerializedDatabasePanel {
    connections: Vec<ConnectionConfig>,
    /// Which type tab was selected. Defaults to `DbType::Sqlite` (its
    /// `Default` impl) for panels serialized before this field existed.
    #[serde(default)]
    active_type: DbType,
    /// Each type's last-selected server, independent of the others — so
    /// switching Postgres → MySQL → back to Postgres restores exactly what
    /// was open. Empty (and every lookup falling through to "first
    /// connection of this type") for panels serialized before this existed.
    #[serde(default)]
    active_connection_by_type: HashMap<DbType, ConnectionId>,
}

/// The databases of connection `id` to list: those with a live session and
/// those remembered from last session that are not connected yet, sorted by
/// name and without repeats. SQLite's empty name is never listed.
fn remembered_names(
    live: &HashSet<SchemaKey>,
    parked: &HashMap<ConnectionId, Vec<String>>,
    id: ConnectionId,
) -> Vec<String> {
    let mut names: Vec<String> = live
        .iter()
        .filter(|(connection_id, database)| *connection_id == id && !database.is_empty())
        .map(|(_, database)| database.clone())
        .collect();
    if let Some(parked) = parked.get(&id) {
        names.extend(parked.iter().cloned());
    }
    names.sort();
    names.dedup();
    names
}

pub struct DatabasePanel {
    focus_handle: FocusHandle,
    /// Held so the "Workbench" button can open/activate a real workspace tab
    /// (`workbench::WorkbenchTab`) — see `workbench::open`.
    workspace: WeakEntity<Workspace>,
    connections: Vec<ConnectionConfig>,
    /// The selected type tab (`[SQLite][PostgreSQL][MySQL][MSSQL]`), which
    /// filters the connection tab strip below it to that type's servers.
    active_type: DbType,
    /// Each type's last-selected server tab, so switching type tabs away and
    /// back restores the same server/database instead of resetting. Kept in
    /// sync with `active_connection` by `sync_active_connection_for_type`
    /// and every server-tab click.
    active_connection_by_type: HashMap<DbType, ConnectionId>,
    /// Which connection's tab is showing in the content area, scoped to
    /// `active_type`. `None` shows the Add Connection form (the tab strip's
    /// trailing "+" tab) — including whenever `active_type` has zero
    /// connections. See `sync_active_connection_for_type`.
    active_connection: Option<ConnectionId>,
    registry: ConnectionRegistry,
    /// Set while `self.registry` has been temporarily moved into an
    /// in-flight async connect/schema-fetch task (see `connect_network`'s
    /// doc comment) — every other registry-touching entry point no-ops
    /// while this is set, so a second click can't run against the
    /// placeholder `Default` registry left behind by `mem::take` and race
    /// the first task's eventual write-back.
    registry_busy: bool,
    statuses: HashMap<ConnectionId, ConnectionStatus>,
    schemas: HashMap<SchemaKey, SchemaState>,
    tree_states: HashMap<SchemaKey, Entity<TreeState>>,
    /// Separate tree state for each workbench sidebar so its scroll and
    /// selection do not follow the docked schema explorer.
    workbench_tree_states: HashMap<SchemaKey, Entity<TreeState>>,
    /// Shared filter for the schema explorer. Only one connection tab is
    /// visible at a time, so a single `InputState` is enough for now.
    schema_filter: Entity<InputState>,
    /// The active center-pane tab. Shared across connections (per-connection
    /// tab state is a natural follow-up, not done here).
    content_tab: ContentTab,
    new_connection_title: Entity<InputState>,
    new_connection_path: String,
    new_connection_host: Entity<InputState>,
    new_connection_port: Entity<InputState>,
    /// Set when the form opens with an empty port: `render` then fills in
    /// the type's default, since setting an input's value needs the window.
    new_connection_port_pending: bool,
    new_connection_username: Entity<InputState>,
    /// Oracle only: the service name (a PDB such as `FREEPDB1`). Oracle has
    /// no server-level database list to pick from, so unlike the other
    /// network types this is asked for up front and saved as the
    /// connection's `database`.
    new_connection_service: Entity<InputState>,
    new_connection_ssl: bool,
    new_connection_password: Entity<InputState>,
    /// Set by `add_connection` when it bails out on a missing required
    /// field, cleared on the next submit attempt or when the form reopens.
    add_connection_error: Option<String>,
    next_connection_id: u64,
    next_database_id: u64,
    /// The connection currently showing the "Create database" / "Register
    /// existing database" text input under its discovered-database picker —
    /// only one at a time, mirroring the spec's "only one connection
    /// expanded" navigation model.
    creating_database_for: Option<ConnectionId>,
    new_database_name: Entity<InputState>,
    /// The connection currently showing the "Open existing" picker (the
    /// discovered-but-not-yet-connected database list) — only one at a time,
    /// same model as `creating_database_for`.
    opening_database_for: Option<ConnectionId>,
    /// Live SQL workbenches, one per `(connection, database)` that has had
    /// one opened this session — see `workbench::WorkbenchState`.
    workbenches: HashMap<SchemaKey, WorkbenchState>,
    schema_graphs: HashMap<SchemaKey, Entity<graph::SchemaGraphView>>,
    /// Every `(connection, database)`'s persisted query history, including
    /// ones with no currently-open `WorkbenchState` — seeds a fresh
    /// `WorkbenchState.history` the next time that key's workbench opens.
    workbench_history: HashMap<SchemaKey, Vec<String>>,
    /// Which `(connection, database)` sessions are live. A copy of what the
    /// registry holds, kept here because the registry is moved into the task
    /// for as long as a connect, schema fetch or query is running (see
    /// `registry_busy`), and a tab must not look disconnected for that long.
    /// Updated by [`Self::refresh_live_keys`] wherever the set can change.
    live_keys: HashSet<SchemaKey>,
    /// Databases this workspace had open last session that are not connected
    /// now. They stay in the database tab strip, marked as not connected, and
    /// connect when their tab is chosen or a restored tab for them is shown.
    parked_databases: HashMap<ConnectionId, Vec<String>>,
    /// Databases a restored tab or a Connect button has asked to be dialled
    /// and that have not finished yet.
    pending_dials: HashSet<SchemaKey>,
    /// Set while the saved connections are being reconnected after a restart.
    /// A database asked for by a restored tab waits for this to clear, so it
    /// cannot be dialled in between and make that reconnect fail on a database
    /// that is already connected.
    startup_reconnect: bool,
    /// The workspace this panel belongs to, for saving which databases it had
    /// open. `None` for a workspace that is not saved.
    workspace_id: Option<workspace::WorkspaceId>,
    _filter_subscription: Subscription,
    _persist_task: Task<()>,
    _schema_persist_task: Task<()>,
    _workbench_persist_task: Task<()>,
    _live_persist_task: Task<()>,
}

impl DatabasePanel {
    /// Loads the panel for a workspace, following the same
    /// `WeakEntity<Workspace>` + `AsyncWindowContext` convention as the other
    /// dock panels' `load` functions (see `initialize_panels` in
    /// `zed::zed`), so it can be added to the dock alongside them.
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = DatabasePanel::new(workspace, window, cx);
            panel.update(cx, |panel, cx| panel.reconnect_saved_connections(cx));
            panel
        })
    }

    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let workspace_handle = workspace.weak_handle();
        let workspace_id = workspace.database_id();
        cx.new(|cx| {
            let SerializedDatabasePanel {
                connections,
                active_type,
                active_connection_by_type,
            } = Self::load_persisted_state(cx);
            let next_connection_id = connections
                .iter()
                .map(|connection| connection.id.0)
                .max()
                .map_or(1, |max| max + 1);

            let cached_schemas = Self::load_persisted_schema_cache(cx);
            let mut schemas = HashMap::default();
            let mut tree_states = HashMap::default();
            for (key, (tables, views)) in cached_schemas {
                let items = schema_tree_items(&tables, &views, "");
                let tree_state = cx.new(|cx| TreeState::new(cx).items(items));
                tree_states.insert(key.clone(), tree_state);
                schemas.insert(
                    key,
                    SchemaState::Loaded {
                        tables: tables.into(),
                        views: views.into(),
                    },
                );
            }

            // The databases this workspace had open last session. None is
            // connected yet; they are listed as not connected and dialled when
            // they are chosen or a restored tab for them shows.
            let mut parked_databases: HashMap<ConnectionId, Vec<String>> = HashMap::default();
            if let Some(workspace_id) = workspace_id {
                let saved = persistence::DatabasePanelTabsDb::global(cx)
                    .live_for_workspace(workspace_id)
                    .unwrap_or_default();
                for (id, database) in saved {
                    if !database.is_empty() && connections.iter().any(|c| c.id == id) {
                        parked_databases.entry(id).or_default().push(database);
                    }
                }
            }

            let new_connection_title = cx.new(|cx| InputState::new(window, cx));
            let new_connection_host = cx.new(|cx| InputState::new(window, cx));
            let new_connection_port = cx.new(|cx| InputState::new(window, cx));
            let new_connection_username = cx.new(|cx| InputState::new(window, cx));
            let new_connection_service =
                cx.new(|cx| InputState::new(window, cx).placeholder("e.g. FREEPDB1"));
            let new_connection_password = cx.new(|cx| InputState::new(window, cx).masked(true));
            let new_database_name = cx.new(|cx| InputState::new(window, cx));
            let schema_filter =
                cx.new(|cx| InputState::new(window, cx).placeholder("Filter tables or columns…"));

            let _filter_subscription =
                cx.subscribe_in(&schema_filter, window, Self::on_schema_filter_changed);

            // The remembered server for `active_type` if it still exists and
            // still matches that type, else that type's first connection,
            // else `None` (empty-type-tab state — see
            // `sync_active_connection_for_type`, which this mirrors).
            let active_connection = active_connection_by_type
                .get(&active_type)
                .copied()
                .filter(|id| {
                    connections
                        .iter()
                        .any(|connection| connection.id == *id && connection.db_type == active_type)
                })
                .or_else(|| {
                    connections
                        .iter()
                        .find(|connection| connection.db_type == active_type)
                        .map(|connection| connection.id)
                });

            Self {
                focus_handle: cx.focus_handle(),
                workspace: workspace_handle,
                connections,
                active_type,
                active_connection_by_type,
                active_connection,
                registry: ConnectionRegistry::new(),
                registry_busy: false,
                statuses: HashMap::default(),
                schemas,
                tree_states,
                workbench_tree_states: HashMap::default(),
                schema_filter,
                content_tab: ContentTab::default(),
                new_connection_title,
                new_connection_path: String::new(),
                new_connection_host,
                new_connection_port,
                new_connection_port_pending: false,
                new_connection_username,
                new_connection_service,
                new_connection_ssl: false,
                new_connection_password,
                add_connection_error: None,
                next_connection_id,
                next_database_id: 1,
                creating_database_for: None,
                new_database_name,
                opening_database_for: None,
                workbenches: HashMap::default(),
                schema_graphs: HashMap::default(),
                workbench_history: Self::load_persisted_workbench_history(cx),
                live_keys: HashSet::default(),
                parked_databases,
                pending_dials: HashSet::default(),
                startup_reconnect: false,
                workspace_id,
                _filter_subscription,
                _persist_task: Task::ready(()),
                _schema_persist_task: Task::ready(()),
                _workbench_persist_task: Task::ready(()),
                _live_persist_task: Task::ready(()),
            }
        })
    }

    fn load_persisted_state(cx: &App) -> SerializedDatabasePanel {
        KeyValueStore::global(cx)
            .read_kvp(DATABASE_CONNECTIONS_KVP_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<SerializedDatabasePanel>(&json).ok())
            .unwrap_or_default()
    }

    /// Reconnects saved connections after the panel is restored. Network
    /// connections and schema fetches temporarily own the registry, so they
    /// must be started one at a time instead of racing through `connect`.
    fn reconnect_saved_connections(&mut self, cx: &mut Context<Self>) {
        let ids = self
            .connections
            .iter()
            .map(|connection| connection.id)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return;
        }

        self.startup_reconnect = true;
        let panel = cx.entity();
        cx.spawn(async move |_, cx| {
            for id in ids {
                loop {
                    let started = panel.update(cx, |panel, cx| {
                        if panel.registry_busy {
                            false
                        } else {
                            panel.connect(id, cx);
                            true
                        }
                    });
                    if started {
                        break;
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(100))
                        .await;
                }

                loop {
                    let busy = panel.update(cx, |panel, _| panel.registry_busy);
                    if !busy {
                        break;
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(100))
                        .await;
                }
            }
            panel.update(cx, |panel, cx| {
                panel.startup_reconnect = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Recomputes [`Self::live_keys`] from the registry and saves which
    /// databases are open. Call it wherever a session may have been added or
    /// removed, and only while the registry is in the panel (not moved into a
    /// task).
    fn refresh_live_keys(&mut self, cx: &mut Context<Self>) {
        let mut live = HashSet::default();
        for connection in &self.connections {
            let id = connection.id;
            for database in self.registry.live_databases(id) {
                live.insert((id, database));
            }
            // SQLite, and a MySQL connection with no default database, live
            // under the empty name, which `live_databases` leaves out.
            if self.registry.is_database_connected(id, "") {
                live.insert((id, String::new()));
            }
        }
        // A database that is connected is no longer waiting to be.
        for (id, database) in &live {
            if let Some(parked) = self.parked_databases.get_mut(id) {
                parked.retain(|name| name != database);
            }
        }
        self.parked_databases.retain(|_, names| !names.is_empty());
        self.live_keys = live;
        self.persist_live_databases(cx);
        cx.notify();
    }

    /// The databases of `id` to list in its tab strip: the connected ones and
    /// those remembered from last session, sorted by name.
    fn remembered_databases(&self, id: ConnectionId) -> Vec<String> {
        remembered_names(&self.live_keys, &self.parked_databases, id)
    }

    /// Saves, for this workspace, the databases to list again next launch:
    /// the connected ones and those still waiting to be. After a short delay,
    /// like [`Self::persist_connections`].
    fn persist_live_databases(&mut self, cx: &mut Context<Self>) {
        let Some(workspace_id) = self.workspace_id else {
            return;
        };
        let databases: Vec<(ConnectionId, String)> = self
            .connections
            .iter()
            .flat_map(|connection| {
                let id = connection.id;
                self.remembered_databases(id)
                    .into_iter()
                    .map(move |database| (id, database))
            })
            .collect();
        let db = persistence::DatabasePanelTabsDb::global(cx);
        self._live_persist_task = cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(SERIALIZATION_THROTTLE_TIME)
                .await;
            cx.background_spawn(async move { db.save_live(workspace_id, databases).await })
                .await
                .ok();
        });
    }

    /// Drops a remembered database that was never reconnected (the "x" on its
    /// not-connected tab), so it is not listed again.
    fn forget_parked_database(&mut self, id: ConnectionId, database: &str, cx: &mut Context<Self>) {
        if let Some(parked) = self.parked_databases.get_mut(&id) {
            parked.retain(|name| name != database);
        }
        self.parked_databases.retain(|_, names| !names.is_empty());
        self.persist_live_databases(cx);
        cx.notify();
    }

    /// Connects `key`'s database if it is not connected, once the registry is
    /// free. This is what a restored tab does when it first shows, and what
    /// the Connect and Retry buttons do.
    pub(crate) fn request_database(&mut self, key: SchemaKey, cx: &mut Context<Self>) {
        if self.live_keys.contains(&key)
            || !self.connections.iter().any(|c| c.id == key.0)
            || !self.pending_dials.insert(key.clone())
        {
            return;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            // Wait for the registry, and for the reconnect after a restart,
            // which this must not get in front of.
            loop {
                let ready = this
                    .update(cx, |this, _| !this.registry_busy && !this.startup_reconnect)
                    .unwrap_or(true);
                if ready {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
            }
            this.update(cx, |this, cx| {
                // The reconnect may have connected it in the meantime.
                if !this.live_keys.contains(&key) {
                    this.dial_database(&key, cx);
                }
            })
            .ok();
            loop {
                let busy = this.update(cx, |this, _| this.registry_busy).unwrap_or(false);
                if !busy {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
            }
            this.update(cx, |this, cx| {
                this.pending_dials.remove(&key);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Dials `key`'s database the way each kind of connection does it: a
    /// SQLite file or an Oracle service is the connection itself, and a
    /// server's database is chosen by name.
    fn dial_database(&mut self, key: &SchemaKey, cx: &mut Context<Self>) {
        let (id, database) = key;
        let Some(db_type) = self
            .connections
            .iter()
            .find(|connection| connection.id == *id)
            .map(|connection| connection.db_type)
        else {
            return;
        };
        match db_type {
            DbType::Sqlite | DbType::Oracle => self.connect(*id, cx),
            DbType::Postgres | DbType::MySql | DbType::MsSql => {
                self.select_database(*id, database.clone(), cx)
            }
        }
    }

    /// True when `key`'s database has no live session, so a tab for it has
    /// nothing to show yet.
    pub(crate) fn database_unavailable(&self, key: &SchemaKey) -> bool {
        !self.live_keys.contains(key)
    }

    /// What a tab for a database that is not connected shows instead of its
    /// content: that it is connecting, or why it is not, with a button to try.
    pub(crate) fn render_database_placeholder(
        &self,
        key: &SchemaKey,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let id = key.0;
        let connecting = self.pending_dials.contains(key)
            || matches!(self.statuses.get(&id), Some(ConnectionStatus::Connecting));
        let failure = match self.statuses.get(&id) {
            Some(ConnectionStatus::Error(message)) if !connecting => Some(message.clone()),
            _ => None,
        };
        let name = if key.1.is_empty() {
            self.connections
                .iter()
                .find(|connection| connection.id == id)
                .map(|connection| connection.title.clone())
                .unwrap_or_default()
        } else {
            key.1.clone()
        };
        let headline = if connecting {
            format!("Connecting to {name}…")
        } else if failure.is_some() {
            format!("Could not connect to {name}")
        } else {
            format!("{name} is not connected")
        };
        let retry_label = if failure.is_some() { "Retry" } else { "Connect" };
        let key = key.clone();

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(if connecting {
                Spinner::new().into_any_element()
            } else {
                Icon::new(GIconName::SquareTerminal)
                    .with_size(Size::Large)
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element()
            })
            .child(Label::new(headline).size(LabelSize::Default))
            .children(failure.map(|message| {
                Label::new(message)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
            }))
            .when(!connecting, |column| {
                column.child(
                    Button::new(("database-connect", id.0))
                        .label(retry_label)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.request_database(key.clone(), cx);
                        })),
                )
            })
            .into_any_element()
    }

    fn load_persisted_schema_cache(
        cx: &App,
    ) -> HashMap<SchemaKey, (Vec<TableInfo>, Vec<ViewInfo>)> {
        KeyValueStore::global(cx)
            .read_kvp(DATABASE_SCHEMA_CACHE_KVP_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<SerializedSchemaCache>(&json).ok())
            .map(|cache| {
                cache
                    .entries
                    .into_iter()
                    .map(|entry| {
                        (
                            (entry.connection_id, entry.database),
                            (entry.tables, entry.views),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Persists `self.connections` after a short debounce, mirroring
    /// `git_panel::GitPanel::serialize`'s throttled-write pattern so rapid
    /// successive edits (e.g. typing in the connection list) don't hammer
    /// the key-value store.
    fn persist_connections(&mut self, cx: &mut Context<Self>) {
        let connections = self.connections.clone();
        let active_type = self.active_type;
        let active_connection_by_type = self.active_connection_by_type.clone();
        let kvp = KeyValueStore::global(cx);

        self._persist_task = cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(SERIALIZATION_THROTTLE_TIME)
                .await;
            cx.background_spawn(async move {
                let json = serde_json::to_string(&SerializedDatabasePanel {
                    connections,
                    active_type,
                    active_connection_by_type,
                })?;
                kvp.write_kvp(DATABASE_CONNECTIONS_KVP_KEY.to_string(), json)
                    .await?;
                anyhow::Ok(())
            })
            .await
            .ok();
        });
    }

    /// Persists the `Loaded` schemas (ignoring in-flight `Loading`/`Error`
    /// states, which are useless as a restart-time cache) after a short
    /// debounce, mirroring `persist_connections`.
    fn persist_schema_cache(&mut self, cx: &mut Context<Self>) {
        let entries: Vec<SerializedSchemaCacheEntry> = self
            .schemas
            .iter()
            .filter_map(|((connection_id, database), state)| match state {
                SchemaState::Loaded { tables, views } => Some(SerializedSchemaCacheEntry {
                    connection_id: *connection_id,
                    database: database.clone(),
                    tables: tables.to_vec(),
                    views: views.to_vec(),
                }),
                SchemaState::Loading | SchemaState::Error(_) => None,
            })
            .collect();
        let kvp = KeyValueStore::global(cx);

        self._schema_persist_task = cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(SERIALIZATION_THROTTLE_TIME)
                .await;
            cx.background_spawn(async move {
                let json = serde_json::to_string(&SerializedSchemaCache { entries })?;
                kvp.write_kvp(DATABASE_SCHEMA_CACHE_KVP_KEY.to_string(), json)
                    .await?;
                anyhow::Ok(())
            })
            .await
            .ok();
        });
    }

    /// Fetches (or re-fetches, for Refresh) `id`'s schema, dispatching to
    /// whichever driver it's connected with via
    /// `ConnectionRegistry::fetch_schema`. Runs on the real tokio runtime
    /// (`gpui_tokio::Tokio::spawn_result`) since the network drivers need
    /// one; `self.registry` is moved into the task for the duration (see
    /// `registry_busy`'s doc comment) since `fetch_schema` needs to hold it
    /// across the query's `.await`s.
    fn fetch_schema(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        if self.registry_busy || !self.registry.is_connected(id) {
            return;
        }
        let Some(database) = self.registry.active_database(id) else {
            return;
        };
        self.fetch_schema_for((id, database), cx);
    }

    /// [`Self::fetch_schema`] for one specific database of a connection, which
    /// need not be its active one: a tab shows its own database whichever is
    /// active. Does nothing unless that database has a live session.
    fn fetch_schema_for(&mut self, key: SchemaKey, cx: &mut Context<Self>) {
        let id = key.0;
        if self.registry_busy || !self.registry.is_database_connected(id, &key.1) {
            return;
        }
        let database = key.1.clone();

        self.schemas.insert(key.clone(), SchemaState::Loading);
        self.registry_busy = true;
        cx.notify();

        let registry = std::mem::take(&mut self.registry);

        cx.spawn(async move |this, cx| {
            let result = gpui_tokio::Tokio::spawn_result(cx, async move {
                let schema = registry.fetch_schema_database(id, &database).await;
                anyhow::Ok((registry, schema))
            })
            .await;

            this.update(cx, |this, cx| {
                this.registry_busy = false;
                match result {
                    Ok((registry, Ok(schema))) => {
                        this.registry = registry;
                        this.update_tree_items(key.clone(), &schema.tables, &schema.views, cx);
                        this.refresh_schema_graph(&key, &schema.tables, cx);
                        // The workbench sidebar keeps its own tree (so its
                        // scroll and selection don't follow the explorer's),
                        // so it needs the new tables pushed in too.
                        let items = schema_tree_items(&schema.tables, &schema.views, "");
                        match this.workbench_tree_states.get(&key) {
                            Some(tree_state) => {
                                tree_state.update(cx, |tree_state, cx| {
                                    tree_state.set_items(items, cx)
                                });
                            }
                            // A workbench restored before its database had a
                            // schema (not cached, and not connected until
                            // now) has no tree yet; give it one.
                            None if this.workbenches.contains_key(&key) => {
                                let tree_state = cx.new(|cx| TreeState::new(cx).items(items));
                                this.workbench_tree_states.insert(key.clone(), tree_state);
                            }
                            None => {}
                        }
                        this.schemas.insert(
                            key.clone(),
                            SchemaState::Loaded {
                                tables: schema.tables.into(),
                                views: schema.views.into(),
                            },
                        );
                        this.refresh_workbench_candidates(&key);
                        this.persist_schema_cache(cx);
                    }
                    Ok((registry, Err(err))) => {
                        this.registry = registry;
                        this.schemas
                            .insert(key, SchemaState::Error(err.to_string()));
                    }
                    Err(err) => {
                        // The tokio task itself failed to join (panicked or
                        // was cancelled) — the registry it owned is lost
                        // along with every connection that was live in it.
                        this.registry = ConnectionRegistry::new();
                        this.statuses.clear();
                        this.schemas
                            .insert(key, SchemaState::Error(err.to_string()));
                        this.refresh_live_keys(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Creates or updates `key`'s `TreeState` with `tables`'/`views`' current
    /// shape, applying the active schema filter.
    fn update_tree_items(
        &mut self,
        key: SchemaKey,
        tables: &[TableInfo],
        views: &[ViewInfo],
        cx: &mut Context<Self>,
    ) {
        let needle = self.schema_filter.read(cx).value().to_lowercase();
        let items = schema_tree_items(tables, views, &needle);
        match self.tree_states.get(&key) {
            Some(tree_state) => {
                tree_state.update(cx, |tree_state, cx| tree_state.set_items(items, cx));
            }
            None => {
                let tree_state = cx.new(|cx| TreeState::new(cx).items(items));
                self.tree_states.insert(key, tree_state);
            }
        }
    }

    /// The schema explorer filter input changed — re-filter the active
    /// connection's tree, preserving a previously selected table when it
    /// still matches.
    fn on_schema_filter_changed(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, InputEvent::Change) {
            return;
        }
        let Some(id) = self.active_connection else {
            return;
        };
        let Some(database) = self.registry.active_database(id) else {
            return;
        };
        let key: SchemaKey = (id, database);
        let Some(SchemaState::Loaded { tables, views }) = self.schemas.get(&key) else {
            return;
        };
        let tables = tables.clone();
        let views = views.clone();
        let previous = self.selected_schema_item(id, cx);
        self.update_tree_items(key.clone(), &tables, &views, cx);

        let needle = self.schema_filter.read(cx).value().to_lowercase();
        let reselect = match previous {
            Some(SchemaSelection::Table { name }) => tables
                .iter()
                .find(|table| table.name == name)
                .filter(|table| table_matches(table, &needle))
                .map(|table| table_tree_item(table, &needle)),
            Some(SchemaSelection::View { name }) => views
                .iter()
                .find(|view| view.name == name)
                .filter(|view| view_matches(view, &needle))
                .map(|view| view_tree_item(view, &needle)),
            _ => None,
        };
        let Some(item) = reselect else {
            return;
        };
        if let Some(tree_state) = self.tree_states.get(&key) {
            tree_state.update(cx, |tree_state, cx| {
                tree_state.set_selected_item(Some(&item), cx);
            });
        }
    }

    /// The schema state for `id`'s currently active database, if any (`None`
    /// both when `id` isn't connected and when its active database's schema
    /// hasn't been fetched yet).
    pub(crate) fn active_schema_state(&self, id: ConnectionId) -> Option<&SchemaState> {
        let database = self.registry.active_database(id)?;
        self.schemas.get(&(id, database))
    }

    /// The `TreeState` for `id`'s currently active database, if any.
    pub(crate) fn active_tree_state(&self, id: ConnectionId) -> Option<Entity<TreeState>> {
        let database = self.registry.active_database(id)?;
        self.tree_states.get(&(id, database)).cloned()
    }

    /// What (if anything) is selected in `id`'s active database's schema
    /// tree, decoded from the item's id — a `view::`-prefixed id is a view
    /// (or one of its columns); otherwise `table`, `table::column`, or
    /// `table::idx::index` (checked in that order of specificity, since a
    /// column id is itself a `table::column`-shaped prefix of an index id).
    fn selected_schema_item(&self, id: ConnectionId, cx: &App) -> Option<SchemaSelection> {
        let tree_state = self.active_tree_state(id)?;
        let item = tree_state.read(cx).selected_item()?;
        let id_str = item.id.as_str();
        if let Some(rest) = id_str.strip_prefix("view::") {
            if let Some((view, column)) = rest.split_once("::") {
                Some(SchemaSelection::ViewColumn {
                    view: view.to_string(),
                    column: column.to_string(),
                })
            } else {
                Some(SchemaSelection::View {
                    name: rest.to_string(),
                })
            }
        } else if let Some((table, index)) = id_str.split_once("::idx::") {
            Some(SchemaSelection::Index {
                table: table.to_string(),
                index: index.to_string(),
            })
        } else if let Some((table, column)) = id_str.split_once("::") {
            Some(SchemaSelection::Column {
                table: table.to_string(),
                column: column.to_string(),
            })
        } else {
            Some(SchemaSelection::Table {
                name: id_str.to_string(),
            })
        }
    }

    /// The table behind `connection`'s current tree selection, if any, along
    /// with which of its columns or indexes (at most one of either) is
    /// selected — the content pane's Tables tab and its Columns/Indexes
    /// sections highlight whichever one that is.
    fn selected_table<'a>(
        &self,
        connection: &ConnectionConfig,
        tables: &'a [TableInfo],
        cx: &App,
    ) -> Option<TableSelection<'a>> {
        let selection = self.selected_schema_item(connection.id, cx)?;
        match selection {
            SchemaSelection::Table { name } => {
                tables
                    .iter()
                    .find(|table| table.name == name)
                    .map(|table| TableSelection {
                        selected_column: None,
                        selected_index: None,
                        table,
                    })
            }
            SchemaSelection::Column { table, column } => tables
                .iter()
                .find(|candidate| candidate.name == table)
                .map(|table| TableSelection {
                    selected_column: Some(column),
                    selected_index: None,
                    table,
                }),
            SchemaSelection::Index { table, index } => tables
                .iter()
                .find(|candidate| candidate.name == table)
                .map(|table| TableSelection {
                    selected_column: None,
                    selected_index: Some(index),
                    table,
                }),
            SchemaSelection::View { .. } | SchemaSelection::ViewColumn { .. } => None,
        }
    }

    /// The view behind `connection`'s current tree selection, if any, along
    /// with which of its columns (if any) is selected — mirrors
    /// `selected_table` for the Views tab/inspector.
    fn selected_view<'a>(
        &self,
        connection: &ConnectionConfig,
        views: &'a [ViewInfo],
        cx: &App,
    ) -> Option<ViewSelection<'a>> {
        let selection = self.selected_schema_item(connection.id, cx)?;
        match selection {
            SchemaSelection::View { name } => {
                views
                    .iter()
                    .find(|view| view.name == name)
                    .map(|view| ViewSelection {
                        selected_column: None,
                        view,
                    })
            }
            SchemaSelection::ViewColumn { view, column } => views
                .iter()
                .find(|candidate| candidate.name == view)
                .map(|view| ViewSelection {
                    selected_column: Some(column),
                    view,
                }),
            _ => None,
        }
    }

    /// Clears the explorer tree's selection (used by the content pane's
    /// "Back to list" action).
    fn clear_tree_selection(&self, id: ConnectionId, cx: &mut Context<Self>) {
        if let Some(tree_state) = self.active_tree_state(id) {
            tree_state.update(cx, |tree_state, cx| {
                tree_state.set_selected_index(None, cx);
            });
        }
    }

    /// Builds the new connection from the "Add Connection" form fields,
    /// saves it, clears the form, and kicks off a connect attempt. For
    /// network types with a password typed in, the password is written to
    /// the credential store before connecting (`save_password_and_connect`);
    /// with the field left empty, `connect` falls straight through to its
    /// "no saved password" error, same as reconnecting a saved connection
    /// with none stored.
    fn add_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.registry_busy {
            return;
        }
        self.add_connection_error = None;

        let value =
            |state: &Entity<InputState>, cx: &App| state.read(cx).value().trim().to_string();
        let title = value(&self.new_connection_title, cx);
        let host = value(&self.new_connection_host, cx);
        let username = value(&self.new_connection_username, cx);
        let port = value(&self.new_connection_port, cx);
        let service = value(&self.new_connection_service, cx);
        let db_type = self.active_type;
        let id = ConnectionId(self.next_connection_id);

        let mut missing = Vec::new();
        if title.is_empty() {
            missing.push("Title");
        }
        if db_type == DbType::Sqlite && self.new_connection_path.trim().is_empty() {
            missing.push("Database File");
        }
        if db_type != DbType::Sqlite {
            if host.is_empty() {
                missing.push("Host");
            }
            if username.is_empty() {
                missing.push("Username");
            }
        }
        if db_type == DbType::Oracle && service.is_empty() {
            missing.push("Service Name");
        }
        if !missing.is_empty() {
            self.add_connection_error = Some(format!("{} required.", missing.join(", ")));
            cx.notify();
            return;
        }

        let config = match db_type {
            DbType::Sqlite => {
                let path = self.new_connection_path.trim().to_string();
                ConnectionConfig {
                    id,
                    title,
                    db_type,
                    host: None,
                    port: None,
                    username: None,
                    sqlite_path: Some(path),
                    database: None,
                    ssl: false,
                    databases: Vec::new(),
                }
            }
            DbType::Postgres | DbType::MySql | DbType::MsSql | DbType::Oracle => {
                let port = port.parse().unwrap_or_else(|_| default_port(db_type));
                ConnectionConfig {
                    id,
                    title,
                    db_type,
                    host: Some(host),
                    port: Some(port),
                    username: Some(username),
                    sqlite_path: None,
                    // No database picked yet — `connect` discovers what's on
                    // the server and lets the user pick from that list
                    // instead of requiring the exact name upfront. Oracle has
                    // nothing to discover (one service is one database), so
                    // its service name is the database from the start.
                    database: (db_type == DbType::Oracle).then_some(service),
                    ssl: self.new_connection_ssl,
                    databases: Vec::new(),
                }
            }
        };

        self.next_connection_id += 1;
        self.connections.push(config.clone());
        self.active_connection = Some(id);
        self.active_connection_by_type.insert(db_type, id);
        self.persist_connections(cx);

        let password = self.new_connection_password.read(cx).value().to_string();
        self.new_connection_path.clear();
        self.new_connection_ssl = false;
        for state in [
            &self.new_connection_title,
            &self.new_connection_host,
            &self.new_connection_port,
            &self.new_connection_username,
            &self.new_connection_service,
            &self.new_connection_password,
        ] {
            state.update(cx, |state, cx| state.set_value("", window, cx));
        }
        cx.notify();

        match db_type {
            DbType::Sqlite => self.connect(id, cx),
            DbType::Postgres | DbType::MySql | DbType::MsSql | DbType::Oracle if !password.is_empty() => {
                self.save_password_and_connect(config, password, cx);
            }
            DbType::Postgres | DbType::MySql | DbType::MsSql | DbType::Oracle => self.connect(id, cx),
        }
    }

    /// Writes `password` to the credential store keyed by this connection's
    /// `credential_url`, then connects — matching the plan's "write
    /// immediately, connect reads from the store" flow so reconnecting later
    /// (no local form state to read from) works identically to this first
    /// connect.
    fn save_password_and_connect(
        &mut self,
        connection: ConnectionConfig,
        password: String,
        cx: &mut Context<Self>,
    ) {
        let id = connection.id;
        let (Some(host), Some(port), Some(username)) =
            (connection.host, connection.port, connection.username)
        else {
            return;
        };
        let key = credential_url(connection.db_type, &host, port);

        cx.spawn(async move |this, cx| {
            let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
            let result = credentials_provider
                .write_credentials(&key, &username, password.as_bytes(), cx)
                .await;

            this.update(cx, |this, cx| match result {
                Ok(()) => this.connect(id, cx),
                Err(err) => {
                    this.statuses.insert(
                        id,
                        ConnectionStatus::Error(format!("Failed to save credentials: {err}")),
                    );
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn connect(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        if self.registry_busy {
            return;
        }
        let Some(connection) = self.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };

        match connection.db_type {
            DbType::Sqlite => {
                let Some(path) = connection.sqlite_path else {
                    return;
                };
                match self.registry.connect_sqlite(id, &path) {
                    Ok(()) => {
                        self.statuses.insert(id, ConnectionStatus::Connected);
                        self.refresh_live_keys(cx);
                        self.fetch_schema(id, cx);
                    }
                    Err(err) => {
                        self.statuses
                            .insert(id, ConnectionStatus::Error(err.to_string()));
                    }
                }
                cx.notify();
            }
            DbType::Oracle if connection.database.is_none() => {
                self.statuses.insert(
                    id,
                    ConnectionStatus::Error("This Oracle connection has no service name.".into()),
                );
                cx.notify();
            }
            DbType::Postgres | DbType::MySql | DbType::MsSql | DbType::Oracle => {
                if connection.database.is_some() {
                    // A specific database was already picked (either just
                    // now via `select_database`, or this is a persisted
                    // connection from a prior session) — dial it directly.
                    self.connect_network(connection, cx);
                } else {
                    // First-time connect: discover what's on the server and
                    // let the user pick, instead of requiring them to
                    // already know the exact database name.
                    self.discover_databases(id, cx);
                }
            }
        }
    }

    /// The toast message for a workbench/graph tab whose connection can no
    /// longer be found on restore.
    pub(crate) fn reopen_failure_message(&self, key: &SchemaKey) -> String {
        let (id, database) = key;
        match self.connections.iter().find(|c| c.id == *id) {
            Some(connection) => {
                let db_type = header::db_type_label(connection.db_type);
                if database.is_empty() {
                    format!("{db_type} {} could not be connected", connection.title)
                } else {
                    format!(
                        "{db_type} {}/{database} could not be connected",
                        connection.title
                    )
                }
            }
            None => "Database connection could not be reopened".to_string(),
        }
    }

    /// Connects a network connection (Postgres/MySQL/MSSQL): looks up its
    /// password from the credential store, then dials out on the real tokio
    /// runtime via `gpui_tokio::Tokio::spawn_result`.
    ///
    /// `ConnectionRegistry::connect_postgres`/`connect_mysql`/`connect_mssql`
    /// are `&mut self` async methods (the connect call itself needs to hold
    /// the registry mutably across its `.await`s to insert the new handle
    /// atomically with the connect succeeding). GPUI entities can't be
    /// mutably borrowed across an await, so `self.registry` is moved out via
    /// `mem::take` for the task's duration and moved back on completion —
    /// see `registry_busy`, which blocks every other registry-touching entry
    /// point for as long as that's in flight.
    fn connect_network(&mut self, connection: ConnectionConfig, cx: &mut Context<Self>) {
        let id = connection.id;
        let db_type = connection.db_type;
        let (Some(host), Some(port), Some(username)) =
            (connection.host, connection.port, connection.username)
        else {
            return;
        };
        let database = connection
            .database
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| {
                // An unset Postgres `dbname` isn't "no default database" — the
                // server falls back to a database named after the connecting
                // user, which almost never exists (see e.g. `FATAL: database
                // "dev_user" does not exist`). "postgres" (the standard
                // maintenance DB every install has) is the useful default here.
                // MySQL/MSSQL don't have this footgun: an empty MySQL dbname is
                // a normal "no default DB yet" connection, and MSSQL falls back
                // to the login's configured default DB (usually "master").
                if db_type == DbType::Postgres {
                    "postgres".to_string()
                } else {
                    String::new()
                }
            });
        let ssl = connection.ssl;
        let key = credential_url(db_type, &host, port);
        // The database being dialled, which its schema is fetched for once it
        // is connected (it is not necessarily the connection's active one).
        let dialed_database = database.clone();

        log::info!(
            "database_panel: connecting {db_type:?} connection {id:?} \
             (host={host}, port={port}, database={database:?}, username={username:?}, ssl={ssl})"
        );

        self.statuses.insert(id, ConnectionStatus::Connecting);
        self.registry_busy = true;
        cx.notify();

        let mut registry = std::mem::take(&mut self.registry);

        cx.spawn(async move |this, cx| {
            let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
            let password = match credentials_provider.read_credentials(&key, cx).await {
                Ok(Some((_, bytes))) => String::from_utf8(bytes).unwrap_or_default(),
                Ok(None) => {
                    log::error!("database_panel: connect {id:?} failed: no saved password");
                    this.update(cx, |this, cx| {
                        this.registry = registry;
                        this.registry_busy = false;
                        this.statuses.insert(
                            id,
                            ConnectionStatus::Error("No saved password for this connection".into()),
                        );
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                Err(err) => {
                    log::error!(
                        "database_panel: connect {id:?} failed reading saved credentials: {err}"
                    );
                    this.update(cx, |this, cx| {
                        this.registry = registry;
                        this.registry_busy = false;
                        this.statuses
                            .insert(id, ConnectionStatus::Error(err.to_string()));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };

            let params = NetworkConnectParams {
                host,
                port,
                database,
                username,
                password,
                ssl,
            };

            let result = gpui_tokio::Tokio::spawn_result(cx, async move {
                let outcome = match db_type {
                    DbType::Postgres => registry.connect_postgres(id, params).await,
                    DbType::MySql => registry.connect_mysql(id, params).await,
                    DbType::MsSql => registry.connect_mssql(id, params).await,
                    DbType::Oracle => registry.connect_oracle(id, params).await,
                    DbType::Sqlite => {
                        unreachable!("sqlite connections never reach connect_network")
                    }
                };
                anyhow::Ok((registry, outcome))
            })
            .await;

            this.update(cx, |this, cx| {
                this.registry_busy = false;
                match result {
                    Ok((registry, Ok(()))) => {
                        log::info!("database_panel: connect {id:?} succeeded");
                        this.registry = registry;
                        this.statuses.insert(id, ConnectionStatus::Connected);
                        this.refresh_live_keys(cx);
                        this.fetch_schema_for((id, dialed_database), cx);
                        return;
                    }
                    Ok((registry, Err(err))) => {
                        log::error!("database_panel: connect {id:?} failed: {err}");
                        this.registry = registry;
                        this.statuses
                            .insert(id, ConnectionStatus::Error(err.to_string()));
                    }
                    Err(err) => {
                        log::error!(
                            "database_panel: connect {id:?} task panicked/cancelled: {err}"
                        );
                        this.registry = ConnectionRegistry::new();
                        this.statuses.clear();
                        this.statuses
                            .insert(id, ConnectionStatus::Error(err.to_string()));
                        this.refresh_live_keys(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Discovers every database visible on a network connection's server and
    /// stores the result on `connection.databases`, so the panel can render
    /// a picker instead of requiring the user to already know the exact
    /// database name (matches Forge's original two-step "connect to server,
    /// then pick a database" flow — see `DATABASE_PANEL_SPEC.md`'s
    /// Navigation Phase 3). Does not touch `self.registry` — no live driver
    /// connection is opened for the picker itself, only for whichever
    /// database is eventually selected via `select_database`.
    fn discover_databases(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        if self.registry_busy {
            return;
        }
        let Some(connection) = self.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        let db_type = connection.db_type;
        let (Some(host), Some(port), Some(username)) =
            (connection.host, connection.port, connection.username)
        else {
            return;
        };
        let ssl = connection.ssl;
        let key = credential_url(db_type, &host, port);

        log::info!(
            "database_panel: discovering databases on {db_type:?} connection {id:?} \
             (host={host}, port={port}, username={username:?})"
        );

        // `id` may already have live database sessions (discovery is also
        // used to refresh the "Open existing" picker's candidates on an
        // already-connected server) — this probe is a separate, one-off
        // admin connection that never touches `self.registry`, so it must
        // not stomp a genuinely-`Connected` status with its own transient
        // Connecting/Disconnected bookkeeping below.
        let already_connected = self.registry.is_connected(id);
        if !already_connected {
            self.statuses.insert(id, ConnectionStatus::Connecting);
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
            let password = match credentials_provider.read_credentials(&key, cx).await {
                Ok(Some((_, bytes))) => String::from_utf8(bytes).unwrap_or_default(),
                Ok(None) => {
                    log::error!("database_panel: discover {id:?} failed: no saved password");
                    this.update(cx, |this, cx| {
                        if !already_connected {
                            this.statuses.insert(
                                id,
                                ConnectionStatus::Error(
                                    "No saved password for this connection".into(),
                                ),
                            );
                        }
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                Err(err) => {
                    log::error!(
                        "database_panel: discover {id:?} failed reading saved credentials: {err}"
                    );
                    this.update(cx, |this, cx| {
                        if !already_connected {
                            this.statuses
                                .insert(id, ConnectionStatus::Error(err.to_string()));
                        }
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };

            let params = NetworkConnectParams {
                host,
                port,
                database: String::new(),
                username,
                password,
                ssl,
            };

            let result = gpui_tokio::Tokio::spawn_result(cx, async move {
                anyhow::Ok(database_backend::list_databases(db_type, params).await)
            })
            .await
            .map_err(|err| err.to_string())
            .and_then(|inner| inner.map_err(|err| err.to_string()));

            this.update(cx, |this, cx| {
                match result {
                    Ok(names) => {
                        log::info!(
                            "database_panel: discover {id:?} found {} databases",
                            names.len()
                        );
                        let mut next_id = this.next_database_id;
                        let databases: Vec<SavedDatabase> = names
                            .into_iter()
                            .map(|name| {
                                next_id += 1;
                                SavedDatabase {
                                    id: DatabaseId(next_id),
                                    name,
                                }
                            })
                            .collect();
                        this.next_database_id = next_id;
                        if let Some(connection) = this.connections.iter_mut().find(|c| c.id == id) {
                            connection.databases = databases;
                        }
                        // Only the "was never connected" probe outcome — a
                        // still-live connection (this discovery just
                        // refreshed its picker candidates) keeps whatever
                        // status the registry actually reflects.
                        if !already_connected {
                            this.statuses.insert(id, ConnectionStatus::Disconnected);
                        }
                        this.persist_connections(cx);
                    }
                    Err(err) => {
                        log::error!("database_panel: discover {id:?} failed: {err}");
                        if !already_connected {
                            this.statuses.insert(id, ConnectionStatus::Error(err));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Picks `name` as the connection's active database, persists that
    /// choice, and dials the actual driver connection (`connect_network`)
    /// for it — mirrors the spec's "selecting a database connects to it and
    /// opens its schema view."
    fn select_database(&mut self, id: ConnectionId, name: String, cx: &mut Context<Self>) {
        if self.registry_busy {
            return;
        }
        // Tuple-key routing: re-selecting a database that is ALREADY connected
        // under `(id, name)` never re-dials — the live session is already in
        // the tuple-key registry, so we just re-point the connection's active
        // database to it. Only an unconnected database falls through to a
        // fresh network dial.
        if self.registry.is_database_connected(id, &name) {
            self.registry.set_active_database(id, &name);
            if let Some(connection) = self.connections.iter_mut().find(|c| c.id == id) {
                // Keep `connection.database` mirroring the registry's active
                // database even on this no-redial path — it's what
                // `persist_connections` writes (so restart reconnects to the
                // right one) and what the inspector's "Database" row reads,
                // not just an initial-connect input.
                connection.database = Some(name.clone());
            }
            // Same "instant switch" reasoning as the no-redial branch above:
            // every successful connect already runs `fetch_schema` once for
            // its database, so a schema entry keyed to `(id, name)` only
            // stays missing here if the very first fetch is still in flight
            // (or failed) — genuinely new work, not a stale-cache refresh.
            if !matches!(
                self.schemas.get(&(id, name)),
                Some(SchemaState::Loaded { .. })
            ) {
                self.fetch_schema(id, cx);
            }
            self.persist_connections(cx);
            cx.notify();
            return;
        }
        let Some(connection) = self.connections.iter_mut().find(|c| c.id == id) else {
            return;
        };
        connection.database = Some(name);
        let connection = connection.clone();
        self.persist_connections(cx);
        self.connect_network(connection, cx);
    }

    /// Opens the "Open existing" picker for `id` (only one connection's at
    /// a time, and mutually exclusive with the "Create database" form — see
    /// `render_database_tabs`'s page match). Re-runs discovery so the
    /// candidate list isn't stale — the databases shown are whatever
    /// `discover_databases` found minus whichever already have a live
    /// session.
    fn open_database_picker(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        self.creating_database_for = None;
        self.opening_database_for = Some(id);
        self.discover_databases(id, cx);
    }

    /// Returns to the default page — closes the "Open existing" picker,
    /// whether via Cancel or after a database was picked from it.
    fn close_database_pickers(&mut self, cx: &mut Context<Self>) {
        self.opening_database_for = None;
        cx.notify();
    }

    /// Opens the "Create database" input for `id`'s picker (only one
    /// connection's at a time, and mutually exclusive with "Open existing").
    fn start_create_database(
        &mut self,
        id: ConnectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.opening_database_for = None;
        self.creating_database_for = Some(id);
        self.new_database_name.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        cx.notify();
    }

    fn cancel_create_database(&mut self, cx: &mut Context<Self>) {
        self.creating_database_for = None;
        cx.notify();
    }

    /// Runs `CREATE DATABASE` on the server, then re-runs discovery so the
    /// new database shows up in the picker.
    fn submit_create_database(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.creating_database_for else {
            return;
        };
        let name = self.new_database_name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        let Some(connection) = self.connections.iter().find(|c| c.id == id).cloned() else {
            return;
        };
        let db_type = connection.db_type;
        let (Some(host), Some(port), Some(username)) =
            (connection.host, connection.port, connection.username)
        else {
            return;
        };
        let ssl = connection.ssl;
        let key = credential_url(db_type, &host, port);

        self.creating_database_for = None;
        self.new_database_name.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        log::info!("database_panel: creating database {name:?} on connection {id:?}");
        cx.notify();

        cx.spawn(async move |this, cx| {
            let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
            let password = match credentials_provider.read_credentials(&key, cx).await {
                Ok(Some((_, bytes))) => String::from_utf8(bytes).unwrap_or_default(),
                _ => {
                    log::error!(
                        "database_panel: create database on {id:?} failed: no saved password"
                    );
                    this.update(cx, |this, cx| {
                        this.statuses.insert(
                            id,
                            ConnectionStatus::Error("No saved password for this connection".into()),
                        );
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };

            let params = NetworkConnectParams {
                host,
                port,
                database: String::new(),
                username,
                password,
                ssl,
            };

            let result = gpui_tokio::Tokio::spawn_result(cx, async move {
                anyhow::Ok(database_backend::create_database(db_type, params, &name).await)
            })
            .await
            .map_err(|err| err.to_string())
            .and_then(|inner| inner.map_err(|err| err.to_string()));

            match result {
                Ok(()) => {
                    log::info!("database_panel: create database on {id:?} succeeded");
                    this.update(cx, |this, cx| this.discover_databases(id, cx))
                        .ok();
                }
                Err(err) => {
                    log::error!("database_panel: create database on {id:?} failed: {err}");
                    this.update(cx, |this, cx| {
                        this.statuses.insert(id, ConnectionStatus::Error(err));
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    /// Registers a database name that's already known to exist (e.g. one
    /// discovery couldn't see due to permissions) as the connection's active
    /// database, without attempting to create it.
    fn submit_register_database(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.creating_database_for else {
            return;
        };
        let name = self.new_database_name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        self.creating_database_for = None;
        self.new_database_name.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.select_database(id, name, cx);
    }

    fn disconnect(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        if self.registry_busy {
            return;
        }
        // Disconnecting the connection is not closing its databases: they stay
        // in the tab strip, not connected, and connect again when chosen.
        let open: Vec<String> = self
            .live_keys
            .iter()
            .filter(|(connection_id, database)| *connection_id == id && !database.is_empty())
            .map(|(_, database)| database.clone())
            .collect();
        self.parked_databases.entry(id).or_default().extend(open);
        self.registry.disconnect(id).ok();
        self.statuses.insert(id, ConnectionStatus::Disconnected);
        self.refresh_live_keys(cx);
    }

    /// Closes one database's live session on `id` (the database tab strip's
    /// "x") — leaves the connection itself, and any other database still
    /// open on it, untouched. The registry picks another still-live database
    /// as active if one exists (see `ConnectionRegistry::disconnect_database`);
    /// `connection.database` is kept in sync with whatever that ends up
    /// being (or `None`, if this was the last one), same reasoning as
    /// `select_database`'s no-redial path.
    fn disconnect_database(&mut self, id: ConnectionId, database: String, cx: &mut Context<Self>) {
        if self.registry_busy {
            return;
        }
        if self.registry.disconnect_database(id, &database).is_err() {
            return;
        }
        let active = self.registry.active_database(id);
        if let Some(connection) = self.connections.iter_mut().find(|c| c.id == id) {
            connection.database = active;
        }
        self.persist_connections(cx);
        self.refresh_live_keys(cx);
    }

    /// Confirms before dropping a saved connection — irreversible (there is
    /// no undo for a deleted `ConnectionConfig`, unlike disconnecting, which
    /// just drops the live session) and previously a single un-gated click.
    /// Uses GPUI's own `window.prompt`, not `gpui_component`'s
    /// `open_dialog`/a custom `ModalView` — see `open_workspace_modal`'s doc
    /// comment in this file for why a `gpui_component`-Root-dependent dialog
    /// isn't an option here, and `project_panel::remove`'s delete-file
    /// confirmation for the same pattern already used elsewhere in Zed.
    fn delete_connection(&mut self, id: ConnectionId, window: &mut Window, cx: &mut Context<Self>) {
        if self.registry_busy {
            return;
        }
        let Some(title) = self
            .connections
            .iter()
            .find(|connection| connection.id == id)
            .map(|connection| connection.title.clone())
        else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete the connection \u{201c}{title}\u{201d}?"),
            Some("This removes the saved connection and its cached schema. It does not affect the database itself, and cannot be undone."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |this, cx| {
                this.registry.disconnect(id).ok();
                this.statuses.remove(&id);
                this.connections.retain(|connection| connection.id != id);
                this.parked_databases.remove(&id);
                this.refresh_live_keys(cx);
                // A connection can have several `(id, database)` schema/tree-state
                // slots now (one per database that was ever opened this session) —
                // drop all of them, not just a single `id`-keyed one.
                this.schemas.retain(|key, _| key.0 != id);
                this.tree_states.retain(|key, _| key.0 != id);
                this.workbench_tree_states.retain(|key, _| key.0 != id);
                if this.active_connection == Some(id) {
                    this.sync_active_connection_for_type();
                }
                this.persist_connections(cx);
                this.persist_schema_cache(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Handles a click on the type tab strip (`[SQLite][PostgreSQL][MySQL]
    /// [MSSQL]`): switches `active_type`, which filters the connection tab
    /// strip below it, and restores that type's last-selected
    /// server/database (or the Add Connection form if it has none yet).
    fn select_type_tab(&mut self, db_type: DbType, cx: &mut Context<Self>) {
        if self.active_type == db_type {
            return;
        }
        self.active_type = db_type;
        self.add_connection_error = None;
        self.sync_active_connection_for_type();
        self.persist_connections(cx);
        cx.notify();
    }

    /// Recomputes `active_connection` for `active_type`: that type's
    /// remembered server if it still exists and still matches the type,
    /// else that type's first connection, else `None` (the empty-type-tab
    /// state, which falls through to the Add Connection form).
    fn sync_active_connection_for_type(&mut self) {
        self.active_connection = self
            .active_connection_by_type
            .get(&self.active_type)
            .copied()
            .filter(|id| {
                self.connections.iter().any(|connection| {
                    connection.id == *id && connection.db_type == self.active_type
                })
            })
            .or_else(|| {
                self.connections
                    .iter()
                    .find(|connection| connection.db_type == self.active_type)
                    .map(|connection| connection.id)
            });
    }

    /// Handles a click on the connection tab strip: `ix < filtered_ids.len()`
    /// (that type's connections, in the order the strip rendered them)
    /// selects that connection; the trailing index (the "+" tab) opens the
    /// Add Connection form instead, locked to `active_type`.
    fn select_connection_tab(
        &mut self,
        filtered_ids: &[ConnectionId],
        ix: usize,
        cx: &mut Context<Self>,
    ) {
        match filtered_ids.get(ix) {
            Some(&id) => {
                self.active_connection = Some(id);
                self.active_connection_by_type.insert(self.active_type, id);
                self.persist_connections(cx);
                cx.notify();
            }
            None => self.start_add_connection(cx),
        }
    }

    /// Opens the Add Connection form for `active_type` (the tab strip's
    /// trailing "+"). Pre-fills the network port default the old type
    /// dropdown's `on_change` used to, since there's no dropdown anymore —
    /// the type is fully determined by whichever type tab is selected.
    fn start_add_connection(&mut self, cx: &mut Context<Self>) {
        self.active_connection = None;
        self.add_connection_error = None;
        self.new_connection_port_pending = self.active_type != DbType::Sqlite
            && self.new_connection_port.read(cx).value().trim().is_empty();
        cx.notify();
    }

    /// `self.registry.is_connected` (a live session actually exists) is the
    /// source of truth here, not just `self.statuses` — that map is written
    /// by several async flows (discovery, connect) that don't always know
    /// about each other, so trusting it alone risks showing "Disconnected"
    /// for a connection that's genuinely live (or vice versa).
    fn status_indicator(&self, id: ConnectionId) -> (SharedString, Color) {
        if self.registry.is_connected(id) {
            return ("●".into(), Color::Success);
        }
        match self.statuses.get(&id) {
            None | Some(ConnectionStatus::Disconnected) => ("○".into(), Color::Muted),
            Some(ConnectionStatus::Connecting) => ("◐".into(), Color::Warning),
            Some(ConnectionStatus::Connected) => ("●".into(), Color::Success),
            Some(ConnectionStatus::Error(_)) => ("!".into(), Color::Error),
        }
    }

    fn render_add_connection_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        // Determined entirely by the selected type tab — there's no type
        // picker in this form itself, since you can only reach it from
        // inside a specific type's "+" tab (see `start_add_connection`).
        let db_type = self.active_type;
        let busy = self.registry_busy;
        // The theme's default unfocused input border (`cx.theme().input`) is
        // too close to this panel's background to read as a field boundary
        // — users can't tell where to click. Force a higher-contrast border
        // explicitly rather than relying on that token.
        let input_border = cx.theme().border;
        // Two fields to a row, each label above its input. The form is
        // capped so that a field is about 512px wide however wide the panel
        // is, and it still narrows with the panel.
        let form_width = px(1048.);

        let title = self.new_connection_title.clone();
        let host = self.new_connection_host.clone();
        let port = self.new_connection_port.clone();
        let username = self.new_connection_username.clone();
        let service = self.new_connection_service.clone();
        let password = self.new_connection_password.clone();
        let ssl = self.new_connection_ssl;

        let ssl_entity = entity.clone();
        let browse_entity = entity.clone();
        let connect_entity = entity.clone();

        let text_field = move |label: &'static str, state: &Entity<InputState>| {
            field()
                .label(label)
                .child(Input::new(state).border_1().border_color(input_border))
        };

        let fields = SettingItem::render(move |_options, _window, _cx| {
            let form = v_form().columns(2).child(text_field("Title", &title));
            let form = match db_type {
                DbType::Sqlite => form,
                DbType::Postgres | DbType::MySql | DbType::MsSql | DbType::Oracle => {
                    let ssl_entity = ssl_entity.clone();
                    let form = form
                        .child(text_field("Host", &host))
                        .child(text_field("Port", &port));
                    let form = if db_type == DbType::Oracle {
                        form.child(text_field("Service Name", &service))
                    } else {
                        form
                    };
                    form.child(text_field("Username", &username))
                        .child(
                            field().label("Password").child(
                                Input::new(&password)
                                    .mask_toggle()
                                    .border_1()
                                    .border_color(input_border),
                            ),
                        )
                        .child(field().label("SSL").child(
                            Switch::new("add-connection-ssl").checked(ssl).on_click(
                                move |checked, _, cx| {
                                    ssl_entity.update(cx, |this, cx| {
                                        this.new_connection_ssl = *checked;
                                        cx.notify();
                                    });
                                },
                            ),
                        ))
                }
            };
            div()
                .w_full()
                .max_w(form_width)
                .child(form)
                .into_any_element()
        });

        let connect = SettingItem::render(move |_options, _window, _cx| {
            let connect_entity = connect_entity.clone();
            h_flex()
                .child(
                    Button::new("add-connection")
                        .label("Connect")
                        .primary()
                        .disabled(busy)
                        .on_click(move |_, window, cx| {
                            connect_entity.update(cx, |this, cx| {
                                this.add_connection(window, cx);
                            });
                        }),
                )
                .into_any_element()
        });

        let mut group = SettingGroup::new().item(fields);
        if db_type == DbType::Sqlite {
            // The database file is chosen only through the OS file picker —
            // no free-text path — so it always points at a real file.
            group = group.item(SettingItem::render(move |_options, _window, cx| {
                let path = browse_entity.read(cx).new_connection_path.clone();
                let browse_entity = browse_entity.clone();
                let (path_label, path_color) = if path.trim().is_empty() {
                    ("No file selected".to_string(), Color::Muted)
                } else {
                    (path, Color::Default)
                };
                v_flex()
                    .w_full()
                    .gap_3()
                    .child(div().text_sm().child("Database File"))
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(
                                div().min_w_0().child(
                                    Label::new(path_label)
                                        .size(LabelSize::Small)
                                        .color(path_color)
                                        .truncate(),
                                ),
                            )
                            .child(
                                Button::new("browse-sqlite-path")
                                    .outline()
                                    .label("Choose file…")
                                    .on_click(move |_, window, cx| {
                                        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
                                            files: true,
                                            directories: false,
                                            multiple: false,
                                            prompt: Some("Select SQLite Database".into()),
                                        });
                                        let browse_entity = browse_entity.clone();
                                        window
                                            .spawn(cx, async move |cx| {
                                                let Some(mut paths) = prompt
                                                    .await
                                                    .ok()
                                                    .and_then(Result::ok)
                                                    .flatten()
                                                else {
                                                    return;
                                                };
                                                let Some(path) = paths.pop() else {
                                                    return;
                                                };
                                                let _ = browse_entity.update_in(
                                                    cx,
                                                    |this, window, cx| {
                                                        // Default the title to the file name
                                                        // so a pick is enough to connect.
                                                        if this
                                                            .new_connection_title
                                                            .read(cx)
                                                            .value()
                                                            .trim()
                                                            .is_empty()
                                                            && let Some(stem) = path.file_stem()
                                                        {
                                                            let stem =
                                                                stem.to_string_lossy().into_owned();
                                                            this.new_connection_title.update(
                                                                cx,
                                                                |state, cx| {
                                                                    state
                                                                        .set_value(stem, window, cx)
                                                                },
                                                            );
                                                        }
                                                        this.new_connection_path =
                                                            path.to_string_lossy().into_owned();
                                                        cx.notify();
                                                    },
                                                );
                                            })
                                            .detach();
                                    }),
                            ),
                    )
                    .into_any_element()
            }));
        }
        let group = group.item(connect);

        v_flex()
            .size_full()
            .when_some(self.add_connection_error.clone(), |this, message| {
                this.child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .child(
                            Icon::new(GIconName::TriangleAlert)
                                .with_size(Size::XSmall)
                                .text_color(cx.theme().danger_foreground),
                        )
                        .child(
                            Label::new(message)
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        ),
                )
            })
            .child(
                Settings::new("database-panel-add-connection")
                    .sidebar_width(px(0.))
                    .page(
                        SettingPage::new(format!(
                            "Add {} Connection",
                            header::db_type_label(db_type)
                        ))
                        .group(group),
                    ),
            )
    }

    /// Renders the selected connection tab's body: the connection header
    /// (`DatabaseHeader`: status/actions + database tab strip) on top, and
    /// the three-pane explorer split below — or a centered state/error view
    /// while connecting, loading, failing, or when the schema is empty.
    fn render_connection_body(
        &mut self,
        connection: &ConnectionConfig,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = connection.id;
        let is_connected = self.registry.is_connected(id);

        // The header block (status/actions bar + database tabs + create/open
        // pickers) has no fixed height — with enough server/database tabs it
        // can grow past what the dock has room for, and unlike the
        // three-pane explorer below it (which scrolls internally, pane by
        // pane), nothing here could reach whatever got pushed past the
        // bottom edge. Scrolling the whole body as one unit, with the
        // explorer given a floor height instead of `flex_1`-shrinking to
        // nothing, means everything stays reachable either way.
        div()
            .id(("database-connection-body-scroll", id.0))
            .w_full()
            .size_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w_full()
                    .child(self.render_connection_header(connection, cx))
                    .child(div().w_full().min_h(px(420.)).child(self.render_body_split(
                        connection,
                        id,
                        is_connected,
                        cx,
                    ))),
            )
    }

    /// The three-pane explorer shell: `h_resizable` split with a resizable
    /// explorer sidebar (left) and inspector (right) around a flex-grow
    /// content pane. Only rendered once a schema is actually loaded;
    /// everything else routes to `render_connection_state` /
    /// `render_connection_error`.
    fn render_body_split(
        &mut self,
        connection: &ConnectionConfig,
        id: ConnectionId,
        is_connected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if !is_connected {
            return self
                .render_connection_state(connection, cx)
                .into_any_element();
        }

        let (tables, views) = match self.active_schema_state(id) {
            None | Some(SchemaState::Loading) => {
                return self
                    .render_connection_state(connection, cx)
                    .into_any_element();
            }
            Some(SchemaState::Error(message)) => {
                return self
                    .render_connection_error(connection, message, cx)
                    .into_any_element();
            }
            Some(SchemaState::Loaded { tables, views })
                if tables.is_empty() && views.is_empty() =>
            {
                return self
                    .render_connection_state(connection, cx)
                    .into_any_element();
            }
            Some(SchemaState::Loaded { tables, views }) => (tables.clone(), views.clone()),
        };

        h_resizable(("database-body", id.0))
            .child(
                resizable_panel()
                    .size(px(280.))
                    .size_range(px(200.)..px(420.))
                    .flex_none()
                    .child(self.render_explorer_pane(connection, &tables, &views, cx)),
            )
            .child(
                resizable_panel().child(self.render_content_pane(connection, &tables, &views, cx)),
            )
            .child(
                resizable_panel()
                    .size(px(320.))
                    .size_range(px(240.)..px(560.))
                    .flex_none()
                    .child(self.render_inspector_pane(connection, &tables, &views, cx)),
            )
            .into_any_element()
    }

    /// The centered "not connected / loading / empty schema" state.
    fn render_connection_state(
        &self,
        connection: &ConnectionConfig,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = connection.id;
        let is_connected = self.registry.is_connected(id);
        let is_loading = matches!(
            self.active_schema_state(id),
            None | Some(SchemaState::Loading)
        );

        let body: gpui::AnyElement = if is_connected && is_loading {
            h_flex()
                .items_center()
                .gap_2()
                .child(Spinner::new().with_size(Size::Medium))
                .child(
                    Label::new("Loading schema…")
                        .size(LabelSize::Default)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else if is_connected {
            v_flex()
                .items_center()
                .gap_2()
                .child(
                    Icon::new(GIconName::Inbox)
                        .with_size(Size::Large)
                        .text_color(cx.theme().muted_foreground),
                )
                .child(Label::new("No tables").size(LabelSize::Large))
                .child(
                    Label::new("This database has no tables yet.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            let detail = if self.registry_busy {
                "Waiting on another connection to finish before this one can connect.".to_string()
            } else {
                match self.statuses.get(&id) {
                    Some(ConnectionStatus::Error(message)) => {
                        format!("Could not connect: {message}")
                    }
                    _ => "Connect to this database to explore its schema.".to_string(),
                }
            };
            v_flex()
                .items_center()
                .gap_3()
                .child(
                    Icon::new(GIconName::Inbox)
                        .with_size(Size::Large)
                        .text_color(cx.theme().muted_foreground),
                )
                .child(Label::new("Not connected").size(LabelSize::Large))
                .child(
                    Label::new(detail)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Button::new(("connect", id.0))
                        .primary()
                        .disabled(self.registry_busy)
                        .label("Connect")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.connect(id, cx);
                        })),
                )
                .into_any_element()
        };

        v_flex()
            .w_full()
            .size_full()
            .items_center()
            .justify_center()
            .px_6()
            .child(body)
    }

    /// The centered "schema failed to load" state with a Retry action.
    fn render_connection_error(
        &self,
        connection: &ConnectionConfig,
        message: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = connection.id;
        v_flex()
            .w_full()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .px_6()
            .child(
                Icon::new(GIconName::TriangleAlert)
                    .with_size(Size::Large)
                    .text_color(cx.theme().danger_foreground),
            )
            .child(
                Label::new("Failed to load schema")
                    .size(LabelSize::Large)
                    .color(Color::Error),
            )
            .child(
                Label::new(message.to_string())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Button::new(("retry-schema", id.0))
                    .outline()
                    .disabled(self.registry_busy)
                    .label("Retry")
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.fetch_schema(id, cx);
                    })),
            )
    }
}

impl Focusable for DatabasePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DatabasePanel {}

impl Panel for DatabasePanel {
    fn persistent_name() -> &'static str {
        "Database Panel"
    }

    fn panel_key() -> &'static str {
        "DatabasePanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Bottom)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // Fixed to the bottom dock — see `position_is_valid`.
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(440.)
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<ui::IconName> {
        DatabasePanelSettings::get_global(cx)
            .button
            .then_some(ui::IconName::DatabaseZap)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Databases")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        22
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.database_panel.get_or_insert_default().button = Some(false);
        }))
    }
}

impl DatabasePanel {
    /// The panel-level type tab strip (`[SQLite][PostgreSQL][MySQL]
    /// [MSSQL]`): fixed 4 tabs regardless of how many connections (if any)
    /// exist per type — it's an entry point to a type you haven't used yet,
    /// not just a filter over ones you have. Filters `render_connection_tabs`
    /// below it to `active_type`.
    fn render_type_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_index = DB_TYPES
            .iter()
            .position(|db_type| *db_type == self.active_type)
            .unwrap_or(0);

        TabBar::new("database-type-tabs")
            .with_variant(TabVariant::Underline)
            .px_3()
            .children(
                DB_TYPES
                    .iter()
                    .map(|db_type| Tab::new().label(header::db_type_label(*db_type))),
            )
            .selected_index(selected_index)
            .on_click(cx.listener(move |this, ix: &usize, _, cx| {
                if let Some(db_type) = DB_TYPES.get(*ix).copied() {
                    this.select_type_tab(db_type, cx);
                }
            }))
    }

    /// The connection tab strip — one `Tab` per `active_type` connection
    /// (server), plus a trailing "+" tab that opens the Add Connection form
    /// locked to that type, mirroring `npm_manager_panel`'s `project_tabs`
    /// (see its doc comment) and the original Forge panel's connection
    /// pills.
    fn render_connection_tabs(
        &self,
        connections: &[ConnectionConfig],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active_type = self.active_type;
        let filtered: Vec<&ConnectionConfig> = connections
            .iter()
            .filter(|connection| connection.db_type == active_type)
            .collect();
        let filtered_ids: Vec<ConnectionId> =
            filtered.iter().map(|connection| connection.id).collect();
        let add_tab_ix = filtered.len();
        let selected_index = self
            .active_connection
            .and_then(|id| filtered.iter().position(|connection| connection.id == id))
            .unwrap_or(add_tab_ix);

        TabBar::new("database-connection-tabs")
            .with_variant(TabVariant::Underline)
            .px_3()
            .children(filtered.iter().map(|connection| {
                let (indicator, color) = self.status_indicator(connection.id);
                Tab::new()
                    .prefix(Label::new(indicator).color(color))
                    .label(connection.title.clone())
            }))
            .child(
                Tab::new()
                    .prefix(Icon::new(GIconName::Plus))
                    .label("Add Connection"),
            )
            .selected_index(selected_index)
            .on_click(cx.listener(move |this, ix: &usize, _, cx| {
                this.select_connection_tab(&filtered_ids, *ix, cx);
            }))
    }
}

impl Render for DatabasePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.new_connection_port_pending) {
            let port = default_port(self.active_type).to_string();
            self.new_connection_port
                .update(cx, |state, cx| state.set_value(port, window, cx));
        }
        let connections = self.connections.clone();
        let active_connection = self.active_connection.and_then(|id| {
            connections
                .iter()
                .find(|connection| connection.id == id)
                .cloned()
        });

        let body = match active_connection {
            Some(connection) => self
                .render_connection_body(&connection, cx)
                .into_any_element(),
            None => self.render_add_connection_form(cx).into_any_element(),
        };

        v_flex()
            .id("database-panel")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .bg(cx.theme().sidebar)
            .child(
                PanelHeader::new("Database")
                    .icon(Icon::new(GIconName::Database).text_color(cx.theme().foreground)),
            )
            .child(self.render_type_tabs(cx))
            .child(self.render_connection_tabs(&connections, cx))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(keys: &[(u64, &str)]) -> HashSet<SchemaKey> {
        keys.iter()
            .map(|(id, database)| (ConnectionId(*id), database.to_string()))
            .collect()
    }

    fn parked(entries: &[(u64, &[&str])]) -> HashMap<ConnectionId, Vec<String>> {
        entries
            .iter()
            .map(|(id, names)| {
                (
                    ConnectionId(*id),
                    names.iter().map(|name| name.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_connections_tab_strip_lists_connected_and_remembered_databases_together() {
        let names = remembered_names(
            &live(&[(1, "shop"), (1, "audit")]),
            &parked(&[(1, &["reports", "archive"])]),
            ConnectionId(1),
        );
        assert_eq!(names, ["archive", "audit", "reports", "shop"]);
    }

    #[test]
    fn a_database_that_is_both_connected_and_remembered_is_listed_once() {
        let names = remembered_names(
            &live(&[(1, "shop")]),
            &parked(&[(1, &["shop", "audit"])]),
            ConnectionId(1),
        );
        assert_eq!(names, ["audit", "shop"]);
    }

    #[test]
    fn another_connections_databases_are_not_listed() {
        let names = remembered_names(
            &live(&[(1, "shop"), (2, "orders")]),
            &parked(&[(2, &["billing"])]),
            ConnectionId(1),
        );
        assert_eq!(names, ["shop"]);
    }

    #[test]
    fn the_empty_name_a_sqlite_file_lives_under_is_not_listed() {
        let names = remembered_names(&live(&[(1, "")]), &parked(&[]), ConnectionId(1));
        assert!(names.is_empty());
    }
}

//! Conversion from the schema IR/layout into `gpui_flow` graph state.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, ThemeColor,
    menu::{ContextMenuExt as _, PopupMenuItem},
};
use gpui_flow::{Controls, FlowGraph, FlowState, Minimap};
use gpui_flow::{EdgeType, FlowEdge, FlowNode, HandleDef, HandlePosition, HandleType};

use super::{
    ir::{GraphRelationship, SchemaGraph, TableId},
    layout::{SchemaLayout, TableLayout},
};
use workspace::{Item, Workspace};

pub fn flow_graph(graph: &SchemaGraph, layout: &SchemaLayout) -> (Vec<FlowNode>, Vec<FlowEdge>) {
    let mut table_ids = graph.tables.keys().cloned().collect::<Vec<_>>();
    table_ids.sort();

    let nodes = table_ids
        .iter()
        .filter_map(|table_id| {
            let table = graph.tables.get(table_id)?;
            let table_layout = layout.tables.get(table_id)?;
            Some(table_node(table_id, table_layout, table, graph))
        })
        .collect::<Vec<_>>();
    let edges = graph
        .relationships
        .iter()
        .filter_map(|relationship| relationship_edge(relationship, graph, layout))
        .collect::<Vec<_>>();

    (nodes, edges)
}

pub struct SchemaGraphView {
    flow: gpui::Entity<FlowGraph>,
    controls: gpui::Entity<Controls>,
    minimap: gpui::Entity<Minimap>,
    focus_handle: FocusHandle,
    table_count: usize,
    relationship_count: usize,
    ignored_cycle_relationships: usize,
    /// Kept so `update_schema` can rebuild the node renderer, whose context
    /// menus call back into the panel.
    panel: WeakEntity<super::DatabasePanel>,
    connection_id: super::ConnectionId,
}

impl SchemaGraphView {
    pub fn new(
        graph: &SchemaGraph,
        layout: &SchemaLayout,
        panel: WeakEntity<super::DatabasePanel>,
        connection_id: super::ConnectionId,
        cx: &mut Context<Self>,
    ) -> Self {
        let (flow, controls, minimap) = Self::build_flow(graph, layout, &panel, connection_id, cx);
        Self {
            flow,
            controls,
            minimap,
            focus_handle: cx.focus_handle(),
            table_count: graph.tables.len(),
            relationship_count: graph.relationships.len(),
            ignored_cycle_relationships: layout.ignored_cycle_relationships.len(),
            panel,
            connection_id,
        }
    }

    fn build_flow(
        graph: &SchemaGraph,
        layout: &SchemaLayout,
        panel: &WeakEntity<super::DatabasePanel>,
        connection_id: super::ConnectionId,
        cx: &mut Context<Self>,
    ) -> (gpui::Entity<FlowGraph>, gpui::Entity<Controls>, gpui::Entity<Minimap>) {
        let (nodes, edges) = flow_graph(graph, layout);
        let menu_items = Rc::new(table_menu_items_by_table(graph));
        let state = cx.new(|_| FlowState::new(nodes, edges));
        let panel = panel.clone();
        let flow = cx.new(|cx| {
            FlowGraph::new(state.clone(), cx).default_renderer(move |node, window, cx| {
                render_table_card(node, &menu_items, panel.clone(), connection_id, window, cx)
            })
        });
        let controls = cx.new(|_| Controls::new(state.clone()));
        let minimap = cx.new(|_| Minimap::new(state));
        (flow, controls, minimap)
    }

    /// Rebuilds the graph in place from a freshly fetched schema, so a graph
    /// tab that is already open follows a Refresh (or a DDL statement)
    /// instead of showing the tables it was first opened with. Pan and zoom
    /// reset, since the layout is recomputed.
    pub(crate) fn update_schema(
        &mut self,
        graph: &SchemaGraph,
        layout: &SchemaLayout,
        cx: &mut Context<Self>,
    ) {
        let panel = self.panel.clone();
        let (flow, controls, minimap) = Self::build_flow(graph, layout, &panel, self.connection_id, cx);
        self.flow = flow;
        self.controls = controls;
        self.minimap = minimap;
        self.table_count = graph.tables.len();
        self.relationship_count = graph.relationships.len();
        self.ignored_cycle_relationships = layout.ignored_cycle_relationships.len();
        cx.notify();
    }
}

impl EventEmitter<()> for SchemaGraphView {}

impl Focusable for SchemaGraphView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SchemaGraphView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        self.flow.update(cx, |flow, _| {
            flow.set_theme_colors(
                color_u32(colors.background),
                color_u32(colors.border),
                color_u32(colors.background),
                color_u32(colors.border),
                color_u32(colors.accent),
            );
        });
        self.controls.update(cx, |controls, _| {
            controls.theme_colors(colors.popover, colors.border, colors.foreground);
        });
        self.minimap.update(cx, |minimap, _| {
            minimap.theme_colors(colors.popover, colors.border);
        });
        div()
            .size_full()
            .relative()
            .bg(colors.background)
            .child(self.flow.clone())
            .when(
                self.table_count > 1 && self.relationship_count == 0,
                |this| this.child(self.no_relationship_notice(colors)),
            )
            .when(self.ignored_cycle_relationships > 0, |this| {
                this.child(self.cycle_notice(colors))
            })
            .child(
                div()
                    .absolute()
                    .bottom(px(16.0))
                    .left(px(16.0))
                    .child(self.controls.clone()),
            )
            .child(
                div()
                    .absolute()
                    .bottom(px(16.0))
                    .right(px(16.0))
                    .child(self.minimap.clone()),
            )
    }
}

impl SchemaGraphView {
    /// A schema with tables but no foreign keys lays every table out in one
    /// layer, so the graph renders as a grid with no lines and no obvious
    /// reason why. Say so, rather than leaving the user to guess.
    fn no_relationship_notice(&self, colors: ThemeColor) -> AnyElement {
        notice_banner(
            colors,
            format!(
                "No foreign keys found across {} tables - nothing to relate, so they are shown \
                 in a grid. Declare a FOREIGN KEY constraint to see relationships.",
                self.table_count
            ),
        )
    }

    fn cycle_notice(&self, colors: ThemeColor) -> AnyElement {
        notice_banner(
            colors,
            format!(
                "{} circular reference(s) cannot be laid out in dependency order; they are still \
                 drawn.",
                self.ignored_cycle_relationships
            ),
        )
    }
}

fn notice_banner(colors: ThemeColor, message: String) -> AnyElement {
    div()
        .absolute()
        .top(px(16.0))
        .left(px(16.0))
        .max_w(px(420.0))
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.popover)
        .text_color(colors.foreground)
        .text_sm()
        .child(message)
        .into_any_element()
}

pub struct SchemaGraphTab {
    graph: gpui::Entity<SchemaGraphView>,
    key: super::SchemaKey,
    title: SharedString,
    database_panel: WeakEntity<super::DatabasePanel>,
    /// Set once this tab has asked for its database to be connected, which it
    /// does the first time it shows (see `WorkbenchTab::connect_requested`).
    connect_requested: bool,
}

impl SchemaGraphTab {
    fn new(
        graph: gpui::Entity<SchemaGraphView>,
        key: super::SchemaKey,
        title: SharedString,
        database_panel: WeakEntity<super::DatabasePanel>,
    ) -> Self {
        Self {
            graph,
            key,
            title,
            database_panel,
            connect_requested: false,
        }
    }
}

impl EventEmitter<()> for SchemaGraphTab {}

impl Focusable for SchemaGraphTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.graph.read(cx).focus_handle(cx)
    }
}

impl Render for SchemaGraphTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(panel) = self.database_panel.upgrade() else {
            return self.graph.clone().into_any_element();
        };
        if !self.connect_requested {
            self.connect_requested = true;
            let panel = panel.clone();
            let key = self.key.clone();
            window.defer(cx, move |_, cx| {
                panel.update(cx, |panel, cx| panel.request_database(key, cx));
            });
        }
        if panel.read(cx).database_unavailable(&self.key) {
            let key = self.key.clone();
            panel.update(cx, |panel, cx| panel.render_database_placeholder(&key, cx))
        } else {
            self.graph.clone().into_any_element()
        }
    }
}

impl Item for SchemaGraphTab {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.title.clone())
    }
}

impl workspace::SerializableItem for SchemaGraphTab {
    fn serialized_item_kind() -> &'static str {
        "database_graph"
    }

    fn cleanup(
        workspace_id: workspace::WorkspaceId,
        alive_items: Vec<workspace::ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = crate::persistence::DatabasePanelTabsDb::global(cx);
        cx.background_spawn(async move { db.delete_unloaded(workspace_id, alive_items).await })
    }

    /// Rebuilds the graph at once, from the cached schema, without waiting
    /// for the database: the tab connects it the first time it shows (see
    /// `connect_requested`), and until then says it is not connected. Only a
    /// connection that has since been deleted fails the restore, with a toast
    /// naming it.
    fn deserialize(
        _project: Entity<project::Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let db = crate::persistence::DatabasePanelTabsDb::global(cx);
        window.spawn(cx, async move |cx| {
            let persisted = db
                .tab_for_item(item_id, workspace_id)?
                .filter(|tab| tab.kind == crate::persistence::TabKind::Graph)
                .ok_or_else(|| anyhow::anyhow!("No schema graph tab persisted for item"))?;
            let key: super::SchemaKey = (persisted.connection_id, persisted.database);

            let workspace_entity = workspace
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("Workspace released before graph restore"))?;
            let panel = workspace_entity
                .read_with(cx, |workspace, cx| {
                    workspace.panel::<super::DatabasePanel>(cx)
                })
                .ok_or_else(|| anyhow::anyhow!("Database panel not available"))?;

            let connection_exists = panel.read_with(cx, |panel, _| {
                panel
                    .connections
                    .iter()
                    .any(|connection| connection.id == key.0)
            });
            if !connection_exists {
                log::warn!(
                    "database_panel: schema graph restore for {key:?} failed: connection no longer exists"
                );
                let message = panel.read_with(cx, |panel, _| panel.reopen_failure_message(&key));
                workspace_entity.update(cx, |workspace, cx| {
                    workspace.show_toast(
                        workspace::Toast::new(
                            workspace::notifications::NotificationId::unique::<SchemaGraphTab>(),
                            message,
                        ),
                        cx,
                    );
                });
                anyhow::bail!("database connection could not be reopened");
            }

            cx.update(|_window, cx| {
                let (id, database) = key.clone();
                let (graph, title) = panel.update(cx, |panel, cx| {
                    let tables = match panel.schemas.get(&key) {
                        Some(super::SchemaState::Loaded { tables, .. }) => tables.clone(),
                        // No schema cached for it yet: the graph starts empty
                        // and is filled in when the database connects (see
                        // `refresh_schema_graph`).
                        _ => std::rc::Rc::from([]),
                    };
                    let graph = panel.schema_graph_for_key(&key, &tables, cx);
                    (graph, panel.schema_graph_tab_title(id, &database))
                });
                anyhow::Ok(cx.new(|_| SchemaGraphTab::new(graph, key, title, panel.downgrade())))
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
                crate::persistence::TabKind::Graph,
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

impl super::DatabasePanel {
    /// The schema graph tab's display title for `id`'s `database` — shared
    /// between `open_schema_graph_tab` (a live, already-connected click) and
    /// `SchemaGraphTab::deserialize` (a tab restored from a previous
    /// session, where the connection is re-established as part of the same
    /// call).
    fn schema_graph_tab_title(&self, id: super::ConnectionId, database: &str) -> SharedString {
        self.connections
            .iter()
            .find(|connection| connection.id == id)
            .map(|connection| {
                if database.is_empty() {
                    format!("Schema: {}", connection.title)
                } else {
                    format!("Schema: {} / {database}", connection.title)
                }
            })
            .unwrap_or_else(|| "Schema Graph".to_string())
            .into()
    }

    pub(crate) fn open_schema_graph_tab(
        &mut self,
        id: super::ConnectionId,
        window: &mut Window,
        cx: &mut Context<super::DatabasePanel>,
    ) {
        let database = self.registry.active_database(id).unwrap_or_default();
        let key = (id, database.clone());
        let Some(super::SchemaState::Loaded { tables, .. }) = self.schemas.get(&key) else {
            return;
        };
        let tables = tables.clone();
        let graph = self.schema_graph_for_key(&key, &tables, cx);
        let title = self.schema_graph_tab_title(id, &database);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let panel = cx.entity().downgrade();
        workspace.update(cx, |workspace, cx| {
            open_schema_graph(graph, key, title, panel, workspace, window, cx);
        });
    }

    /// Pushes a freshly fetched schema into `key`'s graph view, if one has
    /// been created — `schema_graph_for_key` only builds it once, so without
    /// this a Refresh would update the tree but leave the graph stale.
    pub(crate) fn refresh_schema_graph(
        &mut self,
        key: &super::SchemaKey,
        tables: &[database_backend::TableInfo],
        cx: &mut Context<super::DatabasePanel>,
    ) {
        let Some(view) = self.schema_graphs.get(key).cloned() else {
            return;
        };
        let schema = database_backend::Schema {
            tables: tables.to_vec(),
            views: Vec::new(),
        };
        let graph = SchemaGraph::from_schema(&schema);
        let layout = SchemaLayout::compute(&graph, crate::layout::LayoutConfig::default());
        view.update(cx, |view, cx| view.update_schema(&graph, &layout, cx));
    }

    pub(crate) fn schema_graph_for_key(
        &mut self,
        key: &super::SchemaKey,
        tables: &[database_backend::TableInfo],
        cx: &mut Context<super::DatabasePanel>,
    ) -> gpui::Entity<SchemaGraphView> {
        if !self.schema_graphs.contains_key(key) {
            let schema = database_backend::Schema {
                tables: tables.to_vec(),
                views: Vec::new(),
            };
            let graph = SchemaGraph::from_schema(&schema);
            let layout = SchemaLayout::compute(&graph, crate::layout::LayoutConfig::default());
            let panel = cx.entity().downgrade();
            let connection_id = key.0;
            let view = cx.new(|cx| SchemaGraphView::new(&graph, &layout, panel, connection_id, cx));
            self.schema_graphs.insert(key.clone(), view);
        }
        self.schema_graphs[key].clone()
    }
}

fn open_schema_graph(
    graph: gpui::Entity<SchemaGraphView>,
    key: super::SchemaKey,
    title: SharedString,
    database_panel: WeakEntity<super::DatabasePanel>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .active_pane()
        .read(cx)
        .items()
        .find_map(|item| item.downcast::<SchemaGraphTab>())
        .filter(|tab| tab.read(cx).title == title && key == tab.read(cx).key);

    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
    } else {
        let tab = cx.new(|_| SchemaGraphTab::new(graph, key, title, database_panel));
        workspace.add_item_to_active_pane(Box::new(tab), None, true, window, cx);
    }
}

/// Table nodes only — a `HashMap` keyed by `FlowNode.id` (the table name),
/// built once per graph in `SchemaGraphView::new` and shared (via `Rc`) by
/// every node's `render_table_card` call, so it's computed from
/// `SchemaGraph`/`GraphRelationship` once rather than per-render.
fn table_menu_items_by_table(graph: &SchemaGraph) -> HashMap<String, Vec<(String, String)>> {
    graph
        .tables
        .keys()
        .map(|table_id| (table_id.0.clone(), table_menu_items(table_id, graph)))
        .collect()
}

/// The node context menu's canned queries for one table: a preview, a row
/// count, and one "preview joined with X" entry per relationship this table
/// participates in (skipping self-referential FKs, which would need a table
/// alias to join validly — not worth it for a canned query). Each relies
/// only on metadata already in `GraphTable`/`GraphRelationship`, so there's
/// no extra round-trip to the database to build the menu.
fn table_menu_items(table_id: &TableId, graph: &SchemaGraph) -> Vec<(String, String)> {
    let Some(table) = graph.tables.get(table_id) else {
        return Vec::new();
    };
    let name = &table.name;
    let mut items = vec![
        (
            "Preview rows".to_string(),
            format!("SELECT * FROM {name} LIMIT 100;"),
        ),
        (
            "Row count".to_string(),
            format!("SELECT COUNT(*) FROM {name};"),
        ),
    ];

    for relationship in &graph.relationships {
        let (related_id, pairs): (&TableId, Vec<(&str, &str)>) =
            if relationship.source_table == *table_id {
                (
                    &relationship.target_table,
                    relationship
                        .source_columns
                        .iter()
                        .zip(relationship.target_columns.iter())
                        .map(|(this_col, related_col)| {
                            (this_col.name.as_str(), related_col.name.as_str())
                        })
                        .collect(),
                )
            } else if relationship.target_table == *table_id {
                (
                    &relationship.source_table,
                    relationship
                        .target_columns
                        .iter()
                        .zip(relationship.source_columns.iter())
                        .map(|(this_col, related_col)| {
                            (this_col.name.as_str(), related_col.name.as_str())
                        })
                        .collect(),
                )
            } else {
                continue;
            };
        if related_id == table_id {
            continue;
        }
        let Some(related_table) = graph.tables.get(related_id) else {
            continue;
        };
        let related_name = &related_table.name;
        let on_clause = pairs
            .iter()
            .map(|(this_col, related_col)| {
                format!("{name}.{this_col} = {related_name}.{related_col}")
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let join_columns = pairs
            .iter()
            .map(|(this_col, _)| *this_col)
            .collect::<Vec<_>>()
            .join(", ");
        items.push((
            format!("Preview joined with {related_name} (via {join_columns})"),
            format!("SELECT * FROM {name} JOIN {related_name} ON {on_clause} LIMIT 100;"),
        ));
    }
    items
}

fn render_table_card(
    node: &FlowNode,
    menu_items: &HashMap<String, Vec<(String, String)>>,
    panel: WeakEntity<super::DatabasePanel>,
    connection_id: super::ConnectionId,
    _window: &mut gpui::Window,
    cx: &mut gpui::App,
) -> AnyElement {
    let theme = cx.theme();
    let card = div()
        .id(SharedString::from(format!("db-graph-node-{}", node.id)))
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded_md()
        .bg(theme.colors.background)
        .border_1()
        .border_color(theme.colors.border)
        .text_color(theme.colors.foreground)
        .child(
            div()
                .pb_1()
                .border_b_1()
                .border_color(theme.colors.border)
                .font_weight(FontWeight::BOLD)
                .child(node.label.clone()),
        )
        .children(node.properties.iter().map(|(name, type_name)| {
            div()
                .flex()
                .justify_between()
                .gap_2()
                .text_xs()
                .child(name.clone())
                .child(
                    div()
                        .text_color(theme.colors.muted_foreground)
                        .child(type_name.clone()),
                )
        }));

    let Some(items) = menu_items
        .get(node.id.as_ref())
        .filter(|items| !items.is_empty())
    else {
        return card.into_any_element();
    };
    let items = items.clone();

    card.context_menu(move |menu, _window, _cx| {
        let mut menu = menu;
        for (label, sql) in items.iter().cloned() {
            let panel = panel.clone();
            menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let sql = sql.clone();
                panel
                    .update(cx, |panel, cx| {
                        panel.run_canned_query(connection_id, sql, window, cx);
                    })
                    .ok();
            }));
        }
        menu
    })
    .into_any_element()
}

fn color_u32(color: gpui::Hsla) -> u32 {
    let color = color.to_rgb();
    ((color.r * 255.0).round() as u32) << 16
        | ((color.g * 255.0).round() as u32) << 8
        | (color.b * 255.0).round() as u32
}

fn table_node(
    table_id: &TableId,
    table_layout: &TableLayout,
    table: &super::ir::GraphTable,
    graph: &SchemaGraph,
) -> FlowNode {
    let mut handles = Vec::new();
    let mut seen = HashSet::new();

    for relationship in &graph.relationships {
        if relationship.source_table == *table_id {
            for column in &relationship.source_columns {
                let id = handle_id(column);
                if seen.insert((id.clone(), HandleType::Source)) {
                    handles.push(HandleDef::source(HandlePosition::Left).id(id));
                }
            }
        }
        if relationship.target_table == *table_id {
            for column in &relationship.target_columns {
                let id = handle_id(column);
                if seen.insert((id.clone(), HandleType::Target)) {
                    handles.push(HandleDef::target(HandlePosition::Right).id(id));
                }
            }
        }
    }

    let mut node = FlowNode::new(
        table_id.0.clone(),
        table_layout.position.x,
        table_layout.position.y,
    )
    .label(table.name.clone())
    .node_type("database-table")
    .size(table_layout.width, table_layout.height)
    .handles(handles);
    node.properties = table
        .columns
        .iter()
        .map(|column| {
            (
                column.id.name.clone().into(),
                column.type_name.clone().into(),
            )
        })
        .collect();
    // Read-only explorer: you may rearrange cards to read the schema, but not
    // rewire or delete them - the layout is recomputed from the live schema
    // anyway, so an edit here would be silently discarded on refresh.
    node.draggable = true;
    node.connectable = false;
    node.deletable = false;
    node
}

fn relationship_edge(
    relationship: &GraphRelationship,
    graph: &SchemaGraph,
    layout: &SchemaLayout,
) -> Option<FlowEdge> {
    if !layout.tables.contains_key(&relationship.source_table)
        || !layout.tables.contains_key(&relationship.target_table)
        || !graph.tables.contains_key(&relationship.source_table)
        || !graph.tables.contains_key(&relationship.target_table)
    {
        return None;
    }

    let mut edge = FlowEdge::new(
        relationship.id.clone(),
        relationship.source_table.0.clone(),
        relationship.target_table.0.clone(),
    )
    .edge_type(EdgeType::SmoothStep {
        border_radius: 8.0,
        offset: 24.0,
    })
    .label(cardinality_label(relationship));

    if let Some(column) = relationship.source_columns.first() {
        edge = edge.source_handle(handle_id(column));
    }
    if let Some(column) = relationship.target_columns.first() {
        edge = edge.target_handle(handle_id(column));
    }
    Some(edge)
}

fn handle_id(column: &super::ir::ColumnId) -> String {
    format!("{}.{}", column.table.0, column.name)
}

fn cardinality_label(relationship: &GraphRelationship) -> &'static str {
    match relationship.cardinality {
        super::ir::Cardinality::OneToOne => "1 : 1",
        super::ir::Cardinality::OneToMany => "1 : N",
        super::ir::Cardinality::ManyToMany => "N : N",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{Cardinality, ColumnId, GraphColumn, GraphRelationship, GraphTable},
        layout::{LayoutConfig, SchemaLayout},
    };
    use std::collections::HashMap;

    fn graph() -> SchemaGraph {
        let users = TableId::new("users");
        let posts = TableId::new("posts");
        SchemaGraph {
            tables: HashMap::from([
                (
                    users.clone(),
                    GraphTable {
                        id: users.clone(),
                        name: "users".into(),
                        columns: vec![GraphColumn {
                            id: ColumnId::new(&users, "id"),
                            type_name: "integer".into(),
                            nullable: false,
                            primary_key: true,
                        }],
                        indexes: Vec::new(),
                        is_join_table: false,
                        join_targets: Vec::new(),
                    },
                ),
                (
                    posts.clone(),
                    GraphTable {
                        id: posts.clone(),
                        name: "posts".into(),
                        columns: vec![
                            GraphColumn {
                                id: ColumnId::new(&posts, "id"),
                                type_name: "integer".into(),
                                nullable: false,
                                primary_key: true,
                            },
                            GraphColumn {
                                id: ColumnId::new(&posts, "user_id"),
                                type_name: "integer".into(),
                                nullable: false,
                                primary_key: false,
                            },
                        ],
                        indexes: Vec::new(),
                        is_join_table: false,
                        join_targets: Vec::new(),
                    },
                ),
            ]),
            relationships: vec![GraphRelationship {
                id: "posts: user_id -> users: id".into(),
                source_table: posts.clone(),
                source_columns: vec![ColumnId::new(&posts, "user_id")],
                target_table: users.clone(),
                target_columns: vec![ColumnId::new(&users, "id")],
                cardinality: Cardinality::OneToMany,
            }],
        }
    }

    #[test]
    fn builds_column_anchored_nodes_and_edges() {
        let graph = graph();
        let layout = SchemaLayout::compute(&graph, LayoutConfig::default());
        let (nodes, edges) = flow_graph(&graph, &layout);

        assert_eq!(nodes.len(), 2);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].source_handle.as_deref(), Some("posts.user_id"));
        assert_eq!(edges[0].target_handle.as_deref(), Some("users.id"));
        assert_eq!(edges[0].label.as_deref(), Some("1 : N"));
        assert!(nodes.iter().all(|node| node.draggable));
        assert!(!nodes.iter().any(|node| node.connectable));
        assert!(!nodes.iter().any(|node| node.deletable));
        assert!(nodes.iter().any(|node| {
            node.id.as_ref() == "posts"
                && node.handles.iter().any(|handle| {
                    handle.id.as_deref() == Some("posts.user_id")
                        && handle.handle_type == HandleType::Source
                        && handle.position == HandlePosition::Left
                })
        }));
    }
}

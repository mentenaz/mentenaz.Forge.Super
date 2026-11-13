# database_panel

A bottom-dock database client: manage connections, browse schemas, run SQL
and view a schema graph for SQLite, PostgreSQL, MySQL/MariaDB and SQL Server.

## Why it exists

So that inspecting a table or running a quick query does not mean leaving the
editor for a separate database tool. The architecture and phased delivery
plan are in
[`DATABASE_PANEL_SPEC.md`](../gpui_component/DATABASE_PANEL_SPEC.md).

## Using it

Open with `ctrl-k d` (`cmd-k d` on macOS), the status bar icon (tooltip
"Databases"), or `database panel: toggle focus`.

### 1. Add a connection

Click **Add Connection**, choose the database type and fill in the details.
SQLite needs only a file (**Choose file…**); the network engines need host,
port, username and password.

The password goes to the system credential store, not into the saved
connection.

### 2. Connect and choose a database

The header shows the connection's title, type and live status, with
**Refresh**, **Connect** / **Disconnect** and **Delete**. Errors appear on an
inline row beneath it, with **Retry**.

For server connections a tab strip lists the databases discovered on the
server. **+ Create database** creates a new one; **Open existing ▾** attaches
one that is already there.

### 3. Explore

The body is three resizable panes:

| Pane | Contents |
| --- | --- |
| Explorer (left) | Searchable tree of tables and views, expanding to columns and indexes. Filter with "Filter tables or columns…" |
| Content (centre) | Tabs: Overview, Tables, Views, Relationships. Selecting a table shows its columns and foreign keys |
| Inspector (right) | Metadata for whatever is selected — a table or a single column |

### 4. Run SQL

**Workbench** opens a SQL workbench as a tab in the main editor area: a
schema tree on the left, and an editor, Run button and results table on the
right.

- Right-click an item in the tree to insert a ready-made statement.
- Autocomplete suggests tables and columns from the loaded schema.
- Results can be copied or exported as JSON or SQL (**Copy JSON**,
  **Copy SQL**, **Export JSON**, **Export SQL**).

### 5. View the schema graph

**Schema Graph** opens a tab drawing tables as nodes and foreign keys as
edges, laid out automatically, on a pannable and zoomable canvas.

### Restarting

Workbench and Schema Graph tabs are remembered and reopened on the next
launch, and so are the databases each workspace had open.

- **Every tab comes back at once.** A tab does not wait for its database
  while the app starts.
- **Only the active database of each connection connects at startup.** The
  others are listed in the database tab strip marked "not connected", and a
  tab for one says so, with a **Connect** button. Choosing the database in the
  strip, or showing its tab for the first time, connects it.
- **A failure stays on the tab.** It shows the reason and **Retry**; the tab
  is not closed.
- **The list is saved per workspace**, in `database_panel_live_databases`, so
  two workspaces that use the same server each get back their own databases.
  A database you close with its "x" is dropped from the list; disconnecting a
  whole connection keeps its databases listed.
- A connection that has since been deleted still fails to restore its tabs,
  with a toast.

A tab runs its queries, and refreshes its schema, against its own database,
not whichever one the connection has active.

## How it is wired

- `database_panel::init(cx)` registers `database_panel::ToggleFocus` and the
  two serializable tab types (`WorkbenchTab`, `SchemaGraphTab`).
- `DatabasePanel::load(workspace, cx)` is awaited in `initialize_panels`
  (`crates/zed/src/zed.rs`).
- Implements `workspace::dock::Panel`, fixed to the bottom dock (the
  `database_panel.button` setting hides its status bar button),
  `activation_priority() = 22`.

The workbench and graph tabs are thin shells holding an
`Entity<DatabasePanel>` and the key of the database they are for. Their state
(editor contents, results, history) stays on the panel, one per connection
and database, so closing and reopening a tab does not lose it.

## Layout

| File | Contents |
| --- | --- |
| `src/database_panel.rs` | Panel state, connection forms, `Panel` impl |
| `src/header.rs` | Connection header and database tab strip |
| `src/explorer.rs` | Schema tree with filter |
| `src/content.rs` | Overview / Tables / Views / Relationships |
| `src/inspector.rs` | Selected-object metadata |
| `src/workbench.rs` | SQL workbench tab and schema-driven completion |
| `src/sql_highlight.rs` | SQL syntax highlighter |
| `src/ir.rs` | UI-independent schema graph representation |
| `src/layout.rs` | Deterministic graph layout |
| `src/graph.rs` | Conversion to `gpui_flow` and the graph tab |
| `src/persistence.rs` | Which workbench/graph tabs were open, and which databases each workspace had open |

All database access goes through
[`database_backend`](../database_backend/README.md). The graph canvas is
[`gpui_flow`](../gpui_flow/README.md).

### Implementation notes

- **SQL highlighting is hand-written.** The vendored editor's tree-sitter
  highlighter is a no-op in this tree (the feature is declared but not wired
  to a grammar), so `sql_highlight.rs` plugs a keyword/string/number/comment
  scanner into the editor's separate highlighter hook instead.
- **Completion is not LSP-backed.** `SqlSchemaCompletionProvider` implements
  the editor's `CompletionProvider` directly from the loaded schema.
- `ir.rs` and `layout.rs` are deliberately free of GPUI so the graph logic
  can be tested on its own.

## Further reading

- [`PHASE1_PANEL_PLAN.md`](PHASE1_PANEL_PLAN.md) — the explorer shell plan.
- [`DB_SCHEMA_GRAPH_ARCHITECTURE.md`](DB_SCHEMA_GRAPH_ARCHITECTURE.md) — the
  schema graph design.

## Development

```sh
cargo check -p database_panel -j 8
cargo test -p database_panel -j 8
```

Tests cover the schema IR, the graph layout, the graph conversion and parts
of the workbench.

To run the tests of every fork crate at once: `script/test-fork-crates.ps1`.

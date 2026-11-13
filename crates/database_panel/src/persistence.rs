//! Persists which workbench/schema-graph tabs were open, so they can be
//! reopened (against a freshly re-established connection) on the next
//! launch — see `workbench::WorkbenchTab`'s and `graph::SchemaGraphTab`'s
//! `SerializableItem` impls, which are this table's only readers/writers.
//!
//! Mirrors `designer_panel::persistence::DesignerDb`: one row per open item,
//! keyed by `(workspace_id, item_id)`.

use anyhow::Result;
use database_backend::ConnectionId;
use db::{
    query,
    sqlez::{domain::Domain, statement::Statement, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use workspace::{ItemId, WorkspaceDb, WorkspaceId};

/// Which kind of tab a persisted row describes — the two `SerializableItem`
/// impls that share this table (`WorkbenchTab`, `SchemaGraphTab`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabKind {
    Workbench,
    Graph,
}

impl TabKind {
    fn as_str(self) -> &'static str {
        match self {
            TabKind::Workbench => "workbench",
            TabKind::Graph => "graph",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "workbench" => Some(TabKind::Workbench),
            "graph" => Some(TabKind::Graph),
            _ => None,
        }
    }
}

pub(crate) struct PersistedTab {
    pub(crate) kind: TabKind,
    pub(crate) connection_id: ConnectionId,
    pub(crate) database: String,
}

pub(crate) struct DatabasePanelTabsDb(ThreadSafeConnection);

impl Domain for DatabasePanelTabsDb {
    const NAME: &str = stringify!(DatabasePanelTabsDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE database_panel_tabs (
                workspace_id INTEGER,
                item_id INTEGER,
                kind TEXT NOT NULL,
                connection_id INTEGER NOT NULL,
                database TEXT NOT NULL,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
            ) STRICT;
        ),
        // The databases a workspace had open, so the next launch can list
        // them again. A connection can hold several databases at once, but
        // the saved connection records only one, so without this the others
        // were forgotten on restart.
        sql!(
            CREATE TABLE database_panel_live_databases (
                workspace_id INTEGER NOT NULL,
                position INTEGER NOT NULL,
                connection_id INTEGER NOT NULL,
                database TEXT NOT NULL,
                PRIMARY KEY(workspace_id, position),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
            ) STRICT;
        ),
    ];
}

db::static_connection!(DatabasePanelTabsDb, [WorkspaceDb]);

impl DatabasePanelTabsDb {
    pub(crate) async fn save_tab(
        &self,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        kind: TabKind,
        connection_id: ConnectionId,
        database: String,
    ) -> Result<()> {
        let kind = kind.as_str();
        self.write(move |conn| {
            let mut statement = Statement::prepare(
                conn,
                "INSERT INTO database_panel_tabs(workspace_id, item_id, kind, connection_id, database)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(workspace_id, item_id) DO UPDATE SET
                     kind = excluded.kind,
                     connection_id = excluded.connection_id,
                     database = excluded.database",
            )?;
            let mut next = statement.bind(&workspace_id, 1)?;
            next = statement.bind(&item_id, next)?;
            next = statement.bind(&kind, next)?;
            next = statement.bind(&connection_id.0, next)?;
            statement.bind(&database, next)?;
            statement.exec()
        })
        .await
    }

    pub(crate) fn tab_for_item(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
    ) -> Result<Option<PersistedTab>> {
        let row = self.tab_row(item_id, workspace_id)?;
        Ok(row.and_then(|(kind, connection_id, database)| {
            Some(PersistedTab {
                kind: TabKind::from_str(&kind)?,
                connection_id: ConnectionId(connection_id),
                database,
            })
        }))
    }

    query! {
        fn tab_row(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<(String, u64, String)>> {
            SELECT kind, connection_id, database
            FROM database_panel_tabs
            WHERE item_id = ? AND workspace_id = ?
        }
    }

    /// Replaces the list of databases `workspace_id` had open with
    /// `databases`, in order.
    pub(crate) async fn save_live(
        &self,
        workspace_id: WorkspaceId,
        databases: Vec<(ConnectionId, String)>,
    ) -> Result<()> {
        self.write(move |conn| {
            let mut delete = Statement::prepare(
                conn,
                "DELETE FROM database_panel_live_databases WHERE workspace_id = ?1",
            )?;
            delete.bind(&workspace_id, 1)?;
            delete.exec()?;

            for (position, (connection_id, database)) in databases.iter().enumerate() {
                let mut insert = Statement::prepare(
                    conn,
                    "INSERT INTO database_panel_live_databases(workspace_id, position, connection_id, database)
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                let mut next = insert.bind(&workspace_id, 1)?;
                next = insert.bind(&(position as u64), next)?;
                next = insert.bind(&connection_id.0, next)?;
                insert.bind(database, next)?;
                insert.exec()?;
            }
            Ok(())
        })
        .await
    }

    /// The databases `workspace_id` had open when it was last saved, in the
    /// order they were saved.
    pub(crate) fn live_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<(ConnectionId, String)>> {
        Ok(self
            .live_rows(workspace_id)?
            .into_iter()
            .map(|(connection_id, database)| (ConnectionId(connection_id), database))
            .collect())
    }

    query! {
        fn live_rows(workspace_id: WorkspaceId) -> Result<Vec<(u64, String)>> {
            SELECT connection_id, database
            FROM database_panel_live_databases
            WHERE workspace_id = ?
            ORDER BY position
        }
    }

    pub(crate) async fn delete_unloaded(
        &self,
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
    ) -> Result<()> {
        self.write(move |conn| {
            if alive_items.is_empty() {
                let mut statement = Statement::prepare(
                    conn,
                    "DELETE FROM database_panel_tabs WHERE workspace_id = ?1",
                )?;
                statement.bind(&workspace_id, 1)?;
                return statement.exec();
            }

            let placeholders = (0..alive_items.len())
                .map(|index| format!("?{}", index + 2))
                .collect::<Vec<_>>()
                .join(", ");
            let query = format!(
                "DELETE FROM database_panel_tabs
                 WHERE workspace_id = ?1 AND item_id NOT IN ({placeholders})"
            );
            let mut statement = Statement::prepare(conn, &query)?;
            let mut next = statement.bind(&workspace_id, 1)?;
            for item_id in &alive_items {
                next = statement.bind(item_id, next)?;
            }
            statement.exec()
        })
        .await
    }
}

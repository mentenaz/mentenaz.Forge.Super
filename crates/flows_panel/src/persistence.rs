//! Flow persistence: per-project run history in the scoped key-value store
//! (`history:<flow_id>` under the workspace-root namespace, capped at
//! `MAX_HISTORY_ENTRIES`).
//!
//! This module used to also declare a `DesignerDb` SQL domain recording
//! which flow a workspace's Designer had open. It was removed: nothing read
//! it, and it collided with `designer_panel::persistence::DesignerDb` — both
//! registered a migration domain *named* `DesignerDb` that created a
//! `designer_flows` table, with different columns. The migrator runs only
//! one registration per domain name (whichever the linker happened to list
//! first), so which crate's schema the app got was not something either
//! crate controlled. Which flows are open is `designer_panel`'s to persist.

use db::AppDatabase;
use db::kvp::KeyValueStore;
use workflow_engine::history::{RunHistoryEntry, push_entry};

/// The scoped-kv key under which `flow_id`'s run history is stored.
pub fn history_key(flow_id: &str) -> String {
    format!("history:{flow_id}")
}

/// Reads the retained run history for `flow_id`, oldest-first. Empty (not
/// an error) for a flow that never ran, a flow whose id changed, or a
/// corrupt stored blob — history is diagnostic, not load-bearing.
pub fn read_flow_history(
    store: &KeyValueStore,
    namespace: &str,
    flow_id: &str,
) -> anyhow::Result<Vec<RunHistoryEntry>> {
    let Some(raw) = store.scoped(namespace).read(&history_key(flow_id))? else {
        return Ok(Vec::new());
    };
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

/// Appends `entry` to `flow_id`'s retained history (capped at
/// [`MAX_HISTORY_ENTRIES`], oldest dropped first) and writes the list back.
pub async fn append_flow_history(
    store: &KeyValueStore,
    namespace: &str,
    flow_id: &str,
    entry: RunHistoryEntry,
) -> anyhow::Result<()> {
    let mut entries = read_flow_history(store, namespace, flow_id)?;
    push_entry(&mut entries, entry);
    let json = serde_json::to_string(&entries)?;
    store
        .scoped(namespace)
        .write(history_key(flow_id), json)
        .await
}

/// Fires-and-forgets an [`append_flow_history`] on the background thread
/// pool — the run-finish path calls this once a run completes, and doesn't
/// need to wait for the write.
pub fn write_flow_history(
    db: &AppDatabase,
    cx: &mut db::gpui::App,
    namespace: String,
    flow_id: String,
    entry: RunHistoryEntry,
) {
    let store = KeyValueStore::from_app_db(db);
    db::write_and_log(cx, move || async move {
        let mut entries = read_flow_history(&store, &namespace, &flow_id)?;
        push_entry(&mut entries, entry);
        let json = serde_json::to_string(&entries)?;
        store
            .scoped(&namespace)
            .write(history_key(&flow_id), json)
            .await
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use workflow_engine::schema::RunOutcome;

    fn entry(id: &str, outcome: RunOutcome) -> RunHistoryEntry {
        RunHistoryEntry {
            id: id.to_string(),
            time: "2026-09-11T00:00:00Z".to_string(),
            outcome,
            stopped: false,
            actions: Vec::new(),
        }
    }

    /// A fresh in-memory app database with every registered migration run.
    fn open_store() -> KeyValueStore {
        KeyValueStore::from_app_db(&AppDatabase::test_new())
    }

    #[gpui::test]
    async fn the_designers_table_has_one_owner() {
        // Regression test for the duplicate `DesignerDb` domain this module
        // used to declare: with `designer_panel` linked in (it is a
        // dependency of this crate), the migrated database must have the
        // schema `designer_panel::persistence` queries — an `item_id`/`path`
        // table — not the older `flow_path` one.
        let app_db = AppDatabase::test_new();
        let columns = app_db
            .0
            .select::<String>("SELECT name FROM pragma_table_info('designer_flows') ORDER BY cid")
            .unwrap()()
        .unwrap();
        assert_eq!(columns, vec!["workspace_id", "item_id", "path"]);
    }

    #[gpui::test]
    async fn history_appends_and_stays_capped_at_max_entries() {
        let store = open_store();
        let namespace = "E:/project-root";
        let flow_id = "order-processing";

        assert!(
            read_flow_history(&store, namespace, flow_id)
                .unwrap()
                .is_empty()
        );

        for i in 0..(workflow_engine::history::MAX_HISTORY_ENTRIES + 5) {
            append_flow_history(
                &store,
                namespace,
                flow_id,
                entry(&format!("run{i}"), RunOutcome::Succeeded),
            )
            .await
            .unwrap();
        }

        let entries = read_flow_history(&store, namespace, flow_id).unwrap();
        assert_eq!(entries.len(), workflow_engine::history::MAX_HISTORY_ENTRIES);
        // The oldest 5 were dropped — the first entry remaining is run5.
        assert_eq!(entries[0].id, "run5");
    }

    #[gpui::test]
    async fn history_namespaces_do_not_collide() {
        let store = open_store();

        append_flow_history(
            &store,
            "E:/project-a",
            "order-processing",
            entry("run1", RunOutcome::Succeeded),
        )
        .await
        .unwrap();

        assert_eq!(
            read_flow_history(&store, "E:/project-a", "order-processing")
                .unwrap()
                .len(),
            1
        );
        assert!(
            read_flow_history(&store, "E:/project-b", "order-processing")
                .unwrap()
                .is_empty()
        );
    }
}

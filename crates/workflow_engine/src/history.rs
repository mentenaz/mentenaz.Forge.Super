//! Run-history record types, capped at the most recent
//! [`MAX_HISTORY_ENTRIES`] runs (oldest dropped first).
//!
//! On Forge these persisted as `.forge/history/<flowId>.history.json` files
//! (`load_history`/`append_run` — see the git history); in this tree the
//! storage moves to `db`'s scoped `KeyValueStore` (key `history:<flow_id>`,
//! see `DesignerDb`), so only the record shapes and the cap constant live
//! here. Pure data, no `gpui` dependency, same as the rest of this crate —
//! the Designer builds a [`RunHistoryEntry`] from the same `StatusSink`
//! events that already drive the live canvas/Run Log, then stores it via
//! that adapter once the run finishes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::schema::RunOutcome;

pub const MAX_HISTORY_ENTRIES: usize = 50;

/// One action's result within one run. `outcome` reuses
/// `schema::RunOutcome` (already `Succeeded`/`Failed`/`Skipped`,
/// serialized capitalized) rather than inventing a second, differently-cased
/// vocabulary — this is the exact same set of values `runAfter` itself
/// already uses in `flow.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionHistoryRecord {
    pub id: String,
    pub name: String,
    /// The action's own `type_id` (`"http"`, `"script"`, `"Foreach"`,
    /// ...) — same value regardless of a `script` action's underlying
    /// runtime (Python/Node/PowerShell/.NET all report through this one
    /// generic `type_id`; the runtime itself only matters to
    /// `execute_script`, not to how a result gets recorded here).
    #[serde(rename = "type")]
    pub type_id: String,
    pub outcome: RunOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunHistoryEntry {
    pub id: String,
    /// RFC 3339 (`chrono::Utc::now().to_rfc3339()`).
    pub time: String,
    /// Only `Succeeded`/`Failed` ever appear here — a whole *run* doesn't
    /// "skip"; reusing `RunOutcome` rather than a second bespoke enum for
    /// just two of its three variants.
    pub outcome: RunOutcome,
    /// The run was stopped by the user before it ended. Its `outcome` is then
    /// `Succeeded`: the servers did what they were meant to until they were
    /// stopped. Entries saved before this field existed load as `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stopped: bool,
    pub actions: Vec<ActionHistoryRecord>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Appends `entry`, then drops the oldest entries beyond
/// [`MAX_HISTORY_ENTRIES`] (the *front* of the list — entries are stored
/// oldest-first, so this keeps the most recent runs). Pure — the caller
/// (the Designer's history adapter) persists the resulting list.
pub fn push_entry(entries: &mut Vec<RunHistoryEntry>, entry: RunHistoryEntry) {
    entries.push(entry);
    if entries.len() > MAX_HISTORY_ENTRIES {
        let excess = entries.len() - MAX_HISTORY_ENTRIES;
        entries.drain(0..excess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, outcome: RunOutcome) -> ActionHistoryRecord {
        ActionHistoryRecord {
            id: id.to_string(),
            name: id.to_string(),
            type_id: "log".to_string(),
            outcome,
            message: None,
            response: None,
        }
    }

    fn entry(id: &str, outcome: RunOutcome) -> RunHistoryEntry {
        RunHistoryEntry {
            id: id.to_string(),
            time: "2026-09-11T00:00:00Z".to_string(),
            outcome,
            stopped: false,
            actions: vec![record("a", RunOutcome::Succeeded)],
        }
    }

    #[test]
    fn caps_at_max_history_entries_dropping_the_oldest_first() {
        let mut entries = Vec::new();
        for i in 0..(MAX_HISTORY_ENTRIES + 5) {
            push_entry(
                &mut entries,
                entry(&format!("run{i}"), RunOutcome::Succeeded),
            );
        }

        assert_eq!(entries.len(), MAX_HISTORY_ENTRIES);
        // The oldest 5 were dropped — the first entry remaining is run5.
        assert_eq!(entries[0].id, "run5");
        assert_eq!(
            entries[MAX_HISTORY_ENTRIES - 1].id,
            format!("run{}", MAX_HISTORY_ENTRIES + 4)
        );
    }

    #[test]
    fn action_outcome_serializes_capitalized_matching_run_after_convention() {
        let rec = record("a", RunOutcome::Succeeded);
        let json = serde_json::to_string(&rec).unwrap();
        assert!(json.contains("\"outcome\":\"Succeeded\""), "{json}");
    }

    #[test]
    fn response_and_message_are_omitted_when_absent() {
        let rec = record("a", RunOutcome::Succeeded);
        let json = serde_json::to_string(&rec).unwrap();
        assert!(!json.contains("message"));
        assert!(!json.contains("response"));
    }

    #[test]
    fn a_stopped_run_round_trips_and_an_old_entry_loads_as_not_stopped() {
        let mut stopped = entry("run-stopped", RunOutcome::Succeeded);
        stopped.stopped = true;
        let json = serde_json::to_string(&stopped).unwrap();
        assert!(json.contains("\"stopped\":true"), "{json}");
        let back: RunHistoryEntry = serde_json::from_str(&json).unwrap();
        assert!(back.stopped);

        // A run that was not stopped does not write the field at all...
        let normal = serde_json::to_string(&entry("run-ok", RunOutcome::Succeeded)).unwrap();
        assert!(!normal.contains("stopped"), "{normal}");
        // ...and an entry saved before the field existed loads as not stopped.
        let old = r#"{"id":"old","time":"2026-09-11T00:00:00Z","outcome":"Failed","actions":[]}"#;
        let loaded: RunHistoryEntry = serde_json::from_str(old).unwrap();
        assert!(!loaded.stopped);
    }
}

//! §3 of `DOCS/workflow-schema/execution-engine-design.md` — the core
//! scheduler. Given one `ActionMap` (a single nesting level — a
//! container's own body is scheduled by a *separate* recursive call from
//! its own executor, not by this same pass reaching into it, per §5, not
//! yet written) plus a `RunContext`, runs every action respecting
//! `runAfter`, executing all currently-eligible actions concurrently.
//!
//! Deliberately agnostic of what any given action type actually *does* —
//! `execute` is injected by the caller (§4/§5's job, not this module's).
//! Per the doc's suggested build order, this is tested here against fake
//! no-op executors; real per-action-type dispatch comes later.

use std::collections::{HashMap, VecDeque};
use std::future::Future;

use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use serde_json::{Map, Value};

use super::{Action, ActionMap, RunContext, RunOutcome, WorkflowDefinition};

/// The outcome of running one `ActionMap` level to completion.
#[derive(Debug, Default)]
pub struct LevelResult {
    pub outcomes: HashMap<String, RunOutcome>,
    /// Failure message for every action whose outcome is `Failed` —
    /// `Try`/`Catch` (§5) needs the message text for
    /// `<tryId>.outputs.error`, not just the fact that it failed.
    pub errors: HashMap<String, String>,
}

impl LevelResult {
    /// Whether every action in this level either succeeded or was
    /// deliberately skipped — no `Failed` outcomes. `Try`'s own executor
    /// (§5) is exactly "run the body, check this."
    pub fn all_ok(&self) -> bool {
        self.outcomes.values().all(|o| *o != RunOutcome::Failed)
    }
}

/// **Locked**: validate before running, don't discover cycles at runtime.
/// Topologically sorts every `ActionMap` in `def` — recursively, each
/// nesting level independently, since `runAfter` never crosses a nesting
/// boundary (confirmed by both the schema and `workflow_json.rs`'s own
/// load/save). A cycle (or a `runAfter` dependency naming an id that
/// doesn't exist in the same scope) is a hard error, meant to be checked
/// once before a flow starts — never call `run_level` on unvalidated data.
pub fn validate(def: &WorkflowDefinition) -> Result<(), String> {
    validate_map(&def.actions)
}

fn validate_map(actions: &ActionMap) -> Result<(), String> {
    topo_order(actions)?;
    for action in actions.values() {
        if let Some(body) = &action.actions {
            validate_map(body)?;
        }
        if let Some(branch) = &action.else_branch {
            validate_map(&branch.actions)?;
        }
        if let Some(branch) = &action.catch {
            validate_map(&branch.actions)?;
        }
    }
    Ok(())
}

/// Kahn's algorithm over one level's `runAfter` edges. Returns the
/// topological order (unused by `run_level`, which has its own
/// outcome-driven work queue — this function exists purely to detect
/// cycles/dangling deps ahead of time) or an error naming what's wrong.
fn topo_order(actions: &ActionMap) -> Result<Vec<String>, String> {
    let mut in_degree: HashMap<&str, usize> = actions.keys().map(|k| (k.as_str(), 0)).collect();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, action) in actions {
        for dep in action.run_after.keys() {
            if !actions.contains_key(dep) {
                return Err(format!(
                    "action \"{id}\" has runAfter dependency \"{dep}\" which doesn't exist in the same scope"
                ));
            }
            *in_degree.get_mut(id.as_str()).unwrap() += 1;
            dependents
                .entry(dep.as_str())
                .or_default()
                .push(id.as_str());
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(k, _)| *k)
        .collect();
    let mut order = Vec::new();
    while let Some(id) = queue.pop_front() {
        order.push(id.to_string());
        if let Some(deps) = dependents.get(id) {
            for &dependent_id in deps {
                let degree = in_degree.get_mut(dependent_id).unwrap();
                *degree -= 1;
                if *degree == 0 {
                    queue.push_back(dependent_id);
                }
            }
        }
    }

    if order.len() != actions.len() {
        let stuck: Vec<&str> = in_degree
            .iter()
            .filter(|(_, degree)| **degree > 0)
            .map(|(k, _)| *k)
            .collect();
        return Err(format!(
            "cycle detected among actions: {}",
            stuck.join(", ")
        ));
    }
    Ok(order)
}

/// Grouping one level's action ids by `runAfter` depth — index 0 is every
/// parallel root (empty `runAfter`), index 1 depends only on index-0
/// actions, and so on. Same Kahn's-algorithm shape as [`topo_order`]
/// (in-degree + dependents map), just collecting each BFS frontier as its
/// own group instead of flattening into one order — `pub(crate)` rather
/// than private like `topo_order`, since `designer_panel::workflow_json`'s
/// auto-layout (canvas positions for a freshly created/positionless flow)
/// is the one caller outside this module; `run_level` itself doesn't need
/// this, it has its own outcome-driven work queue that doesn't care about
/// depth. Not runtime-facing — this is purely "how many columns deep for
/// layout," unrelated to `run_level`'s actual outcome-gated eligibility.
/// `#[allow(dead_code)]` until `designer_panel` (M4) wires the auto-layout
/// call site.
#[allow(dead_code)]
pub fn compute_levels(actions: &ActionMap) -> Result<Vec<Vec<String>>, String> {
    let mut in_degree: HashMap<String, usize> = actions.keys().map(|k| (k.clone(), 0)).collect();
    let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
    for (id, action) in actions {
        for dep in action.run_after.keys() {
            if !actions.contains_key(dep) {
                return Err(format!(
                    "action \"{id}\" has runAfter dependency \"{dep}\" which doesn't exist in the same scope"
                ));
            }
            *in_degree.get_mut(id).unwrap() += 1;
            dependents.entry(dep.clone()).or_default().push(id.clone());
        }
    }

    let mut levels: Vec<Vec<String>> = Vec::new();
    let mut visited = 0usize;
    let mut current: Vec<String> = in_degree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(k, _)| k.clone())
        .collect();
    current.sort();
    while !current.is_empty() {
        visited += current.len();
        let mut next: Vec<String> = Vec::new();
        for id in &current {
            if let Some(deps) = dependents.get(id) {
                for dependent_id in deps {
                    let degree = in_degree.get_mut(dependent_id).unwrap();
                    *degree -= 1;
                    if *degree == 0 {
                        next.push(dependent_id.clone());
                    }
                }
            }
        }
        levels.push(std::mem::take(&mut current));
        next.sort();
        current = next;
    }

    if visited != actions.len() {
        return Err("cycle detected while computing layout levels".to_string());
    }
    Ok(levels)
}

/// Runs every action in `actions` (one nesting level), respecting
/// `runAfter`, executing everything currently eligible concurrently. See
/// the design doc §3's pseudocode — this is a direct translation of it:
/// an action is "decidable" once every `runAfter` dependency has resolved
/// (regardless of that dependency's own outcome); once decidable, it
/// actually runs if `runAfter` is empty or *any* listed dependency's
/// resolved outcome is in that dependency's own allowed-outcomes list
/// (an OR-join filtered by outcome), otherwise it's `Skipped` — a
/// dependency existing but never running because *it* was skipped still
/// counts as "resolved" for this purpose (`Skipped` is itself an outcome
/// that can be matched in `runAfter`, e.g. cleanup-on-skip patterns).
///
/// `actions` and `ctx` must outlive the whole call — this only polls
/// futures locally (`FuturesUnordered`), it never spawns them onto a
/// separate executor/thread; that's `execute`'s job for any action type
/// that needs it (`backend::on_tokio`, per §4).
// `'a` is named explicitly (rather than three independently-elided
// lifetimes on `execute`'s parameters) so a caller like `container.rs`'s
// `execute_action` — which returns one boxed future borrowing from *all
// three* of `id`/`action`/`ctx` at once — has a single lifetime to tie
// that borrow to. Three separate elided lifetimes can't be unified into
// one boxed-future return type the way a single named `'a` can.
pub async fn run_level<'a, F, Fut>(
    actions: &'a ActionMap,
    ctx: &'a RunContext,
    execute: F,
) -> LevelResult
where
    F: Fn(&'a str, &'a Action, &'a RunContext) -> Fut,
    Fut: Future<Output = Result<Map<String, Value>, String>> + 'a,
{
    let mut outcomes: HashMap<String, RunOutcome> = HashMap::new();
    let mut errors: HashMap<String, String> = HashMap::new();
    let mut pending: VecDeque<&String> = actions.keys().collect();
    let mut running = FuturesUnordered::new();

    loop {
        let mut still_pending = VecDeque::new();
        while let Some(id) = pending.pop_front() {
            let action = &actions[id];
            // A stopped run starts nothing new: whatever is still waiting is
            // skipped.
            if ctx.is_stopped() {
                outcomes.insert(id.clone(), RunOutcome::Skipped);
                continue;
            }
            let deps_resolved = action
                .run_after
                .keys()
                .all(|dep| outcomes.contains_key(dep));
            if !deps_resolved {
                still_pending.push_back(id);
                continue;
            }
            let should_run = action.run_after.is_empty()
                || action
                    .run_after
                    .iter()
                    .any(|(dep, allowed)| allowed.contains(outcomes.get(dep).unwrap()));
            if should_run {
                let owned_id = id.clone();
                running.push(execute(id, action, ctx).map(move |result| (owned_id, result)));
            } else {
                outcomes.insert(id.clone(), RunOutcome::Skipped);
            }
        }
        pending = still_pending;

        if running.is_empty() {
            if pending.is_empty() {
                break;
            }
            // Should be unreachable after `validate()` rejects cycles up
            // front — treated as an internal error, not a silent hang.
            for id in pending.drain(..) {
                errors.insert(
                    id.clone(),
                    "scheduler stuck: dependency graph has an unresolvable cycle \
                     (this should have been caught by validate() before the run started)"
                        .to_string(),
                );
                outcomes.insert(id.clone(), RunOutcome::Failed);
            }
            break;
        }

        if let Some((id, result)) = running.next().await {
            match result {
                Ok(outputs) => {
                    ctx.record_outputs(&id, outputs);
                    outcomes.insert(id, RunOutcome::Succeeded);
                }
                Err(message) => {
                    errors.insert(id.clone(), message);
                    outcomes.insert(id, RunOutcome::Failed);
                }
            }
        }
    }

    LevelResult { outcomes, errors }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::RunOutcome as RO;

    fn action_after(id: &str, deps: &[(&str, &[RO])]) -> (String, Action) {
        let mut action = Action::new("noop");
        for (dep, outcomes) in deps {
            action
                .run_after
                .insert((*dep).to_string(), outcomes.to_vec());
        }
        (id.to_string(), action)
    }

    fn map_of(actions: Vec<(String, Action)>) -> ActionMap {
        actions.into_iter().collect()
    }

    fn ok_executor(
        _id: &str,
        _action: &Action,
        _ctx: &RunContext,
    ) -> impl Future<Output = Result<Map<String, Value>, String>> {
        std::future::ready(Ok(Map::new()))
    }

    #[tokio::test]
    async fn a_stopped_run_starts_nothing_new() {
        let (id_a, a) = action_after("a", &[]);
        let (id_b, b) = action_after("b", &[("a", &[RO::Succeeded])]);
        let actions = map_of(vec![(id_a, a), (id_b, b)]);
        let ctx = RunContext::new();
        ctx.control().stop();

        let result = run_level(&actions, &ctx, |_, _, _| async { Ok(Map::new()) }).await;

        assert_eq!(result.outcomes.get("a"), Some(&RunOutcome::Skipped));
        assert_eq!(result.outcomes.get("b"), Some(&RunOutcome::Skipped));
    }

    #[test]
    fn compute_levels_groups_by_depth() {
        // a, b are parallel roots; c depends on both; d depends only on c.
        let actions = map_of(vec![
            ("a".to_string(), Action::new("noop")),
            ("b".to_string(), Action::new("noop")),
            action_after("c", &[("a", &[RO::Succeeded]), ("b", &[RO::Succeeded])]),
            action_after("d", &[("c", &[RO::Succeeded])]),
        ]);
        let levels = compute_levels(&actions).unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0], vec!["a".to_string(), "b".to_string()]);
        assert_eq!(levels[1], vec!["c".to_string()]);
        assert_eq!(levels[2], vec!["d".to_string()]);
    }

    #[test]
    fn compute_levels_rejects_a_cycle() {
        let (id_a, a) = action_after("a", &[("b", &[RO::Succeeded])]);
        let (id_b, b) = action_after("b", &[("a", &[RO::Succeeded])]);
        let actions = map_of(vec![(id_a, a), (id_b, b)]);
        assert!(compute_levels(&actions).is_err());
    }

    #[test]
    fn accepts_a_valid_linear_chain() {
        let mut actions = ActionMap::new();
        actions.insert("a".into(), Action::new("noop"));
        let (id, action) = action_after("b", &[("a", &[RO::Succeeded])]);
        actions.insert(id, action);
        let def = WorkflowDefinition {
            id: "x".into(),
            name: "X".into(),
            actions,
            outputs: Default::default(),
        };
        assert!(validate(&def).is_ok());
    }

    #[test]
    fn rejects_a_two_node_cycle() {
        let mut actions = ActionMap::new();
        let (id_a, a) = action_after("a", &[("b", &[RO::Succeeded])]);
        let (id_b, b) = action_after("b", &[("a", &[RO::Succeeded])]);
        actions.insert(id_a, a);
        actions.insert(id_b, b);
        let def = WorkflowDefinition {
            id: "x".into(),
            name: "X".into(),
            actions,
            outputs: Default::default(),
        };
        let err = validate(&def).unwrap_err();
        assert!(err.contains("cycle"), "expected a cycle error, got: {err}");
    }

    #[test]
    fn rejects_a_dangling_run_after_reference() {
        let mut actions = ActionMap::new();
        let (id, action) = action_after("a", &[("neverExisted", &[RO::Succeeded])]);
        actions.insert(id, action);
        let def = WorkflowDefinition {
            id: "x".into(),
            name: "X".into(),
            actions,
            outputs: Default::default(),
        };
        let err = validate(&def).unwrap_err();
        assert!(
            err.contains("neverExisted"),
            "expected the dangling id in the error, got: {err}"
        );
    }

    #[test]
    fn catches_a_cycle_nested_inside_a_foreach_body() {
        let mut body = ActionMap::new();
        let (id_a, a) = action_after("x", &[("y", &[RO::Succeeded])]);
        let (id_b, b) = action_after("y", &[("x", &[RO::Succeeded])]);
        body.insert(id_a, a);
        body.insert(id_b, b);

        let mut outer = Action::new("Foreach");
        outer.actions = Some(body);
        let mut actions = ActionMap::new();
        actions.insert("loop".into(), outer);

        let def = WorkflowDefinition {
            id: "x".into(),
            name: "X".into(),
            actions,
            outputs: Default::default(),
        };
        assert!(validate(&def).is_err());
    }

    #[tokio::test]
    async fn runs_independent_actions_concurrently() {
        let actions = map_of(vec![
            ("a".to_string(), Action::new("noop")),
            ("b".to_string(), Action::new("noop")),
        ]);
        let ctx = RunContext::new();

        let started = std::time::Instant::now();
        let result = run_level(&actions, &ctx, |_id, _action, _ctx| async {
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            Ok(Map::new())
        })
        .await;
        let elapsed = started.elapsed();

        assert_eq!(result.outcomes.get("a"), Some(&RunOutcome::Succeeded));
        assert_eq!(result.outcomes.get("b"), Some(&RunOutcome::Succeeded));
        // Sequential would take >=120ms; concurrent should stay well under
        // that even with scheduling overhead.
        assert!(
            elapsed < std::time::Duration::from_millis(110),
            "took {elapsed:?} — actions ran sequentially, not concurrently"
        );
    }

    #[tokio::test]
    async fn respects_run_after_ordering_and_records_outputs() {
        let mut actions = ActionMap::new();
        actions.insert("a".into(), Action::new("noop"));
        let (id, action) = action_after("b", &[("a", &[RunOutcome::Succeeded])]);
        actions.insert(id, action);
        let ctx = RunContext::new();

        let result = run_level(&actions, &ctx, |id, _action, ctx| {
            let id = id.to_string();
            let ctx = ctx.clone();
            async move {
                if id == "b" {
                    // "a" must already be recorded by the time "b" runs.
                    let seen = ctx
                        .evaluate(&serde_json::json!({"var": "a.outputs.done"}))
                        .unwrap();
                    assert_eq!(seen, Value::from(true));
                }
                let mut outputs = Map::new();
                outputs.insert("done".into(), Value::from(true));
                Ok(outputs)
            }
        })
        .await;

        assert_eq!(result.outcomes.get("a"), Some(&RunOutcome::Succeeded));
        assert_eq!(result.outcomes.get("b"), Some(&RunOutcome::Succeeded));
    }

    #[tokio::test]
    async fn skips_an_action_whose_dependency_outcome_doesnt_match() {
        let mut actions = ActionMap::new();
        actions.insert("a".into(), Action::new("noop"));
        let (id, action) = action_after("b", &[("a", &[RunOutcome::Failed])]);
        actions.insert(id, action);
        let ctx = RunContext::new();

        // "a" always succeeds; "b" only wants to run if "a" failed.
        let result = run_level(&actions, &ctx, |_id, _action, _ctx| async {
            Ok(Map::new())
        })
        .await;

        assert_eq!(result.outcomes.get("a"), Some(&RunOutcome::Succeeded));
        assert_eq!(result.outcomes.get("b"), Some(&RunOutcome::Skipped));
    }

    #[tokio::test]
    async fn any_match_across_multiple_deps_is_enough_to_run() {
        // "c" depends on both "a" (wants Failed) and "b" (wants Succeeded).
        // "a" will actually fail, "b" will succeed — an OR-join, so "c"
        // should still run even though it's not "every dep matched the
        // same outcome."
        let mut actions = ActionMap::new();
        actions.insert("a".into(), Action::new("fails"));
        actions.insert("b".into(), Action::new("noop"));
        let (id, action) = action_after(
            "c",
            &[
                ("a", &[RunOutcome::Failed]),
                ("b", &[RunOutcome::Succeeded]),
            ],
        );
        actions.insert(id, action);
        let ctx = RunContext::new();

        let result = run_level(&actions, &ctx, |id, action, _ctx| {
            let should_fail = action.type_id == "fails";
            let id = id.to_string();
            async move {
                if should_fail {
                    Err(format!("{id} failed on purpose"))
                } else {
                    Ok(Map::new())
                }
            }
        })
        .await;

        assert_eq!(result.outcomes.get("a"), Some(&RunOutcome::Failed));
        assert_eq!(result.outcomes.get("b"), Some(&RunOutcome::Succeeded));
        assert_eq!(result.outcomes.get("c"), Some(&RunOutcome::Succeeded));
    }

    #[tokio::test]
    async fn a_failed_action_is_recorded_with_its_error_message() {
        let mut actions = ActionMap::new();
        actions.insert("a".into(), Action::new("noop"));
        let ctx = RunContext::new();

        let result = run_level(&actions, &ctx, |_id, _action, _ctx| async {
            Err::<Map<String, Value>, String>("boom".to_string())
        })
        .await;

        assert_eq!(result.outcomes.get("a"), Some(&RunOutcome::Failed));
        assert_eq!(result.errors.get("a"), Some(&"boom".to_string()));
        assert!(!result.all_ok());
    }

    #[tokio::test]
    async fn unvalidated_cycle_fails_closed_instead_of_hanging() {
        // Deliberately bypasses `validate()` to prove `run_level` itself
        // can't hang forever even if a cycle slips through — the "should
        // be unreachable" branch.
        let mut actions = ActionMap::new();
        let (id_a, a) = action_after("a", &[("b", &[RunOutcome::Succeeded])]);
        let (id_b, b) = action_after("b", &[("a", &[RunOutcome::Succeeded])]);
        actions.insert(id_a, a);
        actions.insert(id_b, b);
        let ctx = RunContext::new();

        let result = run_level(&actions, &ctx, ok_executor).await;
        assert_eq!(result.outcomes.get("a"), Some(&RunOutcome::Failed));
        assert_eq!(result.outcomes.get("b"), Some(&RunOutcome::Failed));
        assert!(result.errors.get("a").unwrap().contains("stuck"));
    }
}

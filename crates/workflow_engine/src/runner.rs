//! §6 of `DOCS/workflow-schema/execution-engine-design.md` — the top-level
//! entry point a UI layer calls to actually run a flow: validates it
//! (§3's cycle check), drives the root `ActionMap` through the scheduler,
//! and reports every action's status transitions (including nested ones,
//! at any depth) via the given [`StatusSink`].

use std::path::PathBuf;

use super::container::execute_action;
use super::engine::RunContext;
use super::process_tracker::ProcessOwner;
use super::scheduler::{run_level, validate};
use super::schema::WorkflowDefinition;
use super::status::{RunStatus, StatusSink};

/// Runs `def` to completion. `solution_root` is only consulted by `script`
/// actions' `file` mode (see `executors::execute_leaf`'s doc comment).
/// Owns its arguments rather than borrowing them — this is meant to be
/// driven from `backend::on_tokio` (`F: Future + Send + 'static`), and a
/// single flow run isn't a hot path where the clone cost matters.
pub async fn run_workflow(
    def: WorkflowDefinition,
    solution_root: PathBuf,
    status: StatusSink,
) -> Result<(), String> {
    validate(&def)?;
    // A server this run starts replaces the one the flow's previous run left
    // running for the same node, rather than piling up beside it.
    let ctx = RunContext::new().with_process_owner(ProcessOwner {
        flow: format!("{}|{}", solution_root.display(), def.id),
        run: uuid::Uuid::new_v4().to_string(),
    });
    let result = run_level(&def.actions, &ctx, |id, action, ctx| {
        execute_action(id, action, ctx, &solution_root, &status)
    })
    .await;

    for (id, outcome) in &result.outcomes {
        if *outcome == super::schema::RunOutcome::Skipped {
            status.report(id, RunStatus::Skipped, None);
        }
    }

    if result.all_ok() {
        Ok(())
    } else {
        let first_error = result
            .errors
            .values()
            .next()
            .cloned()
            .unwrap_or_else(|| "one or more actions failed".to_string());
        Err(first_error)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::Value;

    use super::*;
    use crate::schema::{Action, ActionMap};

    #[tokio::test]
    async fn runs_a_flat_flow_end_to_end_and_reports_status_for_every_action() {
        let mut actions = ActionMap::new();
        let mut a = Action::new("log");
        a.inputs.insert("level".into(), Value::from("info"));
        a.inputs.insert("message".into(), Value::from("first"));
        actions.insert("a".to_string(), a);

        let mut b = Action::new("log");
        b.inputs.insert("level".into(), Value::from("info"));
        b.inputs.insert("message".into(), Value::from("second"));
        b.run_after
            .insert("a".to_string(), vec![crate::schema::RunOutcome::Succeeded]);
        actions.insert("b".to_string(), b);

        let def = WorkflowDefinition {
            id: "x".into(),
            name: "X".into(),
            actions,
            outputs: Default::default(),
        };

        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        let status = StatusSink::new(move |id, s, _detail| {
            sink_events.lock().unwrap().push((id.to_string(), s))
        });

        let result = run_workflow(def, PathBuf::from("."), status).await;
        assert!(result.is_ok());

        let recorded = events.lock().unwrap().clone();
        assert!(recorded.contains(&("a".to_string(), RunStatus::Succeeded)));
        assert!(recorded.contains(&("b".to_string(), RunStatus::Succeeded)));
    }

    #[tokio::test]
    async fn a_cyclic_flow_is_rejected_before_anything_runs() {
        let mut actions = ActionMap::new();
        let mut a = Action::new("log");
        a.run_after
            .insert("b".to_string(), vec![crate::schema::RunOutcome::Succeeded]);
        let mut b = Action::new("log");
        b.run_after
            .insert("a".to_string(), vec![crate::schema::RunOutcome::Succeeded]);
        actions.insert("a".to_string(), a);
        actions.insert("b".to_string(), b);
        let def = WorkflowDefinition {
            id: "x".into(),
            name: "X".into(),
            actions,
            outputs: Default::default(),
        };

        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        let status = StatusSink::new(move |id, s, _detail| {
            sink_events.lock().unwrap().push((id.to_string(), s))
        });

        let err = run_workflow(def, PathBuf::from("."), status)
            .await
            .unwrap_err();
        assert!(err.contains("cycle"));
        // Rejected before anything was dispatched — no status events at all.
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failing_action_surfaces_its_message_and_reports_failed() {
        let mut actions = ActionMap::new();
        actions.insert("bad".to_string(), Action::new("TotallyMadeUp"));
        let def = WorkflowDefinition {
            id: "x".into(),
            name: "X".into(),
            actions,
            outputs: Default::default(),
        };

        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        let status = StatusSink::new(move |id, s, _detail| {
            sink_events.lock().unwrap().push((id.to_string(), s))
        });

        let err = run_workflow(def, PathBuf::from("."), status)
            .await
            .unwrap_err();
        assert!(err.contains("unknown action type"));
        assert!(
            events
                .lock()
                .unwrap()
                .contains(&("bad".to_string(), RunStatus::Failed))
        );
    }

    /// Every `transform` action in `.forge/order-processing.flow.json`
    /// (the canonical example) was migrated this session from the old
    /// `strategy: String` field to a real `expression` — see the design
    /// doc's §4 "known follow-up cost" note. This is the structural half
    /// of proving that migration: every `transform`, at every nesting
    /// depth, actually has `expression` and not a stray leftover
    /// `strategy`. Fast — no actual run.
    #[test]
    fn reference_flow_transform_actions_use_expression_not_the_old_strategy_field() {
        fn check(actions: &crate::schema::ActionMap) {
            for (id, action) in actions {
                if action.type_id == "transform" {
                    assert!(
                        action.inputs.contains_key("expression"),
                        "{id}: still missing \"expression\""
                    );
                    assert!(
                        !action.inputs.contains_key("strategy"),
                        "{id}: still has a stray \"strategy\" field"
                    );
                }
                if let Some(body) = &action.actions {
                    check(body);
                }
                if let Some(branch) = &action.else_branch {
                    check(&branch.actions);
                }
                if let Some(branch) = &action.catch {
                    check(&branch.actions);
                }
            }
        }

        let text = include_str!("../fixtures/order-processing.flow.json");
        let def: WorkflowDefinition = serde_json::from_str(text).unwrap();
        check(&def.actions);
    }

    /// The *behavioral* half — and, honestly, a record of what's still
    /// blocking a literal fully-`Succeeded` run of this specific
    /// reference flow. `waitForApi` (`WaitForPort`, port 4000) has no real
    /// service listening in any environment this test runs in, so it
    /// fails, and the fake `api.forge.dev` domain means every `http`
    /// action fails too — both real, expected, environment-independent
    /// outcomes, not bugs. What changed (decided 2026-09-11, "loosen
    /// runAfter gating"): `fetchOrder`/`fetchCustomer`/`backoff`/`ifVip`'s
    /// `runAfter` now accept `[Succeeded, Failed]` on their
    /// network-dependent dependency instead of `[Succeeded]` only, so a
    /// failed network call no longer cascade-skips everything downstream
    /// of it — and `foreach`'s own `foreach` expression, plus the pricing
    /// `transform`s, now fall back to a literal default via `{"or": [...]}`
    /// when the (failed) `http` data they'd normally read is absent. The
    /// **overall run still reports `Failed`** (most leaves that touch a
    /// network call are genuinely, correctly `Failed` — none of this is
    /// faked away), but every container type now actually *dispatches and
    /// runs its real logic* instead of being skipped before ever running.
    /// `notify` became a real webhook POST in the same pass as this test
    /// (§4, decided 2026-09-12) — its `url`s here (`hooks.forge.dev`) are
    /// exactly as fake as `http`'s `api.forge.dev`, so `notify` actions now
    /// fail for the same honest reason `http` ones already did, which
    /// changed *which* containers succeed vs. fail (see below) without
    /// changing whether they execute at all:
    /// - `Foreach` iterates the fallback list (both items) and its
    ///   per-item `Try`/`Catch` genuinely attempts recovery — `chargeItem`
    ///   fails, `logChargeFailure` (the catch body) succeeds, but
    ///   `notifyChargeFailure` (also in the catch body) now fails too, so
    ///   the catch handler *itself* doesn't fully succeed and `tryCharge`
    ///   correctly resolves `Failed`, not recovered — a real demonstration
    ///   of `Try`/`Catch` correctly propagating a failure when recovery
    ///   itself doesn't succeed, not just the happy "recovered" path
    ///   (that path is already covered by `container.rs`'s own unit tests
    ///   with a mocked catch handler).
    /// - `Until` fails on iteration 0 (`pollStatus` unreachable) — genuinely
    ///   dispatched its body, not skipped.
    /// - `If` evaluates its expression, correctly picks the `False` branch
    ///   (`fetchCustomer` failed, so `tier` never resolves `"vip"`), and
    ///   runs every action in it — three succeed, `notify2` fails (same
    ///   fake-webhook reason), so `If` itself resolves `Failed` too,
    ///   correctly propagated from its chosen branch.
    /// `until`'s `runAfter` on `foreach` was loosened to
    /// `[Succeeded, Failed]` (same "loosen runAfter gating" pattern as
    /// `fetchOrder`/`fetchCustomer`/`backoff`/`ifVip` already had) so
    /// `Until`/`If` still get exercised despite `Foreach` now failing too.
    /// Ignored by default: `waitForApi`'s own `timeout_ms` is a real 30
    /// seconds.
    #[tokio::test]
    #[ignore = "real 30s WaitForPort timeout — run explicitly with `cargo test -- --ignored`"]
    async fn reference_flow_exercises_every_container_type_despite_no_real_network() {
        let text = include_str!("../fixtures/order-processing.flow.json");
        let def: WorkflowDefinition = serde_json::from_str(text).unwrap();

        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        let status = StatusSink::new(move |id, s, detail| {
            sink_events
                .lock()
                .unwrap()
                .push((id.to_string(), s, detail))
        });

        let result = run_workflow(def, PathBuf::from("."), status).await;
        eprintln!("RUN RESULT: {result:?}");
        for (id, s, detail) in events.lock().unwrap().iter() {
            eprintln!("  {id}: {s:?} {detail:?}");
        }
        assert!(
            result.is_err(),
            "still expected: waitForApi/fetchOrder/etc. are genuinely network-unreachable here"
        );

        let recorded: Vec<(String, RunStatus)> = events
            .lock()
            .unwrap()
            .iter()
            .map(|(id, s, _)| (id.clone(), *s))
            .collect();
        let last_status_for = |id: &str| {
            recorded
                .iter()
                .rev()
                .find(|(i, _)| i == id)
                .map(|(_, s)| *s)
        };

        // Root-level: real network failures stay real failures.
        assert_eq!(last_status_for("setRegion"), Some(RunStatus::Succeeded));
        assert_eq!(last_status_for("waitForApi"), Some(RunStatus::Failed));
        assert_eq!(last_status_for("fetchOrder"), Some(RunStatus::Failed));
        assert_eq!(last_status_for("fetchCustomer"), Some(RunStatus::Failed));
        assert_eq!(last_status_for("validate"), Some(RunStatus::Succeeded));

        // Foreach: no longer skipped — iterates the fallback array. Its
        // per-item Try/Catch genuinely attempts recovery: chargeItem
        // fails, the catch body's logChargeFailure succeeds but
        // notifyChargeFailure also fails (fake webhook), so the catch
        // handler doesn't fully succeed and tryCharge/foreach correctly
        // resolve Failed — a real "recovery itself failed" propagation,
        // not a skip.
        assert_eq!(last_status_for("foreach"), Some(RunStatus::Failed));
        assert_eq!(last_status_for("tryCharge"), Some(RunStatus::Failed));
        assert_eq!(last_status_for("chargeItem"), Some(RunStatus::Failed));
        assert_eq!(
            last_status_for("logChargeFailure"),
            Some(RunStatus::Succeeded)
        );
        assert_eq!(
            last_status_for("notifyChargeFailure"),
            Some(RunStatus::Failed)
        );

        // Until: no longer skipped despite Foreach failing (loosened gate)
        // — genuinely dispatches its body; fails on iteration 0 since
        // pollStatus can't reach a real API (not a skip, not faked into
        // passing).
        assert_eq!(last_status_for("until"), Some(RunStatus::Failed));

        // If: no longer skipped despite Until failing — picks the False
        // branch (fetchCustomer failed, so tier never resolves "vip"), and
        // runs every action in it. Three succeed; notify2 fails (same fake
        // webhook reason), so If itself correctly propagates that as
        // Failed too — not silently swallowed.
        assert_eq!(last_status_for("ifVip"), Some(RunStatus::Failed));
        assert_eq!(
            last_status_for("standardPricing"),
            Some(RunStatus::Succeeded)
        );
        assert_eq!(last_status_for("applyTax"), Some(RunStatus::Succeeded));
        assert_eq!(last_status_for("logStandard"), Some(RunStatus::Succeeded));
        assert_eq!(last_status_for("notify2"), Some(RunStatus::Failed));

        // notifyDone/handleError/logFinal never get a matching outcome
        // from their own (un-loosened) gate — legitimately Skipped, not a
        // bug: they were never claimed to run despite ifVip failing.
        assert_eq!(last_status_for("notifyDone"), Some(RunStatus::Skipped));
        assert_eq!(last_status_for("logFinal"), Some(RunStatus::Skipped));
    }

    #[tokio::test]
    async fn skipped_actions_are_reported_without_a_running_event() {
        let mut actions = ActionMap::new();
        actions.insert("a".to_string(), Action::new("TotallyMadeUp")); // fails
        let mut b = Action::new("log");
        b.run_after
            .insert("a".to_string(), vec![crate::schema::RunOutcome::Succeeded]);
        actions.insert("b".to_string(), b);
        let def = WorkflowDefinition {
            id: "x".into(),
            name: "X".into(),
            actions,
            outputs: Default::default(),
        };

        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        let status = StatusSink::new(move |id, s, _detail| {
            sink_events.lock().unwrap().push((id.to_string(), s))
        });

        let _ = run_workflow(def, PathBuf::from("."), status).await;
        let recorded = events.lock().unwrap().clone();
        assert!(recorded.contains(&("b".to_string(), RunStatus::Skipped)));
        assert!(!recorded.contains(&("b".to_string(), RunStatus::Running)));
    }
}

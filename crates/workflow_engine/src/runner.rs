//! §6 of `DOCS/workflow-schema/execution-engine-design.md` — the top-level
//! entry point a UI layer calls to actually run a flow: validates it
//! (§3's cycle check), drives the root `ActionMap` through the scheduler,
//! and reports every action's status transitions (including nested ones,
//! at any depth) via the given [`StatusSink`].

use std::path::PathBuf;
use std::time::Duration;

use super::async_rt::on_tokio;
use super::container::execute_action;
use super::engine::RunContext;
use super::process_tracker::{self, ProcessOwner};
use super::run_control::{RunControl, STOPPED_MESSAGE};
use super::scheduler::{run_level, validate};
use super::schema::WorkflowDefinition;
use super::status::{RunStatus, StatusSink};

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    /// Every node finished, and every server the run started has exited
    /// cleanly.
    Completed,
    /// A node failed, or a server exited with an error. Carries the first
    /// error.
    Failed(String),
    /// The run was stopped through its [`RunControl`].
    Stopped,
}

/// How often a run checks whether the servers it started are still alive.
const SERVICE_POLL: Duration = Duration::from_millis(200);

/// How many of a server's last output lines go into the error when it dies.
const SERVICE_FAILURE_LOG_LINES: usize = 10;

/// Runs `def` to completion. `solution_root` is only consulted by `script`
/// actions' `file` mode (see `executors::execute_leaf`'s doc comment).
/// Owns its arguments rather than borrowing them — this is meant to be
/// driven from `backend::on_tokio` (`F: Future + Send + 'static`), and a
/// single flow run isn't a hot path where the clone cost matters.
///
/// A run with servers in it does not finish when its last node does: it
/// lasts until the servers exit, one of them fails, or the run is stopped.
/// A caller that needs to stop the run uses [`run_workflow_controlled`].
pub async fn run_workflow(
    def: WorkflowDefinition,
    solution_root: PathBuf,
    status: StatusSink,
) -> Result<(), String> {
    match run_workflow_controlled(def, solution_root, status, RunControl::new()).await {
        RunEnd::Completed => Ok(()),
        RunEnd::Failed(message) => Err(message),
        RunEnd::Stopped => Err(STOPPED_MESSAGE.to_string()),
    }
}

/// [`run_workflow`] with a handle for stopping the run.
///
/// What a run is:
///
/// 1. The nodes run, in the order `runAfter` says. A `StartProcess` node with
///    `ready_*` settings finishes as soon as its server is up, so the nodes
///    after it can start, and the server is recorded in `control`.
/// 2. When the nodes are done, the run **keeps watching those servers**. It
///    reports each one's final status as it exits, and ends when none is left.
/// 3. It ends early when a node fails or a server exits with an error (the
///    other servers are stopped), or when `control.stop()` is called (every
///    server is stopped and nodes that have not started are skipped).
///
/// A finite `script` node that is already running when the run is stopped is
/// not interrupted; it finishes, and the run then ends.
pub async fn run_workflow_controlled(
    def: WorkflowDefinition,
    solution_root: PathBuf,
    status: StatusSink,
    control: RunControl,
) -> RunEnd {
    if let Err(message) = validate(&def) {
        return RunEnd::Failed(message);
    }
    // A server this run starts replaces the one the flow's previous run left
    // running for the same node, rather than piling up beside it.
    let owner = ProcessOwner {
        flow: format!("{}|{}", solution_root.display(), def.id),
        run: uuid::Uuid::new_v4().to_string(),
    };
    let ctx = RunContext::new()
        .with_process_owner(owner.clone())
        .with_control(control.clone());
    let result = run_level(&def.actions, &ctx, |id, action, ctx| {
        execute_action(id, action, ctx, &solution_root, &status)
    })
    .await;

    for (id, outcome) in &result.outcomes {
        if *outcome == super::schema::RunOutcome::Skipped {
            status.report(id, RunStatus::Skipped, None);
        }
    }

    let first_error = (!result.all_ok()).then(|| {
        result
            .errors
            .values()
            .next()
            .cloned()
            .unwrap_or_else(|| "one or more actions failed".to_string())
    });

    // Stopping a process waits for the operating system, so this part runs on
    // the engine's own runtime and not on the caller's thread.
    on_tokio(wind_down(control, owner, first_error)).await
}

/// What happens after the last node has finished: stop what has to be
/// stopped, or watch the servers until the run is over.
async fn wind_down(
    control: RunControl,
    owner: ProcessOwner,
    first_error: Option<String>,
) -> RunEnd {
    let registry = process_tracker::registry();

    // A stop wins over everything else, including an error it caused.
    if control.is_stopped() {
        stop_services(&control, &owner, "stopped");
        return RunEnd::Stopped;
    }
    if let Some(error) = first_error {
        stop_services(&control, &owner, "stopped because the run failed");
        return RunEnd::Failed(error);
    }

    loop {
        if control.is_stopped() {
            stop_services(&control, &owner, "stopped");
            return RunEnd::Stopped;
        }

        let services = control.services();
        if services.is_empty() {
            return RunEnd::Completed;
        }

        let mut failure = None;
        for service in services {
            let Some(code) = registry.exit_code_if_exited(&service.process_id) else {
                continue;
            };
            control.remove_service(&service.process_id);
            if code == 0 {
                service.status.report(
                    &service.action_id,
                    RunStatus::Succeeded,
                    Some("exited with code 0".to_string()),
                );
            } else {
                let output = registry.log_tail(&service.process_id, SERVICE_FAILURE_LOG_LINES);
                let message = service_failure_message(&service.action_id, code, &output);
                service
                    .status
                    .report(&service.action_id, RunStatus::Failed, Some(message.clone()));
                failure.get_or_insert(message);
            }
        }
        if let Some(message) = failure {
            stop_services(&control, &owner, "stopped because another server failed");
            return RunEnd::Failed(message);
        }

        tokio::time::sleep(SERVICE_POLL).await;
    }
}

/// Stops every server this run started and reports each one's node as
/// finished, with `detail` saying why. A server that was stopped did what it
/// was meant to until then, so its node is `Succeeded`, not `Failed`.
fn stop_services(control: &RunControl, owner: &ProcessOwner, detail: &str) {
    process_tracker::registry().stop_run(owner);
    for service in control.services() {
        control.remove_service(&service.process_id);
        service.status.report(
            &service.action_id,
            RunStatus::Succeeded,
            Some(detail.to_string()),
        );
    }
}

fn service_failure_message(action_id: &str, code: i32, output: &[String]) -> String {
    let mut message = format!("{action_id}: the server exited with code {code}");
    let output = output
        .iter()
        .map(|line| line.trim_end())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if !output.is_empty() {
        message.push_str(":\n");
        message.push_str(&output.join("\n"));
    }
    message
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

    // ---- runs with servers in them -------------------------------------

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use tokio::net::TcpListener;

    use crate::run_control::RunControl;
    use crate::schema::RunOutcome;

    fn long_command(seconds: u64) -> (&'static str, Vec<String>) {
        #[cfg(target_os = "windows")]
        return (
            "cmd",
            vec![
                "/C".to_string(),
                format!("ping -n {} 127.0.0.1 > nul", seconds + 1),
            ],
        );
        #[cfg(not(target_os = "windows"))]
        return ("sleep", vec![seconds.to_string()]);
    }

    /// Prints `text`, runs for about a second, then exits with `code`.
    fn exits_later(code: i32, text: &str) -> (&'static str, Vec<String>) {
        #[cfg(target_os = "windows")]
        return (
            "cmd",
            vec![
                "/C".to_string(),
                format!("echo {text} & ping -n 2 127.0.0.1 > nul & exit {code}"),
            ],
        );
        #[cfg(not(target_os = "windows"))]
        return (
            "sh",
            vec![
                "-c".to_string(),
                format!("echo {text}; sleep 1; exit {code}"),
            ],
        );
    }

    fn failing_command(code: i32) -> (&'static str, Vec<String>) {
        #[cfg(target_os = "windows")]
        return (
            "cmd",
            vec!["/C".to_string(), format!("echo boom 1>&2 & exit {code}")],
        );
        #[cfg(not(target_os = "windows"))]
        return (
            "sh",
            vec!["-c".to_string(), format!("echo boom >&2; exit {code}")],
        );
    }

    fn start_node(command: &str, args: Vec<String>, extra: Vec<(&str, Value)>) -> Action {
        let mut node = Action::new("StartProcess");
        node.inputs.insert("command".into(), Value::from(command));
        node.inputs.insert("args".into(), serde_json::json!(args));
        for (key, value) in extra {
            node.inputs.insert(key.into(), value);
        }
        node
    }

    fn flow(id: &str, actions: Vec<(&str, Action)>) -> WorkflowDefinition {
        let mut map = ActionMap::new();
        for (node_id, node) in actions {
            map.insert(node_id.to_string(), node);
        }
        WorkflowDefinition {
            id: id.into(),
            name: id.into(),
            actions: map,
            outputs: Default::default(),
        }
    }

    async fn open_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { while listener.accept().await.is_ok() {} });
        port
    }

    type Events = Arc<Mutex<Vec<(String, RunStatus)>>>;

    fn recording_sink() -> (StatusSink, Events) {
        let events: Events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = events.clone();
        let sink = StatusSink::new(move |id, status, _| {
            sink_events.lock().unwrap().push((id.to_string(), status))
        });
        (sink, events)
    }

    fn saw(events: &Events, id: &str, status: RunStatus) -> bool {
        events.lock().unwrap().contains(&(id.to_string(), status))
    }

    async fn wait_until(events: &Events, id: &str, status: RunStatus) {
        for _ in 0..200 {
            if saw(events, id, status) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{id} never reached {status:?}");
    }

    fn any_running(action_id: &str) -> bool {
        process_tracker::registry().list().iter().any(|p| {
            p.action_id == action_id && p.status == process_tracker::ProcessStatus::Running
        })
    }

    #[tokio::test]
    async fn a_run_with_a_ready_server_lasts_until_it_is_stopped() {
        let port = open_port().await;
        let (command, args) = long_command(60);
        let def = flow(
            "run-lasts",
            vec![(
                "lasting-web",
                start_node(command, args, vec![("ready_port", Value::from(port))]),
            )],
        );
        let (status, events) = recording_sink();
        let control = RunControl::new();
        let finished = Arc::new(AtomicBool::new(false));

        let run = {
            let finished = finished.clone();
            let control = control.clone();
            async move {
                let end = run_workflow_controlled(def, PathBuf::from("."), status, control).await;
                finished.store(true, Ordering::SeqCst);
                end
            }
        };
        let stopper = {
            let events = events.clone();
            let control = control.clone();
            let finished = finished.clone();
            async move {
                wait_until(&events, "lasting-web", RunStatus::Ready).await;
                // The node is done, but the run must still be going.
                tokio::time::sleep(Duration::from_millis(600)).await;
                assert!(
                    !finished.load(Ordering::SeqCst),
                    "the run ended while its server was still running"
                );
                assert_eq!(control.running_services(), 1);
                control.stop();
            }
        };

        let (end, ()) = futures::join!(run, stopper);

        assert_eq!(end, RunEnd::Stopped);
        // The node went Running, then Ready, then finished when it was stopped.
        assert!(saw(&events, "lasting-web", RunStatus::Running));
        assert!(saw(&events, "lasting-web", RunStatus::Ready));
        assert!(saw(&events, "lasting-web", RunStatus::Succeeded));
        assert!(!any_running("lasting-web"));
        assert_eq!(control.running_services(), 0);
    }

    #[tokio::test]
    async fn a_server_that_exits_with_an_error_fails_the_run_and_stops_the_others() {
        let port = open_port().await;
        let (good_command, good_args) = long_command(60);
        let (bad_command, bad_args) = exits_later(3, "up");
        let def = flow(
            "run-fails",
            vec![
                (
                    "healthy-server",
                    start_node(
                        good_command,
                        good_args,
                        vec![("ready_port", Value::from(port))],
                    ),
                ),
                (
                    "dying-server",
                    start_node(
                        bad_command,
                        bad_args,
                        vec![("ready_text", Value::from("up"))],
                    ),
                ),
            ],
        );
        let (status, events) = recording_sink();

        let end = run_workflow_controlled(def, PathBuf::from("."), status, RunControl::new()).await;

        match end {
            RunEnd::Failed(message) => {
                assert!(message.contains("dying-server"), "{message}");
                assert!(message.contains("exited with code 3"), "{message}");
            }
            other => panic!("expected the run to fail, got {other:?}"),
        }
        assert!(saw(&events, "dying-server", RunStatus::Ready));
        assert!(saw(&events, "dying-server", RunStatus::Failed));
        assert!(!any_running("healthy-server"));
    }

    #[tokio::test]
    async fn a_failing_node_stops_the_servers_the_run_started() {
        let port = open_port().await;
        let (command, args) = long_command(60);
        let (fail_command, fail_args) = failing_command(2);
        let mut boom = start_node(fail_command, fail_args, vec![]);
        boom.run_after
            .insert("early-server".to_string(), vec![RunOutcome::Succeeded]);
        let def = flow(
            "run-node-fails",
            vec![
                (
                    "early-server",
                    start_node(command, args, vec![("ready_port", Value::from(port))]),
                ),
                ("boom", boom),
            ],
        );
        let (status, events) = recording_sink();

        let end = run_workflow_controlled(def, PathBuf::from("."), status, RunControl::new()).await;

        assert!(matches!(end, RunEnd::Failed(_)), "{end:?}");
        assert!(saw(&events, "early-server", RunStatus::Ready));
        assert!(saw(&events, "boom", RunStatus::Failed));
        assert!(!any_running("early-server"));
    }

    #[tokio::test]
    async fn servers_that_all_exit_cleanly_complete_the_run() {
        let (command, args) = exits_later(0, "up");
        let def = flow(
            "run-clean",
            vec![(
                "short-lived",
                start_node(command, args, vec![("ready_text", Value::from("up"))]),
            )],
        );
        let (status, events) = recording_sink();

        let end = run_workflow_controlled(def, PathBuf::from("."), status, RunControl::new()).await;

        // What actually happened, for a failure message: the events, and what
        // each process of this node was and printed.
        let what_happened = || {
            let registry = process_tracker::registry();
            let processes: Vec<_> = registry
                .list()
                .iter()
                .filter(|p| p.action_id == "short-lived")
                .map(|p| (format!("{:?}", p.status), registry.log_snapshot(&p.id)))
                .collect();
            format!(
                "end: {end:?}
events: {:?}
processes: {processes:?}",
                events.lock().unwrap()
            )
        };
        assert_eq!(end, RunEnd::Completed, "{}", what_happened());
        assert!(
            saw(&events, "short-lived", RunStatus::Ready),
            "{}",
            what_happened()
        );
        assert!(
            saw(&events, "short-lived", RunStatus::Succeeded),
            "{}",
            what_happened()
        );
    }

    #[tokio::test]
    async fn a_flow_of_finite_commands_completes_when_they_end() {
        let (command, args) = exits_later(0, "built");
        let def = flow(
            "run-finite",
            vec![("build-step", start_node(command, args, vec![]))],
        );
        let (status, events) = recording_sink();
        let control = RunControl::new();

        let end = run_workflow_controlled(def, PathBuf::from("."), status, control.clone()).await;

        assert_eq!(end, RunEnd::Completed);
        assert!(saw(&events, "build-step", RunStatus::Succeeded));
        assert!(!saw(&events, "build-step", RunStatus::Ready));
        assert_eq!(control.running_services(), 0);
    }

    #[tokio::test]
    async fn stopping_before_the_run_starts_skips_every_node() {
        let (command, args) = long_command(60);
        let def = flow(
            "run-pre-stopped",
            vec![("never-started", start_node(command, args, vec![]))],
        );
        let (status, events) = recording_sink();
        let control = RunControl::new();
        control.stop();

        let end = run_workflow_controlled(def, PathBuf::from("."), status, control).await;

        assert_eq!(end, RunEnd::Stopped);
        assert!(saw(&events, "never-started", RunStatus::Skipped));
        assert!(!saw(&events, "never-started", RunStatus::Running));
        assert!(!any_running("never-started"));
    }

    #[tokio::test]
    async fn a_commands_output_reaches_the_runs_sink_with_its_node_id() {
        let (command, args) = exits_later(0, "hello from the run");
        let def = flow(
            "run-output",
            vec![("talker", start_node(command, args, vec![]))],
        );
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = lines.clone();
        let sink = StatusSink::none().with_output(move |_scope, id, _stream, line| {
            seen.lock().unwrap().push(format!("{id}: {line}"))
        });

        let end = run_workflow_controlled(def, PathBuf::from("."), sink, RunControl::new()).await;

        assert_eq!(end, RunEnd::Completed);
        // The forwarder is a separate task: give it a moment.
        for _ in 0..60 {
            if lines
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("talker: hello from the run"))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "the output never reached the sink: {:?}",
            lines.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn run_workflow_still_returns_ok_for_a_flow_that_ends_by_itself() {
        let (command, args) = exits_later(0, "done");
        let def = flow(
            "run-compat",
            vec![("compat-node", start_node(command, args, vec![]))],
        );

        let result = run_workflow(def, PathBuf::from("."), StatusSink::none()).await;

        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn run_workflow_reports_a_failed_node_as_an_error() {
        let (command, args) = failing_command(5);
        let def = flow(
            "run-compat-fail",
            vec![("compat-fail", start_node(command, args, vec![]))],
        );

        let err = run_workflow(def, PathBuf::from("."), StatusSink::none())
            .await
            .unwrap_err();

        assert!(err.contains("exited with code 5"), "{err}");
    }
}

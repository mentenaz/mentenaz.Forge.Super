//! Builds a `WorkflowDefinition` from a list of services — the generation
//! half of "Add Task Chain" (`project_scan` is the detection half; the
//! Flows panel's `TaskChainWizard` — `src/forge_shell/panels/flows_panel.rs`
//! — collects/edits the service list via UI and calls [`build`]). Pure
//! data, no `gpui`, same convention as the rest of this module.

use std::collections::HashSet;

use indexmap::IndexMap;
use serde_json::Value;

use super::schema::{Action, ActionMap, RunOutcome, WorkflowDefinition};

#[derive(Debug, Clone)]
pub struct ServiceSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Relative to the solution root; empty or `"."` means "the root
    /// itself" and is omitted from the generated action entirely (see
    /// `execute_start_process`'s own `cwd`-optional handling).
    pub cwd: String,
    pub port: u16,
}

/// One `StartProcess` + one gated `WaitForPort` per service, all as
/// independent parallel roots (no `runAfter` *between* services) — the
/// scheduler already runs independent root actions concurrently (proven
/// throughout `backend::workflow`'s own test suite, `scheduler.rs` and
/// `container.rs`'s `Foreach` both), so "start N services concurrently"
/// falls out of the existing engine for free; this is just the
/// two-action-per-service shape that makes it so.
pub fn build(
    id: impl Into<String>,
    name: impl Into<String>,
    services: &[ServiceSpec],
) -> WorkflowDefinition {
    let mut actions = ActionMap::new();
    let mut used_ids: HashSet<String> = HashSet::new();

    for (index, service) in services.iter().enumerate() {
        let suffix = sanitize_suffix(&service.name, index);
        let start_id = unique_id(format!("start{suffix}"), &mut used_ids);
        let wait_id = unique_id(format!("wait{suffix}"), &mut used_ids);

        let mut start = Action::new("StartProcess");
        start.label = Some(format!("Start {}", service.name));
        start.inputs.insert(
            "command".to_string(),
            Value::String(service.command.clone()),
        );
        start.inputs.insert(
            "args".to_string(),
            Value::Array(service.args.iter().cloned().map(Value::String).collect()),
        );
        if !service.cwd.is_empty() && service.cwd != "." {
            start
                .inputs
                .insert("cwd".to_string(), Value::String(service.cwd.clone()));
        }
        // Best-effort guess, same spirit as the port itself (see
        // `ServiceSpec::port`'s own doc trail back through
        // `project_scan::ServiceKind::default_port`) — good enough for the
        // common local-dev-server case, editable afterward like everything
        // else this generates.
        start.inputs.insert(
            "url".to_string(),
            Value::String(format!("http://localhost:{}", service.port)),
        );
        // A dev server never exits, so without a ready setting the node would
        // wait for it to and the nodes after it would never start. The node is
        // Ready, and releases them, once the port answers.
        start
            .inputs
            .insert("ready_port".to_string(), Value::from(service.port));
        start.pos = Some((index as f32 * 260.0, 80.0));

        let mut wait = Action::new("WaitForPort");
        wait.label = Some(format!("Wait For {}", service.name));
        wait.inputs
            .insert("port".to_string(), Value::from(service.port));
        wait.inputs
            .insert("timeout_ms".to_string(), Value::from(30_000));
        wait.run_after
            .insert(start_id.clone(), vec![RunOutcome::Succeeded]);
        wait.pos = Some((index as f32 * 260.0, 220.0));

        actions.insert(start_id, start);
        actions.insert(wait_id, wait);
    }

    WorkflowDefinition {
        id: id.into(),
        name: name.into(),
        actions,
        outputs: IndexMap::new(),
    }
}

/// Strips everything but ASCII alphanumerics — the "start"/"wait" prefix
/// already guarantees the *combined* id starts with a letter (the schema's
/// `actionId` requirement), so this only needs to keep the remainder
/// legal, not worry about its own first character. Falls back to
/// `Service<index>` if a name had no alphanumeric characters at all (e.g.
/// a service named just "🚀").
fn sanitize_suffix(name: &str, index: usize) -> String {
    let cleaned: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if cleaned.is_empty() {
        format!("Service{index}")
    } else {
        cleaned
    }
}

fn unique_id(base: String, used: &mut HashSet<String>) -> String {
    if used.insert(base.clone()) {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::validate;

    fn service(name: &str, command: &str, args: &[&str], cwd: &str, port: u16) -> ServiceSpec {
        ServiceSpec {
            name: name.to_string(),
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_string(),
            port,
        }
    }

    #[test]
    fn builds_a_start_and_wait_pair_per_service() {
        let services = vec![
            service("frontend", "npm", &["run", "dev"], "frontend", 5173),
            service("backend", "npm", &["run", "start"], "backend", 3000),
        ];
        let def = build("fullstack", "Full Stack", &services);

        assert_eq!(def.actions.len(), 4);
        let start = &def.actions["startfrontend"];
        assert_eq!(start.type_id, "StartProcess");
        assert_eq!(start.inputs["command"], Value::from("npm"));
        assert_eq!(start.inputs["cwd"], Value::from("frontend"));
        assert_eq!(start.inputs["url"], Value::from("http://localhost:5173"));
        // Ready once the port answers: without it a server's node would wait
        // for the command to exit, and the run would never get past it.
        assert_eq!(start.inputs["ready_port"], Value::from(5173));
        assert!(
            start.run_after.is_empty(),
            "StartProcess actions must be independent roots"
        );

        let wait = &def.actions["waitfrontend"];
        assert_eq!(wait.type_id, "WaitForPort");
        assert_eq!(wait.inputs["port"], Value::from(5173));
        assert!(wait.run_after.contains_key("startfrontend"));
    }

    #[test]
    fn services_are_independent_roots_so_they_schedule_concurrently() {
        // Not a timing test (see container.rs's own concurrency proof) —
        // just confirms the *shape* that makes concurrency automatic: no
        // StartProcess action depends on any other service's actions.
        let services = vec![
            service("frontend", "npm", &["run", "dev"], "frontend", 5173),
            service("backend", "npm", &["run", "start"], "backend", 3000),
            service("pyserver", "uvicorn", &["main:app"], "pyserver", 8000),
        ];
        let def = build("fullstack", "Full Stack", &services);
        for (id, action) in &def.actions {
            if action.type_id == "StartProcess" {
                assert!(action.run_after.is_empty(), "{id} should have no runAfter");
            }
        }
    }

    #[test]
    fn omits_cwd_when_root_relative_dir_is_a_bare_dot() {
        let services = vec![service("app", "npm", &["start"], ".", 3000)];
        let def = build("x", "X", &services);
        assert!(!def.actions["startapp"].inputs.contains_key("cwd"));
    }

    #[test]
    fn duplicate_service_names_get_distinct_ids() {
        let services = vec![
            service("api", "npm", &["start"], "api1", 3000),
            service("api", "npm", &["start"], "api2", 3001),
        ];
        let def = build("x", "X", &services);
        assert_eq!(def.actions.len(), 4);
        assert!(def.actions.contains_key("startapi"));
        assert!(def.actions.contains_key("startapi2"));
    }

    #[test]
    fn a_name_with_no_alphanumeric_characters_falls_back_to_a_safe_id() {
        let services = vec![service("🚀", "npm", &["start"], ".", 3000)];
        let def = build("x", "X", &services);
        assert!(def.actions.contains_key("startService0"));
    }

    #[test]
    fn the_generated_flow_passes_validate() {
        let services = vec![
            service("frontend", "npm", &["run", "dev"], "frontend", 5173),
            service("backend", "npm", &["run", "start"], "backend", 3000),
        ];
        let def = build("fullstack", "Full Stack", &services);
        assert!(validate(&def).is_ok());
    }

    #[test]
    fn an_empty_service_list_builds_a_valid_empty_flow() {
        let def = build("x", "X", &[]);
        assert!(def.actions.is_empty());
        assert!(validate(&def).is_ok());
    }
}

//! Control of one flow run: asking it to stop, and the servers it has started.
//!
//! A `StartProcess` node that has become ready leaves its server running after
//! the node itself has finished, so a run cannot simply end when its last node
//! does. The servers are recorded here, and
//! `runner::run_workflow_controlled` watches them until they all end, one of
//! them fails, or the run is stopped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::status::StatusSink;

/// What a node that was interrupted by a stop reports as its error. A caller
/// can compare against it to show "stopped" and not a failure.
pub const STOPPED_MESSAGE: &str = "stopped";

/// One server a run started and is still watching.
#[derive(Clone)]
pub(crate) struct Service {
    pub action_id: String,
    pub process_id: String,
    /// Reports the node's final status once the server ends. It is the sink
    /// as it was when the node ran, so a report from inside a loop iteration
    /// lands in that iteration's scope.
    pub status: StatusSink,
}

#[derive(Default)]
struct Inner {
    stopped: AtomicBool,
    services: Mutex<Vec<Service>>,
}

/// Cheap to clone: every clone is the same run. Hand one to
/// `run_workflow_controlled` and keep another to call [`RunControl::stop`].
#[derive(Clone, Default)]
pub struct RunControl {
    inner: Arc<Inner>,
}

impl RunControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the run to stop: no new node starts, the servers the run started
    /// are killed, and the run ends as `RunEnd::Stopped`. Safe to call more
    /// than once, and from any thread.
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    /// How many servers the run has started and not yet seen end.
    pub fn running_services(&self) -> usize {
        self.inner.services.lock().unwrap().len()
    }

    pub(crate) fn add_service(&self, service: Service) {
        self.inner.services.lock().unwrap().push(service);
    }

    pub(crate) fn services(&self) -> Vec<Service> {
        self.inner.services.lock().unwrap().clone()
    }

    pub(crate) fn remove_service(&self, process_id: &str) {
        self.inner
            .services
            .lock()
            .unwrap()
            .retain(|service| service.process_id != process_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::RunStatus;

    fn service(action_id: &str, process_id: &str) -> Service {
        Service {
            action_id: action_id.to_string(),
            process_id: process_id.to_string(),
            status: StatusSink::none(),
        }
    }

    #[test]
    fn stop_is_seen_by_every_clone() {
        let control = RunControl::new();
        let other = control.clone();
        assert!(!other.is_stopped());
        control.stop();
        assert!(other.is_stopped());
        // Stopping twice is fine.
        control.stop();
        assert!(control.is_stopped());
    }

    #[test]
    fn services_are_added_listed_and_removed_by_process_id() {
        let control = RunControl::new();
        control.add_service(service("web", "p1"));
        control.add_service(service("api", "p2"));
        assert_eq!(control.running_services(), 2);

        control.remove_service("p1");
        let left = control.services();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].action_id, "api");

        // An unknown id removes nothing.
        control.remove_service("never-added");
        assert_eq!(control.running_services(), 1);
    }

    #[test]
    fn a_service_reports_through_its_own_sink() {
        use std::sync::Mutex;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in_sink = seen.clone();
        let control = RunControl::new();
        control.add_service(Service {
            action_id: "web".to_string(),
            process_id: "p1".to_string(),
            status: StatusSink::new(move |id, status, _| {
                seen_in_sink.lock().unwrap().push((id.to_string(), status))
            }),
        });

        for service in control.services() {
            service
                .status
                .report(&service.action_id, RunStatus::Succeeded, None);
        }
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("web".to_string(), RunStatus::Succeeded)]
        );
    }
}

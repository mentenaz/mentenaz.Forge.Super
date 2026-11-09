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

/// Where a server can be reached, as far as its node's settings say. The
/// address it prints once it is up is looked for later (see
/// [`RunControl::service_links`]).
#[derive(Clone, Default)]
pub(crate) struct LinkHints {
    /// The node's `url` input.
    pub url: Option<String>,
    /// The node's `ready_url` input.
    pub ready_url: Option<String>,
    /// The node's `ready_port` input.
    pub ready_port: Option<u16>,
}

/// One server a run started and is still watching.
#[derive(Clone)]
pub(crate) struct Service {
    pub action_id: String,
    /// What to call it: the node's label, or its id when it has none.
    pub name: String,
    pub hints: LinkHints,
    pub process_id: String,
    /// Reports the node's final status once the server ends. It is the sink
    /// as it was when the node ran, so a report from inside a loop iteration
    /// lands in that iteration's scope.
    pub status: StatusSink,
}

/// A running server and the address to reach it on, for showing in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceLink {
    pub action_id: String,
    pub name: String,
    /// What to show: an `http(s)` address, or `localhost:PORT` for a server
    /// that only has a port. `None` when nothing says where it is.
    pub address: Option<String>,
    /// Whether `address` is an `http(s)` address that a browser can open.
    pub openable: bool,
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

    /// The servers the run has up and where to reach each, in the order they
    /// came up. The address is, in order of preference: the node's `url`, an
    /// address the server printed on this machine (`Local: http://...`), the
    /// node's `ready_url`, then `localhost:` and its `ready_port`.
    pub fn service_links(&self) -> Vec<ServiceLink> {
        self.services()
            .into_iter()
            .map(|service| {
                let hints = &service.hints;
                let printed = || {
                    let log = crate::process_tracker::registry().log_snapshot(&service.process_id);
                    printed_local_url(&log)
                };
                let address = hints
                    .url
                    .clone()
                    .or_else(printed)
                    .or_else(|| hints.ready_url.clone())
                    .or_else(|| hints.ready_port.map(|port| format!("localhost:{port}")));
                let openable = address
                    .as_deref()
                    .is_some_and(|a| a.starts_with("http://") || a.starts_with("https://"));
                ServiceLink {
                    action_id: service.action_id,
                    name: service.name,
                    address,
                    openable,
                }
            })
            .collect()
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

/// The first address a server printed that points at this machine, as a
/// browser can open it. Vite prints `Local: http://127.0.0.1:5173/` and
/// `Network: http://192.168.0.5:5173/`; only the first is wanted, since a
/// server that listens on every address (`0.0.0.0`) is reached at `localhost`.
fn printed_local_url(log: &[String]) -> Option<String> {
    log.iter().find_map(|line| {
        let line = crate::executors::strip_ansi(line);
        let start = line.find("http://").or_else(|| line.find("https://"))?;
        let url = line[start..]
            .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')'))
            .next()?
            .trim_end_matches(['.', ',', ';']);
        let authority = url.split_once("://")?.1.split(['/', '?', '#']).next()?;
        let host = match authority.rsplit_once(':') {
            // `[::1]` has colons of its own: a port follows only the `]`.
            Some((host, port)) if !port.ends_with(']') => host,
            _ => authority,
        };
        match host {
            "localhost" | "127.0.0.1" | "[::1]" => Some(url.to_string()),
            "0.0.0.0" | "[::]" => Some(url.replacen(host, "localhost", 1)),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::RunStatus;

    fn service(action_id: &str, process_id: &str) -> Service {
        Service {
            action_id: action_id.to_string(),
            name: action_id.to_string(),
            hints: LinkHints::default(),
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
            name: "web".to_string(),
            hints: LinkHints::default(),
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

    fn lines(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn the_local_address_a_server_prints_is_found_and_the_network_one_ignored() {
        let vite = lines(&[
            "> frontend@0.0.0 dev",
            "\u{1b}[32m\u{1b}[1mVITE\u{1b}[22m v8.3.6\u{1b}[39m  ready in 522 ms",
            "  \u{1b}[32m->\u{1b}[39m  \u{1b}[1mLocal\u{1b}[22m:   \u{1b}[36mhttp://127.0.0.1:5173/\u{1b}[39m",
            "  -> Network: http://192.168.0.5:5173/",
        ]);
        assert_eq!(
            printed_local_url(&vite).as_deref(),
            Some("http://127.0.0.1:5173/")
        );
    }

    #[test]
    fn a_server_that_listens_on_every_address_is_reached_at_localhost() {
        assert_eq!(
            printed_local_url(&lines(&["Listening on http://0.0.0.0:8000"])).as_deref(),
            Some("http://localhost:8000")
        );
        assert_eq!(
            printed_local_url(&lines(&["Starting at http://localhost:3000, ready."])).as_deref(),
            Some("http://localhost:3000")
        );
    }

    #[test]
    fn an_address_that_is_not_on_this_machine_is_not_taken() {
        assert_eq!(
            printed_local_url(&lines(&[
                "docs at https://example.com/guide",
                "Network: http://192.168.0.5:5173/",
                "no address here",
            ])),
            None
        );
        assert_eq!(printed_local_url(&[]), None);
    }

    #[test]
    fn a_service_link_prefers_url_then_printed_then_ready_url_then_port() {
        let control = RunControl::new();
        let hinted = |name: &str, hints: LinkHints| Service {
            hints,
            ..service(name, &format!("no-such-process-{name}"))
        };
        control.add_service(hinted(
            "a",
            LinkHints {
                url: Some("http://localhost:1".to_string()),
                ready_url: Some("http://localhost:2/health".to_string()),
                ready_port: Some(3),
            },
        ));
        control.add_service(hinted(
            "b",
            LinkHints {
                url: None,
                ready_url: Some("http://localhost:2/health".to_string()),
                ready_port: Some(3),
            },
        ));
        control.add_service(hinted(
            "c",
            LinkHints {
                ready_port: Some(6379),
                ..LinkHints::default()
            },
        ));
        control.add_service(hinted("d", LinkHints::default()));

        let links = control.service_links();
        let addresses: Vec<_> = links
            .iter()
            .map(|l| (l.address.as_deref(), l.openable))
            .collect();
        assert_eq!(
            addresses,
            vec![
                (Some("http://localhost:1"), true),
                (Some("http://localhost:2/health"), true),
                (Some("localhost:6379"), false),
                (None, false),
            ]
        );
        assert_eq!(links[0].name, "a");
    }
}

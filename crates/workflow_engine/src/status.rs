//! §6 of `DOCS/workflow-schema/execution-engine-design.md` — per-action
//! status transitions, threaded through `execute_action` and every
//! container executor so a UI layer can render progress live. Deliberately
//! just a callback (`StatusSink`), not a broadcast channel keyed by
//! `(run_id, action_id)` — this module stays gpui-free (see the parent
//! module's own doc: "pure data/logic, no gpui dependency"); the caller
//! (`ForgeShell`/`DesignerPanel`) decides how to fan updates out to the
//! canvas, and for a single Designer tab running its own flow, a direct
//! callback into an mpsc channel is simpler than AppState-level broadcast
//! infrastructure with no second subscriber to justify it yet.

use std::sync::Arc;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    /// A server the node started is up, its ready settings have passed, and it
    /// is still running, so the nodes after it can start. A later `Succeeded`
    /// or `Failed` report says how it ended.
    Ready,
    Succeeded,
    Failed,
    Skipped,
}

/// One `Foreach` iteration a status report was made inside.
///
/// A loop body's actions run once per item under the same action ids, so the
/// id alone can't say which run of the body a report belongs to. A report
/// carries one of these per enclosing loop, outermost first.
#[derive(Debug, Clone, PartialEq)]
pub struct IterationScope {
    /// Id of the `Foreach` action.
    pub container_id: String,
    /// Zero-based position of this iteration's item in the list.
    pub index: usize,
    /// How many items the list has.
    pub total: usize,
    /// The item this iteration is bound to (`$item` in its expressions).
    pub item: Value,
}

/// Which stream a line of a command's output came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

type Callback = dyn Fn(&[IterationScope], &str, RunStatus, Option<String>) + Send + Sync;

type OutputCallback = dyn Fn(&[IterationScope], &str, OutputStream, &str) + Send + Sync;

/// Cheap to `Clone` (an `Arc` underneath) so every recursive
/// `execute_action`/container-executor call can carry its own copy without
/// threading a reference through every lifetime in `container.rs`.
#[derive(Clone)]
pub struct StatusSink {
    callback: Option<Arc<Callback>>,
    /// Where the lines a running command prints go, when a caller wants them.
    output: Option<Arc<OutputCallback>>,
    /// The loop iterations this sink reports from inside, outermost first.
    scope: Arc<[IterationScope]>,
}

impl StatusSink {
    /// For callers that don't care about status — every existing engine
    /// test uses this, so adding live status didn't require touching any
    /// of them beyond passing this in.
    pub fn none() -> Self {
        Self {
            callback: None,
            output: None,
            scope: Arc::from([]),
        }
    }

    /// A sink that is told the action and its status, and not which loop
    /// iteration it ran in. Reports from every iteration of a loop body
    /// arrive under the same action id.
    pub fn new(f: impl Fn(&str, RunStatus, Option<String>) + Send + Sync + 'static) -> Self {
        Self::with_scope(move |_scope, action_id, status, detail| f(action_id, status, detail))
    }

    /// A sink that is also told the enclosing `Foreach` iterations, so it
    /// can keep each iteration's results apart. The slice is empty for an
    /// action outside any loop.
    pub fn with_scope(
        f: impl Fn(&[IterationScope], &str, RunStatus, Option<String>) + Send + Sync + 'static,
    ) -> Self {
        Self {
            callback: Some(Arc::new(f)),
            output: None,
            scope: Arc::from([]),
        }
    }

    /// The sink a loop hands to one iteration of its body: the same
    /// callback, with `iteration` added as the innermost scope.
    pub fn in_iteration(&self, iteration: IterationScope) -> Self {
        let mut scope = self.scope.to_vec();
        scope.push(iteration);
        Self {
            callback: self.callback.clone(),
            output: self.output.clone(),
            scope: scope.into(),
        }
    }

    /// `detail` is the human-readable "what actually happened" a UI log
    /// wants — the serialized `outputs` map for a `Succeeded` leaf, the
    /// error message for `Failed`, `None` for `Running`/`Skipped` (nothing
    /// to show yet).
    pub fn report(&self, action_id: &str, status: RunStatus, detail: Option<String>) {
        if let Some(f) = &self.callback {
            f(&self.scope, action_id, status, detail);
        }
    }

    /// Also asks for the lines a command prints while it runs. `f` is told
    /// the enclosing loop iterations, the action, the stream and the line
    /// (without its line ending). It is called from the engine's own threads,
    /// as lines arrive, so it must not block.
    ///
    /// A sink without this never receives output, and the engine does not
    /// forward any for it.
    pub fn with_output(
        mut self,
        f: impl Fn(&[IterationScope], &str, OutputStream, &str) + Send + Sync + 'static,
    ) -> Self {
        self.output = Some(Arc::new(f));
        self
    }

    /// Whether [`Self::with_output`] was used.
    pub fn wants_output(&self) -> bool {
        self.output.is_some()
    }

    /// One line a command printed. Does nothing for a sink that did not ask
    /// for output.
    pub fn report_output(&self, action_id: &str, stream: OutputStream, line: &str) {
        if let Some(f) = &self.output {
            f(&self.scope, action_id, stream, line);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    type Seen = Arc<Mutex<Vec<(usize, String, OutputStream, String)>>>;

    fn collecting_sink() -> (StatusSink, Seen) {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in_sink = seen.clone();
        let sink = StatusSink::none().with_output(move |scope, action_id, stream, line| {
            seen_in_sink.lock().unwrap().push((
                scope.len(),
                action_id.to_string(),
                stream,
                line.to_string(),
            ))
        });
        (sink, seen)
    }

    #[test]
    fn output_reaches_the_callback() {
        let (sink, seen) = collecting_sink();
        assert!(sink.wants_output());

        sink.report_output("web", OutputStream::Stdout, "ready in 300ms");
        sink.report_output("web", OutputStream::Stderr, "warning: slow");

        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                (
                    0,
                    "web".to_string(),
                    OutputStream::Stdout,
                    "ready in 300ms".to_string()
                ),
                (
                    0,
                    "web".to_string(),
                    OutputStream::Stderr,
                    "warning: slow".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_loop_iteration_keeps_the_output_callback_and_reports_its_scope() {
        let (sink, seen) = collecting_sink();
        let inner = sink.in_iteration(IterationScope {
            container_id: "each".to_string(),
            index: 1,
            total: 3,
            item: Value::Null,
        });

        inner.report_output("web", OutputStream::Stdout, "line");

        // One enclosing loop.
        assert_eq!(seen.lock().unwrap()[0].0, 1);
    }

    #[test]
    fn a_sink_that_did_not_ask_for_output_ignores_it() {
        let plain = StatusSink::new(|_, _, _| {});
        assert!(!plain.wants_output());
        // Must not panic, and has nowhere to go.
        plain.report_output("web", OutputStream::Stdout, "line");
        StatusSink::none().report_output("web", OutputStream::Stderr, "line");
    }
}

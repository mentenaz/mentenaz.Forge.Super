//! The "Run Results" tab — a per-flow, live-updating read-only view of the
//! most recent run, opened from the Designer's toolbar (see
//! [`crate::ViewRunResults`]).
//!
//! It shares the Designer's [`SharedRunState`] via the path-keyed registry
//! (`run_state::run_state_for`) rather than a directly-passed handle, so the
//! tab keeps working (and keeps updating) even if the Designer tab that
//! started the run was closed. State is merged by a ~10 Hz poll started on
//! first render, mirroring `DesignerPanel::start_run_poll`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, scroll::Scrollbar, spinner::Spinner,
    tag::Tag, v_flex,
};
use workspace::{Workspace, item::Item};

use crate::run_state::{LineKind, RunLogLine, RunPhase, RunState, SharedRunState, run_state_for};
use crate::{format_detail, format_ms, phase_color};

/// Displays one flow's live run results. Bound to a flow by its absolute
/// `.flow.json` path and built from the path alone (a `Workspace` action
/// handler has no `Entity<DesignerPanel>` to borrow a run handle from).
pub struct RunResults {
    focus_handle: FocusHandle,
    path: PathBuf,
    flow_name: SharedString,
    run_state: SharedRunState,
    snapshot: RunState,
    log_scroll: ScrollHandle,
    poll_started: bool,
}

impl RunResults {
    /// Opens the singleton results tab for `path`: reactivates an existing
    /// one (the designer can only open one, and it shares the live run
    /// state), otherwise creates it and adds it to the active pane.
    pub fn open(workspace: &mut Workspace, path: &Path, window: &mut Window, cx: &mut App) {
        let existing = workspace.panes().iter().find_map(|pane| {
            pane.read(cx).items().find_map(|item| {
                item.downcast::<RunResults>()
                    .filter(|r| r.read(cx).path == path)
            })
        });
        if let Some(entity) = existing {
            workspace.activate_item(&entity, true, true, window, cx);
            return;
        }

        let flow_name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "flow".to_string());
        let run_state = run_state_for(path);
        let entity =
            cx.new(|cx| RunResults::new(flow_name.into(), path.to_path_buf(), run_state, cx));
        workspace.add_item_to_active_pane(Box::new(entity), None, true, window, cx);
    }

    fn new(
        flow_name: SharedString,
        path: PathBuf,
        run_state: SharedRunState,
        cx: &mut App,
    ) -> Self {
        let snapshot = run_state.lock().unwrap().clone();
        Self {
            focus_handle: cx.focus_handle(),
            path,
            flow_name,
            run_state,
            snapshot,
            log_scroll: ScrollHandle::new(),
            poll_started: false,
        }
    }

    /// First render starts the idle poll: snapshots the shared state onto
    /// this tab while a run is in flight (then a few times a minute while
    /// idle), notifying only when something actually changed — so the tab
    /// stays live across new runs without the Designer having to poke it.
    fn start_poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.poll_started {
            return;
        }
        self.poll_started = true;
        let run_state = self.run_state.clone();
        let mut last_revision = self.snapshot.revision;
        let mut last_live =
            self.snapshot.started_at.is_some() && self.snapshot.finished_at.is_none();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let (revision, live) = {
                    let snapshot = run_state.lock().unwrap();
                    (
                        snapshot.revision,
                        snapshot.started_at.is_some() && snapshot.finished_at.is_none(),
                    )
                };
                if revision != last_revision || live != last_live {
                    last_revision = revision;
                    last_live = live;
                    let updated = this
                        .update_in(cx, |this, _window, cx| {
                            this.snapshot = run_state.lock().unwrap().clone();
                            cx.notify();
                        })
                        .is_ok();
                    if !updated {
                        break;
                    }
                }
                let delay = if live {
                    Duration::from_millis(100)
                } else {
                    Duration::from_millis(500)
                };
                cx.background_executor().timer(delay).await;
            }
        })
        .detach();
    }

    fn outcome_tag(&self) -> Tag {
        if self.snapshot.stopped {
            return Tag::warning().outline();
        }
        match self.snapshot.outcome {
            None if self.snapshot.started_at.is_none() => Tag::secondary().outline(),
            None => Tag::info().outline(),
            Some(workflow_engine::RunOutcome::Succeeded) => Tag::success().outline(),
            Some(workflow_engine::RunOutcome::Failed) => Tag::danger().outline(),
            Some(workflow_engine::RunOutcome::Skipped) => Tag::secondary().outline(),
        }
    }

    fn render_action_rows(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut actions: Vec<&crate::run_state::ActionRun> = self.snapshot.nodes.values().collect();
        actions.sort_by_key(|run| run.started_at);
        actions
            .into_iter()
            .map(|run| self.render_action_row(run, cx).into_any_element())
            .collect()
    }

    fn render_action_row(
        &self,
        run: &crate::run_state::ActionRun,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tag = match run.phase {
            RunPhase::Running => Tag::primary().outline(),
            RunPhase::Ready => Tag::info().outline(),
            RunPhase::Succeeded => Tag::success().outline(),
            RunPhase::Failed => Tag::danger().outline(),
            RunPhase::Skipped => Tag::secondary().outline(),
        };
        let detail_color = if run.phase == RunPhase::Failed {
            cx.theme().danger
        } else {
            cx.theme().foreground
        };
        let detail = run.detail.as_deref().filter(|d| !d.is_empty());

        v_flex()
            .w_full()
            .gap_1()
            .p_2()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(tag.child(run.phase.label()))
                    .when(run.phase == RunPhase::Running, |el| {
                        el.child(Spinner::new().xsmall())
                    })
                    .when_some(run.elapsed(), |el, d| {
                        el.child(
                            div()
                                .ml_auto()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format_ms(d)),
                        )
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground)
                    .child(run.name.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(run.type_id.clone()),
            )
            .when_some(detail, |el, d| {
                el.child(
                    div()
                        .text_xs()
                        .font_family("monospace")
                        .text_color(detail_color)
                        .child(format_detail(d)),
                )
            })
    }

    fn render_log_row(
        &self,
        index: usize,
        line: &RunLogLine,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if line.kind != LineKind::Status {
            return self.render_output_row(index, line, cx).into_any_element();
        }
        let color = phase_color(line.phase, cx.theme());
        let detail = line.detail.as_deref().unwrap_or("");
        let phase_label = if line.phase == RunPhase::Running {
            "running\u{2026}".to_string()
        } else {
            format!(
                "{} in {:.1}s",
                line.phase.label(),
                (line.time_ms as f64) / 1000.0
            )
        };

        h_flex()
            .id(SharedString::from(format!("run-results-log-row-{index}")))
            .w_full()
            .gap_2()
            .items_center()
            .px_3()
            .py_1()
            .child(div().size_1p5().rounded_full().flex_shrink_0().bg(color))
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(cx.theme().foreground)
                    .child(line.name.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(line.type_id.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .font_family("monospace")
                    .text_color(cx.theme().muted_foreground)
                    .child(if detail.is_empty() {
                        phase_label
                    } else {
                        detail.to_string()
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{}ms", line.time_ms)),
            )
            .into_any_element()
    }

    /// One line a command printed: quieter than a status row, in monospace, and
    /// red for stderr.
    fn render_output_row(
        &self,
        index: usize,
        line: &RunLogLine,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let text_color = if line.kind == LineKind::Stderr {
            cx.theme().danger
        } else {
            cx.theme().muted_foreground
        };
        h_flex()
            .id(SharedString::from(format!("run-results-log-row-{index}")))
            .w_full()
            .gap_2()
            .items_center()
            .px_3()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(line.name.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .font_family("monospace")
                    .text_color(text_color)
                    .child(line.detail.clone().unwrap_or_default()),
            )
    }

    fn render_transcript(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let log_count = self.snapshot.log.len();
        v_flex()
            .id("run-results-transcript")
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Icon::new(IconName::SquareTerminal)
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground)
                            .child("Run transcript"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(if log_count == 0 {
                                "No events yet".to_string()
                            } else {
                                format!("{log_count} events")
                            }),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .size_full()
                    .child(
                        div()
                            .id("run-results-transcript-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&mut self.log_scroll)
                            .children(self.snapshot.log.iter().enumerate().map(|(i, line)| {
                                self.render_log_row(i, line, cx).into_any_element()
                            })),
                    )
                    .child(Scrollbar::vertical(&self.log_scroll)),
            )
    }
}

impl Focusable for RunResults {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for RunResults {}

impl Item for RunResults {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("{}.flow.json \u{00b7} Results", self.flow_name).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<ui::Icon> {
        Some(ui::Icon::new(ui::IconName::ListTree))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(format!("Run results for {}", self.flow_name).into())
    }
}

impl Render for RunResults {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.start_poll(window, cx);

        let has_results = !self.snapshot.log.is_empty() || !self.snapshot.nodes.is_empty();
        let (succeeded, failed, skipped) = self.snapshot.totals();
        let run_is_live = self.snapshot.started_at.is_some() && self.snapshot.finished_at.is_none();
        let tag = self.outcome_tag();
        let tag_text = if self.snapshot.stopped {
            "Stopped"
        } else if self.snapshot.outcome.is_none() && self.snapshot.started_at.is_some() {
            "Running"
        } else if self.snapshot.outcome.is_none() {
            "No run yet"
        } else {
            "Finished"
        };

        v_flex()
            .id("run-results-panel")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .w_full()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(IconName::Network).text_color(cx.theme().foreground))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(cx.theme().foreground)
                                    .child(self.flow_name.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .when(run_is_live, |el| el.child(Spinner::new().xsmall()))
                            .child(tag.child(tag_text))
                            .when_some(self.snapshot.duration(), |el, d| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format_ms(d)),
                                )
                            })
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!(
                                        "{succeeded} ok \u{00b7} {failed} failed \u{00b7} {skipped} skipped"
                                    )),
                            ),
                    ),
            )
            .when_some(self.snapshot.error.clone(), |el, error| {
                el.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .id("run-results-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py_2()
                    .child(
                        v_flex()
                            .w_full()
                            .gap_2()
                            .px_3()
                            .when(has_results, |el| el.children(self.render_action_rows(cx)))
                            .when(!has_results, |el| {
                                el.child(
                                    h_flex()
                                        .w_full()
                                        .justify_center()
                                        .py_8()
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(
                                                    "Run the flow from the Designer to populate results.",
                                                ),
                                        ),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .h(gpui::px(200.0))
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(self.render_transcript(cx)),
            )
    }
}

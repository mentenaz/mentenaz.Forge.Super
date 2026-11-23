//! Keeps Forge's panels from running a project's own tooling until the project
//! is trusted: the same decision, and the same prompt, that keep tasks and
//! language servers from running in Restricted Mode.
//!
//! Three parts work together:
//!
//! * [`watch_project`] follows a project's worktrees and the editor's trust
//!   decisions, and publishes the restricted folders to
//!   [`forge_trust_gate`]. The backends that start `cargo`, `dotnet`, `npm`,
//!   `python` and scripts ask that crate before they spawn, so nothing runs in a
//!   restricted folder whichever panel (or tab) asked for it.
//! * [`restricted_notice`] is what a panel shows instead of its content while
//!   its project is restricted: a short explanation and a "Trust this project
//!   to scan it" button that opens the editor's usual trust dialog.
//! * [`on_trust_changed`] lets a panel scan again as soon as the answer
//!   changes.

use std::collections::HashSet;
use std::path::PathBuf;

use gpui::{
    App, Context, Entity, EntityId, FontWeight, Global, IntoElement, ParentElement, Styled,
    Subscription, div,
};
use gpui_component::{
    ActiveTheme, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use project::{
    Project,
    trusted_worktrees::{TrustedWorktrees, TrustedWorktreesEvent},
};

/// Counts the times the set of restricted folders changed. Views that show the
/// trust state observe it, and [`on_trust_changed`] wraps that for panels.
#[derive(Default)]
pub struct TrustEpoch(pub u64);

impl Global for TrustEpoch {}

#[derive(Default)]
struct Watched(HashSet<EntityId>);

impl Global for Watched {}

/// The visible worktrees of `project`, split into the ones the editor trusts and
/// the ones in Restricted Mode. Asking is what puts a worktree in Restricted Mode
/// in the first place, as it is for tasks, so this is also what makes the editor
/// offer its trust dialog. Without a trust system (tests, headless runs) every
/// worktree counts as trusted, and with `trust_all_worktrees` `can_trust` says so
/// at once, so a folder that opens already trusted is marked trusted on the first
/// look.
fn classify_worktrees(project: &Entity<Project>, cx: &mut App) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let trusted_store = TrustedWorktrees::try_get_global(cx);
    let worktree_store = project.read(cx).worktree_store();
    let worktrees: Vec<_> = project.read(cx).visible_worktrees(cx).collect();
    let mut trusted = Vec::new();
    let mut restricted = Vec::new();
    for worktree in worktrees {
        let (id, path) = {
            let worktree = worktree.read(cx);
            (worktree.id(), worktree.abs_path().to_path_buf())
        };
        let can_trust = match &trusted_store {
            Some(store) => store.update(cx, |store, cx| store.can_trust(&worktree_store, id, cx)),
            None => true,
        };
        if can_trust {
            trusted.push(path);
        } else {
            restricted.push(path);
        }
    }
    (trusted, restricted)
}

/// Whether any worktree of `project` is in Restricted Mode.
pub fn project_restricted(project: &Entity<Project>, cx: &mut App) -> bool {
    !classify_worktrees(project, cx).1.is_empty()
}

fn refresh(project: &Entity<Project>, cx: &mut App) {
    let (trusted, restricted) = classify_worktrees(project, cx);
    if forge_trust_gate::set_project_roots(project.entity_id().as_u64(), trusted, restricted) {
        let next = cx.try_global::<TrustEpoch>().map_or(0, |epoch| epoch.0) + 1;
        cx.set_global(TrustEpoch(next));
        // The dock decides what to draw from the trust state, and a panel that
        // is not being redrawn would keep showing the old answer.
        cx.refresh_windows();
    }
}

/// Switches the trust gate on and follows every project that opens. The editor
/// calls this once, before it opens any project: from then on a folder is allowed
/// only after it has been marked trusted, so a panel that scans before the editor
/// has looked is held back and scans again when it has.
pub fn init(cx: &mut App) {
    forge_trust_gate::enforce();
    cx.observe_new(|_: &mut Project, _, cx| {
        let project = cx.entity();
        watch_project(&project, cx);
    })
    .detach();
}

/// Starts following `project`: publishes which of its folders are trusted and which
/// are restricted to [`forge_trust_gate`], and keeps that current as worktrees come and go and as
/// the user trusts them. Safe to call more than once for the same project.
pub fn watch_project(project: &Entity<Project>, cx: &mut App) {
    let id = project.entity_id();
    if !cx.default_global::<Watched>().0.insert(id) {
        return;
    }
    // Known to the gate from this moment, and not classified until the first look
    // below, so nothing it cannot place is let through in between.
    forge_trust_gate::begin_project(id.as_u64());

    // Called for a project that may still be being built, whose state cannot be
    // read yet, so the first look waits until the current update is over.
    let first_look = project.downgrade();
    cx.defer(move |cx| {
        if let Some(project) = first_look.upgrade() {
            refresh(&project, cx);
        }
    });

    cx.subscribe(project, |project, event: &project::Event, cx| {
        if matches!(
            event,
            project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_)
        ) {
            refresh(&project, cx);
        }
    })
    .detach();

    if let Some(trusted) = TrustedWorktrees::try_get_global(cx) {
        let weak_project = project.downgrade();
        cx.subscribe(&trusted, move |_, _: &TrustedWorktreesEvent, cx| {
            if let Some(project) = weak_project.upgrade() {
                refresh(&project, cx);
            }
        })
        .detach();
    }

    cx.observe_release(project, move |_, cx| {
        forge_trust_gate::clear_owner(id.as_u64());
        cx.default_global::<Watched>().0.remove(&id);
    })
    .detach();
}

/// Calls `on_changed` when a project is trusted or restricted, so a panel can
/// scan again (or stop). Keep the returned subscription with the panel's others.
pub fn on_trust_changed<T: 'static>(
    cx: &mut Context<T>,
    on_changed: impl Fn(&mut T, &mut Context<T>) + 'static,
) -> Subscription {
    cx.observe_global::<TrustEpoch>(move |this, cx| on_changed(this, cx))
}

/// A slim banner for a panel that stays usable while its project is restricted
/// but refuses some of its actions: `message` says which, and the button asks to
/// trust the project, the same way [`restricted_notice`] does.
pub fn restricted_banner(message: &str, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .w_full()
        .flex_shrink_0()
        .gap_2()
        .items_center()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.warning.opacity(0.1))
        .child(Icon::new(IconName::TriangleAlert).small().text_color(theme.warning))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(theme.foreground)
                .child(message.to_string()),
        )
        .child(
            Button::new("forge-trust-banner")
                .outline()
                .small()
                .label("Trust project")
                .on_click(|_, window, cx| {
                    if let Ok(action) = cx.build_action("workspace::ToggleWorktreeSecurity", None) {
                        window.dispatch_action(action, cx);
                    }
                }),
        )
}

/// What a panel shows instead of its content while its project is restricted.
pub fn restricted_notice(title: &str, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_3()
        .p_6()
        .child(Icon::new(IconName::TriangleAlert).text_color(theme.muted_foreground))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(format!("{title} is waiting for you to trust this project")),
        )
        .child(div().text_sm().text_color(theme.muted_foreground).child(
            "This project is in Restricted Mode. Scanning it runs the project's own \
                     tools (cargo, dotnet, npm, python) with its own configuration, so Forge \
                     Super has not started any of them here.",
        ))
        .child(
            Button::new("forge-trust-project")
                .primary()
                .small()
                .label("Trust this project to scan it")
                .on_click(|_, window, cx| {
                    if let Ok(action) = cx.build_action("workspace::ToggleWorktreeSecurity", None) {
                        window.dispatch_action(action, cx);
                    }
                }),
        )
}

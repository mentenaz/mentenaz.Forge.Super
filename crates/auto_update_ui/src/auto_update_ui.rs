use auto_update::{AutoUpdater, release_notes_url};
use gpui::{App, DismissEvent, actions, prelude::*};
use release_channel::ReleaseChannel;
use workspace::{
    Workspace,
    notifications::{
        NotificationId, show_app_notification, simple_message_notification::MessageNotification,
    },
};
use zed_actions::ShowUpdateNotification;

actions!(
    auto_update,
    [
        /// Opens the release notes for the current version in the browser.
        ViewReleaseNotesLocally
    ]
);

pub fn init(cx: &mut App) {
    notify_if_app_was_updated(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, cx| {
        workspace.register_action(|_workspace, _: &ViewReleaseNotesLocally, _window, cx| {
            open_release_notes(cx);
        });

        if matches!(
            ReleaseChannel::global(cx),
            ReleaseChannel::Nightly | ReleaseChannel::Dev
        ) {
            workspace.register_action(|_workspace, _: &ShowUpdateNotification, _window, cx| {
                show_update_notification(cx);
            });
        }
    })
    .detach();
}

/// The release notes are a page on the Forge Super site, built from the GitHub
/// releases, so they open in the browser rather than in an editor tab.
fn open_release_notes(cx: &mut App) {
    if let Some(url) = release_notes_url(cx) {
        cx.open_url(&url);
    }
}

struct UpdateNotification;

fn show_update_notification(cx: &mut App) {
    let Some(updater) = AutoUpdater::get(cx) else {
        return;
    };

    let mut version = updater.read(cx).current_version();
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::EMPTY;
    let app_name = ReleaseChannel::global(cx).display_name();

    show_app_notification(
        NotificationId::unique::<UpdateNotification>(),
        cx,
        move |cx| {
            cx.new(|cx| {
                MessageNotification::new(format!("Updated to {app_name} {}", version), cx)
                    .primary_message("View Release Notes")
                    .primary_on_click(move |_window, cx| {
                        open_release_notes(cx);
                        cx.emit(DismissEvent);
                    })
                    .show_suppress_button(false)
            })
        },
    );
}

/// Shows a notification across all workspaces if an update was previously automatically installed
/// and this notification had not yet been shown.
pub fn notify_if_app_was_updated(cx: &mut App) {
    let Some(updater) = AutoUpdater::get(cx) else {
        return;
    };

    if let ReleaseChannel::Nightly = ReleaseChannel::global(cx) {
        return;
    }

    let should_show_notification = updater.read(cx).should_show_update_notification(cx);

    cx.spawn(async move |cx| {
        let should_show_notification = should_show_notification.await?;

        if should_show_notification {
            cx.update(|cx| {
                show_update_notification(cx);
                updater.update(cx, |updater, cx| {
                    updater
                        .set_should_show_update_notification(false, cx)
                        .detach_and_log_err(cx);
                });
            });
        }
        anyhow::Ok(())
    })
    .detach();
}

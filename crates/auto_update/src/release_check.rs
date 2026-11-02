//! Notify-only update check against the GitHub releases of this fork.
//!
//! Forge Super never updates itself (see `update_explanation`): installing
//! comes from winget, the installer or apt. This only asks GitHub which is the
//! newest published release and says so when it is newer than the running
//! build. Nothing is downloaded or installed.
//!
//! Releases are tagged `dev-vN`, and `N` is the build number the workflow
//! passes to the compiler as `FORGE_BUILD_NUMBER`. A build without one (a
//! local or untagged build) has nothing to compare, so it never checks.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};
use client::Client;
use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, DismissEvent, PromptLevel, TaskExt, Window};
use http_client::{HttpClient as _, HttpClientWithUrl};
use release_channel::FORGE_DOCS_URL;
use serde::Deserialize;
use settings::Settings as _;
use smol::io::AsyncReadExt as _;
use workspace::{
    Workspace,
    notifications::{
        NotificationId, show_app_notification, simple_message_notification::MessageNotification,
    },
};

use crate::AutoUpdateSetting;

/// `latest` is the newest published release that is not a draft or a
/// pre-release, which is what a tag push produces (manual test runs publish a
/// "dev-build" pre-release that must not count).
const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/mentenaz/mentenaz.Forge.Super/releases/latest";

/// "yes" or "no", once the user has answered the first-launch question.
const CONSENT_KEY: &str = "forge-update-check-consent";
/// Unix seconds of the last automatic check.
const LAST_CHECK_KEY: &str = "forge-update-check-last";
/// The build number of a release the user chose to skip.
const SKIPPED_KEY: &str = "forge-update-check-skipped";
/// The build number that last ran, to notice when the app has been updated.
const LAST_SEEN_KEY: &str = "forge-last-seen-build";

const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The build number this binary was made from, if it was made from a release.
fn current_build_number() -> Option<u32> {
    option_env!("FORGE_BUILD_NUMBER")?
        .parse()
        .ok()
        .filter(|build| *build > 0)
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
}

async fn fetch_latest_build(http_client: Arc<HttpClientWithUrl>) -> Result<u32> {
    let mut response = http_client
        .get(LATEST_RELEASE_URL, Default::default(), true)
        .await?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub answered {}",
        response.status()
    );

    let release: LatestRelease =
        serde_json::from_slice(&body).context("could not read the release information")?;
    release
        .tag_name
        .strip_prefix("dev-v")
        .and_then(|build| build.parse().ok())
        .with_context(|| format!("unrecognised release tag {:?}", release.tag_name))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

struct UpdateAvailableNotification;
struct UpdateCheckResultNotification;
struct UpdatedNotification;

fn updating_docs_url() -> String {
    format!("{FORGE_DOCS_URL}/updating")
}

fn show_update_available(latest: u32, cx: &mut App) {
    show_app_notification(
        NotificationId::unique::<UpdateAvailableNotification>(),
        cx,
        move |cx| {
            cx.new(|cx| {
                MessageNotification::new(format!("Forge Super dev-v{latest} is available"), cx)
                    .primary_message("How to Update")
                    .primary_on_click(move |_window, cx| {
                        cx.open_url(&updating_docs_url());
                        cx.emit(DismissEvent);
                    })
                    .secondary_message("Skip This Version")
                    .secondary_on_click(move |_window, cx| {
                        let kvp = KeyValueStore::global(cx);
                        cx.background_spawn(async move {
                            kvp.write_kvp(SKIPPED_KEY.to_string(), latest.to_string())
                                .await
                        })
                        .detach_and_log_err(cx);
                        cx.emit(DismissEvent);
                    })
                    .show_suppress_button(false)
            })
        },
    );
}

/// Shown once, the first time a newer build runs than the one before it.
fn show_updated(current: u32, cx: &mut App) {
    show_app_notification(
        NotificationId::unique::<UpdatedNotification>(),
        cx,
        move |cx| {
            cx.new(|cx| {
                MessageNotification::new(format!("Updated to Forge Super dev-v{current}"), cx)
                    .primary_message("View Release Notes")
                    .primary_on_click(move |_window, cx| {
                        cx.open_url(&format!("{FORGE_DOCS_URL}/releases"));
                        cx.emit(DismissEvent);
                    })
                    .show_suppress_button(false)
            })
        },
    );
}

fn show_check_result(message: String, cx: &mut App) {
    show_app_notification(
        NotificationId::unique::<UpdateCheckResultNotification>(),
        cx,
        move |cx| cx.new(|cx| MessageNotification::new(message.clone(), cx)),
    );
}

/// "Check for Updates": always runs, ignores a skipped version, and always
/// says what it found.
pub fn check_manually(cx: &mut App) {
    let Some(current) = current_build_number() else {
        show_check_result(
            format!(
                "This build was not made from a numbered release, so there is nothing to compare. \
                 Releases are listed at {FORGE_DOCS_URL}/releases"
            ),
            cx,
        );
        return;
    };

    let http_client = Client::global(cx).http_client();
    cx.spawn(async move |cx| {
        let result = fetch_latest_build(http_client).await;
        cx.update(|cx| match result {
            Ok(latest) if latest > current => show_update_available(latest, cx),
            Ok(_) => show_check_result(
                format!("Forge Super is up to date (dev-v{current})."),
                cx,
            ),
            Err(error) => {
                log::warn!("update check failed: {error:#}");
                show_check_result(format!("Could not check for updates: {error:#}"), cx);
            }
        });
    })
    .detach();
}

/// Runs when the first workspace window opens. It tells you when the app has
/// been updated since it last ran, and then (if you agreed to it) checks for a
/// newer release: asking once whether to check at all, then at most once a day.
pub fn init(cx: &mut App) {
    let Some(current) = current_build_number() else {
        return;
    };

    static STARTED: AtomicBool = AtomicBool::new(false);

    cx.observe_new(move |_: &mut Workspace, window: Option<&mut Window>, cx| {
        let Some(window) = window else {
            return;
        };
        if STARTED.swap(true, Ordering::SeqCst) {
            return;
        }

        let workspace = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let kvp = cx.update(|_, cx| KeyValueStore::global(cx))?;
            let stored = cx
                .background_spawn({
                    let kvp = kvp.clone();
                    async move {
                        anyhow::Ok((
                            kvp.read_kvp(CONSENT_KEY)?,
                            kvp.read_kvp(LAST_CHECK_KEY)?,
                            kvp.read_kvp(SKIPPED_KEY)?,
                            kvp.read_kvp(LAST_SEEN_KEY)?,
                        ))
                    }
                })
                .await?;
            let (consent, last_check, skipped, last_seen) = stored;

            // A first run has nothing to compare with, so it just records the
            // build; a different build than last time means the app was updated.
            if last_seen.as_deref() != Some(current.to_string().as_str()) {
                kvp.write_kvp(LAST_SEEN_KEY.to_string(), current.to_string())
                    .await?;
                if last_seen.is_some() {
                    cx.update(|_, cx| show_updated(current, cx))?;
                }
            }

            if !cx.update(|_, cx| AutoUpdateSetting::get_global(cx).0)? {
                return anyhow::Ok(());
            }

            let consent = match consent.as_deref() {
                Some(answer) => answer == "yes",
                None => {
                    let answer = workspace.update_in(cx, |_, window, cx| {
                        window.prompt(
                            PromptLevel::Info,
                            "Check for updates automatically?",
                            Some(
                                "Forge Super does not update itself. It can ask GitHub once a \
                                 day whether a newer release exists, and tell you if there is \
                                 one. Nothing is downloaded or installed.\n\n\
                                 You can change this later with \"auto_update\" in your \
                                 settings, or check yourself from Help > Check for Updates.",
                            ),
                            &["Check Automatically", "Not Now"],
                            cx,
                        )
                    })?;
                    let agreed = answer.await == Ok(0);
                    let value = if agreed { "yes" } else { "no" };
                    kvp.write_kvp(CONSENT_KEY.to_string(), value.to_string())
                        .await?;
                    agreed
                }
            };
            if !consent {
                return Ok(());
            }

            let due = last_check
                .and_then(|secs| secs.parse::<u64>().ok())
                .is_none_or(|secs| now_secs().saturating_sub(secs) >= CHECK_INTERVAL.as_secs());
            if !due {
                return Ok(());
            }

            let http_client = cx.update(|_, cx| Client::global(cx).http_client())?;
            let latest = match fetch_latest_build(http_client).await {
                Ok(latest) => latest,
                Err(error) => {
                    // Quietly: an automatic check should never nag about being offline.
                    log::warn!("update check failed: {error:#}");
                    return Ok(());
                }
            };
            kvp.write_kvp(LAST_CHECK_KEY.to_string(), now_secs().to_string())
                .await?;

            let newer = latest > current;
            let skipped = skipped.and_then(|build| build.parse::<u32>().ok()) == Some(latest);
            if newer && !skipped {
                cx.update(|_, cx| show_update_available(latest, cx))?;
            }
            Ok(())
        })
        .detach_and_log_err(cx);
    })
    .detach();
}

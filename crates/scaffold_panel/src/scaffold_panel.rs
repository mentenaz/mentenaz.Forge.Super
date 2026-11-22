//! Scaffold panel: creates new projects from the Forge scaffolding engine
//! (`forge-scaffold`). This is phase 1 of `docs/scaffold-panel-plan.md`: the
//! panel in the dock, the engine's `Fetch` built on this fork's HTTP client,
//! the generator list read from the local templates folder, and a manual
//! "Check for updates". The new-project wizard comes in later phases.
//!
//! The engine draws nothing, runs no commands and makes no network calls of its
//! own; everything slow here (reading the templates folder, the index check and
//! the downloads) runs in `cx.background_spawn`, never on the main thread.

mod scaffold_panel_settings;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use forge_scaffold::{
    CheckStatus, ENV_TEMPLATES_ROOT, Fetch, FetchResult, GeneratorInfo, GeneratorList,
    ScaffoldError, TemplateStore, unix_now,
};
use futures::AsyncReadExt as _;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, actions, div, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    panel_header::PanelHeader,
    spinner::Spinner,
    tag::Tag,
    v_flex,
};
use http_client::{HttpClient, HttpRequestExt as _};
use settings::{DockSide, Settings as _};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::scaffold_panel_settings::ScaffoldPanelSettings;

actions!(
    scaffold_panel,
    [
        /// Toggles focus on the Scaffold panel.
        ToggleFocus
    ]
);

/// The live templates index, published by the templates repo's release builder.
const INDEX_URL: &str = "https://downloads.mentenaz-server.com/forge-scaffold/index.json";

/// The most a single download may be. The index and the archives are small; the
/// engine also checks the size and SHA-256 it was promised, this only stops a
/// server that never ends.
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Registers the Scaffold panel's actions on every workspace. Call once at app
/// startup, alongside the other panels' `init` functions.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ScaffoldPanel>(window, cx);
        });
    })
    .detach();
}

/// The engine's half of the network contract, on this fork's HTTP client: a GET
/// that follows redirects, sends `If-None-Match` when it has an ETag, and maps
/// an HTTP error or a network failure to `ScaffoldError::Fetch`.
///
/// The engine's `Fetch` is synchronous, so this blocks the thread it is called
/// on: only ever call it from `cx.background_spawn`.
struct HttpFetch {
    client: Arc<dyn HttpClient>,
}

impl Fetch for HttpFetch {
    fn get(&self, url: &str, etag: Option<&str>) -> forge_scaffold::Result<FetchResult> {
        let fail = |reason: String| ScaffoldError::Fetch {
            url: url.to_string(),
            reason,
        };
        let mut builder = http_client::Builder::new()
            .method(http_client::Method::GET)
            .uri(url)
            .follow_redirects(http_client::RedirectPolicy::FollowAll);
        if let Some(etag) = etag {
            builder = builder.header("If-None-Match", etag);
        }
        let request = builder
            .body(http_client::AsyncBody::empty())
            .map_err(|error| fail(error.to_string()))?;

        futures::executor::block_on(async {
            let mut response = self
                .client
                .send(request)
                .await
                .map_err(|error| fail(format!("{error:#}")))?;
            let status = response.status();
            if status == http_client::StatusCode::NOT_MODIFIED {
                return Ok(FetchResult::NotModified);
            }
            if !status.is_success() {
                return Err(fail(format!("HTTP {status}")));
            }
            let etag = response
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let mut bytes = Vec::new();
            response
                .body_mut()
                .take(MAX_DOWNLOAD_BYTES + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|error| fail(error.to_string()))?;
            if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
                return Err(fail("the response is larger than allowed".to_string()));
            }
            Ok(FetchResult::Body { bytes, etag })
        })
    }
}

/// What the list shows.
enum Listing {
    Loading,
    Ready(GeneratorList),
    Failed(String),
}

pub struct ScaffoldPanel {
    focus_handle: FocusHandle,
    listing: Listing,
    /// A check for updates is running.
    checking: bool,
    /// The outcome of the last check, shown under the header.
    notice: Option<String>,
    http: Arc<dyn HttpClient>,
}

/// Where the templates live: next to the Node and Python version stores in the
/// app's own data folder (so an isolated build keeps its own), not in the
/// engine's default under the user's roaming profile. The engine's
/// `FORGE_TEMPLATES` override still wins, for trying things out.
fn templates_dir() -> PathBuf {
    match std::env::var_os(ENV_TEMPLATES_ROOT).filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => paths::data_dir().join("scaffold_templates"),
    }
}

/// Reads the templates folder: no network. Runs off the main thread.
fn read_listing() -> std::result::Result<GeneratorList, String> {
    let store = TemplateStore::open(templates_dir()).map_err(|error| error.to_string())?;
    store.list_generators().map_err(|error| error.to_string())
}

/// Checks the index now (a manual check ignores the interval), then reads the
/// list again. Blocks on the network: runs off the main thread.
fn check_and_read(fetch: &HttpFetch) -> std::result::Result<(String, GeneratorList), String> {
    let store = TemplateStore::open(templates_dir()).map_err(|error| error.to_string())?;
    let outcome = store
        .check_updates(fetch, INDEX_URL, unix_now(), true, 0)
        .map_err(|error| error.to_string())?;
    let notice = match (outcome.status, outcome.updates.len()) {
        (CheckStatus::Skipped, _) => "Checked a moment ago.".to_string(),
        (_, 0) => "Templates are up to date.".to_string(),
        (_, 1) => "1 template can be downloaded or updated.".to_string(),
        (_, count) => format!("{count} templates can be downloaded or updated."),
    };
    let list = store.list_generators().map_err(|error| error.to_string())?;
    Ok((notice, list))
}

impl ScaffoldPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            ScaffoldPanel::new(workspace, window, cx)
        })
    }

    pub fn new(
        _workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let http = cx.http_client();
        cx.new(|cx| {
            let mut panel = Self {
                focus_handle: cx.focus_handle(),
                listing: Listing::Loading,
                checking: false,
                notice: None,
                http,
            };
            panel.reload(cx);
            panel
        })
    }

    /// Reads the templates folder again, off the main thread.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.listing = Listing::Loading;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async { read_listing() }).await;
            this.update(cx, |this, cx| {
                this.listing = match result {
                    Ok(list) => Listing::Ready(list),
                    Err(error) => Listing::Failed(error),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The manual "Check for updates": asks the index, then shows the new list.
    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if self.checking {
            return;
        }
        self.checking = true;
        self.notice = None;
        cx.notify();
        let fetch = HttpFetch {
            client: self.http.clone(),
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { check_and_read(&fetch) })
                .await;
            this.update(cx, |this, cx| {
                this.checking = false;
                match result {
                    Ok((notice, list)) => {
                        this.notice = Some(notice);
                        this.listing = Listing::Ready(list);
                    }
                    Err(error) => this.notice = Some(format!("Could not check: {error}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_row(&self, info: &GeneratorInfo, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        // Tinted, not filled: a filled tag is unreadable in themes where the
        // fill and its foreground are close.
        let (label, color) = if info.local {
            ("Custom", theme.warning)
        } else if info.installed {
            ("Installed", theme.success)
        } else {
            ("Available", theme.info)
        };
        v_flex()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(div().font_semibold().child(info.title.clone()))
                    .child(
                        Tag::custom(color.opacity(0.2), color, color.opacity(0.3))
                            .small()
                            .child(label),
                    ),
            )
            .children(
                info.description
                    .clone()
                    .filter(|description| !description.is_empty())
                    .map(|description| div().text_xs().text_color(muted).child(description)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("Latest: {}", info.latest_version)),
            )
    }
}

impl Focusable for ScaffoldPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ScaffoldPanel {}

impl Panel for ScaffoldPanel {
    fn persistent_name() -> &'static str {
        "Scaffold Panel"
    }

    fn panel_key() -> &'static str {
        "ScaffoldPanel"
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        match ScaffoldPanelSettings::get_global(cx).dock {
            DockSide::Left => DockPosition::Left,
            DockSide::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        settings::update_settings_file(<dyn fs::Fs>::global(cx), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left | DockPosition::Bottom => DockSide::Left,
                DockPosition::Right => DockSide::Right,
            };
            settings.scaffold_panel.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(280.)
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<ui::IconName> {
        ScaffoldPanelSettings::get_global(cx)
            .button
            .then_some(ui::IconName::Box)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Scaffold")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        30
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.scaffold_panel.get_or_insert_default().button = Some(false);
        }))
    }
}

impl Render for ScaffoldPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (sidebar, border, danger) = (theme.sidebar, theme.border, theme.danger);

        let header = PanelHeader::new("Scaffold").actions([Button::new("scaffold-check")
            .ghost()
            .small()
            .label(if self.checking {
                "Checking…"
            } else {
                "Check for updates"
            })
            .disabled(self.checking)
            .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(cx)))
            .into_any_element()]);

        let body = match &self.listing {
            Listing::Loading => h_flex()
                .gap_2()
                .items_center()
                .p_3()
                .child(Spinner::new().small())
                .child(div().text_sm().text_color(muted).child("Loading templates…"))
                .into_any_element(),
            Listing::Failed(error) => div()
                .p_3()
                .text_sm()
                .text_color(danger)
                .child(format!("Could not read the templates: {error}"))
                .into_any_element(),
            Listing::Ready(list) if list.generators.is_empty() => div()
                .p_3()
                .text_sm()
                .text_color(muted)
                .child("No templates yet. Use Check for updates to download them.")
                .into_any_element(),
            Listing::Ready(list) => v_flex()
                .children(list.generators.iter().map(|info| self.render_row(info, cx)))
                .children(list.problems.iter().map(|problem| {
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(danger)
                        .child(problem.clone())
                }))
                .into_any_element(),
        };

        // Every root fills the dock, loading and error states included.
        v_flex()
            .id("scaffold-panel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(sidebar)
            .border_r_1()
            .border_color(border)
            .child(header)
            .children(self.notice.clone().map(|notice| {
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child(notice)
            }))
            // The list scrolls inside the panel: there are dozens of rows, and
            // the header stays put above them.
            .child(
                div()
                    .id("scaffold-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

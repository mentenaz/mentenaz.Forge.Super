//! Scaffold panel: creates new projects from the Forge scaffolding engine
//! (`forge-scaffold`). This is phase 1 of `docs/scaffold-panel-plan.md`: the
//! panel in the dock, the engine's `Fetch` built on this fork's HTTP client,
//! the generator list read from the local templates folder, and a manual
//! "Check for updates". The new-project wizard comes in later phases.
//!
//! The engine draws nothing, runs no commands and makes no network calls of its
//! own; everything slow here (reading the templates folder, the index check and
//! the downloads) runs in `cx.background_spawn`, never on the main thread.

mod new_project;
mod scaffold_panel_settings;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use forge_scaffold::{
    CheckStatus, ENV_TEMPLATES_ROOT, Fetch, FetchResult, GeneratorInfo, GeneratorList,
    ScaffoldError, TemplateStore, unix_now,
};
use futures::AsyncReadExt as _;
use gpui::{
    Action, AnyElement, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Subscription, prelude::FluentBuilder as _,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render,
    StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, actions, div, px,
};
use db::kvp::KeyValueStore;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    panel_header::PanelHeader,
    spinner::Spinner,
    tab::{Tab, TabBar, TabVariant},
    tag::Tag,
    v_flex,
};
use http_client::{HttpClient, HttpRequestExt as _};
use settings::{DockSide, Settings as _};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::new_project::NewProjectView;
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

/// Where the chosen tag filter is remembered, in the same database the other
/// panels keep their small bits of state in. An empty value means no filter.
/// The search text is deliberately not saved: a panel that reopens already
/// filtered by last week's search looks broken.
const TAG_FILTER_KEY: &str = "scaffold_panel.tag_filter";

/// Where the chosen list tab is remembered (as `all`, `installed` or `local`).
const TAB_KEY: &str = "scaffold_panel.tab";

/// The tabs over the list. Every template is in `All`; `Installed` is what has
/// been downloaded; `Local` is a folder the user made themselves (it stays empty
/// until there are any, and says so).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListTab {
    All,
    Installed,
    Local,
}

impl ListTab {
    const ALL: [(ListTab, &'static str, &'static str); 3] = [
        (ListTab::All, "All templates", "all"),
        (ListTab::Installed, "Installed", "installed"),
        (ListTab::Local, "Local templates", "local"),
    ];

    fn from_saved(saved: &str) -> Self {
        Self::ALL
            .iter()
            .find(|(_, _, key)| *key == saved)
            .map_or(ListTab::All, |(tab, _, _)| *tab)
    }

    fn saved(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(tab, _, _)| *tab == self)
            .map_or("all", |(_, _, key)| key)
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|(tab, _, _)| *tab == self)
            .unwrap_or(0)
    }
}

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
    workspace: WeakEntity<Workspace>,
    /// The search box over the list; typing re-renders the list.
    search: Entity<InputState>,
    /// Only generators carrying this tag are shown. Remembered between sessions.
    tag_filter: Option<String>,
    /// Which tab of the list is showing. Remembered between sessions.
    tab: ListTab,
    _search_subscription: Subscription,
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
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let http = cx.http_client();
        let workspace = workspace.weak_handle();
        cx.new(|cx| {
            let search =
                cx.new(|cx| InputState::new(window, cx).placeholder("Search templates…"));
            let search_subscription = cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    cx.notify();
                }
            });
            let mut panel = Self {
                focus_handle: cx.focus_handle(),
                listing: Listing::Loading,
                checking: false,
                notice: None,
                http,
                workspace,
                search,
                tag_filter: None,
                tab: ListTab::All,
                _search_subscription: search_subscription,
            };
            panel.reload(cx);
            Self::restore_tag_filter(cx);
            Self::restore_tab(cx);
            panel
        })
    }

    /// Sets (or clears) the tag filter and remembers it.
    fn set_tag_filter(&mut self, tag: Option<String>, cx: &mut Context<Self>) {
        self.tag_filter = tag.clone();
        cx.notify();
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            kvp.write_kvp(TAG_FILTER_KEY.to_string(), tag.unwrap_or_default())
                .await
                .ok();
        })
        .detach();
    }

    /// Restores the tag filter of the last session. Reads the IDE's database, so
    /// off the main thread; until it lands the list is unfiltered.
    fn restore_tag_filter(cx: &mut Context<Self>) {
        let kvp = KeyValueStore::global(cx);
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_spawn(async move { kvp.read_kvp(TAG_FILTER_KEY) })
                .await
                .ok()
                .flatten()
                .filter(|tag| !tag.is_empty());
            let Some(tag) = saved else {
                return;
            };
            this.update(cx, |panel, cx| {
                panel.tag_filter = Some(tag);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Switches to the tab at `index` and remembers it.
    fn set_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((tab, _, _)) = ListTab::ALL.get(index) else {
            return;
        };
        self.tab = *tab;
        cx.notify();
        let kvp = KeyValueStore::global(cx);
        let saved = tab.saved().to_string();
        cx.background_spawn(async move {
            kvp.write_kvp(TAB_KEY.to_string(), saved).await.ok();
        })
        .detach();
    }

    /// Restores the tab of the last session, off the main thread; until it lands
    /// the list shows every template.
    fn restore_tab(cx: &mut Context<Self>) {
        let kvp = KeyValueStore::global(cx);
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_spawn(async move { kvp.read_kvp(TAB_KEY) })
                .await
                .ok()
                .flatten();
            let Some(saved) = saved else {
                return;
            };
            this.update(cx, |panel, cx| {
                panel.tab = ListTab::from_saved(&saved);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Whether `info` belongs on the current tab. A downloaded template is the
    /// one the row tags "Installed"; a folder of the user's own is "Custom" and
    /// belongs to the Local tab alone.
    fn in_tab(&self, info: &GeneratorInfo) -> bool {
        match self.tab {
            ListTab::All => true,
            ListTab::Installed => info.installed && !info.local,
            ListTab::Local => info.local,
        }
    }

    /// Whether `info` passes the tab, the tag filter and the search text (already
    /// lower case). The search looks at the title, the id, the description and
    /// the tags.
    fn matches_filter(&self, info: &GeneratorInfo, query: &str) -> bool {
        if !self.in_tab(info) {
            return false;
        }
        if let Some(tag) = &self.tag_filter
            && !info.tags.iter().any(|candidate| candidate == tag)
        {
            return false;
        }
        if query.is_empty() {
            return true;
        }
        let contains = |text: &str| text.to_lowercase().contains(query);
        contains(&info.title)
            || contains(&info.id)
            || info.description.as_deref().is_some_and(|d| contains(d))
            || info.tags.iter().any(|tag| contains(tag))
    }

    /// The search box and one chip per tag the generators carry.
    fn render_filters(&self, list: &GeneratorList, shown: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let tags: BTreeSet<&String> = list.generators.iter().flat_map(|g| g.tags.iter()).collect();
        let filtering = self.tag_filter.is_some()
            || !self.search.read(cx).value().is_empty()
            || self.tab != ListTab::All;
        // What is on the current tab: the count a filter or a search narrows down.
        let on_tab = list
            .generators
            .iter()
            .filter(|info| self.in_tab(info))
            .count();
        v_flex()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            // The tabs sit above the search box.
            .child(
                TabBar::new("scaffold-list-tabs")
                    .with_variant(TabVariant::Underline)
                    .children(
                        ListTab::ALL
                            .iter()
                            .map(|(_, label, _)| Tab::new().label(*label)),
                    )
                    .selected_index(self.tab.index())
                    .on_click(cx.listener(|this, index: &usize, _, cx| this.set_tab(*index, cx))),
            )
            .child(Input::new(&self.search))
            .when(!tags.is_empty(), |column| {
                column.child(h_flex().flex_wrap().gap_1().children(
                    tags.into_iter().enumerate().map(|(index, tag)| {
                        let selected = self.tag_filter.as_deref() == Some(tag.as_str());
                        let chip = Button::new(("scaffold-tag", index)).small().label(tag.clone());
                        let chip = if selected { chip.primary() } else { chip.outline() };
                        let tag = tag.clone();
                        chip.on_click(cx.listener(move |this, _, _, cx| {
                            // Clicking the chosen tag again clears it.
                            let next = (this.tag_filter.as_deref() != Some(tag.as_str()))
                                .then(|| tag.clone());
                            this.set_tag_filter(next, cx);
                        }))
                    }),
                ))
            })
            .when(filtering, |column| {
                column.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("Showing {shown} of {on_tab}")),
                )
            })
            .into_any_element()
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

    /// Opens the new-project form for `generator` in a workspace tab.
    fn open_new_project(
        &mut self,
        generator: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let http = self.http.clone();
        workspace.update(cx, |workspace, cx| {
            let weak_workspace = workspace.weak_handle();
            let view = cx.new(|cx| {
                NewProjectView::new(generator, title.into(), http, weak_workspace, window, cx)
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    }

    fn render_row(
        &self,
        index: usize,
        info: &GeneratorInfo,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (generator, title) = (info.id.clone(), info.title.clone());
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (border, hover) = (theme.border, theme.accent);
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
            .id(("scaffold-generator", index))
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .cursor_pointer()
            .hover(|style| style.bg(hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_new_project(generator.clone(), title.clone(), window, cx);
            }))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    // The title shortens instead of pushing the tag out of a
                    // narrow panel.
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_semibold()
                            .child(info.title.clone()),
                    )
                    .child(
                        div().flex_shrink_0().child(
                            Tag::custom(color.opacity(0.2), color, color.opacity(0.3))
                                .small()
                                .child(label),
                        ),
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
            .into_any_element()
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
        let query = self.search.read(cx).value().trim().to_lowercase();

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
            Listing::Ready(list) => {
                let rows: Vec<&GeneratorInfo> = list
                    .generators
                    .iter()
                    .filter(|info| self.matches_filter(info, &query))
                    .collect();
                if rows.is_empty() {
                    let narrowed = self.tag_filter.is_some() || !query.is_empty();
                    // An empty tab with nothing narrowing it says what belongs
                    // there; otherwise it is the filters that found nothing.
                    let message = match (self.tab, narrowed) {
                        (ListTab::Installed, false) => {
                            "Nothing downloaded yet. Pick a template under All templates and \
                             download it, and it is listed here."
                        }
                        (ListTab::Local, false) => {
                            "No local templates yet. A template folder you make yourself will \
                             be listed here."
                        }
                        _ => "No templates match.",
                    };
                    v_flex()
                        .gap_2()
                        .p_3()
                        .child(div().text_sm().text_color(muted).child(message))
                        .child(
                            h_flex()
                                .gap_2()
                                .when(narrowed, |row| {
                                    row.child(
                                        Button::new("scaffold-clear-filters")
                                            .outline()
                                            .small()
                                            .label("Clear filters")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.search.update(cx, |search, cx| {
                                                    search.set_value("", window, cx);
                                                });
                                                this.set_tag_filter(None, cx);
                                            })),
                                    )
                                })
                                .when(self.tab != ListTab::All, |row| {
                                    row.child(
                                        Button::new("scaffold-show-all")
                                            .outline()
                                            .small()
                                            .label("Show all templates")
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.set_tab(0, cx)),
                                            ),
                                    )
                                }),
                        )
                        .into_any_element()
                } else {
                    v_flex()
                        .children(
                            rows.iter()
                                .enumerate()
                                .map(|(index, info)| self.render_row(index, info, cx)),
                        )
                        .children(list.problems.iter().map(|problem| {
                            div()
                                .px_3()
                                .py_1()
                                .text_xs()
                                .text_color(danger)
                                .child(problem.clone())
                        }))
                        .into_any_element()
                }
            }
        };

        // The search box and tag chips sit above the list, outside its scroll.
        let filters = match &self.listing {
            Listing::Ready(list) if !list.generators.is_empty() => {
                let shown = list
                    .generators
                    .iter()
                    .filter(|info| self.matches_filter(info, &query))
                    .count();
                Some(self.render_filters(list, shown, cx))
            }
            _ => None,
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
            .children(filters)
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

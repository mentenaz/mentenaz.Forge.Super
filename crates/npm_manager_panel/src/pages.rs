//! The five `SettingPage`s of the npm manager tab.
//!
//! Each page is a *view shell*: a cheap `SettingPage` whose body is a single
//! `SettingItem::render` that embeds a dedicated child `Entity<...>` view. The
//! heavy rows live in those child views and are built — by reading the panel
//! live through a `WeakEntity<NpmManagerPanel>` in each view's `Render::render`
//! — only for the page currently open in the `Settings` widget, which renders
//! just the active page's items.
//!
//! Building the pages therefore no longer clones the package lists up front,
//! so a re-render of the panel (e.g. the 60s auto-refresh) never traces through
//! every installed / outdated / audit / search row the way the old
//! `build_*_page` functions did.

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, ClipboardItem, Context, Entity, FocusHandle,
    FontWeight, Hsla, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    Render, StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Size, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    setting::{SettingGroup, SettingItem, SettingPage},
    spinner::Spinner,
    table::{DataTable, TableState},
    tag::Tag,
    text::TextView,
    v_flex,
};
use npm_backend::{
    AuditAdvisory, AuditFix, AuditReport, PackageManager, UpdateKind, Verdict, advisory_markdown,
    classify_update, ghsa_id, parse_github_advisory,
};

use crate::audit_card::{
    AdvisoryState, AffectedDelegate, fix_label, friendly_fetch_error, is_direct, is_plain_fix,
    severity_color, table_height, via_label,
};
use crate::search::fetch_json;
use crate::{
    ActSelectedPackage, NpmManagerPanel, OpenSelectedPackage, SelectNextPackage, SelectPrevPackage,
};

/// The key context each package-list page's own container sets — see
/// `npm_manager_panel::init`'s `cx.bind_keys` for the up/down/enter/space
/// bindings scoped to it, and why they live there rather than in the JSON
/// keymap (mirrors `gpui_component::table::data_table`'s own `DataTable`
/// context for the same reason).
const PACKAGE_LIST_CONTEXT: &str = "NpmPackageList";

/// Moves `selected` one row up (`forward: false`) or down (`forward: true`)
/// within a `len`-row list, wrapping at both ends — matches
/// `gpui_component::table::TableState`'s default `loop_selection` behavior
/// — and starting from the top row on the very first press.
fn step_selected(selected: Option<usize>, len: usize, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match selected {
        // Nothing selected yet: the first press lands on the nearest end
        // rather than stepping past it.
        None => {
            if forward {
                0
            } else {
                len - 1
            }
        }
        Some(ix) => {
            if forward {
                (ix + 1) % len
            } else {
                (ix + len - 1) % len
            }
        }
    })
}

/// The five page views created alongside the panel. `PageViews::new` builds
/// them from a `WeakEntity` of the panel so they can read its state live in
/// their own `Render::render` without owning any of the data.
pub(super) struct PageViews {
    pub general: Entity<GeneralPage>,
    pub search: Entity<SearchPage>,
    pub installed: Entity<InstalledPage>,
    pub updates: Entity<UpdatesPage>,
    pub vuln: Entity<VulnPage>,
}

impl PageViews {
    pub(super) fn new(panel: WeakEntity<NpmManagerPanel>, cx: &mut App) -> Self {
        PageViews {
            general: cx.new(|_| GeneralPage::new(panel.clone())),
            search: cx.new(|cx| SearchPage::new(panel.clone(), cx)),
            installed: cx.new(|cx| InstalledPage::new(panel.clone(), cx)),
            updates: cx.new(|cx| UpdatesPage::new(panel.clone(), cx)),
            vuln: cx.new(|cx| VulnPage::new(panel.clone(), cx)),
        }
    }
}

pub(super) fn build_all(project: &crate::NpmProject, pages: &PageViews) -> Vec<SettingPage> {
    vec![
        build_general_page(pages),
        build_search_page(pages),
        build_installed_page(project, pages),
        build_updates_page(project, pages),
        build_vuln_page(project, pages),
    ]
}

/// A `SettingItem` that simply embeds a page view; the shell adds nothing else
/// per frame, so re-renders stay O(1) regardless of the lists' length.
fn embed_view<T>(view: Entity<T>) -> SettingItem
where
    T: Render + 'static,
{
    SettingItem::render(move |_options, _window, _cx| view.clone().into_any_element())
}

// ── General ────────────────────────────────────────────────────────────────

pub(super) struct GeneralPage {
    panel: WeakEntity<NpmManagerPanel>,
}

impl GeneralPage {
    fn new(panel: WeakEntity<NpmManagerPanel>) -> Self {
        Self { panel }
    }
}

impl Render for GeneralPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.panel.upgrade() else {
            return div().into_any_element();
        };
        let panel = view.read(cx);
        let project = panel.active();
        let theme = cx.theme();

        let manager = project.package_manager.cli_name().to_string();
        let dev = panel.install_as_dev;
        let busy = panel.running_action.is_some()
            || project.installed_loading
            || project.outdated_loading
            || project.audit_loading;
        let running_action = panel.running_action.clone();
        let error = panel.error.clone();
        let engine = project.engine.clone();

        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_1()
                    .children(["npm", "yarn", "pnpm", "bun"].map(|name| {
                        let view = view.clone();
                        Button::new(format!("npm-pm-{name}"))
                            .label(name)
                            .with_size(Size::Small)
                            .map(|button| {
                                if manager == name {
                                    button.primary()
                                } else {
                                    button.outline()
                                }
                            })
                            .on_click(move |_, _window, cx| {
                                view.update(cx, |panel, cx| {
                                    panel.active_mut().package_manager = match name {
                                        "yarn" => PackageManager::Yarn,
                                        "pnpm" => PackageManager::Pnpm,
                                        "bun" => PackageManager::Bun,
                                        _ => PackageManager::Npm,
                                    };
                                    cx.notify();
                                });
                            })
                    })),
            )
            .child(
                Button::new("npm-toggle-dev")
                    .label(if dev {
                        "Save new packages as devDependencies"
                    } else {
                        "Save new packages as dependencies"
                    })
                    .with_size(Size::Small)
                    .map(|button| {
                        if dev {
                            button.primary()
                        } else {
                            button.outline()
                        }
                    })
                    .on_click({
                        let view = view.clone();
                        move |_, _window, cx| {
                            view.update(cx, |panel, cx| {
                                panel.install_as_dev = !panel.install_as_dev;
                                cx.notify();
                            });
                        }
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("npm-reload")
                            .icon(IconName::Redo2)
                            .label("Refresh")
                            .with_size(Size::Small)
                            .on_click({
                                let view = view.clone();
                                move |_, _window, cx| {
                                    view.update(cx, |panel, cx| panel.reload_visible(cx));
                                }
                            }),
                    )
                    .when(busy, |row| {
                        row.child(Spinner::new().with_size(Size::XSmall))
                    }),
            )
            .child({
                let mut status = v_flex().gap_1().text_xs();
                if let Some(action) = &running_action {
                    status = status.child(
                        div()
                            .text_color(theme.foreground)
                            .child(format!("{action}…")),
                    );
                }
                if let Some(err) = &error {
                    status =
                        status.child(div().text_color(theme.danger).truncate().child(err.clone()));
                }
                if let Some(engine) = &engine {
                    // A wrong version is a warning while browsing; installing
                    // asks first (`NpmManagerPanel::run_checked`).
                    let (verdict, text) = engine.summary();
                    let color = match verdict {
                        Verdict::Fine => theme.success,
                        Verdict::Unknown => theme.muted_foreground,
                        Verdict::Problem => theme.warning,
                    };
                    status = status.child(
                        div()
                            .id("npm-engine-status")
                            .text_color(color)
                            .truncate()
                            .child(text.clone())
                            .tooltip(move |window, cx| {
                                gpui_component::tooltip::Tooltip::new(text.clone())
                                    .build(window, cx)
                            }),
                    );
                }
                status
            })
            .into_any_element()
    }
}

fn build_general_page(pages: &PageViews) -> SettingPage {
    SettingPage::new("General")
        .default_open(true)
        .resettable(false)
        .icon(Icon::new(IconName::Settings))
        .description("The package manager and install behavior npm commands use.")
        .group(
            SettingGroup::new()
                .title("Package manager")
                .description("Which CLI the install / remove / update actions run.")
                .item(embed_view(pages.general.clone())),
        )
}

// ── Search ─────────────────────────────────────────────────────────────────

pub(super) struct SearchPage {
    panel: WeakEntity<NpmManagerPanel>,
    focus_handle: FocusHandle,
    /// Index into `panel.search_results` — see `InstalledPage::selected`.
    /// Scoped to just the results list below the search box (not the whole
    /// page), so the search `Input`'s own arrow-key/Enter behavior while
    /// typing is untouched — see the results `v_flex`'s own
    /// `track_focus`/`key_context` below, not the page root.
    selected: Option<usize>,
}

impl SearchPage {
    fn new(panel: WeakEntity<NpmManagerPanel>, cx: &mut Context<Self>) -> Self {
        Self {
            panel,
            focus_handle: cx.focus_handle(),
            selected: None,
        }
    }

    fn select_next(&mut self, _: &SelectNextPackage, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let len = view.read(cx).search_results.len();
        self.selected = step_selected(self.selected, len, true);
        cx.notify();
    }

    fn select_prev(&mut self, _: &SelectPrevPackage, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let len = view.read(cx).search_results.len();
        self.selected = step_selected(self.selected, len, false);
        cx.notify();
    }

    fn open_selected(
        &mut self,
        _: &OpenSelectedPackage,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let Some(name) = self
            .selected
            .and_then(|ix| view.read(cx).search_results.get(ix))
            .map(|r| r.name.clone())
        else {
            return;
        };
        view.update(cx, |panel, cx| panel.fetch_details_and_readme(name, cx));
    }

    /// Space's action on this page: Install — not destructive, so unlike
    /// Remove/Uninstall this needs no confirmation, matching the existing
    /// Install button.
    fn act_on_selected(
        &mut self,
        _: &ActSelectedPackage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let Some((name, version, dev)) = self.selected.and_then(|ix| {
            let panel = view.read(cx);
            let r = panel.search_results.get(ix)?;
            Some((r.name.clone(), r.version.clone(), panel.install_as_dev))
        }) else {
            return;
        };
        view.update(cx, |panel, cx| {
            panel.install_pkg(&name, &version, dev, window, cx)
        });
    }
}

impl Render for SearchPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.panel.upgrade() else {
            return div().into_any_element();
        };
        let panel = view.read(cx);
        let theme = cx.theme();

        let search_input = panel.search_input.clone();
        let search_total = panel.search_total;
        let search_loading = panel.search_loading;
        let search_error = panel.search_error.clone();
        let results = &panel.search_results;
        let selected = self.selected;
        let installed = &panel.active().installed;
        let outdated = &panel.active().outdated;

        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&search_input).xsmall().w_full()),
                    )
                    .child(
                        Button::new("npm-search-go")
                            .icon(IconName::Search)
                            .label("Search")
                            .with_size(Size::Small)
                            .on_click({
                                let view = view.clone();
                                move |_, _window, cx| {
                                    view.update(cx, |panel, cx| {
                                        panel.search_page = 0;
                                        panel.search_npm(cx);
                                    });
                                }
                            }),
                    ),
            )
            .child({
                let content: AnyElement = if search_loading {
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Spinner::new().with_size(Size::XSmall))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Searching npmjs.com…"),
                        )
                        .into_any_element()
                } else if let Some(err) = &search_error {
                    div()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(err.clone())
                        .into_any_element()
                } else if results.is_empty() {
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Search the npm registry for a package to install.")
                        .into_any_element()
                } else {
                    let mut list = v_flex()
                        .id("npm-search-results-list")
                        .track_focus(&self.focus_handle)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                window.focus(&this.focus_handle, cx);
                            }),
                        )
                        .key_context(PACKAGE_LIST_CONTEXT)
                        .on_action(cx.listener(Self::select_next))
                        .on_action(cx.listener(Self::select_prev))
                        .on_action(cx.listener(Self::open_selected))
                        .on_action(cx.listener(Self::act_on_selected))
                        .gap_2()
                        .w_full();
                    list = list.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("{search_total} packages found")),
                    );
                    for (ix, result) in results.iter().enumerate() {
                        let name = result.name.clone();
                        let version = result.version.clone();
                        let description = result.description.clone().unwrap_or_default();
                        let compat = result.compat;
                        let is_selected = selected == Some(ix);

                        // Card header: the package name, then the compat
                        // badge when the engines check produced an answer.
                        let mut header = h_flex().gap_2().items_center().w_full();
                        header = header.child(
                            div()
                                .id(format!("npm-result-label-{name}"))
                                .text_sm()
                                .font_semibold()
                                .text_color(theme.foreground)
                                .child(name.clone()),
                        );
                        match compat {
                            Some(true) => {
                                header = header.child(
                                    div()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded_md()
                                        .bg(theme.success.opacity(0.2))
                                        .text_color(theme.success)
                                        .text_xs()
                                        .child("node OK"),
                                );
                            }
                            Some(false) => {
                                header = header.child(
                                    div()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded_md()
                                        .bg(theme.danger.opacity(0.2))
                                        .text_color(theme.danger)
                                        .text_xs()
                                        .child("node mismatch"),
                                );
                            }
                            None => {}
                        }

                        // "latest" line: `[name@version]`, per the card spec.
                        let latest = h_flex()
                            .gap_1()
                            .items_center()
                            .w_full()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("latest"),
                            )
                            .child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(theme.border.opacity(0.5))
                                    .text_color(theme.foreground)
                                    .text_xs()
                                    .child(format!("{name}@{version}")),
                            );

                        // The install/update/uninstall actions on the right
                        // edge: Install when the package isn't installed, else
                        // Update (only when the project reports one) plus
                        // Uninstall.
                        let installed_version = installed
                            .iter()
                            .find(|p| p.name == name)
                            .map(|p| p.version.clone());
                        let update_to = outdated
                            .iter()
                            .find(|p| p.name == name)
                            .map(|p| p.latest.clone());

                        let mut actions = h_flex().gap_2().items_center();
                        if installed_version.is_some() {
                            if let Some(latest) = update_to {
                                actions = actions.child(
                                    Button::new(format!("npm-update-{name}"))
                                        .label("Update")
                                        .with_size(Size::Small)
                                        .on_click({
                                            let view = view.clone();
                                            let name = name.clone();
                                            move |_, window, cx| {
                                                view.update(cx, |panel, cx| {
                                                    panel.update_pkg(&name, &latest, window, cx);
                                                });
                                            }
                                        }),
                                );
                            }
                            actions = actions.child(
                                Button::new(format!("npm-uninstall-{name}"))
                                    .label("Uninstall")
                                    .with_size(Size::Small)
                                    .on_click({
                                        let view = view.clone();
                                        let name = name.clone();
                                        move |_, window, cx| {
                                            view.update(cx, |panel, cx| {
                                                panel.remove_package(&name, window, cx);
                                            });
                                        }
                                    }),
                            );
                        } else {
                            actions = actions.child(
                                Button::new(format!("npm-install-{name}"))
                                    .label("Install")
                                    .with_size(Size::Small)
                                    .on_click({
                                        let view = view.clone();
                                        let name = name.clone();
                                        let version = version.clone();
                                        move |_, window, cx| {
                                            view.update(cx, |panel, cx| {
                                                panel.install_pkg(
                                                    &name,
                                                    &version,
                                                    panel.install_as_dev,
                                                    window,
                                                    cx,
                                                );
                                            });
                                        }
                                    }),
                            );
                        }

                        let card = v_flex()
                            .id(("npm-search-card", ix))
                            .gap_1()
                            .w_full()
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(if is_selected {
                                theme.primary
                            } else {
                                theme.border.opacity(0.6)
                            })
                            .when(is_selected, |card| card.bg(theme.primary.opacity(0.05)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected = Some(ix);
                                cx.notify();
                            }))
                            .child(header)
                            .child(latest)
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .w_full()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(description),
                                    )
                                    .child(actions),
                            )
                            .child(
                                h_flex().w_full().child(
                                    Button::new(format!("npm-more-info-{name}"))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Info)
                                        .label("More info")
                                        .on_click({
                                            let view = view.clone();
                                            let name = name.clone();
                                            move |_, _window, cx| {
                                                view.update(cx, |panel, cx| {
                                                    panel
                                                        .fetch_details_and_readme(name.clone(), cx);
                                                });
                                            }
                                        }),
                                ),
                            );
                        list = list.child(card);
                    }
                    list = list.when(results.len() < search_total, |list| {
                        list.child(
                            div().w_full().child(
                                Button::new("npm-search-more")
                                    .ghost()
                                    .label("Load more")
                                    .with_size(Size::Small)
                                    .on_click({
                                        let view = view.clone();
                                        move |_, _window, cx| {
                                            view.update(cx, |panel, cx| {
                                                panel.search_page += 1;
                                                panel.search_npm(cx);
                                            });
                                        }
                                    }),
                            ),
                        )
                    });
                    list.into_any_element()
                };
                content
            })
            .into_any_element()
    }
}

fn build_search_page(pages: &PageViews) -> SettingPage {
    SettingPage::new("Search")
        .resettable(false)
        .icon(Icon::new(IconName::Search))
        .description("Find packages on the npm registry and install them.")
        .group(
            SettingGroup::new()
                .title("npmjs.com")
                .item(embed_view(pages.search.clone())),
        )
}

// ── Installed ──────────────────────────────────────────────────────────────

pub(super) struct InstalledPage {
    panel: WeakEntity<NpmManagerPanel>,
    focus_handle: FocusHandle,
    /// Index into `project.installed` — the row `up`/`down`/`enter`/`space`
    /// act on. `None` until the list first gets focus (a click, or Tab).
    selected: Option<usize>,
}

impl InstalledPage {
    fn new(panel: WeakEntity<NpmManagerPanel>, cx: &mut Context<Self>) -> Self {
        Self {
            panel,
            focus_handle: cx.focus_handle(),
            selected: None,
        }
    }

    fn select_next(&mut self, _: &SelectNextPackage, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let len = view.read(cx).active().installed.len();
        self.selected = step_selected(self.selected, len, true);
        cx.notify();
    }

    fn select_prev(&mut self, _: &SelectPrevPackage, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let len = view.read(cx).active().installed.len();
        self.selected = step_selected(self.selected, len, false);
        cx.notify();
    }

    fn open_selected(
        &mut self,
        _: &OpenSelectedPackage,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let Some(name) = self
            .selected
            .and_then(|ix| view.read(cx).active().installed.get(ix))
            .map(|pkg| pkg.name.clone())
        else {
            return;
        };
        view.update(cx, |panel, cx| panel.fetch_details(name, cx));
    }

    /// Space's action on this page: Remove (confirmed — see
    /// `remove_package`'s own doc comment).
    fn act_on_selected(
        &mut self,
        _: &ActSelectedPackage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let Some(name) = self
            .selected
            .and_then(|ix| view.read(cx).active().installed.get(ix))
            .map(|pkg| pkg.name.clone())
        else {
            return;
        };
        view.update(cx, |panel, cx| panel.remove_package(&name, window, cx));
    }
}

impl Render for InstalledPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.panel.upgrade() else {
            return div().into_any_element();
        };
        let panel = view.read(cx);
        let theme = cx.theme();
        let project = panel.active();
        let installed = &project.installed;
        let installed_loading = project.installed_loading;

        if installed_loading {
            // Rows appear once `npm ls` returns; the sidebar badge spins meanwhile.
            return div().into_any_element();
        }

        let selected = self.selected;
        let mut list = v_flex()
            .id("npm-installed-list")
            .track_focus(&self.focus_handle)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle, cx);
                }),
            )
            .key_context(PACKAGE_LIST_CONTEXT)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::act_on_selected))
            .gap_1p5();
        if installed.is_empty() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("No packages installed — run `npm install` first."),
            );
        } else {
            for (ix, pkg) in installed.iter().enumerate() {
                let name = pkg.name.clone();
                let version = pkg.version.clone();
                let is_dev = pkg.is_dev;
                let is_selected = selected == Some(ix);

                // Card header: the package name (clickable → details), then
                // the dev badge in the same slot as the search cards' compat
                // badge.
                let mut header = h_flex().gap_2().items_center().w_full();
                header = header.child(
                    div()
                        .id(format!("npm-installed-{name}"))
                        .text_sm()
                        .font_semibold()
                        .text_color(theme.foreground)
                        .child(name.clone())
                        .cursor_pointer()
                        .on_click({
                            let view = view.clone();
                            let name = name.clone();
                            move |_, _window, cx| {
                                view.update(cx, |panel, cx| {
                                    panel.fetch_details(name.clone(), cx);
                                });
                            }
                        }),
                );
                header = header.when(is_dev, |header| {
                    header.child(
                        div()
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .bg(theme.warning.opacity(0.2))
                            .text_color(theme.warning)
                            .text_xs()
                            .child("dev"),
                    )
                });

                // "installed" line: `[name@version]`, per the card spec.
                let installed_line = h_flex()
                    .gap_1()
                    .items_center()
                    .w_full()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("installed"),
                    )
                    .child(
                        div()
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .bg(theme.border.opacity(0.5))
                            .text_color(theme.foreground)
                            .text_xs()
                            .child(format!("{name}@{version}")),
                    );

                let card = v_flex()
                    .id(("npm-installed-card", ix))
                    .gap_1()
                    .w_full()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if is_selected {
                        theme.primary
                    } else {
                        theme.border.opacity(0.6)
                    })
                    .when(is_selected, |card| card.bg(theme.primary.opacity(0.05)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = Some(ix);
                        cx.notify();
                    }))
                    .child(header)
                    .child(installed_line)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .w_full()
                            .child(div().flex_1().min_w_0())
                            .child(
                                Button::new(format!("npm-remove-{name}"))
                                    .danger()
                                    .label("Remove")
                                    .with_size(Size::Small)
                                    .on_click({
                                        let view = view.clone();
                                        let name = name.clone();
                                        move |_, window, cx| {
                                            view.update(cx, |panel, cx| {
                                                panel.remove_package(&name, window, cx);
                                            });
                                        }
                                    }),
                            ),
                    )
                    .child(
                        h_flex().w_full().child(
                            Button::new(format!("npm-more-info-{name}"))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Info)
                                .label("More info")
                                .on_click({
                                    let view = view.clone();
                                    let name = name.clone();
                                    move |_, _window, cx| {
                                        view.update(cx, |panel, cx| {
                                            panel.fetch_details(name.clone(), cx);
                                        });
                                    }
                                }),
                        ),
                    );
                list = list.child(card);
            }
        }
        list.into_any_element()
    }
}

fn build_installed_page(project: &crate::NpmProject, pages: &PageViews) -> SettingPage {
    let count = project.installed.len();
    let loading = project.installed_loading;
    SettingPage::new("Installed")
        .resettable(false)
        .icon(Icon::new(IconName::Inbox))
        .description("Packages installed in this workspace.")
        .group(
            SettingGroup::new()
                .title("Installed packages")
                .item(embed_view(pages.installed.clone())),
        )
        .sidebar_badge(move |_, cx| count_badge(count, loading, cx))
}

// ── Updates ────────────────────────────────────────────────────────────────

pub(super) struct UpdatesPage {
    panel: WeakEntity<NpmManagerPanel>,
    focus_handle: FocusHandle,
    /// Index into `project.outdated` — see `InstalledPage::selected`.
    selected: Option<usize>,
}

impl UpdatesPage {
    fn new(panel: WeakEntity<NpmManagerPanel>, cx: &mut Context<Self>) -> Self {
        Self {
            panel,
            focus_handle: cx.focus_handle(),
            selected: None,
        }
    }

    fn select_next(&mut self, _: &SelectNextPackage, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let len = view.read(cx).active().outdated.len();
        self.selected = step_selected(self.selected, len, true);
        cx.notify();
    }

    fn select_prev(&mut self, _: &SelectPrevPackage, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let len = view.read(cx).active().outdated.len();
        self.selected = step_selected(self.selected, len, false);
        cx.notify();
    }

    fn open_selected(
        &mut self,
        _: &OpenSelectedPackage,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let Some(name) = self
            .selected
            .and_then(|ix| view.read(cx).active().outdated.get(ix))
            .map(|pkg| pkg.name.clone())
        else {
            return;
        };
        view.update(cx, |panel, cx| panel.fetch_details(name, cx));
    }

    /// Space's action on this page: Update to latest — not destructive (a
    /// version bump), so unlike Remove/Uninstall this needs no confirmation,
    /// matching the existing Update button.
    fn act_on_selected(
        &mut self,
        _: &ActSelectedPackage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let Some((name, latest)) = self
            .selected
            .and_then(|ix| view.read(cx).active().outdated.get(ix))
            .map(|pkg| (pkg.name.clone(), pkg.latest.clone()))
        else {
            return;
        };
        view.update(cx, |panel, cx| panel.update_pkg(&name, &latest, window, cx));
    }
}

impl Render for UpdatesPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.panel.upgrade() else {
            return div().into_any_element();
        };
        let panel = view.read(cx);
        let theme = cx.theme();
        let project = panel.active();
        let outdated = &project.outdated;
        let outdated_loading = project.outdated_loading;

        if outdated_loading {
            return div().into_any_element();
        }

        let selected = self.selected;
        let mut list = v_flex()
            .id("npm-updates-list")
            .track_focus(&self.focus_handle)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle, cx);
                }),
            )
            .key_context(PACKAGE_LIST_CONTEXT)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::act_on_selected))
            .gap_1p5();
        if outdated.is_empty() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("All packages are up to date."),
            );
        } else {
            list = list.child(
                h_flex().gap_1().children(
                    [
                        ("npm-update-all-safe", "Update All Safe", "safe"),
                        ("npm-update-all-patch", "Update All Patch", "patch"),
                    ]
                    .map(|(id, label, ty)| {
                        let view = view.clone();
                        Button::new(id)
                            .label(label)
                            .with_size(Size::Small)
                            .on_click(move |_, window, cx| {
                                view.update(cx, |panel, cx| {
                                    panel.update_all(ty, window, cx);
                                });
                            })
                    }),
                ),
            );
            for (ix, pkg) in outdated.iter().enumerate() {
                let name = pkg.name.clone();
                let current = pkg.current.clone();
                let latest = pkg.latest.clone();
                let is_selected = selected == Some(ix);
                let kind = classify_update(&pkg.current, &pkg.latest);
                // Red is reserved for the Vulnerabilities page — "major"
                // here doesn't mean "broken", just "bigger diff to
                // review", matching `dotnet_panel`/`node_panel`'s corrected
                // scheme (and this page's own Vulnerabilities badge below).
                let (kind_color, kind_label) = match kind {
                    UpdateKind::Major => (theme.warning, "major"),
                    UpdateKind::Minor => (theme.info, "minor"),
                    UpdateKind::Patch => (theme.muted_foreground, "patch"),
                    UpdateKind::Current => (theme.muted_foreground, "same"),
                };

                // Card header: the package name (clickable → details), then
                // the update-kind badge in the same slot as the search cards'
                // compat badge.
                let mut header = h_flex().gap_2().items_center().w_full();
                header = header.child(
                    div()
                        .id(format!("npm-outdated-{name}"))
                        .text_sm()
                        .font_semibold()
                        .text_color(theme.foreground)
                        .child(name.clone())
                        .cursor_pointer()
                        .on_click({
                            let view = view.clone();
                            let name = name.clone();
                            move |_, _window, cx| {
                                view.update(cx, |panel, cx| {
                                    panel.fetch_details(name.clone(), cx);
                                });
                            }
                        }),
                );
                header = header.child(
                    div()
                        .px_1p5()
                        .py_0p5()
                        .rounded_md()
                        .bg(kind_color.opacity(0.2))
                        .text_color(kind_color)
                        .text_xs()
                        .child(kind_label),
                );

                // "update" line: `[current → latest]`, per the card spec.
                let update_line = h_flex()
                    .gap_1()
                    .items_center()
                    .w_full()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("update"),
                    )
                    .child(
                        div()
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .bg(theme.border.opacity(0.5))
                            .text_color(theme.foreground)
                            .text_xs()
                            .child(format!("{current} → {latest}")),
                    );

                let card = v_flex()
                    .id(("npm-outdated-card", ix))
                    .gap_1()
                    .w_full()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if is_selected {
                        theme.primary
                    } else {
                        theme.border.opacity(0.6)
                    })
                    .when(is_selected, |card| card.bg(theme.primary.opacity(0.05)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = Some(ix);
                        cx.notify();
                    }))
                    .child(header)
                    .child(update_line)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .w_full()
                            .child(div().flex_1().min_w_0())
                            .child(
                                Button::new(format!("npm-update-{name}"))
                                    .label("Update")
                                    .with_size(Size::Small)
                                    .on_click({
                                        let view = view.clone();
                                        let name = name.clone();
                                        let latest = latest.clone();
                                        move |_, window, cx| {
                                            view.update(cx, |panel, cx| {
                                                panel.update_pkg(&name, &latest, window, cx);
                                            });
                                        }
                                    }),
                            ),
                    )
                    .child(
                        h_flex().w_full().child(
                            Button::new(format!("npm-more-info-{name}"))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Info)
                                .label("More info")
                                .on_click({
                                    let view = view.clone();
                                    let name = name.clone();
                                    move |_, _window, cx| {
                                        view.update(cx, |panel, cx| {
                                            panel.fetch_details(name.clone(), cx);
                                        });
                                    }
                                }),
                        ),
                    );
                list = list.child(card);
            }
        }
        list.into_any_element()
    }
}

fn build_updates_page(project: &crate::NpmProject, pages: &PageViews) -> SettingPage {
    let count = project.outdated.len();
    let loading = project.outdated_loading;
    SettingPage::new("Updates")
        .resettable(false)
        .icon(Icon::new(IconName::ArrowUp))
        .description("Packages behind their latest version.")
        .group(
            SettingGroup::new()
                .title("Outdated packages")
                .item(embed_view(pages.updates.clone())),
        )
        .sidebar_badge(move |_, cx| count_badge(count, loading, cx))
}

// ── Vulnerabilities ────────────────────────────────────────────────────────

/// The command a plain fix runs: everything `npm audit` can fix without a
/// breaking upgrade, not only one advisory.
const AUDIT_FIX_ARGS: [&str; 2] = ["audit", "fix"];
/// What a breaking fix would take. It is only offered to copy, never run here.
const AUDIT_FIX_FORCE: &str = "npm audit fix --force";

pub(super) struct VulnPage {
    panel: WeakEntity<NpmManagerPanel>,
    focus_handle: FocusHandle,
    /// Index into the cards on show: the production ones, then the dev ones
    /// when those are expanded.
    selected: Option<usize>,
    /// Whether the dev advisories are shown. The page opens on production only.
    show_dev: bool,
    /// One table of affected packages per advisory with any, built once and kept
    /// between frames (`prod:<id>` / `dev:<id>`).
    tables: HashMap<String, Entity<TableState<AffectedDelegate>>>,
    /// What `tables` was built for: each advisory's key and how many it reaches.
    tables_for: Vec<(String, usize)>,
    /// GitHub advisory ids whose written text is expanded in its card.
    text_open: HashSet<String>,
    /// The written advisories fetched so far, by GitHub id. Kept for the session,
    /// so reading one again costs no request.
    texts: HashMap<String, AdvisoryState>,
}

/// The advisories on show, in order, each with whether it is a dev one.
fn shown(report: &AuditReport, show_dev: bool) -> Vec<(&AuditAdvisory, bool)> {
    let production = report.production.iter().map(|a| (a, false));
    let dev = report.dev.iter().filter(|_| show_dev).map(|a| (a, true));
    production.chain(dev).collect()
}

fn table_key(advisory: &AuditAdvisory, dev: bool) -> String {
    format!("{}:{}", if dev { "dev" } else { "prod" }, advisory.id)
}

impl VulnPage {
    fn new(panel: WeakEntity<NpmManagerPanel>, cx: &mut Context<Self>) -> Self {
        Self {
            panel,
            focus_handle: cx.focus_handle(),
            selected: None,
            show_dev: false,
            tables: HashMap::new(),
            tables_for: Vec::new(),
            text_open: HashSet::new(),
            texts: HashMap::new(),
        }
    }

    /// Opens or closes an advisory's written text, fetching it the first time
    /// (and again after a failure).
    fn toggle_text(&mut self, ghsa: String, cx: &mut Context<Self>) {
        if self.text_open.remove(&ghsa) {
            cx.notify();
            return;
        }
        self.text_open.insert(ghsa.clone());
        let have = matches!(
            self.texts.get(&ghsa),
            Some(AdvisoryState::Ready(_) | AdvisoryState::Loading)
        );
        if !have {
            self.texts.insert(ghsa.clone(), AdvisoryState::Loading);
            let http_client = cx.http_client();
            let url = format!("https://api.github.com/advisories/{ghsa}");
            let this = cx.weak_entity();
            cx.spawn(async move |_: WeakEntity<Self>, cx: &mut AsyncApp| {
                let outcome = fetch_json(&http_client, &url)
                    .await
                    .and_then(|json| parse_github_advisory(&json))
                    .map(|advisory| advisory_markdown(&advisory));
                let _ = cx.update(|app| {
                    if let Some(page) = this.upgrade() {
                        let _ = page.update(app, |page, cx| {
                            let state = match outcome {
                                Ok(markdown) => AdvisoryState::Ready(markdown),
                                Err(error) => AdvisoryState::Failed(friendly_fetch_error(&error)),
                            };
                            page.texts.insert(ghsa, state);
                            cx.notify();
                        });
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn shown_count(&self, cx: &App) -> usize {
        let Some(view) = self.panel.upgrade() else {
            return 0;
        };
        shown(&view.read(cx).active().audit, self.show_dev).len()
    }

    fn select_next(&mut self, _: &SelectNextPackage, _window: &mut Window, cx: &mut Context<Self>) {
        let len = self.shown_count(cx);
        self.selected = step_selected(self.selected, len, true);
        cx.notify();
    }

    fn select_prev(&mut self, _: &SelectPrevPackage, _window: &mut Window, cx: &mut Context<Self>) {
        let len = self.shown_count(cx);
        self.selected = step_selected(self.selected, len, false);
        cx.notify();
    }

    /// Enter: the details of the selected advisory's package.
    fn open_selected(
        &mut self,
        _: &OpenSelectedPackage,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let Some(package) = self.selected.and_then(|ix| {
            shown(&view.read(cx).active().audit, self.show_dev)
                .get(ix)
                .map(|(advisory, _)| advisory.package.clone())
        }) else {
            return;
        };
        view.update(cx, |panel, cx| panel.fetch_details(package, cx));
    }

    /// Space: run the fix, when the selected advisory has one `npm audit fix`
    /// can apply; a no-op otherwise rather than something the card did not offer.
    fn act_on_selected(
        &mut self,
        _: &ActSelectedPackage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        let plain = self.selected.is_some_and(|ix| {
            shown(&view.read(cx).active().audit, self.show_dev)
                .get(ix)
                .is_some_and(|(advisory, _)| is_plain_fix(&advisory.fix))
        });
        if plain {
            self.run_audit_fix(window, cx);
        }
    }

    fn run_audit_fix(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.panel.upgrade() else {
            return;
        };
        view.update(cx, |panel, cx| {
            let args = AUDIT_FIX_ARGS.iter().map(|a| a.to_string()).collect();
            panel.run_checked("audit fix", Ok(args), window, cx);
        });
    }

    /// Builds the table of every advisory's affected packages again, when the
    /// advisories are not the ones the tables were made for.
    fn refresh_tables(
        &mut self,
        report: &AuditReport,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let all = || {
            report
                .production
                .iter()
                .map(|a| (a, false))
                .chain(report.dev.iter().map(|a| (a, true)))
        };
        let signature: Vec<(String, usize)> = all()
            .map(|(a, dev)| (table_key(a, dev), a.affected.len()))
            .collect();
        if signature == self.tables_for {
            return;
        }
        self.tables.clear();
        for (advisory, dev) in all() {
            if advisory.affected.is_empty() {
                continue;
            }
            let delegate = AffectedDelegate::new(advisory.affected.clone());
            let table = cx.new(|cx| TableState::new(delegate, window, cx));
            self.tables.insert(table_key(advisory, dev), table);
        }
        self.tables_for = signature;
    }

    /// One advisory: who it is about, what it is, what it reaches, and what fixes it.
    fn render_card(
        &self,
        ix: usize,
        advisory: &AuditAdvisory,
        dev: bool,
        view: &Entity<NpmManagerPanel>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (border, foreground, muted, primary, success, warning, danger) = (
            theme.border,
            theme.foreground,
            theme.muted_foreground,
            theme.primary,
            theme.success,
            theme.warning,
            theme.danger,
        );
        // The advisory's GitHub id, when its link is one; only those can be read here.
        let ghsa = ghsa_id(&advisory.url);
        let text_open = ghsa.as_ref().is_some_and(|id| self.text_open.contains(id));
        let tint = severity_color(theme, &advisory.severity);
        let is_selected = self.selected == Some(ix);
        let package = advisory.package.clone();

        let header = h_flex()
            .gap_2()
            .items_center()
            .w_full()
            .child(
                div()
                    .px_1p5()
                    .py_0p5()
                    .rounded_md()
                    .bg(tint.opacity(0.2))
                    .text_color(tint)
                    .text_xs()
                    .child(advisory.severity.clone()),
            )
            .child(
                div()
                    .id(("npm-vuln-package", ix))
                    .flex_none()
                    .cursor_pointer()
                    .font_family("Cascadia Mono")
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_sm()
                    .text_color(foreground)
                    .child(package.clone())
                    .on_click({
                        let view = view.clone();
                        let package = package.clone();
                        move |_, _window, cx| {
                            view.update(cx, |panel, cx| {
                                panel.fetch_details(package.clone(), cx);
                            });
                        }
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .child(via_label(advisory).unwrap_or_default()),
            )
            .child(Tag::secondary().xsmall().child(if is_direct(advisory) {
                "direct"
            } else {
                "indirect"
            }))
            .when(dev, |row| row.child(Tag::secondary().xsmall().child("dev")));

        let facts = h_flex()
            .gap_3()
            .items_center()
            .flex_wrap()
            .text_xs()
            .text_color(muted)
            .when(!advisory.range.is_empty(), |row| {
                row.child(format!("Vulnerable: {}", advisory.range))
            })
            .when(!advisory.cwe.is_empty(), |row| {
                row.child(advisory.cwe.join(", "))
            })
            .when(!advisory.url.is_empty(), |row| {
                let url = advisory.url.clone();
                match ghsa.clone() {
                    // Read in the card, without a browser.
                    Some(ghsa) => row.child(
                        Button::new(("npm-vuln-read", ix))
                            .ghost()
                            .xsmall()
                            .label(if text_open {
                                "Hide advisory"
                            } else {
                                "Read advisory"
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_text(ghsa.clone(), cx);
                            })),
                    ),
                    // Not a GitHub advisory: only its page can be opened.
                    None => row.child(
                        Button::new(("npm-vuln-link", ix))
                            .ghost()
                            .xsmall()
                            .label("View advisory")
                            .on_click(move |_, _window, cx| cx.open_url(&url)),
                    ),
                }
            });

        // The written advisory, once asked for.
        let written = ghsa.as_ref().filter(|_| text_open).map(|ghsa| {
            let url = advisory.url.clone();
            let body = match self.texts.get(ghsa) {
                Some(AdvisoryState::Ready(text)) => div()
                    .id(("npm-vuln-text", ix))
                    .w_full()
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .child(
                        // An id of its own: the plain `markdown()` helper takes
                        // its id from the call site, which every card shares.
                        TextView::markdown(format!("npm-vuln-md-{ghsa}"), text.clone())
                            .selectable(true),
                    )
                    .into_any_element(),
                Some(AdvisoryState::Failed(error)) => v_flex()
                    .gap_1()
                    .child(div().text_xs().text_color(danger).child(error.clone()))
                    .child(
                        Button::new(("npm-vuln-browser", ix))
                            .ghost()
                            .xsmall()
                            .label("Open in browser")
                            .on_click(move |_, _window, cx| cx.open_url(&url)),
                    )
                    .into_any_element(),
                _ => h_flex()
                    .gap_2()
                    .items_center()
                    .child(Spinner::new().xsmall())
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("Loading the advisory\u{2026}"),
                    )
                    .into_any_element(),
            };
            div()
                .w_full()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(border)
                .child(body)
        });

        let affected = self
            .tables
            .get(&table_key(advisory, dev))
            .filter(|_| !advisory.affected.is_empty())
            .map(|table| {
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("Also affects ({})", advisory.affected.len())),
                    )
                    .child(
                        div()
                            .w_full()
                            .h(px(table_height(advisory.affected.len())))
                            .child(
                                DataTable::new(table)
                                    .small()
                                    .stripe(true)
                                    .bordered(true)
                                    .scrollbar_visible(true, false),
                            ),
                    )
            });

        let fix_color = match &advisory.fix {
            AuditFix::Available(_) => success,
            AuditFix::Breaking(_) => warning,
            AuditFix::None => muted,
        };
        let footer = h_flex()
            .gap_2()
            .items_center()
            .w_full()
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(fix_color)
                    .child(fix_label(&advisory.fix)),
            )
            .when(matches!(advisory.fix, AuditFix::Breaking(_)), |row| {
                row.child(
                    Button::new(("npm-vuln-copy-force", ix))
                        .ghost()
                        .xsmall()
                        .label("Copy the forced command")
                        .tooltip(format!(
                            "{AUDIT_FIX_FORCE}: it can break the build, so it is not run from here"
                        ))
                        .on_click(|_, _window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                AUDIT_FIX_FORCE.to_string(),
                            ));
                        }),
                )
            });

        v_flex()
            .id(("npm-vuln-card", ix))
            .w_full()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(if is_selected { primary } else { border })
            .when(is_selected, |card| card.bg(primary.opacity(0.08)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = Some(ix);
                cx.notify();
            }))
            .child(header)
            .when(!advisory.title.is_empty(), |card| {
                card.child(
                    div()
                        .text_sm()
                        .text_color(foreground)
                        .child(advisory.title.clone()),
                )
            })
            .child(facts)
            .children(written)
            .children(affected)
            .child(footer)
            .into_any_element()
    }
}

impl Render for VulnPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.panel.upgrade() else {
            return div().into_any_element();
        };
        let (report, loading, error, npm) = {
            let panel = view.read(cx);
            let project = panel.active();
            (
                project.audit.clone(),
                project.audit_loading,
                project.audit_error.clone(),
                project.package_manager.cli_name() == "npm",
            )
        };
        if loading {
            return div().into_any_element();
        }
        self.refresh_tables(&report, window, cx);

        let (danger, success, muted, foreground) = {
            let theme = cx.theme();
            (
                theme.danger,
                theme.success,
                theme.muted_foreground,
                theme.foreground,
            )
        };
        let mut page = v_flex()
            .id("npm-vuln-list")
            .track_focus(&self.focus_handle)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle, cx);
                }),
            )
            .key_context(PACKAGE_LIST_CONTEXT)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::act_on_selected))
            .gap_3();

        // A scan that failed says so; it never reads as a clean project.
        if let Some(error) = error {
            page = page.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .w_full()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(danger)
                            .child(format!("The audit failed: {error}")),
                    )
                    .child(
                        Button::new("npm-vuln-retry")
                            .secondary()
                            .xsmall()
                            .label("Retry")
                            .on_click({
                                let view = view.clone();
                                move |_, _window, cx| {
                                    view.update(cx, |panel, cx| panel.reload_visible(cx));
                                }
                            }),
                    ),
            );
            return page.into_any_element();
        }

        let none_at_all =
            report.production.is_empty() && report.dev.is_empty() && report.dev_error.is_none();
        if none_at_all {
            return page
                .child(
                    div()
                        .text_xs()
                        .text_color(success)
                        .child("No known vulnerabilities for this package set."),
                )
                .into_any_element();
        }

        // One button for every fix `npm audit fix` can make without a breaking change.
        let fixable = report
            .production
            .iter()
            .chain(report.dev.iter())
            .filter(|a| is_plain_fix(&a.fix))
            .count();
        if npm && fixable > 0 {
            page = page.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .w_full()
                    .child(div().flex_1().text_xs().text_color(muted).child(format!(
                        "{fixable} can be fixed without breaking changes. This runs \
                                 `npm audit fix` for the whole project."
                    )))
                    .child(
                        Button::new("npm-vuln-fix-all")
                            .primary()
                            .small()
                            .label("Run npm audit fix")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.run_audit_fix(window, cx);
                            })),
                    ),
            );
        }

        let heading = |title: String| {
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(foreground)
                .child(title)
        };

        // Production: what ships.
        page = page.child(heading(format!("Production ({})", report.production.len())));
        if report.production.is_empty() {
            page = page.child(
                div()
                    .text_xs()
                    .text_color(success)
                    .child("None in what ships."),
            );
        }
        let mut ix = 0;
        for advisory in &report.production {
            page = page.child(self.render_card(ix, advisory, false, &view, cx));
            ix += 1;
        }

        // Dev dependencies: collapsed until asked for.
        if !report.dev.is_empty() || report.dev_error.is_some() {
            let label = if self.show_dev {
                format!("Hide dev dependencies ({})", report.dev.len())
            } else {
                format!("Show dev dependencies ({})", report.dev.len())
            };
            page = page.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .w_full()
                    .child(
                        Button::new("npm-vuln-toggle-dev")
                            .secondary()
                            .small()
                            .label(label)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_dev = !this.show_dev;
                                this.selected = None;
                                cx.notify();
                            })),
                    )
                    .when_some(report.dev_error.clone(), |row, error| {
                        row.child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(danger)
                                .child(format!("The dev scan failed: {error}")),
                        )
                    }),
            );
        }
        if self.show_dev {
            for advisory in &report.dev {
                page = page.child(self.render_card(ix, advisory, true, &view, cx));
                ix += 1;
            }
        }
        page.into_any_element()
    }
}

fn build_vuln_page(project: &crate::NpmProject, pages: &PageViews) -> SettingPage {
    let count = project.audit.production.len() + project.audit.dev.len();
    let production = !project.audit.production.is_empty();
    let loading = project.audit_loading;
    SettingPage::new("Vulnerabilities")
        .resettable(false)
        .icon(Icon::new(IconName::TriangleAlert))
        .description(
            "Findings from `npm audit`: what ships first, then dev dependencies. Click a \
             package for its details.",
        )
        .group(
            SettingGroup::new()
                .title("npm audit advisories")
                .item(embed_view(pages.vuln.clone())),
        )
        .sidebar_badge(move |_, cx| {
            // Red when something ships; grey when the findings are only in dev tooling.
            let color = if production {
                cx.theme().danger
            } else {
                cx.theme().muted_foreground
            };
            colored_count_badge(count, loading, color)
        })
}

// ── Shared ─────────────────────────────────────────────────────────────────

fn count_badge(count: usize, loading: bool, cx: &App) -> AnyElement {
    colored_count_badge(count, loading, cx.theme().primary)
}

/// Like [`count_badge`], but with the pill's fill color set by the caller —
/// used for the Vulnerabilities badge, which needs the error color rather
/// than the primary-theme color the Installed/Updates badges use.
fn colored_count_badge(count: usize, loading: bool, color: Hsla) -> AnyElement {
    h_flex()
        .child(
            div()
                .when(count == 0 && !loading, |this| this.size_0())
                .when(count > 0, |this| {
                    this.flex()
                        .bg(color)
                        .rounded_full()
                        .px_1p5()
                        .min_w_3p5()
                        .h_4()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(badge_text(color))
                        .child(count.to_string())
                })
                .when(count == 0 && loading, |this| {
                    this.child(Spinner::new().with_size(Size::XSmall))
                }),
        )
        .into_any_element()
}

/// The text colour for a count badge filled with `fill`: black on a light
/// fill, white on a dark one.
///
/// The badge used to be white on whatever the theme's accent was, which is
/// unreadable in themes with a light accent (white on lilac, say). Choosing
/// by the fill's brightness keeps the number legible in any theme without
/// relying on the theme to define a matching foreground.
fn badge_text(fill: Hsla) -> Hsla {
    let rgb = fill.to_rgb();
    // How bright the colour looks, green counting most and blue least.
    let brightness = 0.2126 * rgb.r + 0.7152 * rgb.g + 0.0722 * rgb.b;
    if brightness > 0.55 {
        gpui::black()
    } else {
        gpui::white()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_selected_starts_at_the_nearest_end() {
        assert_eq!(step_selected(None, 3, true), Some(0));
        assert_eq!(step_selected(None, 3, false), Some(2));
    }

    #[test]
    fn step_selected_wraps_at_both_ends() {
        assert_eq!(step_selected(Some(0), 3, true), Some(1));
        assert_eq!(step_selected(Some(2), 3, true), Some(0));
        assert_eq!(step_selected(Some(0), 3, false), Some(2));
        assert_eq!(step_selected(Some(1), 3, false), Some(0));
        assert_eq!(step_selected(Some(0), 1, true), Some(0));
        assert_eq!(step_selected(Some(0), 1, false), Some(0));
    }

    #[test]
    fn step_selected_handles_empty_and_shrunken_lists() {
        assert_eq!(step_selected(None, 0, true), None);
        assert_eq!(step_selected(Some(4), 0, false), None);
        // A selection left over from a longer list still lands in range.
        assert!(step_selected(Some(9), 3, true).is_some_and(|ix| ix < 3));
        assert!(step_selected(Some(9), 3, false).is_some_and(|ix| ix < 3));
    }

    #[test]
    fn badge_text_contrasts_with_its_fill() {
        // Light fills take dark text: a lilac accent, a yellow warning.
        assert_eq!(badge_text(gpui::rgb(0xc4b5fd).into()), gpui::black());
        assert_eq!(badge_text(gpui::rgb(0xeab308).into()), gpui::black());
        assert_eq!(badge_text(gpui::white()), gpui::black());
        // Dark fills take light text: a deep blue accent, a red danger.
        assert_eq!(badge_text(gpui::rgb(0x1d4ed8).into()), gpui::white());
        assert_eq!(badge_text(gpui::rgb(0xb91c1c).into()), gpui::white());
        assert_eq!(badge_text(gpui::black()), gpui::white());
    }
}

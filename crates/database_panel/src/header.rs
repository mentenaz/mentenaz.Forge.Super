//! The connection header strip for `DatabasePanel`: a status/identifier row
//! (connection title, db type tag, live status, Refresh / Connect / Disconnect
//! / Delete actions), an inline error row, and the discovered-database tab
//! strip with create/register actions.

use database_backend::{ConnectionConfig, ConnectionId, ConnectionStatus, DbType};
use gpui::{
    ClickEvent, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName as GIconName, Sizable as _, Size,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    popover::Popover,
    tab::{Tab, TabBar, TabVariant},
    tag::Tag,
    v_flex,
};
use ui::{Color, Label, LabelCommon as _, LabelSize};

use crate::DatabasePanel;

pub(crate) fn db_type_label(db_type: DbType) -> &'static str {
    match db_type {
        DbType::Sqlite => "SQLite",
        DbType::Postgres => "PostgreSQL",
        DbType::MySql => "MySQL / MariaDB",
        DbType::MsSql => "MSSQL",
        DbType::Oracle => "Oracle",
    }
}

fn connection_status_label(status: Option<&ConnectionStatus>) -> &'static str {
    match status {
        Some(ConnectionStatus::Connected) => "Connected",
        Some(ConnectionStatus::Connecting) => "Connecting…",
        Some(ConnectionStatus::Error(_)) => "Error",
        None | Some(ConnectionStatus::Disconnected) => "Disconnected",
    }
}

impl DatabasePanel {
    /// The header of the active connection's view: title/status on the left,
    /// connection actions on the right, the last connect/schema error (if
    /// any) below, and the discovered-database tab strip once databases are
    /// known. Mirrors `npm_manager_panel`'s header-row style rather than the
    /// old hand-rolled action bar.
    pub(crate) fn render_connection_header(
        &self,
        connection: &ConnectionConfig,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = connection.id;
        let is_connected = self.registry.is_connected(id);
        let busy = self.registry_busy;
        let (indicator, indicator_color) = self.status_indicator(id);
        let status = self.statuses.get(&id);
        // `is_connected` (the registry — a real live session) wins over
        // `status` whenever they'd disagree, for the same reason
        // `status_indicator` prefers it: `status` is written by async flows
        // (discovery, connect) that don't always know about each other, so
        // it can lag or go stale while a session is still genuinely live.
        let (status_label, error_message) = if is_connected {
            ("Connected", None)
        } else {
            match status {
                Some(ConnectionStatus::Error(message)) => ("Error", Some(message.clone())),
                _ => (connection_status_label(status), None),
            }
        };

        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(("database-connection-header", id.0))
                    .w_full()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Label::new(indicator)
                                    .color(indicator_color)
                                    .weight(FontWeight::BOLD),
                            )
                            .child(
                                Label::new(connection.title.clone())
                                    .size(LabelSize::Default)
                                    .weight(FontWeight::BOLD),
                            )
                            .child(
                                Tag::secondary()
                                    .outline()
                                    .with_size(Size::Small)
                                    .child(db_type_label(connection.db_type)),
                            )
                            .child(Label::new(status_label).size(LabelSize::XSmall).color(
                                if status_label == "Error" {
                                    Color::Error
                                } else {
                                    Color::Muted
                                },
                            ))
                            .when(busy, |this| {
                                this.child(
                                    Tag::warning()
                                        .outline()
                                        .with_size(Size::Small)
                                        .child("Busy — waiting on another connection"),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .when(is_connected, |this| {
                                this.child(
                                    Button::new(("open-workbench", id.0))
                                        .outline()
                                        .disabled(busy)
                                        .icon(GIconName::SquareTerminal)
                                        .label("Workbench")
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, window, cx| {
                                                this.open_workbench_tab(id, window, cx);
                                            },
                                        )),
                                )
                            })
                            .when(is_connected, |this| {
                                this.child(
                                    Button::new(("open-schema-graph", id.0))
                                        .outline()
                                        .disabled(busy)
                                        .label("Schema Graph")
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, window, cx| {
                                                this.open_schema_graph_tab(id, window, cx);
                                            },
                                        )),
                                )
                            })
                            .when(is_connected, |this| {
                                this.child(
                                    Button::new(("refresh-schema", id.0))
                                        .outline()
                                        .disabled(busy)
                                        .label("Refresh")
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, _, cx| {
                                                this.fetch_schema(id, cx);
                                            },
                                        )),
                                )
                            })
                            .child(if is_connected {
                                Button::new(("disconnect", id.0))
                                    .outline()
                                    .disabled(busy)
                                    .label("Disconnect")
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.disconnect(id, cx);
                                    }))
                            } else {
                                Button::new(("connect", id.0))
                                    .primary()
                                    .disabled(busy)
                                    .label("Connect")
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.connect(id, cx);
                                    }))
                            })
                            .child(
                                Button::new(("delete", id.0))
                                    .outline()
                                    .danger()
                                    .disabled(busy)
                                    .icon(Icon::new(GIconName::Delete))
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.delete_connection(id, window, cx);
                                        },
                                    )),
                            ),
                    ),
            )
            .when_some(error_message, |this, message| {
                this.child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .child(
                            Icon::new(GIconName::TriangleAlert)
                                .with_size(Size::XSmall)
                                .text_color(cx.theme().danger_foreground),
                        )
                        .child(
                            Label::new(message)
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        ),
                )
            })
            .when(connection.db_type != DbType::Sqlite, |this| {
                this.child(self.render_database_tabs(connection, cx))
            })
    }

    /// The *connected*-database tab strip for a network connection: one
    /// `Tab` per database with a live session (`registry.live_databases`),
    /// the active one highlighted — not every database discovery found, so a
    /// tab you haven't opened yet never appears here. Below it: "+ Create
    /// database" / "+ Add database" (a not-yet-discovered but known-to-exist
    /// name) and, when discovery found databases you haven't opened yet, an
    /// "Open existing" picker to connect one without typing its exact name.
    fn render_database_tabs(
        &self,
        connection: &ConnectionConfig,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = connection.id;
        let is_creating = self.creating_database_for == Some(id);
        let is_opening = self.opening_database_for == Some(id);
        let busy = self.registry_busy;
        let input_border = cx.theme().border;
        // Every database to list: the connected ones, and those this
        // workspace had open last session that are not connected yet.
        let live = self.remembered_databases(id);
        let active = self.registry.active_database(id);
        let active_ix = active
            .as_ref()
            .and_then(|active| live.iter().position(|name| name == active));
        let muted = cx.theme().muted_foreground;

        let mut tab_bar = TabBar::new(("database-tabs", id.0))
            .with_variant(TabVariant::Underline)
            .px_3()
            .children(live.iter().enumerate().map(|(ix, name)| {
                let connected = self.live_keys.contains(&(id, name.clone()));
                Tab::new().label(name.clone()).disabled(busy).suffix(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .when(!connected, |row| {
                            row.child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("not connected"),
                            )
                        })
                        .child(
                            div()
                                .id(("close-database-tab", ix))
                                .ml_1()
                                .rounded(px(3.))
                                .p(px(1.))
                                .hover(|this| this.bg(cx.theme().secondary))
                                // A click on the "x" must never also select the tab
                                // it's sitting inside of (Tab's own `on_click` wraps
                                // this suffix too) — stop it at mouse-down, before
                                // the tab's click handler ever sees it.
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation()
                                })
                                .on_click({
                                    let name = name.clone();
                                    cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        cx.stop_propagation();
                                        if connected {
                                            this.disconnect_database(id, name.clone(), cx);
                                        } else {
                                            // Never connected this session: just stop
                                            // listing it.
                                            this.forget_parked_database(id, &name, cx);
                                        }
                                    })
                                })
                                .child(Icon::new(GIconName::Close).with_size(Size::XSmall)),
                        ),
                )
            }))
            .on_click(cx.listener({
                let live = live.clone();
                move |this, ix: &usize, _, cx| {
                    if let Some(name) = live.get(*ix) {
                        // A database that is not connected is connected by
                        // choosing it.
                        this.select_database(id, name.clone(), cx);
                    }
                }
            }));
        if let Some(ix) = active_ix {
            tab_bar = tab_bar.selected_index(ix);
        }

        // Databases discovery found on the server that don't already have a
        // live session — the "Open existing" picker's candidates.
        let discoverable: Vec<String> = connection
            .databases
            .iter()
            .map(|database| database.name.clone())
            .filter(|name| !live.contains(name))
            .collect();

        // Exactly one "page" renders below the tab bar at a time — Default
        // (the two entry-point buttons) or Creating (the create/register
        // form) — never both, and Cancel returns to Default rather than
        // stacking on top of it. "Open existing" isn't a page: it's a
        // `Popover` (below), an overlay that floats on top of whatever's
        // already showing instead of pushing it down the page.
        let page = if connection.db_type == DbType::Oracle {
            // One Oracle service is one database: nothing to create, and no
            // other databases to open from this connection.
            div().into_any_element()
        } else if is_creating {
            h_flex()
                .w_full()
                .px_3()
                .gap_2()
                .items_center()
                .child(
                    Input::new(&self.new_database_name)
                        .w_48()
                        .border_1()
                        .border_color(input_border),
                )
                .child(
                    Button::new(("create-database", id.0))
                        .outline()
                        .disabled(busy)
                        .label("Create")
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.submit_create_database(window, cx);
                        })),
                )
                .child(
                    Button::new(("add-database", id.0))
                        .outline()
                        .disabled(busy)
                        .label("Add")
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.submit_register_database(window, cx);
                        })),
                )
                .child(
                    Button::new(("cancel-database", id.0))
                        .ghost()
                        .label("Cancel")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.cancel_create_database(cx);
                        })),
                )
                .into_any_element()
        } else {
            h_flex()
                .w_full()
                .px_3()
                .gap_2()
                .items_center()
                .child(
                    Button::new(("start-create-database", id.0))
                        .ghost()
                        .disabled(busy)
                        .label("+ Create database")
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.start_create_database(id, window, cx);
                        })),
                )
                .when(!discoverable.is_empty() || is_opening, |this| {
                    this.child(self.render_open_database_popover(
                        id,
                        discoverable,
                        is_opening,
                        busy,
                        cx,
                    ))
                })
                .into_any_element()
        };

        v_flex().w_full().gap_1().child(tab_bar).child(page)
    }

    /// "Open existing" as a floating `Popover` anchored under its trigger
    /// button, instead of an inline block in the normal layout flow — opening
    /// it must not push the three-pane explorer (or anything else) down the
    /// page, since a server can have arbitrarily many discovered databases.
    fn render_open_database_popover(
        &self,
        id: ConnectionId,
        discoverable: Vec<String>,
        is_opening: bool,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();

        Popover::new(("open-database-popover", id.0))
            .open(is_opening)
            .on_open_change(move |open, _, cx| {
                // Covers dismissal the trigger button's own click doesn't
                // drive — clicking outside the popover or pressing Escape.
                // `open_database_picker`/`close_database_pickers` still own
                // every other transition; this only syncs the two when the
                // popover closes itself.
                if !*open {
                    entity.update(cx, |this, cx| {
                        this.opening_database_for = None;
                        cx.notify();
                    });
                }
            })
            .trigger(
                Button::new(("open-existing-database", id.0))
                    .ghost()
                    .disabled(busy)
                    .label("Open existing ▾")
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.open_database_picker(id, cx);
                    })),
            )
            .child(
                v_flex()
                    .w(px(220.))
                    .gap_1()
                    .child(
                        div()
                            .id(("open-database-picker", id.0))
                            .max_h(px(200.))
                            .overflow_y_scroll()
                            .child(v_flex().gap_1().children(
                                discoverable.into_iter().enumerate().map(|(ix, name)| {
                                    Button::new(("open-discovered-database", ix))
                                        .ghost()
                                        .disabled(busy)
                                        .label(name.clone())
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, _, cx| {
                                                this.select_database(id, name.clone(), cx);
                                                this.close_database_pickers(cx);
                                            },
                                        ))
                                }),
                            )),
                    )
                    .child(
                        Button::new(("cancel-open-database", id.0))
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.close_database_pickers(cx);
                            })),
                    ),
            )
    }
}

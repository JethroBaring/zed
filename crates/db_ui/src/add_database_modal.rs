//! Modal for adding a new database connection.

use crate::db_panel::DbPanel;
use crate::db_store::{ConnectionConfig, DbKind, DbStore};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Window,
};
use ui::{
    Banner, Button, ButtonStyle, IconName, IconSize, KeyBinding, Modal, ModalFooter, ModalHeader,
    Section, Severity, ToggleButtonGroup, ToggleButtonGroupStyle, ToggleButtonSimple, h_flex,
    prelude::*, rems_from_px, v_flex,
};
use ui_input::InputField;
use uuid::Uuid;
use workspace::ModalView;

fn single_line_input(
    label: impl Into<SharedString>,
    placeholder: &str,
    default_text: Option<&str>,
    tab_index: isize,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputField> {
    cx.new(|cx| {
        let input = InputField::new(window, cx, placeholder)
            .label(label)
            .tab_index(tab_index)
            .tab_stop(true);
        if let Some(text) = default_text {
            input.set_text(text, window, cx);
        }
        input
    })
}

pub struct NewDatabaseModal {
    focus_handle: FocusHandle,
    db_store: Entity<DbStore>,
    db_panel: Entity<DbPanel>,
    selected_kind: DbKind,
    saving: bool,
    name_input: Entity<InputField>,
    host_input: Entity<InputField>,
    port_input: Entity<InputField>,
    user_input: Entity<InputField>,
    password_input: Entity<InputField>,
    error: Option<SharedString>,
}

impl EventEmitter<DismissEvent> for NewDatabaseModal {}
impl ModalView for NewDatabaseModal {}

impl NewDatabaseModal {
    pub fn new(
        db_store: Entity<DbStore>,
        db_panel: Entity<DbPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_input = single_line_input(
            "Connection name *",
            "e.g. local-postgres",
            None,
            1,
            window,
            cx,
        );
        let host_input = single_line_input("Host *", "127.0.0.1", None, 2, window, cx);
        let port_input = single_line_input("Port", "5432", None, 3, window, cx);
        let user_input = single_line_input("Username *", "postgres", None, 4, window, cx);
        let password_input = cx.new(|cx| {
            let input = InputField::new(window, cx, "Password")
                .label("Password")
                .tab_index(5)
                .tab_stop(true);
            input.editor().set_masked(true, window, cx);
            input
        });

        Self {
            focus_handle: cx.focus_handle(),
            db_store,
            db_panel,
            selected_kind: DbKind::Postgres,
            saving: false,
            name_input,
            host_input,
            port_input,
            user_input,
            password_input,
            error: None,
        }
    }

    fn select_kind(&mut self, kind: DbKind, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_kind = kind;
        let default_port = kind.default_port().to_string();
        self.port_input.update(cx, |input: &mut InputField, cx| {
            input
                .editor()
                .set_placeholder_text(&default_port, window, cx);
        });
        let default_user = match kind {
            DbKind::MySql | DbKind::MariaDb => "root",
            DbKind::Postgres => "postgres",
        };
        self.user_input.update(cx, |input: &mut InputField, cx| {
            input
                .editor()
                .set_placeholder_text(default_user, window, cx);
        });
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.try_save(window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        cx.emit(DismissEvent);
    }

    fn on_tab(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        window.focus_next(cx);
    }

    fn on_tab_prev(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        window.focus_prev(cx);
    }

    fn try_save(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }

        let name = self.name_input.read(cx).text(cx).trim().to_string();
        let host = self.host_input.read(cx).text(cx);
        let port_str = self.port_input.read(cx).text(cx);
        let username = self.user_input.read(cx).text(cx);
        let password = self.password_input.read(cx).text(cx);

        if name.is_empty() {
            self.error = Some("Connection name is required.".into());
            cx.notify();
            return;
        }
        if self
            .db_store
            .read(cx)
            .connections()
            .iter()
            .any(|connection| connection.config.name.eq_ignore_ascii_case(&name))
        {
            self.error = Some("A connection with this name already exists.".into());
            cx.notify();
            return;
        }
        if host.trim().is_empty() {
            self.error = Some("Host is required.".into());
            cx.notify();
            return;
        }
        if username.trim().is_empty() {
            self.error = Some("Username is required.".into());
            cx.notify();
            return;
        }
        let port: u16 = if port_str.trim().is_empty() {
            self.selected_kind.default_port()
        } else {
            match port_str.trim().parse() {
                Ok(p) => p,
                Err(_) => {
                    self.error = Some("Port must be a number between 1 and 65535.".into());
                    cx.notify();
                    return;
                }
            }
        };

        let config = ConnectionConfig {
            id: Uuid::new_v4(),
            name,
            kind: self.selected_kind,
            host: host.trim().to_string(),
            port,
            username: username.trim().to_string(),
            database: None,
        };

        self.error = None;
        self.saving = true;
        cx.notify();

        let db_store = self.db_store.clone();
        let db_panel = self.db_panel.clone();
        let config_for_task = config.clone();
        let password_for_task = password.to_string();
        cx.spawn(async move |this, cx| {
            let result = match DbStore::test_connection(
                config_for_task.clone(),
                password_for_task.clone(),
                cx,
            )
            .await
            {
                Ok(schema) => {
                    DbStore::add_connection_with_schema(
                        db_store.clone(),
                        config_for_task.clone(),
                        password_for_task.clone(),
                        schema,
                        cx,
                    )
                    .await
                }
                Err(err) => Err(err),
            };

            if let Some(this) = this.upgrade() {
                this.update(cx, move |this, cx| {
                    this.saving = false;
                    match result {
                        Ok(()) => {
                            db_panel.update(cx, |panel, cx| {
                                panel.rebuild_entries(cx);
                            });
                            cx.emit(DismissEvent);
                        }
                        Err(err) => {
                            this.error = Some(err.to_string().into());
                            cx.notify();
                        }
                    }
                });
            }

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
}

impl Focusable for NewDatabaseModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NewDatabaseModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.focus_handle(cx);
        let selected_kind = self.selected_kind;
        let save_label = if self.saving {
            "Connecting..."
        } else {
            "Save Connection"
        };

        let kind_buttons = ToggleButtonGroup::single_row(
            "db-kind",
            [
                ToggleButtonSimple::new(
                    "PostgreSQL",
                    cx.listener(|this, _, window, cx| {
                        this.select_kind(DbKind::Postgres, window, cx);
                    }),
                )
                .selected(selected_kind == DbKind::Postgres),
                ToggleButtonSimple::new(
                    "MySQL",
                    cx.listener(|this, _, window, cx| {
                        this.select_kind(DbKind::MySql, window, cx);
                    }),
                )
                .selected(selected_kind == DbKind::MySql),
                ToggleButtonSimple::new(
                    "MariaDB",
                    cx.listener(|this, _, window, cx| {
                        this.select_kind(DbKind::MariaDb, window, cx);
                    }),
                )
                .selected(selected_kind == DbKind::MariaDb),
            ],
        )
        .style(ToggleButtonGroupStyle::Outlined);

        v_flex()
            .id("new-database-modal")
            .key_context("NewDatabaseModal")
            .w(rems_from_px(480.))
            .elevation_3(cx)
            .track_focus(&focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_tab_prev))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .child(
                Modal::new("add-database-modal", None)
                    .header(
                        ModalHeader::new()
                            .headline("Add Database Connection")
                            .show_dismiss_button(true),
                    )
                    .when_some(self.error.clone(), |this, error| {
                        this.section(
                            Section::new().child(
                                Banner::new()
                                    .severity(Severity::Warning)
                                    .child(div().text_xs().child(error)),
                            ),
                        )
                    })
                    .child(
                        v_flex()
                            .id("modal-content")
                            .size_full()
                            .tab_group()
                            .pl_3()
                            .pr_4()
                            .pb_3()
                            .gap_3()
                            .child(self.name_input.clone())
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        Label::new("Server type")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(kind_buttons),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(v_flex().flex_1().child(self.host_input.clone()))
                                    .child(v_flex().flex_1().child(self.port_input.clone())),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(v_flex().flex_1().child(self.user_input.clone()))
                                    .child(v_flex().flex_1().child(self.password_input.clone())),
                            ),
                    )
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("cancel-add-db", "Cancel")
                                        .disabled(self.saving)
                                        .key_binding(
                                            KeyBinding::for_action_in(
                                                &menu::Cancel,
                                                &focus_handle,
                                                cx,
                                            )
                                            .map(|kb| kb.size(rems_from_px(12.))),
                                        )
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.cancel(&menu::Cancel, window, cx);
                                        })),
                                )
                                .child(
                                    Button::new("save-db", save_label)
                                        .style(ButtonStyle::Filled)
                                        .icon(if self.saving {
                                            IconName::ArrowCircle
                                        } else {
                                            IconName::Check
                                        })
                                        .icon_size(IconSize::Small)
                                        .disabled(self.saving)
                                        .key_binding(
                                            KeyBinding::for_action_in(
                                                &menu::Confirm,
                                                &focus_handle,
                                                cx,
                                            )
                                            .map(|kb| kb.size(rems_from_px(12.))),
                                        )
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.confirm(&menu::Confirm, window, cx);
                                        })),
                                ),
                        ),
                    ),
            )
    }
}

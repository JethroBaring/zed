use crate::db_query_view::DbQueryView;
use crate::db_store::{CellUpdate, CellUpdateScope, ConnectionConfig, DbStore, QueryKeyValue};
use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, PromptLevel, Render,
    SharedString, Window,
};
use ui::{
    ActiveTheme, Banner, Button, ButtonStyle, IconName, IconSize, KeyBinding, Label, LabelSize,
    Modal, ModalFooter, ModalHeader, Section, Severity, h_flex, prelude::*, rems_from_px, v_flex,
};
use workspace::ModalView;

#[derive(Clone)]
pub struct CellEditTarget {
    pub query_view: Entity<DbQueryView>,
    pub connection: ConnectionConfig,
    pub database: String,
    pub table_name: String,
    pub column_name: String,
    pub column_type: String,
    pub current_value: Option<String>,
    pub primary_key: Vec<QueryKeyValue>,
    pub primary_key_columns: Vec<String>,
}

pub struct EditCellModal {
    focus_handle: FocusHandle,
    target: CellEditTarget,
    value_editor: Entity<Editor>,
    saving: bool,
    error: Option<SharedString>,
}

impl EventEmitter<DismissEvent> for EditCellModal {}
impl ModalView for EditCellModal {}

impl EditCellModal {
    pub fn new(target: CellEditTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let value_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.hide_minimap_by_default(window, cx);
            editor.set_show_gutter(false, cx);
            editor.set_show_line_numbers(false, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_show_edit_predictions(Some(false), window, cx);
            editor.set_use_autoclose(false);
            editor.set_placeholder_text("Enter the new cell value", window, cx);
            editor.set_text(target.current_value.clone().unwrap_or_default(), window, cx);
            editor
        });

        Self {
            focus_handle: value_editor.read(cx).focus_handle(cx),
            target,
            value_editor,
            saving: false,
            error: None,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let new_value = Some(self.value_editor.read(cx).text(cx));
        self.save_value(new_value, window, cx);
    }

    fn save_null(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_value(None, window, cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        cx.emit(DismissEvent);
    }

    fn save_value(
        &mut self,
        new_value: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }

        if self.target.current_value == new_value {
            cx.emit(DismissEvent);
            return;
        }

        self.saving = true;
        self.error = None;

        let target = self.target.clone();
        let confirmation = if target.primary_key.is_empty() {
            let (message, detail) = build_update_warning(&target);
            Some(window.prompt(
                PromptLevel::Warning,
                &message,
                detail.as_deref(),
                &["Update All Rows", "Cancel"],
                cx,
            ))
        } else {
            None
        };

        cx.spawn_in(window, async move |this, cx| {
            if let Some(confirmation) = confirmation
                && confirmation.await? != 0
            {
                let _ = this.update(cx, |this, cx| {
                    this.saving = false;
                    cx.notify();
                });
                return anyhow::Ok(());
            }

            let key = target.connection.keychain_key();
            let password = match cx.update(|_, cx| cx.read_credentials(&key)) {
                Ok(task) => task
                    .await
                    .ok()
                    .flatten()
                    .map(|(_, p): (String, Vec<u8>)| String::from_utf8_lossy(&p).into_owned())
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };

            let update = CellUpdate {
                table_name: target.table_name.clone(),
                column_name: target.column_name.clone(),
                column_type: target.column_type.clone(),
                new_value,
                scope: if target.primary_key.is_empty() {
                    CellUpdateScope::AllRows
                } else {
                    CellUpdateScope::PrimaryKey(target.primary_key.clone())
                },
            };

            match DbStore::update_cell(
                target.connection.clone(),
                target.database.clone(),
                password,
                update,
                cx,
            )
            .await
            {
                Ok(_) => {
                    let _ = target.query_view.update_in(cx, |view, window, cx| {
                        view.run_current_query(window, cx);
                    });
                    let _ = this.update(cx, |this, cx| {
                        this.saving = false;
                        cx.emit(DismissEvent);
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.saving = false;
                        this.error = Some(error.to_string().into());
                        cx.notify();
                    });
                }
            }

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);

        cx.notify();
    }
}

impl Focusable for EditCellModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditCellModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.focus_handle(cx);
        let save_label = if self.saving { "Saving..." } else { "Save" };

        v_flex()
            .id("edit-db-cell-modal")
            .key_context("EditDbCellModal")
            .w(rems_from_px(520.))
            .elevation_3(cx)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .child(
                Modal::new("edit-db-cell-modal", None)
                    .header(
                        ModalHeader::new()
                            .headline("Edit Cell")
                            .description(format!(
                                "{}.{}",
                                self.target.table_name, self.target.column_name
                            ))
                            .show_dismiss_button(true),
                    )
                    .when(self.target.primary_key.is_empty(), |this| {
                        let (_, detail) = build_update_warning(&self.target);
                        this.section(
                            Section::new().child(Banner::new().severity(Severity::Warning).child(
                                div().text_xs().child(detail.unwrap_or_else(|| {
                                    "Saving will update every row in this table.".to_string()
                                })),
                            )),
                        )
                    })
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
                            .id("edit-db-cell-modal-content")
                            .size_full()
                            .pl_3()
                            .pr_4()
                            .pb_3()
                            .gap_3()
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(
                                        Label::new(format!("Column: {}", self.target.column_name))
                                            .size(LabelSize::Small),
                                    )
                                    .child(
                                        Label::new(format!("Type: {}", self.target.column_type))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .h(px(144.))
                                    .rounded_md()
                                    .border_1()
                                    .border_color(cx.theme().colors().border_variant)
                                    .bg(cx.theme().colors().editor_background)
                                    .child(self.value_editor.clone()),
                            ),
                    )
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("cancel-edit-db-cell", "Cancel")
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
                                    Button::new("save-null-db-cell", "Set NULL")
                                        .disabled(self.saving)
                                        .icon(IconName::Dash)
                                        .icon_size(IconSize::Small)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.save_null(window, cx);
                                        })),
                                )
                                .child(
                                    Button::new("save-edit-db-cell", save_label)
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

fn build_update_warning(target: &CellEditTarget) -> (String, Option<String>) {
    if target.primary_key_columns.is_empty() {
        (
            format!("Update every row in {}?", target.table_name),
            Some(format!(
                "This table has no primary key. Saving {} will update every row in {}.",
                target.column_name, target.table_name
            )),
        )
    } else {
        (
            format!("Update every row in {}?", target.table_name),
            Some(format!(
                "This result set does not include the primary key column(s) {}. Saving {} will update every row in {}.",
                target.primary_key_columns.join(", "),
                target.column_name,
                target.table_name
            )),
        )
    }
}

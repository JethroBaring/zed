use crate::db_store::{
    ConnectionConfig, DatabaseSchema, DbKind, DbStore, QueryColumn, QueryExecution,
};
use crate::edit_cell_modal::{CellEditTarget, EditCellModal};
use editor::{CompletionProvider, Editor, ExcerptId};
use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, SharedString, Task,
    WeakEntity, Window, px,
};
use language::{Anchor, Buffer, CodeLabel, ToOffset};
use project::{
    CompletionDisplayOptions, CompletionResponse, CompletionSource,
    lsp_store::CompletionDocumentation,
};
use std::{collections::HashSet, ops::Range, rc::Rc, time::Duration};
use ui::{
    Button, ButtonSize, ButtonStyle, Color, ContextMenu, DefiniteLength, DropdownMenu,
    DropdownStyle, Icon, IconName, IconPosition, Label, LabelSize, Table, TableColumnWidths,
    TableInteractionState, TableResizeBehavior, Tooltip, prelude::*, vertical_divider,
};
use ui_input::InputField;
use workspace::{Item, Workspace};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryTargetKind {
    Table,
    View,
}

impl QueryTargetKind {
    fn label(self) -> &'static str {
        match self {
            QueryTargetKind::Table => "Table",
            QueryTargetKind::View => "View",
        }
    }

    fn icon(self) -> IconName {
        match self {
            QueryTargetKind::Table => IconName::ListTree,
            QueryTargetKind::View => IconName::Eye,
        }
    }
}

enum QueryRunState {
    Idle,
    Busy(SharedString),
    Failed(SharedString),
}

pub struct DbQueryView {
    workspace: WeakEntity<Workspace>,
    db_store: Entity<DbStore>,
    connection: ConnectionConfig,
    database: String,
    target_kind: QueryTargetKind,
    target_name: SharedString,
    focus_handle: FocusHandle,
    query_editor: Entity<Editor>,
    table_interaction_state: Entity<TableInteractionState>,
    column_widths: Entity<TableColumnWidths>,
    query_task: Option<Task<()>>,
    state: QueryRunState,
    result: Option<QueryExecution>,
    current_page: usize,
    page_size: usize,
    search_input: Entity<InputField>,
    query_height: Pixels,
    resizing_query: bool,
    resize_start_mouse_y: Pixels,
    resize_start_height: Pixels,
}

impl DbQueryView {
    pub fn open(
        workspace: Entity<Workspace>,
        db_store: Entity<DbStore>,
        connection: ConnectionConfig,
        database: String,
        target_kind: QueryTargetKind,
        target_name: String,
        window: &mut Window,
        cx: &mut App,
    ) {
        let existing = {
            let workspace = workspace.read(cx);
            workspace.items_of_type::<Self>(cx).find(|view| {
                let view = view.read(cx);
                view.connection.id == connection.id
                    && view.database == database
                    && view.target_kind == target_kind
                    && view.target_name.as_ref() == target_name.as_str()
            })
        };

        if let Some(existing) = existing {
            workspace.update(cx, |workspace, cx| {
                workspace.activate_item(&existing, true, true, window, cx);
            });
            return;
        }

        let workspace_handle = workspace.downgrade();
        let view = cx.new(|cx| {
            Self::new(
                workspace_handle,
                db_store,
                connection,
                database,
                target_kind,
                SharedString::from(target_name),
                window,
                cx,
            )
        });

        workspace.update(cx, |workspace, cx| {
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        });

        view.update(cx, |this, cx| this.run_current_query(window, cx));
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        db_store: Entity<DbStore>,
        connection: ConnectionConfig,
        database: String,
        target_kind: QueryTargetKind,
        target_name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_query = default_query(connection.kind, target_name.as_ref());
        let search_input = cx.new(|cx| {
            InputField::new(window, cx, "Search").start_icon(IconName::MagnifyingGlass)
        });
        let query_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.hide_minimap_by_default(window, cx);
            editor.set_show_completions_on_input(Some(true));
            editor.set_show_edit_predictions(Some(false), window, cx);
            editor.set_use_autoclose(false);
            editor.set_placeholder_text("Write a SQL query", window, cx);
            editor.set_text(initial_query, window, cx);
            editor
        });
        let query_view = cx.entity().downgrade();
        query_editor.update(cx, |editor, _cx| {
            editor.set_completion_provider(Some(Rc::new(DbQueryCompletionProvider::new(
                query_view.clone(),
            ))));
        });
        let focus_handle = query_editor.read(cx).focus_handle(cx);
        let query_buffer = query_editor.read(cx).buffer().read(cx).as_singleton();
        if let (Some(workspace), Some(query_buffer)) = (workspace.upgrade(), query_buffer) {
            let language_registry = workspace.read(cx).app_state().languages.clone();
            let sql_language = language_registry.language_for_name("SQL");
            cx.spawn_in(window, async move |_this, cx| {
                let sql_language = sql_language.await.ok();
                let _ = cx.update(|_, cx| {
                    query_buffer.update(cx, |buffer, cx| {
                        buffer.set_language_registry(language_registry);
                        if let Some(sql_language) = sql_language {
                            buffer.set_language(Some(sql_language), cx);
                        }
                    });
                });
            })
            .detach();
        }

        Self {
            workspace,
            db_store,
            connection,
            database,
            target_kind,
            target_name,
            focus_handle,
            query_editor,
            table_interaction_state: cx.new(|cx| TableInteractionState::new(cx)),
            column_widths: cx.new(|cx| TableColumnWidths::new(1, cx)),
            query_task: None,
            state: QueryRunState::Idle,
            result: None,
            current_page: 0,
            page_size: 50,
            search_input,
            query_height: px(180.),
            resizing_query: false,
            resize_start_mouse_y: px(0.),
            resize_start_height: px(180.),
        }
    }

    pub(crate) fn run_current_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.query_editor.read(cx).text(cx);
        if sql.trim().is_empty() {
            self.state = QueryRunState::Failed("Enter a SQL query.".into());
            cx.notify();
            return;
        }

        self.state = QueryRunState::Busy("Executing query...".into());
        let connection = self.connection.clone();
        let database = self.database.clone();
        let editable_target =
            (self.target_kind == QueryTargetKind::Table).then(|| self.target_name.to_string());

        self.query_task = Some(cx.spawn_in(window, async move |this, cx| {
            let key = connection.keychain_key();
            let password = match cx.update(|_, cx| cx.read_credentials(&key)) {
                Ok(task) => task
                    .await
                    .ok()
                    .flatten()
                    .map(|(_, p): (String, Vec<u8>)| String::from_utf8_lossy(&p).into_owned())
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };

            let result = DbStore::execute_query(
                connection,
                database,
                password,
                sql,
                editable_target,
                cx,
            )
            .await;

            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(result) => this.set_result(result, cx),
                    Err(error) => {
                        this.state = QueryRunState::Failed(error.to_string().into());
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_result(&mut self, result: QueryExecution, cx: &mut Context<Self>) {
        let cols = result.columns.len().max(1);
        self.column_widths
            .update(cx, |widths, cx| *widths = TableColumnWidths::new(cols, cx));
        self.result = Some(result);
        self.state = QueryRunState::Idle;
        self.current_page = 0;
    }

    fn is_busy(&self) -> bool {
        matches!(self.state, QueryRunState::Busy(_))
    }

    fn result_cells_are_editable(&self, result: &QueryExecution) -> bool {
        self.target_kind == QueryTargetKind::Table && result.table_name.is_some() && !self.is_busy()
    }

    fn open_edit_cell_modal(
        &mut self,
        row_index: usize,
        column_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(result) = self.result.as_ref() else {
            return;
        };
        let Some(table_name) = result.table_name.clone() else {
            return;
        };
        let Some(column) = result.columns.get(column_index).cloned() else {
            return;
        };
        let Some(row) = result.rows.get(row_index).cloned() else {
            return;
        };

        let target = CellEditTarget {
            query_view: cx.entity(),
            connection: self.connection.clone(),
            database: self.database.clone(),
            table_name,
            column_name: column.name,
            column_type: column.type_name,
            current_value: row.values.get(column_index).cloned().unwrap_or(None),
            primary_key: row.primary_key,
            primary_key_columns: result.primary_key_columns.clone(),
        };

        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                EditCellModal::new(target, window, cx)
            });
        });
    }

    fn render_toolbar(&self) -> impl IntoElement {
        let server_icon = match self.connection.kind {
            DbKind::Postgres => {
                Icon::from_external_svg("crates/db_ui/db_icons/postgresql.svg".into())
            }
            DbKind::MySql => Icon::from_external_svg("crates/db_ui/db_icons/mysql.svg".into()),
            DbKind::MariaDb => Icon::from_external_svg("crates/db_ui/db_icons/mariadb.svg".into()),
        }
        .size(IconSize::Small)
        .color(Color::Muted);

        h_flex()
            .w_full()
            .items_center()
            .gap_1p5()
            .child(
                server_icon,
            )
            .child(
                Label::new(self.connection.name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new("/")
                    .size(LabelSize::Small)
                    .color(Color::Disabled),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_0p5()
                    .child(
                        Icon::new(IconName::DatabaseZap)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(self.database.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                Label::new("/")
                    .size(LabelSize::Small)
                    .color(Color::Disabled),
            )
            .child(
                Icon::new(self.target_kind.icon())
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Label::new(self.target_name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Default),
            )
    }

    fn render_status_bar(&self, cx: &App) -> impl IntoElement {
        let (icon, message, color) = match (&self.state, self.result.as_ref()) {
            (QueryRunState::Busy(msg), _) => (IconName::ArrowCircle, msg.to_string(), Color::Info),
            (QueryRunState::Failed(err), _) => {
                (IconName::Warning, err.to_string(), Color::Error)
            }
            (QueryRunState::Idle, Some(result)) => (
                IconName::Check,
                format!("{} in {}", result.summary, format_duration(result.elapsed)),
                Color::Muted,
            ),
            (QueryRunState::Idle, None) => (
                IconName::Info,
                "Run a query to see results.".to_string(),
                Color::Muted,
            ),
        };

        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_2()
            .px_2()
            .py_0p5()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().title_bar_background)
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .min_w_0()
                    .flex_1()
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .child(
                        Label::new(message)
                            .size(LabelSize::XSmall)
                            .color(color)
                            .truncate(),
                    ),
            )
    }

    fn render_results(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(result) = self.result.as_ref() else {
            return self
                .render_results_placeholder(
                    "Run the query to load rows into the results grid.",
                    Color::Muted,
                    cx,
                )
                .into_any_element();
        };

        let total_rows = result.rows.len();
        let page_size = self.page_size.max(1);
        let total_pages = if total_rows == 0 {
            1
        } else {
            (total_rows + page_size - 1) / page_size
        };
        // Clamp current page to valid range.
        if self.current_page >= total_pages {
            self.current_page = total_pages.saturating_sub(1);
        }
        let offset = self.current_page.saturating_mul(page_size);
        let visible_rows = total_rows.saturating_sub(offset).min(page_size);

        if result.columns.is_empty() {
            return self
                .render_results_placeholder(result.summary.clone(), Color::Muted, cx)
                .into_any_element();
        }

        let widths =
            vec![DefiniteLength::Fraction(1.0 / result.columns.len() as f32); result.columns.len()];
        let behaviors = vec![TableResizeBehavior::Resizable; result.columns.len()];
        let headers = result
            .columns
            .iter()
            .map(|column| {
                v_flex()
                    .gap_0p5()
                    .py_1()
                    .child(Label::new(column.name.clone()))
                    .child(
                        Label::new(column.type_name.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        Table::new(result.columns.len())
            .interactable(&self.table_interaction_state)
            .striped()
            .empty_table_callback(|_, _| {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Label::new("Query returned no rows.").color(Color::Muted))
                    .into_any_element()
            })
            .column_widths(widths)
            .resizable_columns(behaviors, &self.column_widths, cx)
            .header(headers)
            .uniform_list("db-query-results", visible_rows, cx.processor(Self::render_result_rows))
            .into_any_element()
    }

    fn render_result_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Vec<AnyElement>> {
        let Some(result) = self.result.as_ref() else {
            return Vec::new();
        };

        let editable = self.result_cells_are_editable(result);
        let page_size = self.page_size.max(1);
        let offset = self.current_page.saturating_mul(page_size);
        let rows = range
            .filter_map(|display_index| {
                let row_index = offset + display_index;
                result
                    .rows
                    .get(row_index)
                    .cloned()
                    .map(|row| (row_index, row))
            })
            .collect::<Vec<_>>();

        rows.into_iter()
            .map(|(row_index, row)| {
                row.values
                    .into_iter()
                    .enumerate()
                    .map(|(column_index, value)| {
                        self.render_result_cell(row_index, column_index, value, editable, cx)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn render_result_cell(
        &mut self,
        row_index: usize,
        column_index: usize,
        value: Option<String>,
        editable: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cell = div()
            .id(format!("db-result-cell-{row_index}-{column_index}"))
            .size_full()
            .px_2()
            .py_1()
            .overflow_hidden()
            .child(render_cell_label(value.as_deref()));

        let cell = if editable {
            cell.hover(|style| style.bg(cx.theme().colors().element_hover).cursor_pointer())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_edit_cell_modal(row_index, column_index, window, cx);
                }))
        } else {
            cell
        };

        cell.into_any_element()
    }

    fn render_results_placeholder(
        &self,
        message: impl Into<SharedString>,
        color: Color,
        _cx: &App,
    ) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                Label::new(message)
                    .size(LabelSize::Small)
                    .color(color)
                    .truncate(),
            )
    }
}

impl EventEmitter<()> for DbQueryView {}

impl Focusable for DbQueryView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for DbQueryView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(self.target_kind.icon()))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.target_name.clone()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(
            format!("{} - {}.{}", self.connection.name, self.database, self.target_name).into(),
        )
    }
}

impl Render for DbQueryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_results = self.result.as_ref().is_some();
        let is_busy = self.is_busy();
        let total_rows = self.result.as_ref().map(|r| r.rows.len()).unwrap_or(0);
        let page_size = self.page_size.max(1);
        let total_pages = if total_rows == 0 {
            1
        } else {
            (total_rows + page_size - 1) / page_size
        };
        if self.current_page >= total_pages {
            self.current_page = total_pages.saturating_sub(1);
        }
        let current_page = self.current_page;
        let can_prev = current_page > 0 && !is_busy;
        let can_next = current_page + 1 < total_pages && !is_busy;
        let elapsed_color = self
            .result
            .as_ref()
            .map(|r| {
                let ms = r.elapsed.as_millis();
                if ms < 250 {
                    Color::Success
                } else if ms < 1000 {
                    Color::Info
                } else if ms < 3000 {
                    Color::Warning
                } else {
                    Color::Error
                }
            })
            .unwrap_or(Color::Muted);
        let query_view = cx.entity();
        let page_size_menu = ContextMenu::build(_window, cx, {
            let current_page_size = self.page_size;
            move |menu, _window, _cx| {
                let build_item = |menu: ContextMenu, size: usize| {
                    let query_view = query_view.clone();
                    menu.toggleable_entry(
                        format!("{size}"),
                        current_page_size == size,
                        IconPosition::End,
                        None,
                        move |_window, cx| {
                            query_view.update(cx, |this, cx| {
                                this.page_size = size;
                                this.current_page = 0;
                                cx.notify();
                            });
                        },
                    )
                };

                let menu = build_item(menu, 25);
                let menu = build_item(menu, 50);
                let menu = build_item(menu, 100);
                menu
            }
        });

        let min_query_height = _window.line_height() * 3.0;
        if self.query_height < min_query_height {
            self.query_height = min_query_height;
        }

        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _window, cx| {
                if this.resizing_query && e.pressed_button == Some(MouseButton::Left) {
                    let delta = e.position.y - this.resize_start_mouse_y;
                    let mut new_height = this.resize_start_height + delta;
                    let min_height = _window.line_height() * 3.0;
                    if new_height < min_height {
                        new_height = min_height;
                    }
                    this.query_height = new_height;
                    cx.notify();
                }
            }))
            .capture_any_mouse_up(cx.listener(|this, _e: &MouseUpEvent, _window, cx| {
                if this.resizing_query {
                    this.resizing_query = false;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .w_full()
                    .h(self.query_height)
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.query_editor.clone())
                    .child(
                        div()
                            .id("db-query-resize-handle")
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .h(px(6.))
                            .cursor_row_resize()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, e: &MouseDownEvent, _window, cx| {
                                    this.resizing_query = true;
                                    this.resize_start_mouse_y = e.position.y;
                                    this.resize_start_height = this.query_height;
                                    cx.stop_propagation();
                                }),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().title_bar_background)
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                self.search_input.clone(),
                            )
                            .child(vertical_divider().into_any_element())
                            .child(
                                Button::new("db-run-query", "Run")
                                    .icon(IconName::PlayFilled)
                                    .icon_position(IconPosition::Start)
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Transparent)
                                    .disabled(is_busy)
                                    .tooltip(Tooltip::text("Execute query"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.run_current_query(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("db-insert-row", "Insert")
                                    .icon(IconName::Plus)
                                    .icon_position(IconPosition::Start)
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Transparent)
                                    .disabled(!has_results || is_busy),
                            )
                            .child(
                                Button::new("db-refresh-results", "Refresh")
                                    .icon(IconName::RotateCcw)
                                    .icon_position(IconPosition::Start)
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Transparent)
                                    .disabled(is_busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.run_current_query(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("db-filter-results", "Filter")
                                    .icon(IconName::ListFilter)
                                    .icon_position(IconPosition::Start)
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Transparent)
                                    .disabled(!has_results),
                            )
                            .child(
                                Button::new("db-sort-results", "Sort")
                                    .icon(IconName::ArrowDown)
                                    .icon_position(IconPosition::Start)
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Transparent)
                                    .disabled(!has_results),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Label::new(
                                    self.result
                                        .as_ref()
                                        .map(|r| format_duration(r.elapsed))
                                        .unwrap_or_else(|| "".to_string()),
                                )
                                .size(LabelSize::Small)
                                .color(elapsed_color),
                            )
                            .child(
                                Button::new("db-prev-page", "")
                                    .icon(IconName::ChevronLeft)
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Transparent)
                                    .disabled(!can_prev)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        if this.current_page > 0 {
                                            this.current_page -= 1;
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                Label::new(format!("{} of {}", current_page + 1, total_pages))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Button::new("db-next-page", "")
                                    .icon(IconName::ChevronRight)
                                    .size(ButtonSize::Compact)
                                    .style(ButtonStyle::Transparent)
                                    .disabled(!can_next)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        let total_rows =
                                            this.result.as_ref().map(|r| r.rows.len()).unwrap_or(0);
                                        let page_size = this.page_size.max(1);
                                        let total_pages = if total_rows == 0 {
                                            1
                                        } else {
                                            (total_rows + page_size - 1) / page_size
                                        };
                                        if this.current_page + 1 < total_pages {
                                            this.current_page += 1;
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                DropdownMenu::new("db-page-size", format!("{page_size}"), page_size_menu)
                                    .trigger_size(ButtonSize::Compact)
                                    .style(DropdownStyle::Ghost)
                                    .disabled(is_busy),
                            )
                            .child(
                                Label::new(
                                    self.result
                                        .as_ref()
                                        .map(|r| format!("{}", r.rows.len()))
                                        .unwrap_or_else(|| "0".to_string()),
                                )
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    ),
            )
            // ── results table (flush, no wrapper padding) ─────────────────
            .child(
                div()
                    .size_full()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_results(cx)),
            )
    }
}

fn default_query(kind: DbKind, target_name: &str) -> String {
    let quoted_name = match kind {
        DbKind::Postgres => quote_postgres_identifier(target_name),
        DbKind::MySql | DbKind::MariaDb => quote_mysql_identifier(target_name),
    };

    format!("SELECT * FROM {quoted_name}\nLIMIT 100;")
}

fn quote_postgres_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn quote_mysql_identifier(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}

fn format_duration(duration: Duration) -> String {
    if duration.as_millis() < 1000 {
        format!("{}ms", duration.as_millis())
    } else {
        format!("{:.2}s", duration.as_secs_f64())
    }
}

fn render_cell_label(value: Option<&str>) -> AnyElement {
    let label = match value {
        Some(value) if !value.is_empty() => Label::new(value.to_string())
            .size(LabelSize::Small)
            .into_any_element(),
        Some(_) => Label::new("").size(LabelSize::Small).into_any_element(),
        None => Label::new("NULL")
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element(),
    };

    div()
        .w_full()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(label)
        .into_any_element()
}

struct DbQueryCompletionProvider {
    query_view: WeakEntity<DbQueryView>,
}

impl DbQueryCompletionProvider {
    fn new(query_view: WeakEntity<DbQueryView>) -> Self {
        Self { query_view }
    }

    fn completion_state(&self, cx: &App) -> Option<DbQueryCompletionState> {
        let query_view = self.query_view.upgrade()?;
        let (db_store, connection, database, target_name, result_columns) =
            query_view.read(cx).completion_state();
        let schema = db_store
            .read(cx)
            .connection_by_id(connection.id)
            .and_then(|conn| conn.state.schema())
            .and_then(|s| {
                s.iter_databases()
                    .find(|db| db.name == database)
                    .cloned()
            })
            .unwrap_or_default();

        Some(DbQueryCompletionState {
            connection,
            target_name,
            schema,
            result_columns,
        })
    }
}

impl CompletionProvider for DbQueryCompletionProvider {
    fn completions(
        &self,
        _excerpt_id: ExcerptId,
        buffer: &Entity<Buffer>,
        buffer_position: Anchor,
        _trigger: editor::CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<anyhow::Result<Vec<CompletionResponse>>> {
        let Some(state) = self.completion_state(cx) else {
            return Task::ready(Ok(Vec::new()));
        };

        let buffer = buffer.read(cx);
        let cursor_offset = buffer_position.to_offset(&buffer);
        let mut count_back = 0;
        for ch in buffer.reversed_chars_at(buffer_position) {
            if is_sql_identifier_char(ch) {
                count_back += 1;
            } else {
                break;
            }
        }

        let start_offset = cursor_offset.saturating_sub(count_back);
        let replace_range = buffer.anchor_before(start_offset)..buffer_position;
        let full_text = buffer.text_snapshot().text();
        let context_before_cursor = take_chars(&full_text, start_offset);
        let query = slice_chars(&full_text, start_offset, cursor_offset);
        let context = SqlCompletionContext::from_text(&context_before_cursor);
        let completions = build_sql_completions(&state, &context, &query, replace_range);

        if completions.is_empty() {
            return Task::ready(Ok(Vec::new()));
        }

        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions {
                dynamic_width: true,
            },
            is_incomplete: false,
        }]))
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: Anchor,
        text: &str,
        _trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        match text {
            "." | "\"" | "`" => true,
            " " => {
                let buffer = buffer.read(cx);
                let offset = position.to_offset(&buffer);
                let full_text = buffer.text_snapshot().text();
                let context = SqlCompletionContext::from_text(&take_chars(&full_text, offset));
                matches!(
                    context.scope,
                    SqlCompletionScope::Relations | SqlCompletionScope::Columns
                )
            }
            _ => text
                .chars()
                .last()
                .is_some_and(|ch| is_sql_identifier_char(ch)),
        }
    }

    fn sort_completions(&self) -> bool {
        false
    }

    fn filter_completions(&self) -> bool {
        false
    }
}

impl DbQueryView {
    fn completion_state(
        &self,
    ) -> (Entity<DbStore>, ConnectionConfig, String, String, Vec<QueryColumn>) {
        (
            self.db_store.clone(),
            self.connection.clone(),
            self.database.clone(),
            self.target_name.to_string(),
            self.result
                .as_ref()
                .map(|result| result.columns.clone())
                .unwrap_or_default(),
        )
    }
}

#[derive(Clone)]
struct DbQueryCompletionState {
    connection: ConnectionConfig,
    target_name: String,
    schema: DatabaseSchema,
    result_columns: Vec<QueryColumn>,
}

#[derive(Clone, Copy)]
enum SqlCompletionScope {
    General,
    Relations,
    Columns,
}

struct SqlCompletionContext {
    scope: SqlCompletionScope,
}

impl SqlCompletionContext {
    fn from_text(text: &str) -> Self {
        let after_dot = text.chars().last().is_some_and(|ch| ch == '.');
        if after_dot {
            return Self {
                scope: SqlCompletionScope::Columns,
            };
        }

        let tokens = sql_tokens(text);
        let last_keyword = tokens.last().map(|token| token.as_str());
        let scope = match last_keyword {
            Some(
                "from" | "join" | "update" | "into" | "table" | "delete" | "truncate" | "desc"
                | "describe",
            ) => SqlCompletionScope::Relations,
            Some(
                "select" | "where" | "and" | "or" | "having" | "on" | "set" | "by" | "order"
                | "group" | "limit" | "offset" | "values",
            ) => SqlCompletionScope::Columns,
            _ => SqlCompletionScope::General,
        };

        Self { scope }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum SqlCompletionKind {
    Keyword,
    Table,
    View,
    Column,
}

#[derive(Clone)]
struct SqlCompletionItem {
    label: String,
    new_text: String,
    detail: SharedString,
    kind: SqlCompletionKind,
}

fn build_sql_completions(
    state: &DbQueryCompletionState,
    context: &SqlCompletionContext,
    query: &str,
    replace_range: Range<Anchor>,
) -> Vec<project::Completion> {
    let items = completion_pool(state, context);
    let matched_items = filter_completion_items(items, query);

    matched_items
        .into_iter()
        .map(|item| project::Completion {
            replace_range: replace_range.clone(),
            new_text: item.new_text,
            label: CodeLabel::plain(item.label, None),
            documentation: Some(CompletionDocumentation::SingleLine(item.detail)),
            source: CompletionSource::Custom,
            icon_path: None,
            match_start: None,
            snippet_deduplication_key: None,
            insert_text_mode: None,
            confirm: None,
        })
        .collect()
}

fn completion_pool(
    state: &DbQueryCompletionState,
    context: &SqlCompletionContext,
) -> Vec<SqlCompletionItem> {
    let keyword_items = SQL_KEYWORDS.iter().map(|keyword| SqlCompletionItem {
        label: (*keyword).to_string(),
        new_text: (*keyword).to_string(),
        detail: "SQL keyword".into(),
        kind: SqlCompletionKind::Keyword,
    });

    let table_items = state.schema.tables.iter().map(|name| SqlCompletionItem {
        label: name.clone(),
        new_text: completion_identifier_text(state.connection.kind, name),
        detail: "Table".into(),
        kind: SqlCompletionKind::Table,
    });

    let view_items = state.schema.views.iter().map(|name| SqlCompletionItem {
        label: name.clone(),
        new_text: completion_identifier_text(state.connection.kind, name),
        detail: "View".into(),
        kind: SqlCompletionKind::View,
    });

    let column_items = state.result_columns.iter().map(|column| SqlCompletionItem {
        label: column.name.clone(),
        new_text: completion_identifier_text(state.connection.kind, &column.name),
        detail: format!(
            "{} column{}",
            column.type_name,
            if !state.target_name.is_empty() {
                format!(" on {}", state.target_name)
            } else {
                String::new()
            }
        )
        .into(),
        kind: SqlCompletionKind::Column,
    });

    let items = match context.scope {
        SqlCompletionScope::Relations => table_items.chain(view_items).collect(),
        SqlCompletionScope::Columns => column_items.collect(),
        SqlCompletionScope::General => keyword_items
            .chain(table_items)
            .chain(view_items)
            .chain(column_items)
            .collect(),
    };

    dedupe_completion_items(items)
}

fn filter_completion_items(items: Vec<SqlCompletionItem>, query: &str) -> Vec<SqlCompletionItem> {
    if query.is_empty() {
        return items.into_iter().take(80).collect();
    }

    let normalized_query = query.to_ascii_lowercase();
    let mut prefix_matches = Vec::new();
    let mut contains_matches = Vec::new();

    for item in items {
        let label = item.label.to_ascii_lowercase();
        let insert = item.new_text.to_ascii_lowercase();

        if label.starts_with(&normalized_query) || insert.starts_with(&normalized_query) {
            prefix_matches.push(item);
        } else if label.contains(&normalized_query) || insert.contains(&normalized_query) {
            contains_matches.push(item);
        }
    }

    prefix_matches.extend(contains_matches);
    prefix_matches.truncate(80);
    prefix_matches
}

fn dedupe_completion_items(items: Vec<SqlCompletionItem>) -> Vec<SqlCompletionItem> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();

    for item in items {
        let key = (item.kind, item.label.clone());
        if seen.insert(key) {
            deduped.push(item);
        }
    }

    deduped
}

fn completion_identifier_text(kind: DbKind, name: &str) -> String {
    if is_simple_sql_identifier(name) {
        name.to_string()
    } else {
        match kind {
            DbKind::Postgres => quote_postgres_identifier(name),
            DbKind::MySql | DbKind::MariaDb => quote_mysql_identifier(name),
        }
    }
}

fn is_simple_sql_identifier(identifier: &str) -> bool {
    let mut chars = identifier.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }

    chars.all(is_sql_identifier_char)
}

fn is_sql_identifier_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn take_chars(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

fn slice_chars(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn sql_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if is_sql_identifier_char(ch) {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

const SQL_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "JOIN",
    "LEFT JOIN",
    "RIGHT JOIN",
    "INNER JOIN",
    "OUTER JOIN",
    "ON",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "DISTINCT",
    "AS",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "EXISTS",
    "BETWEEN",
    "LIKE",
    "ILIKE",
    "CREATE",
    "ALTER",
    "DROP",
    "TABLE",
    "VIEW",
    "INDEX",
    "PRIMARY KEY",
    "FOREIGN KEY",
    "UNION",
    "ALL",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "RETURNING",
    "DESC",
    "DESCRIBE",
    "SHOW",
];
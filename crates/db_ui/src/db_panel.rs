//! Database panel: tree view of all configured database connections.
//!
//! The tree hierarchy mirrors the collab_panel's channel tree pattern:
//!   - Connection (depth 1, toggleable)
//!     - Category header: Tables / Views / Functions (depth 2, toggleable)
//!       - Schema item: individual table/view/function name (depth 3)

use crate::add_database_modal::NewDatabaseModal;
use crate::db_query_view::{DbQueryView, QueryTargetKind};
use crate::db_store::{ConnectionState, DbKind, DbStore};
use db::kvp::KEY_VALUE_STORE;
use editor::{Editor, EditorElement, EditorStyle};
use gpui::DismissEvent;
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, FontStyle, ListAlignment, ListState, MouseDownEvent, Point, Render, SharedString,
    Subscription, Task, TextStyle, WeakEntity, Window, actions, div, list, prelude::*, px,
};
use serde::{Deserialize, Serialize};
use theme::{ActiveTheme, ThemeSettings};
use settings::Settings;
use ui::{
    ButtonStyle, Color, ContextMenu, Icon, IconButton, IconButtonShape, IconName, IconSize,
    Indicator, Label, LabelSize, ListHeader, ListItem, Tooltip, WithScrollbar, h_flex,
    prelude::*, v_flex,
};
use util::{ResultExt, TryFutureExt};
use uuid::Uuid;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    db_panel,
    [
        /// Closes the database panel.
        Close,
        /// Toggles the database panel.
        Toggle,
        /// Toggles focus on the database panel.
        ToggleFocus,
    ]
);

const DB_PANEL_KEY: &str = "DbPanel";

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<DbPanel>(window, cx);
    });
    workspace.register_action(|workspace, _: &Toggle, window, cx| {
        if !workspace.toggle_panel_focus::<DbPanel>(window, cx) {
            workspace.close_panel::<DbPanel>(window, cx);
        }
    });
    workspace.register_action(|workspace, _: &Close, window, cx| {
        if let Some(panel) = workspace.panel::<DbPanel>(cx) {
            workspace.remove_panel::<DbPanel>(&panel, window, cx);
        }
    });
}

// ── Serialisation ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Default)]
struct SerializedDbPanel {
    width: Option<ui::Pixels>,
    collapsed_connections: Option<Vec<String>>,
    collapsed_databases: Option<Vec<(String, String)>>,
    #[serde(default)]
    collapsed_categories: Option<Vec<(String, String, String)>>,
}

// ── Entry model ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CategoryKind {
    Tables,
    Views,
    Functions,
}

impl CategoryKind {
    fn label(self) -> &'static str {
        match self {
            CategoryKind::Tables => "Tables",
            CategoryKind::Views => "Views",
            CategoryKind::Functions => "Functions",
        }
    }

    fn icon(self) -> IconName {
        match self {
            CategoryKind::Tables => IconName::ListTree,
            CategoryKind::Views => IconName::Eye,
            CategoryKind::Functions => IconName::Code,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            CategoryKind::Tables => "tables",
            CategoryKind::Views => "views",
            CategoryKind::Functions => "functions",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "tables" => Some(CategoryKind::Tables),
            "views" => Some(CategoryKind::Views),
            "functions" => Some(CategoryKind::Functions),
            _ => None,
        }
    }

    fn as_query_target(self) -> Option<QueryTargetKind> {
        match self {
            CategoryKind::Tables => Some(QueryTargetKind::Table),
            CategoryKind::Views => Some(QueryTargetKind::View),
            CategoryKind::Functions => None,
        }
    }
}

#[derive(Clone)]
enum ListEntry {
    EmptyState,
    Connection {
        id: Uuid,
        name: SharedString,
        kind: DbKind,
        state: ConnectionStateTag,
    },
    Database {
        connection_id: Uuid,
        name: SharedString,
    },
    CategoryHeader {
        connection_id: Uuid,
        database_name: SharedString,
        kind: CategoryKind,
        count: usize,
    },
    CategoryEmpty {
        connection_id: Uuid,
        database_name: SharedString,
        kind: CategoryKind,
    },
    SchemaItem {
        connection_id: Uuid,
        database_name: SharedString,
        category: CategoryKind,
        name: SharedString,
        is_last: bool,
    },
}

/// A lightweight tag summarising the connection state for rendering.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnectionStateTag {
    Disconnected,
    Loading,
    Connected,
    Error,
}

impl ConnectionStateTag {
    fn from_state(s: &ConnectionState) -> Self {
        match s {
            ConnectionState::Disconnected => ConnectionStateTag::Disconnected,
            ConnectionState::Loading => ConnectionStateTag::Loading,
            ConnectionState::Connected(_) => ConnectionStateTag::Connected,
            ConnectionState::Error(_) => ConnectionStateTag::Error,
        }
    }

    fn color(self) -> Color {
        match self {
            ConnectionStateTag::Disconnected => Color::Muted,
            ConnectionStateTag::Loading => Color::Info,
            ConnectionStateTag::Connected => Color::Success,
            ConnectionStateTag::Error => Color::Error,
        }
    }

    fn label(self) -> &'static str {
        match self {
            ConnectionStateTag::Disconnected => "Disconnected",
            ConnectionStateTag::Loading => "Connecting",
            ConnectionStateTag::Connected => "Connected",
            ConnectionStateTag::Error => "Error",
        }
    }
}

// ── DbPanel ───────────────────────────────────────────────────────────────────

pub struct DbPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    db_store: Entity<DbStore>,
    width: Option<ui::Pixels>,
    list_state: ListState,
    entries: Vec<ListEntry>,
    filter_editor: Entity<Editor>,
    /// Connections whose subtree is collapsed.
    collapsed_connections: Vec<Uuid>,
    /// Databases (connection_id, database_name) that are collapsed.
    collapsed_databases: Vec<(Uuid, String)>,
    /// Categories (connection_id, database_name, kind) that are collapsed.
    collapsed_categories: Vec<(Uuid, String, CategoryKind)>,
    context_menu: Option<(Entity<ContextMenu>, Point<ui::Pixels>, Subscription)>,
    pending_serialization: Task<Option<()>>,
    _store_observer: Subscription,
}

impl DbPanel {
    fn new(
        workspace: &Workspace,
        db_store: Entity<DbStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search databases…", window, cx);
            editor
        });

        cx.subscribe(&filter_editor, |this: &mut Self, _, event, cx| {
            if let editor::EditorEvent::BufferEdited = event {
                this.rebuild_entries(cx);
            }
        })
        .detach();

        let store_observer = cx.observe_in(&db_store, window, |this, _store, _window, cx| {
            this.rebuild_entries(cx);
        });

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace: workspace.weak_handle(),
            db_store,
            width: None,
            list_state: ListState::new(0, ListAlignment::Top, px(1000.)),
            entries: Vec::new(),
            filter_editor,
            collapsed_connections: Vec::new(),
            collapsed_databases: Vec::new(),
            collapsed_categories: Vec::new(),
            context_menu: None,
            pending_serialization: Task::ready(None),
            _store_observer: store_observer,
        };
        this.rebuild_entries(cx);
        this
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let serialization_key = workspace
            .read_with(&mut cx, |workspace, _| Self::serialization_key(workspace))
            .ok()
            .flatten();

        // Load serialized state from KV store.
        let serialized = match serialization_key {
            Some(key) => {
                cx.background_spawn(async move {
                    KEY_VALUE_STORE
                        .read_kvp(&key)
                        .log_err()
                        .flatten()
                        .and_then(|s| serde_json::from_str::<SerializedDbPanel>(&s).log_err())
                })
                .await
            }
            None => None,
        };

        workspace.update_in(&mut cx, |workspace, window, cx| {
            let db_store = DbStore::new(cx);
            let panel = cx.new(|cx| DbPanel::new(workspace, db_store.clone(), window, cx));

            // Restore persisted state
            if let Some(ser) = serialized {
                panel.update(cx, |panel, cx| {
                    panel.width = ser.width.map(|w| w.round());
                    if let Some(collapsed) = ser.collapsed_connections {
                        panel.collapsed_connections = collapsed
                            .iter()
                            .filter_map(|s| Uuid::parse_str(s).ok())
                            .collect();
                    }
                    if let Some(dbs) = ser.collapsed_databases {
                        panel.collapsed_databases = dbs
                            .iter()
                            .filter_map(|(id_str, db_name)| {
                                let id = Uuid::parse_str(id_str).ok()?;
                                Some((id, db_name.clone()))
                            })
                            .collect();
                    }
                    if let Some(cats) = ser.collapsed_categories {
                        panel.collapsed_categories = cats
                            .iter()
                            .filter_map(|(id_str, db_name, kind_str)| {
                                let id = Uuid::parse_str(id_str).ok()?;
                                let kind = CategoryKind::from_str(kind_str)?;
                                Some((id, db_name.clone(), kind))
                            })
                            .collect();
                    }
                    panel.rebuild_entries(cx);
                });
            }

            // Load configs from disk then immediately connect
            db_store.update(cx, |store, cx| {
                store.load(cx);
            });
            db_store.update(cx, |store, cx| {
                store.connect_all(cx);
            });

            panel
        })
    }

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{}-{}", DB_PANEL_KEY, id))
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self
            .workspace
            .read_with(cx, |ws, _| DbPanel::serialization_key(ws))
            .ok()
            .flatten()
        else {
            return;
        };
        let width = self.width;
        let collapsed_connections: Vec<String> = self
            .collapsed_connections
            .iter()
            .map(|id| id.to_string())
            .collect();
        let collapsed_databases: Vec<(String, String)> = self
            .collapsed_databases
            .iter()
            .map(|(id, db)| (id.to_string(), db.clone()))
            .collect();
        let collapsed_categories: Vec<(String, String, String)> = self
            .collapsed_categories
            .iter()
            .map(|(id, db, kind)| (id.to_string(), db.clone(), kind.as_str().to_string()))
            .collect();

        self.pending_serialization = cx.background_spawn(
            async move {
                KEY_VALUE_STORE
                    .write_kvp(
                        key,
                        serde_json::to_string(&SerializedDbPanel {
                            width,
                            collapsed_connections: Some(collapsed_connections),
                            collapsed_databases: Some(collapsed_databases),
                            collapsed_categories: Some(collapsed_categories),
                        })?,
                    )
                    .await?;
                anyhow::Ok(())
            }
            .log_err(),
        );
    }

    // ── Entry building ─────────────────────────────────────────────────────────

    pub(crate) fn rebuild_entries(&mut self, cx: &mut Context<Self>) {
        // Preserve current scroll position to avoid jumping to top on every toggle.
        let previous_offset = self.list_state.logical_scroll_top();

        self.entries.clear();

        let store = self.db_store.read(cx);
        let query = self
            .filter_editor
            .read(cx)
            .text(cx)
            .to_lowercase();
        let has_query = !query.is_empty();
        if store.connections().is_empty() {
            self.entries.push(ListEntry::EmptyState);
            self.list_state.reset(self.entries.len());
            cx.notify();
            return;
        }

        for conn in store.connections() {
            let id = conn.config.id;
            let tag = ConnectionStateTag::from_state(&conn.state);

            let mut connection_matches_query = true;
            if has_query {
                let conn_name = conn.config.name.to_lowercase();
                let mut found = conn_name.contains(&query);

                if !found {
                    if let ConnectionState::Connected(schema) = &conn.state {
                        for db in schema.iter_databases() {
                            if db.name.to_lowercase().contains(&query) {
                                found = true;
                                break;
                            }
                            found = db.tables.iter().any(|t| t.to_lowercase().contains(&query))
                                || db.views.iter().any(|v| v.to_lowercase().contains(&query))
                                || db.functions.iter().any(|f| f.to_lowercase().contains(&query));
                            if found {
                                break;
                            }
                        }
                    }
                }

                connection_matches_query = found;
            }

            if has_query && !connection_matches_query {
                continue;
            }

            self.entries.push(ListEntry::Connection {
                id,
                name: conn.config.name.clone().into(),
                kind: conn.config.kind,
                state: tag,
            });

            // Only expand if not collapsed
            if self.collapsed_connections.contains(&id) {
                continue;
            }

            // Only show schema items when connected
            let schema = match &conn.state {
                ConnectionState::Connected(s) => Some(s),
                _ => None,
            };

            let connection_name_matches = has_query
                && conn
                    .config
                    .name
                    .to_lowercase()
                    .contains(&query);

            for db in schema.map(|s| s.iter_databases()).into_iter().flatten() {
                let db_name = db.name.clone();

                if has_query
                    && !connection_name_matches
                    && !db_name.to_lowercase().contains(&query)
                    && !db.tables.iter().any(|t| t.to_lowercase().contains(&query))
                    && !db.views.iter().any(|v| v.to_lowercase().contains(&query))
                    && !db.functions.iter().any(|f| f.to_lowercase().contains(&query))
                {
                    continue;
                }

                self.entries.push(ListEntry::Database {
                    connection_id: id,
                    name: db_name.clone().into(),
                });

                if self.collapsed_databases.contains(&(id, db_name.clone())) {
                    continue;
                }

                let database_matches = has_query && db_name.to_lowercase().contains(&query);

                for category in [
                    CategoryKind::Tables,
                    CategoryKind::Views,
                    CategoryKind::Functions,
                ] {
                    let items: &[String] = match category {
                        CategoryKind::Tables => &db.tables,
                        CategoryKind::Views => &db.views,
                        CategoryKind::Functions => &db.functions,
                    };

                    self.entries.push(ListEntry::CategoryHeader {
                        connection_id: id,
                        database_name: db_name.clone().into(),
                        kind: category,
                        count: items.len(),
                    });

                    if has_query
                        && !connection_name_matches
                        && !database_matches
                        && !items
                            .iter()
                            .any(|name| name.to_lowercase().contains(&query))
                    {
                        continue;
                    }

                    if self.collapsed_categories.contains(&(id, db_name.clone(), category)) {
                        continue;
                    }

                    if items.is_empty() {
                        self.entries.push(ListEntry::CategoryEmpty {
                            connection_id: id,
                            database_name: db_name.clone().into(),
                            kind: category,
                        });
                    } else {
                        let last_ix = items.len().saturating_sub(1);
                        for (i, item_name) in items.iter().enumerate() {
                            self.entries.push(ListEntry::SchemaItem {
                                connection_id: id,
                                database_name: db_name.clone().into(),
                                category,
                                name: item_name.clone().into(),
                                is_last: i == last_ix,
                            });
                        }
                    }
                }
            }
        }

        self.list_state.reset(self.entries.len());
        self.list_state.scroll_to(previous_offset);
        cx.notify();
    }

    // ── Toggle helpers ─────────────────────────────────────────────────────────

    fn toggle_connection(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if let Some(pos) = self.collapsed_connections.iter().position(|c| *c == id) {
            self.collapsed_connections.swap_remove(pos);
        } else {
            self.collapsed_connections.push(id);
        }
        self.rebuild_entries(cx);
        self.serialize(cx);
    }

    fn toggle_database(&mut self, conn_id: Uuid, database_name: String, cx: &mut Context<Self>) {
        let key = (conn_id, database_name);
        if let Some(pos) = self.collapsed_databases.iter().position(|k| *k == key) {
            self.collapsed_databases.swap_remove(pos);
        } else {
            self.collapsed_databases.push(key);
        }
        self.rebuild_entries(cx);
        self.serialize(cx);
    }

    fn toggle_category(
        &mut self,
        conn_id: Uuid,
        database_name: String,
        kind: CategoryKind,
        cx: &mut Context<Self>,
    ) {
        let key = (conn_id, database_name, kind);
        if let Some(pos) = self.collapsed_categories.iter().position(|k| *k == key) {
            self.collapsed_categories.swap_remove(pos);
        } else {
            self.collapsed_categories.push(key);
        }
        self.rebuild_entries(cx);
        self.serialize(cx);
    }

    // ── Modal / context menu ───────────────────────────────────────────────────

    fn open_new_database_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let db_store = self.db_store.clone();
        let db_panel = cx.entity();

        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                NewDatabaseModal::new(db_store, db_panel, window, cx)
            });
        });
    }

    fn deploy_connection_context_menu(
        &mut self,
        position: Point<ui::Pixels>,
        conn_id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();

        let menu = ContextMenu::build(window, cx, |menu, window, _cx| {
            menu.entry(
                "Refresh",
                None,
                window.handler_for(&this, move |this, _window, cx| {
                    this.db_store.update(cx, |store, cx| {
                        store.refresh_connection(conn_id, cx);
                    });
                }),
            )
            .separator()
            .entry(
                "Remove Connection",
                None,
                window.handler_for(&this, move |this, window, cx| {
                    this.db_store.update(cx, |store, cx| {
                        store.remove_connection(conn_id, window, cx);
                    });
                    this.serialize(cx);
                }),
            )
        });

        window.focus(&menu.focus_handle(cx), cx);
        let sub = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
            if this
                .context_menu
                .as_ref()
                .is_some_and(|m| m.0.focus_handle(cx).contains_focused(window, cx))
            {
                cx.focus_self(window);
            }
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, sub));
    }

    // ── Render helpers ─────────────────────────────────────────────────────────

    fn render_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let has_query = !self.filter_editor.read(cx).text(cx).is_empty();

        v_flex()
            .w_full()
            .gap_1()
            .child(
                h_flex()
                    .p_2()
                    .h(ui::Tab::container_height(cx))
                    .gap_1p5()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.render_filter_input(cx))
                    .when(has_query, |this| {
                        this.pr_2p5().child(
                            IconButton::new("clear-db-filter", IconName::Close)
                                .shape(IconButtonShape::Square)
                                .tooltip(Tooltip::text("Clear Filter"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.reset_filter_editor_text(window, cx);
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                ListHeader::new("Databases")
                    .inset(true)
                    .end_slot::<AnyElement>(Some(
                        IconButton::new("add-database", IconName::Plus)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_new_database_modal(window, cx);
                            }))
                            .tooltip(Tooltip::text("Add a database connection"))
                            .into_any_element(),
                    )),
            )
            .into_any_element()
    }

    fn render_empty_state(&mut self, cx: &mut Context<Self>) -> AnyElement {
        ListItem::new("db-empty-state")
            .indent_level(1)
            .indent_step_size(px(20.))
            .start_slot(
                Icon::new(IconName::Plus)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new("Add a database connection").color(Color::Muted))
            .on_click(cx.listener(|this, _, window, cx| {
                this.open_new_database_modal(window, cx);
            }))
            .into_any_element()
    }

    fn reset_filter_editor_text(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.filter_editor.update(cx, |editor, cx| {
            if editor.buffer().read(cx).len(cx).0 > 0 {
                editor.set_text("", window, cx);
                true
            } else {
                false
            }
        })
    }

    fn render_filter_input(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: if self.filter_editor.read(cx).read_only(cx) {
                cx.theme().colors().text_disabled
            } else {
                cx.theme().colors().text
            },
            font_family: settings.ui_font.family.clone(),
            font_features: settings.ui_font.features.clone(),
            font_fallbacks: settings.ui_font.fallbacks.clone(),
            font_size: rems(0.875).into(),
            font_weight: settings.ui_font.weight,
            font_style: FontStyle::Normal,
            line_height: relative(1.3),
            ..Default::default()
        };

        EditorElement::new(
            &self.filter_editor,
            EditorStyle {
                local_player: cx.theme().players().local(),
                text: text_style,
                ..Default::default()
            },
        )
        .into_any_element()
    }

    fn render_connection(
        &mut self,
        id: Uuid,
        name: SharedString,
        kind: DbKind,
        state: ConnectionStateTag,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let disclosed = !self.collapsed_connections.contains(&id);

        div()
            .id(format!("db-connection-row-{id}"))
            .group("")
            .relative()
            .flex()
            .w_full()
            .h_6()
            .child(
                ListItem::new(("db-connection", id.as_u128() as usize))
                    .indent_level(1)
                    .indent_step_size(px(20.))
                    .toggle(Some(disclosed))
                    .on_toggle(cx.listener(move |this, _, _window, cx| {
                        this.toggle_connection(id, cx);
                    }))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.toggle_connection(id, cx);
                    }))
                    .on_secondary_mouse_down(cx.listener(
                        move |this, event: &MouseDownEvent, window, cx| {
                            this.deploy_connection_context_menu(event.position, id, window, cx);
                        },
                    ))
                    .start_slot({
                        let base_icon = match kind {
                            DbKind::Postgres => {
                                Icon::from_external_svg("crates/db_ui/db_icons/postgresql.svg".into())
                            }
                            DbKind::MySql => {
                                Icon::from_external_svg("crates/db_ui/db_icons/mysql.svg".into())
                            }
                            DbKind::MariaDb => {
                                Icon::from_external_svg("crates/db_ui/db_icons/mariadb.svg".into())
                            }
                        }
                        .size(IconSize::Small)
                        .color(if state == ConnectionStateTag::Error {
                            Color::Error
                        } else {
                            Color::Accent
                        });

                        div()
                            .relative()
                            .child(base_icon)
                            .child(
                                div()
                                    .w_1p5()
                                    .absolute()
                                    .right(px(-1.))
                                    .top(px(-1.))
                                    .child(Indicator::dot().color(state.color())),
                            )
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .child(Label::new(name)),
                    ),
            )
            .child(
                h_flex().absolute().right(px(0.)).h_full().child(
                    h_flex()
                        .h_full()
                        .bg(cx.theme().colors().background)
                        .rounded_l_sm()
                        .gap_1()
                        .px_1()
                        .child(
                            IconButton::new(
                                format!("refresh-connection-{id}"),
                                IconName::ArrowCircle,
                            )
                            .style(ButtonStyle::Filled)
                            .shape(IconButtonShape::Square)
                            .icon_size(IconSize::Small)
                            .icon_color(state.color())
                            .disabled(state == ConnectionStateTag::Loading)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.db_store.update(cx, |store, cx| {
                                    store.refresh_connection(id, cx);
                                });
                            }))
                            .tooltip(Tooltip::text("Refresh connection")),
                        )
                        .visible_on_hover(""),
                ),
            )
            .into_any_element()
    }

    fn render_category_header(
        &mut self,
        conn_id: Uuid,
        database_name: SharedString,
        kind: CategoryKind,
        count: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let db_str = database_name.to_string();
        let collapsed = self
            .collapsed_categories
            .contains(&(conn_id, db_str.clone(), kind));
        let disclosed = !collapsed;

        let db_str_for_toggle = db_str.clone();
        let db_str_for_click = db_str.clone();

        div()
            .w_full()
            .h_6()
            .group("")
            .child(
                ListItem::new(format!("db-cat-{}-{}-{}", conn_id, db_str, kind.as_str()))
                    .indent_level(3)
                    .indent_step_size(px(20.))
                    .toggle(Some(disclosed))
                    .on_toggle(cx.listener({
                        let db_str = db_str_for_toggle.clone();
                        move |this, _, _window, cx| {
                            this.toggle_category(conn_id, db_str.clone(), kind, cx);
                        }
                    }))
                    .on_click(cx.listener({
                        let db_str = db_str_for_click.clone();
                        move |this, _, _window, cx| {
                            this.toggle_category(conn_id, db_str.clone(), kind, cx);
                        }
                    }))
                    .start_slot(
                        Icon::new(kind.icon())
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(Label::new(kind.label()))
                                    .when(count > 0, |row| {
                                        row.child(
                                            Label::new(format!("({})", count))
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_category_empty(
        &mut self,
        _conn_id: Uuid,
        _database_name: SharedString,
        kind: CategoryKind,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = match kind {
            CategoryKind::Tables => "No tables found",
            CategoryKind::Views => "No views found",
            CategoryKind::Functions => "No functions found",
        };

        div()
            .w_full()
            .h_6()
            .child(
                ListItem::new(format!("db-empty-{}", kind.as_str()))
                    .indent_level(4)
                    .indent_step_size(px(20.))
                    .selectable(false)
                    .child(Label::new(label).color(Color::Muted)),
            )
            .into_any_element()
    }

    fn render_database(
        &mut self,
        conn_id: Uuid,
        name: SharedString,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let disclosed = !self.collapsed_databases.contains(&(conn_id, name.to_string()));

        div()
            .w_full()
            .h_6()
            .group("")
            .child(
                ListItem::new(format!("db-database-{}", name))
                    .indent_level(2)
                    .indent_step_size(px(20.))
                    .toggle(Some(disclosed))
                    .on_toggle(cx.listener({
                        let value = name.clone();
                        move |this, _, _window, cx| {
                            this.toggle_database(conn_id, value.to_string(), cx);
                        }
                    }))
                    .on_click(cx.listener({
                        let value = name.clone();
                        move |this, _, _window, cx| {
                            this.toggle_database(conn_id, value.to_string(), cx);
                        }
                    }))
                    .start_slot(
                        Icon::new(IconName::DatabaseZap)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(Label::new(name)),
            )
            .into_any_element()
    }

    fn render_schema_item(
        &mut self,
        conn_id: Uuid,
        database_name: SharedString,
        category: CategoryKind,
        name: SharedString,
        _is_last: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let list_item = ListItem::new(format!(
            "db-item-{}-{}-{}-{}",
            conn_id,
            database_name,
            category.as_str(),
            name
        ))
        .indent_level(4)
        .indent_step_size(px(20.))
        .selectable(true)
        .start_slot(
            Icon::new(match category {
                CategoryKind::Tables => IconName::ListTree,
                CategoryKind::Views => IconName::Eye,
                CategoryKind::Functions => IconName::Code,
            })
            .size(IconSize::XSmall)
            .color(Color::Muted),
        )
        .child(Label::new(name.clone()));

        let list_item = if category.as_query_target().is_some() {
            list_item.on_click(cx.listener(move |this, _, window, cx| {
                this.open_schema_item(conn_id, database_name.clone(), category, name.clone(), window, cx);
            }))
        } else {
            list_item
        };

        div().w_full().h_6().child(list_item).into_any_element()
    }

    fn open_schema_item(
        &mut self,
        conn_id: Uuid,
        database_name: SharedString,
        category: CategoryKind,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target_kind) = category.as_query_target() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(connection) = self
            .db_store
            .read(cx)
            .connection_by_id(conn_id)
            .map(|connection| connection.config.clone())
        else {
            return;
        };

        DbQueryView::open(
            workspace,
            self.db_store.clone(),
            connection,
            database_name.to_string(),
            target_kind,
            name.to_string(),
            window,
            cx,
        );
    }

    fn render_list_entry(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(entry) = self.entries.get(ix).cloned() else {
            return div().into_any_element();
        };

        match entry {
            ListEntry::EmptyState => self.render_empty_state(cx),
            ListEntry::Connection {
                id,
                name,
                kind,
                state,
            } => self.render_connection(id, name, kind, state, window, cx),
            ListEntry::Database {
                connection_id,
                name,
            } => self.render_database(connection_id, name, window, cx),
            ListEntry::CategoryHeader {
                connection_id,
                database_name,
                kind,
                count,
            } => self.render_category_header(connection_id, database_name, kind, count, window, cx),
            ListEntry::CategoryEmpty {
                connection_id,
                database_name,
                kind,
            } => self.render_category_empty(connection_id, database_name, kind, window, cx),
            ListEntry::SchemaItem {
                connection_id,
                database_name,
                category,
                name,
                is_last,
            } => self.render_schema_item(
                connection_id,
                database_name,
                category,
                name,
                is_last,
                window,
                cx,
            ),
        }
    }
}

// ── GPUI trait impls ──────────────────────────────────────────────────────────

impl Focusable for DbPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DbPanel {}

impl Panel for DbPanel {
    fn persistent_name() -> &'static str {
        "DbPanel"
    }

    fn panel_key() -> &'static str {
        DB_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn size(&self, _window: &Window, _cx: &App) -> ui::Pixels {
        self.width.unwrap_or(ui::px(300.0))
    }

    fn set_size(&mut self, size: Option<ui::Pixels>, _window: &mut Window, cx: &mut Context<Self>) {
        self.width = size;
        self.serialize(cx);
        cx.notify();
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::DatabaseZap)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Database Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        3
    }
}

impl Render for DbPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                v_flex()
                    .size_full()
                    .gap_1()
                    .child(self.render_header(cx))
                    .child(
                        div()
                            .size_full()
                            .flex_grow()
                            .child(
                                list(
                                    self.list_state.clone(),
                                    cx.processor(Self::render_list_entry),
                                )
                                .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                                .size_full(),
                            )
                            .vertical_scrollbar_for(&self.list_state, window, cx),
                    ),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                gpui::deferred(
                    gpui::anchored()
                        .position(*position)
                        .anchor(gpui::Corner::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

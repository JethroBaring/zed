mod add_database_modal;
mod db_panel;
mod db_query_view;
pub mod db_store;
mod edit_cell_modal;

pub use db_panel::DbPanel;
pub use db_store::DbStore;

pub fn init(cx: &mut gpui::App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _, _cx| {
        db_panel::register(workspace);
    })
    .detach();
}

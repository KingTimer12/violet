//! The folders a database opens into in the tree (schemas, triggers, roles,
//! extensions), and choosing which schemas it shows.

use std::collections::BTreeSet;
use std::sync::Arc;

use egui::RichText;
use violet_app::{CachePolicy, ExecOptions};
use violet_core::{ConnectionId, Language, QueryOutput, Result, ResultSet, TaskKind};

use super::VioletApp;
use super::action::Action;
use super::definition_flow::Family;
use super::icons::Icon;
use super::state::{Explorer, Folder, FolderItem, Loadable, TreeFilter};
use super::theme::{self, Palette};
use super::toasts::Toast;
use super::widgets::{self, Tone};

/// Whether a run of `sql` leaves the loaded catalog behind, so it is loaded
/// again: SQL that may have changed the schema, even if the script failed
/// after some of its statements ran.
pub fn run_reloads_catalog(language: Language, sql: &str, succeeded: bool) -> bool {
    language == Language::Sql && crate::sql::may_have_changed_schema(sql, succeeded)
}

/// Whether a task changes the database's catalog however it ends: a restore
/// cancelled or failed partway has still created some of it.
pub fn task_reloads_catalog(kind: TaskKind) -> bool {
    kind == TaskKind::Restore
}

/// The folders a database of `family` shows, in order.
pub fn folders(family: Family) -> &'static [Folder] {
    match family {
        Family::Postgres => &[
            Folder::Schemas,
            Folder::Triggers,
            Folder::Roles,
            Folder::Extensions,
        ],
        Family::MySql => &[Folder::Schemas, Folder::Triggers, Folder::Roles],
        Family::Sqlite | Family::Other => &[Folder::Schemas, Folder::Triggers],
    }
}

/// The query that lists a folder. Triggers come as name, schema, table;
/// the rest as name and a detail.
fn folder_sql(family: Family, folder: Folder) -> Option<&'static str> {
    Some(match (family, folder) {
        (_, Folder::Schemas) => return None,
        (Family::Postgres, Folder::Triggers) => {
            "SELECT t.tgname, n.nspname, c.relname \
             FROM pg_catalog.pg_trigger t \
             JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE NOT t.tgisinternal \
             ORDER BY 2, 3, 1"
        }
        (Family::Postgres, Folder::Roles) => {
            "SELECT rolname, CASE WHEN rolsuper THEN 'superuser' \
             WHEN rolcanlogin THEN 'login' END \
             FROM pg_catalog.pg_roles \
             WHERE rolname !~ '^pg_' \
             ORDER BY 1"
        }
        (Family::Postgres, Folder::Extensions) => {
            "SELECT extname, extversion FROM pg_catalog.pg_extension ORDER BY 1"
        }
        // The session's database alone; without one, every user database.
        (Family::MySql, Folder::Triggers) => {
            "SELECT TRIGGER_NAME, TRIGGER_SCHEMA, EVENT_OBJECT_TABLE \
             FROM information_schema.TRIGGERS \
             WHERE COALESCE(TRIGGER_SCHEMA = DATABASE(), \
             TRIGGER_SCHEMA NOT IN ('mysql', 'sys', 'performance_schema', 'information_schema')) \
             ORDER BY 2, 3, 1"
        }
        (Family::MySql, Folder::Roles) => {
            "SELECT CONCAT(User, '@', Host), NULL FROM mysql.user ORDER BY 1"
        }
        (Family::Sqlite, Folder::Triggers) => {
            "SELECT name, 'main', tbl_name FROM main.sqlite_schema \
             WHERE type = 'trigger' \
             ORDER BY 3, 1"
        }
        (Family::Other, Folder::Triggers) => {
            "SELECT DISTINCT trigger_name, trigger_schema, event_object_table \
             FROM information_schema.triggers \
             ORDER BY 2, 3, 1"
        }
        (Family::MySql | Family::Sqlite | Family::Other, Folder::Extensions)
        | (Family::Sqlite | Family::Other, Folder::Roles) => {
            return None;
        }
    })
}

/// A folder's entries from its query's rows.
fn folder_items(folder: Folder, set: &ResultSet) -> Vec<FolderItem> {
    set.rows
        .iter()
        .filter_map(|row| {
            let name = row.get(0).flatten()?.to_owned();
            let second = row.get(1).flatten().map(str::to_owned);
            Some(if folder == Folder::Triggers {
                let table = row.get(2).flatten().map(str::to_owned);
                FolderItem {
                    detail: table.clone(),
                    name,
                    schema: second,
                    table,
                }
            } else {
                FolderItem {
                    name,
                    detail: second,
                    schema: None,
                    table: None,
                }
            })
        })
        .collect()
}

impl VioletApp {
    /// Lists a folder of `connection`'s database.
    pub(super) fn load_folder(&mut self, connection: ConnectionId, folder: Folder) {
        let Some(sql) = folder_sql(self.family(connection), folder) else {
            return;
        };
        if self.wait_for_connection(connection, || Action::LoadFolder(connection, folder)) {
            return;
        }
        self.explorer
            .folders
            .insert((connection, folder), Loadable::Loading);
        let options = ExecOptions {
            cache: CachePolicy::Refresh,
            page: None,
        };
        let request = self.backend.execute(connection, sql.to_owned(), options);
        self.folder_requests.insert(request, (connection, folder));
    }

    pub(super) fn folder_read(
        &mut self,
        (connection, folder): (ConnectionId, Folder),
        result: Result<QueryOutput>,
    ) {
        let loaded = result.map(|output| {
            let items = output
                .sets
                .iter()
                .find(|set| set.has_rows())
                .map(|set| folder_items(folder, set))
                .unwrap_or_default();
            Arc::<[FolderItem]>::from(items)
        });
        self.explorer.folders.insert(
            (connection, folder),
            match loaded {
                Ok(items) => Loadable::Ready(items),
                Err(error) => Loadable::Failed(error.message),
            },
        );
    }

    /// Opens the dialog that picks which schemas the tree shows for a
    /// session's database.
    pub(super) fn choose_schemas(&mut self, ctx: &egui::Context, session: ConnectionId) {
        let Some(Loadable::Ready(all)) = self.explorer.schemas.get(&session) else {
            self.toasts
                .push(ctx, Toast::info("The schemas are still loading."));
            return;
        };
        let Some(scope) = self.explorer.schema_scope(session) else {
            return;
        };
        let all = all.to_vec();
        let picked = self.settings.schema_filters.get(&scope).cloned();
        self.chooser = Some(Chooser::new(TreeFilter::Schemas, scope, all, picked));
    }

    /// Opens the dialog that picks which databases the tree shows for a
    /// saved connection that lists them all: a server's, or a key-value
    /// store's numbered ones.
    pub(super) fn choose_databases(&mut self, ctx: &egui::Context, parent: ConnectionId) {
        let list = if self.language(parent) == violet_core::Language::Redis {
            self.explorer.schemas.get(&parent)
        } else {
            self.explorer.databases.get(&parent)
        };
        let Some(Loadable::Ready(all)) = list else {
            self.toasts
                .push(ctx, Toast::info("The databases are still loading."));
            return;
        };
        let scope = Explorer::database_scope(parent);
        let all = all.to_vec();
        let picked = self.settings.database_filters.get(&scope).cloned();
        self.chooser = Some(Chooser::new(TreeFilter::Databases, scope, all, picked));
    }

    /// Keeps the schemas or databases picked; all of them means no choice
    /// at all.
    pub(super) fn set_tree_filter(
        &mut self,
        ctx: &egui::Context,
        kind: TreeFilter,
        scope: String,
        picked: Option<BTreeSet<String>>,
    ) {
        let filters = match kind {
            TreeFilter::Schemas => &mut self.settings.schema_filters,
            TreeFilter::Databases => &mut self.settings.database_filters,
        };
        match picked {
            Some(picked) => filters.insert(scope, picked),
            None => filters.remove(&scope),
        };
        self.explorer
            .schema_filters
            .clone_from(&self.settings.schema_filters);
        self.explorer
            .database_filters
            .clone_from(&self.settings.database_filters);
        self.save_settings(ctx);
    }
}

/// Picking the schemas a database shows, or the databases a connection
/// shows.
pub struct Chooser {
    kind: TreeFilter,
    scope: String,
    all: Vec<String>,
    picked: BTreeSet<String>,
    filter: String,
}

impl Chooser {
    /// Starts from the names picked before, or all of them.
    fn new(
        kind: TreeFilter,
        scope: String,
        all: Vec<String>,
        picked: Option<BTreeSet<String>>,
    ) -> Self {
        let picked = picked.unwrap_or_else(|| all.iter().cloned().collect());
        Self {
            kind,
            scope,
            all,
            picked,
            filter: String::new(),
        }
    }
}

/// Schemas and databases every PostgreSQL or MySQL server has, which few
/// people browse.
fn is_system_name(name: &str) -> bool {
    name.starts_with("pg_")
        || matches!(
            name,
            "information_schema"
                | "mysql"
                | "sys"
                | "performance_schema"
                | "template0"
                | "template1"
        )
}

/// Shows the dialog. Returns whether it is still open.
pub fn show_chooser(ctx: &egui::Context, chooser: &mut Chooser, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let (title, noun, hide) = match chooser.kind {
        TreeFilter::Schemas => ("Schemas to show", "schemas", "Hide system schemas"),
        TreeFilter::Databases => ("Databases to show", "databases", "Hide system databases"),
    };
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("tree-chooser"))
        .frame(super::edit_flow::frame(&palette))
        .show(ctx, |ui| {
            ui.set_width(420.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            ui.label(
                RichText::new(title)
                    .font(theme::semibold(16.0))
                    .color(palette.text),
            );
            ui.label(
                RichText::new(format!(
                    "{} of {} picked. The others only leave the tree: the editor still reaches them.",
                    chooser.picked.len(),
                    chooser.all.len()
                ))
                .color(palette.dim),
            );
            ui.add(widgets::text_input(&mut chooser.filter, &format!("Filter {noun}")));
            ui.horizontal(|ui| {
                if widgets::button(ui, Tone::Ghost, None, "All").clicked() {
                    chooser.picked = chooser.all.iter().cloned().collect();
                }
                if widgets::button(ui, Tone::Ghost, None, "None").clicked() {
                    chooser.picked.clear();
                }
                if widgets::button(ui, Tone::Ghost, None, hide).clicked() {
                    chooser.picked.retain(|name| !is_system_name(name));
                }
            });
            let filter = chooser.filter.trim().to_lowercase();
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    for name in &chooser.all {
                        if !filter.is_empty() && !name.to_lowercase().contains(&filter) {
                            continue;
                        }
                        let mut on = chooser.picked.contains(name);
                        if widgets::checkbox(ui, &mut on, name).changed() {
                            if on {
                                chooser.picked.insert(name.clone());
                            } else {
                                chooser.picked.remove(name);
                            }
                        }
                    }
                });
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let some = !chooser.picked.is_empty();
                    let save = ui
                        .add_enabled_ui(some, |ui| {
                            widgets::button(ui, Tone::Primary, Some(Icon::Check), "Save")
                        })
                        .inner
                        .on_disabled_hover_text(format!("Pick at least one of the {noun}"));
                    if save.clicked() {
                        let all = chooser.picked.len() == chooser.all.len();
                        let picked = (!all).then(|| chooser.picked.clone());
                        actions.push(Action::SetTreeFilter(
                            chooser.kind,
                            chooser.scope.clone(),
                            picked,
                        ));
                        open = false;
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        open = false;
                    }
                });
            });
        });
    open && !modal.should_close()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_has_schemas_first() {
        for family in [
            Family::Postgres,
            Family::MySql,
            Family::Sqlite,
            Family::Other,
        ] {
            assert_eq!(folders(family).first(), Some(&Folder::Schemas));
            for folder in folders(family).iter().skip(1) {
                assert!(
                    folder_sql(family, *folder).is_some(),
                    "{family:?} {folder:?}"
                );
            }
        }
    }

    #[test]
    fn runs_reload_the_catalog_when_the_schema_may_have_changed() {
        let sql = |language, text, ok| run_reloads_catalog(language, text, ok);
        assert!(sql(Language::Sql, "CREATE TABLE t (a int)", true));
        assert!(!sql(Language::Sql, "SELECT 1", true));
        // Only SQL has a schema to change.
        assert!(!sql(Language::Redis, "CREATE TABLE t (a int)", true));
        assert!(sql(
            Language::Sql,
            "DROP TABLE a; SELECT * FROM nope",
            false
        ));
        assert!(!sql(Language::Sql, "DROP TABLE a", false));
    }

    #[test]
    fn a_restore_reloads_the_catalog_however_it_ends() {
        assert!(task_reloads_catalog(TaskKind::Restore));
        assert!(!task_reloads_catalog(TaskKind::Backup));
    }

    #[test]
    fn triggers_carry_their_table() {
        let column = |name: &str| violet_core::ColumnMeta {
            name: name.into(),
            type_name: None,
            numeric: false,
        };
        let mut set = ResultSet {
            columns: vec![column("name"), column("schema"), column("table")],
            ..ResultSet::default()
        };
        set.rows
            .push([Some("audit"), Some("public"), Some("orders")]);
        let items = folder_items(Folder::Triggers, &set);
        assert_eq!(items[0].schema.as_deref(), Some("public"));
        assert_eq!(items[0].table.as_deref(), Some("orders"));
        assert!(is_system_name("pg_catalog") && is_system_name("template1"));
        assert!(!is_system_name("public"));
    }
}

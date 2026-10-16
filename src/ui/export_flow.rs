//! Exporting a tab's rows to a file. A result read a page at a time is read
//! whole first (up to the row limit), so the file holds every row, not just
//! the ones on screen.

use std::path::{Path, PathBuf};

use violet_app::ExecOptions;
use violet_core::{Dialect as _, Page, QueryOutput, Result, ResultSet};

use super::VioletApp;
use super::toasts::Toast;
use super::workspace::{Executed, Outcome, TabId, TabKind};
use crate::export::{self, Format};
use crate::ui::workspace::thousands;

/// An export waiting for the rest of the rows.
pub struct PendingExport {
    pub path: PathBuf,
    pub format: Format,
    pub title: String,
    /// The table `INSERT`s go into, quoted.
    pub table: String,
    pub dialect: violet_core::SqlDialect,
}

impl VioletApp {
    /// Asks where to write `tab_id`'s rows, then writes them, reading the
    /// rows not fetched yet first.
    pub(super) fn export(&mut self, ctx: &egui::Context, tab_id: TabId, format: Format) {
        let Some(tab) = self.workspace.get(tab_id) else {
            return;
        };
        let Some(Outcome::Done { output }) = &tab.outcome else {
            return;
        };
        let Some(set) = output.sets.get(tab.active_set) else {
            return;
        };
        if !set.has_rows() {
            return;
        }
        let dialect = tab
            .connection
            .map_or(violet_core::SqlDialect::ANSI, |id| self.dialect(id));
        let table = match (&tab.kind, &tab.source) {
            (TabKind::Data(view), _) => format!(
                "{}.{}",
                dialect.quote_ident(&view.schema),
                dialect.quote_ident(&view.relation)
            ),
            (TabKind::Script, Some(source)) => {
                let name = dialect.quote_ident(&source.name.text);
                match &source.schema {
                    Some(schema) => format!("{}.{name}", dialect.quote_ident(&schema.text)),
                    None => name,
                }
            }
            (TabKind::Script, None) => dialect.quote_ident("exported_rows"),
        };
        let title = match &tab.kind {
            TabKind::Data(view) => view.relation.clone(),
            TabKind::Script => tab.title.trim_end_matches(".sql").to_owned(),
        };

        let mut picker = rfd::FileDialog::new()
            .set_title(format!("Export rows as {}", format.label()))
            .add_filter(format.label(), &[format.extension()])
            .set_file_name(format!("{}.{}", file_stem(&title), format.extension()));
        let last = PathBuf::from(&self.settings.export_folder);
        if !self.settings.export_folder.is_empty() && last.is_dir() {
            picker = picker.set_directory(last);
        }
        let Some(mut path) = picker.save_file() else {
            return;
        };
        if path.extension().is_none() {
            path.set_extension(format.extension());
        }
        if let Some(folder) = path.parent().and_then(Path::to_str) {
            folder.clone_into(&mut self.settings.export_folder);
            self.save_settings(ctx);
        }
        let pending = PendingExport {
            path,
            format,
            title,
            table,
            dialect,
        };

        // A single query read a page at a time: read all of it first.
        let tab = self.workspace.get(tab_id);
        let more = tab
            .and_then(|tab| tab.paging.as_ref().map(|paging| (tab, paging)))
            .filter(|(_, paging)| paging.has_more);
        if let Some((tab, paging)) = more
            && let Some(connection) = tab.connection
        {
            let executed = tab.last_run.as_ref().map_or_else(
                || Executed {
                    offset: 0,
                    sql: paging.base.clone(),
                    shift: 0,
                },
                |(executed, _)| executed.clone(),
            );
            let options = ExecOptions {
                page: Some(Page {
                    offset: 0,
                    limit: self.backend.max_rows() as u64,
                }),
                ..paging.options
            };
            let request = self.backend.execute(connection, executed.sql, options);
            self.exports.insert(request, pending);
            self.toasts
                .push(ctx, Toast::info("Reading every row to export…"));
            return;
        }
        let Some(tab) = self.workspace.get(tab_id) else {
            return;
        };
        let Some(Outcome::Done { output }) = &tab.outcome else {
            return;
        };
        let Some(set) = output.sets.get(tab.active_set) else {
            return;
        };
        let toast = write(&pending, set, false);
        self.toasts.push(ctx, toast);
    }

    /// Writes an export once its rows are read.
    pub(super) fn export_read(
        &mut self,
        ctx: &egui::Context,
        pending: PendingExport,
        result: Result<QueryOutput>,
    ) {
        let toast = match result {
            Ok(output) => {
                let more = output.page.is_some_and(|page| page.has_more);
                match output.sets.iter().find(|set| set.has_rows()) {
                    Some(set) => write(&pending, set, more),
                    None => {
                        Toast::error("Could not export the rows", "The query returned no rows.")
                    }
                }
            }
            Err(error) => Toast::error("Could not read the rows to export", error.message),
        };
        self.toasts.push(ctx, toast);
    }
}

/// Renders and writes the file; the toast says how it went.
fn write(pending: &PendingExport, set: &ResultSet, more: bool) -> Toast {
    let quote = |name: &str| pending.dialect.quote_ident(name);
    let options = export::Options {
        table: &pending.table,
        quote: &quote,
        title: &pending.title,
    };
    let written = export::render(set, pending.format, &options)
        .and_then(|bytes| Ok(std::fs::write(&pending.path, bytes)?));
    let name = pending.path.file_name().map_or_else(
        || pending.path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    match written {
        Err(error) => Toast::error("Could not export the rows", error.to_string()),
        Ok(()) if more || set.truncated => Toast::info(format!(
            "Exported the first {} rows to {name}: the row limit was reached",
            thousands(set.rows.len())
        )),
        Ok(()) => Toast::success(format!(
            "Exported {} rows to {name}",
            thousands(set.rows.len())
        )),
    }
}

/// `title` without characters file systems reject.
fn file_stem(title: &str) -> String {
    let stem: String = title
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let stem = stem.trim().trim_matches('.');
    if stem.is_empty() {
        "rows".to_owned()
    } else {
        stem.to_owned()
    }
}

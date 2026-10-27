//! Hints on a query's performance: refreshed as the text or the catalog
//! changes, and the indexes they need read on demand.

use violet_app::{CachePolicy, ExecOptions};
use violet_core::{ConnectionId, Language, QueryOutput, Result};

use super::VioletApp;
use super::action::Action;
use super::state::{ConnectionFacts, Explorer, Loadable};
use super::workspace::QueryTab;
use crate::dialect::Family;
use crate::hints::{self, Hint};

/// A tab's hints, and what they were made from.
#[derive(Default)]
pub struct Hints {
    text: String,
    revision: u64,
    pub list: Vec<Hint>,
}

/// Makes `tab`'s hints again when its text or the catalog changed, and asks
/// for the indexes they lack.
pub fn refresh(
    tab: &mut QueryTab,
    explorer: &Explorer,
    language: Language,
    family: Family,
    actions: &mut Vec<Action>,
) {
    let Some(connection) = tab.connection.filter(|_| language == Language::Sql) else {
        tab.hints.list.clear();
        return;
    };
    let revision = explorer.hint_rev;
    if !explorer.link(connection).is_online()
        || (tab.hints.text == tab.sql && tab.hints.revision == revision)
    {
        return;
    }
    let facts = ConnectionFacts {
        explorer,
        id: connection,
        family,
    };
    let analysis = hints::analyze(&tab.sql, &facts);
    for table in &analysis.missing {
        let (schema, name) = facts.resolve(table);
        let key = (connection, schema.clone(), name.clone());
        if !explorer.indexes.contains_key(&key) {
            actions.push(Action::LoadIndexes(
                connection,
                schema.clone(),
                name.clone(),
            ));
        }
        if !explorer.columns.contains_key(&key) {
            actions.push(Action::LoadColumns(connection, schema, name));
        }
    }
    tab.hints = Hints {
        text: tab.sql.clone(),
        revision,
        list: analysis.hints,
    };
}

impl VioletApp {
    /// Reads a table's indexes, unless they are read or on their way.
    pub(super) fn load_indexes(&mut self, connection: ConnectionId, schema: String, table: String) {
        let key = (connection, schema.clone(), table.clone());
        if self.explorer.indexes.contains_key(&key) {
            return;
        }
        let Some(sql) = self.family(connection).index_query(&schema, &table) else {
            return;
        };
        self.explorer.indexes.insert(key, Loadable::Loading);
        let options = ExecOptions {
            cache: CachePolicy::Refresh,
            page: None,
        };
        let request = self.backend.execute(connection, sql, options);
        self.index_requests
            .insert(request, (connection, schema, table));
    }

    /// A table's indexes arrived.
    pub(super) fn indexes_read(
        &mut self,
        table: (ConnectionId, String, String),
        result: &Result<QueryOutput>,
    ) {
        let loaded = match result {
            Ok(output) => Loadable::Ready(
                output
                    .sets
                    .first()
                    .map(hints::parse_indexes)
                    .unwrap_or_default()
                    .into(),
            ),
            Err(error) => Loadable::Failed(error.message.clone()),
        };
        self.explorer.indexes.insert(table, loaded);
        self.explorer.hint_rev += 1;
    }
}

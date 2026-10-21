//! What the editor needs to know of each language a connection's statements
//! are written in: SQL (`crate::sql`) or Redis (`violet_lang::redis`).

use std::ops::Range;

use violet_core::Language;
use violet_lang::redis;

use crate::sql;

/// What runs for the cursor at `cursor`.
pub fn statement_at(language: Language, text: &str, cursor: usize) -> Option<Range<usize>> {
    match language {
        Language::Redis => redis::statement_at(text, cursor),
        Language::Sql | Language::Mongodb => sql::statement_at(text, cursor),
    }
}

/// Whether `text` may carry a password.
pub fn sets_secret(language: Language, text: &str) -> bool {
    match language {
        Language::Redis => redis::sets_secret(text),
        Language::Sql | Language::Mongodb => sql::sets_secret(text),
    }
}

/// `text` as the history keeps it: passwords hidden.
pub fn redact_secrets(language: Language, text: &str) -> String {
    match language {
        Language::Redis => redis::redact_secrets(text),
        Language::Sql | Language::Mongodb => sql::redact_secrets(text),
    }
}

/// What in `text` the user confirms before it runs: SQL changing every
/// row of a table, Redis commands that flush, block or reconfigure the
/// server.
pub fn dangers(language: Language, text: &str) -> Vec<String> {
    match language {
        Language::Redis => redis::dangers(text),
        Language::Sql | Language::Mongodb => sql::unbounded_writes(text),
    }
}

/// The extension scripts are saved with, without the dot.
pub fn extension(language: Language) -> &'static str {
    match language {
        Language::Redis => "redis",
        Language::Sql | Language::Mongodb => "sql",
    }
}

/// What an editor of the language is called in menus.
pub fn editor_name(language: Language) -> &'static str {
    match language {
        Language::Redis => "Redis editor",
        Language::Sql | Language::Mongodb => "SQL editor",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_language_keeps_its_own_secrets() {
        assert_eq!(redact_secrets(Language::Redis, "AUTH hunter2"), "AUTH •••");
        assert!(!sets_secret(Language::Sql, "AUTH hunter2"));
        assert!(sets_secret(Language::Sql, "ALTER ROLE r PASSWORD 'x'"));
        assert_eq!(dangers(Language::Redis, "FLUSHDB").len(), 1);
        assert_eq!(dangers(Language::Sql, "FLUSHDB").len(), 0);
        assert_eq!(extension(Language::Redis), "redis");
        assert_eq!(
            statement_at(
                Language::Redis,
                "GET a
GET b",
                7
            ),
            Some(6..11)
        );
    }
}

//! MongoDB shell statements Violet rewrites: an explain around a query.

/// Methods of a collection whose cursor takes `.explain(mode)` at the end.
const CURSOR_METHODS: &[&str] = &["aggregate", "find"];

/// Methods `db.collection.explain(mode)` wraps, run without reading
/// anything else of the shell.
const EXPLAINABLE: &[&str] = &["count", "distinct", "findAndModify", "remove", "update"];

/// The statement that explains `text`, a mongo shell call on a collection:
/// `.explain("executionStats")` after a `find` or `aggregate`, or
/// `db.collection.explain(...)` around one of the others. Only reads are
/// analyzed (`executionStats` runs the winning plan); a write gets the
/// planner's choice alone.
///
/// # Errors
///
/// Why `text` cannot be explained.
pub fn explain(text: &str, analyze: bool) -> Result<String, String> {
    let text = text.trim().trim_end_matches(';').trim_end();
    let Some(rest) = text.strip_prefix("db.") else {
        return Err("Explain reads a statement on a collection: db.collection.find(...)".into());
    };
    if text.contains(".explain(") {
        return Ok(text.to_owned());
    }
    // `db.orders.find(` or `db.getCollection("orders").find(`.
    let (collection, after) = if rest.starts_with("getCollection(") {
        let close = rest
            .find(')')
            .ok_or("The collection's name is not closed")?;
        (&rest[..=close], &rest[close + 1..])
    } else {
        let end = rest.find('.').ok_or("Name a method after the collection")?;
        (&rest[..end], &rest[end..])
    };
    let after = after
        .strip_prefix('.')
        .ok_or("Name a method after the collection")?;
    let method: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    let reads = matches!(method.as_str(), "find" | "aggregate" | "count" | "distinct");
    let mode = if analyze && reads {
        "executionStats"
    } else {
        "queryPlanner"
    };
    if CURSOR_METHODS.contains(&method.as_str()) {
        Ok(format!("{text}.explain(\"{mode}\")"))
    } else if EXPLAINABLE.contains(&method.as_str()) {
        Ok(format!("db.{collection}.explain(\"{mode}\").{after}"))
    } else {
        Err(format!(
            "MongoDB explains find, aggregate, count, distinct, update, remove and findAndModify, not {}",
            if method.is_empty() { "this" } else { &method }
        ))
    }
}

/// Whether `explain` would only plan, not run, a statement under analyze.
pub fn plans_only(text: &str) -> bool {
    explain(text, true).is_ok_and(|sent| sent.contains("queryPlanner"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_a_cursor_and_the_methods_it_wraps() {
        assert_eq!(
            explain("db.orders.find({ a: 1 }).sort({ b: 1 });", true).as_deref(),
            Ok("db.orders.find({ a: 1 }).sort({ b: 1 }).explain(\"executionStats\")")
        );
        assert_eq!(
            explain("db.orders.aggregate([{ $match: {} }])", false).as_deref(),
            Ok("db.orders.aggregate([{ $match: {} }]).explain(\"queryPlanner\")")
        );
        assert_eq!(
            explain("db.orders.distinct(\"a\")", true).as_deref(),
            Ok("db.orders.explain(\"executionStats\").distinct(\"a\")")
        );
        assert_eq!(
            explain("db.getCollection(\"my orders\").find()", true).as_deref(),
            Ok("db.getCollection(\"my orders\").find().explain(\"executionStats\")")
        );
    }

    #[test]
    fn a_write_is_planned_never_run() {
        assert_eq!(
            explain("db.orders.update({ a: 1 }, { $set: { b: 2 } })", true).as_deref(),
            Ok("db.orders.explain(\"queryPlanner\").update({ a: 1 }, { $set: { b: 2 } })")
        );
        assert!(plans_only("db.orders.remove({})"));
        assert!(!plans_only("db.orders.find()"));
    }

    #[test]
    fn says_what_it_cannot_explain() {
        assert!(explain("show dbs", false).is_err());
        assert!(explain("db.orders.insertOne({})", false).is_err());
        assert!(explain("db.orders", false).is_err());
        // Already an explain: left as written.
        assert_eq!(
            explain("db.orders.find().explain()", false).as_deref(),
            Ok("db.orders.find().explain()")
        );
    }
}

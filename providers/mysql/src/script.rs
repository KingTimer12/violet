//! The statements of a script, as far as naming their results goes.
//!
//! MySQL's results carry no command tag (PostgreSQL's `INSERT 0 3`), so
//! each statement's first word stands in for it.

/// The first word of each statement in `sql`, upper-cased. Statements end
/// at `;` outside quotes and comments.
pub(crate) fn keywords(sql: &str) -> Vec<String> {
    let mut keywords = Vec::new();
    let mut word = String::new();
    // The current statement's first word is complete.
    let mut named = false;
    let mut chars = sql.chars().peekable();
    let finish = |word: &mut String, named: &mut bool, keywords: &mut Vec<String>| {
        if !word.is_empty() {
            keywords.push(word.to_ascii_uppercase());
        }
        word.clear();
        *named = false;
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                named = true;
                // Doubled quotes and (outside backticks) backslashes
                // escape; either way the string goes on.
                while let Some(d) = chars.next() {
                    if d == '\\' && c != '`' {
                        chars.next();
                    } else if d == c {
                        if chars.peek() == Some(&c) {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
            }
            '#' => skip_line(&mut chars),
            '-' if chars.peek() == Some(&'-') => {
                let mut ahead = chars.clone();
                ahead.next();
                if ahead.peek().is_none_or(|c| c.is_whitespace()) {
                    skip_line(&mut chars);
                } else {
                    named |= !word.is_empty();
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for d in chars.by_ref() {
                    if last == '*' && d == '/' {
                        break;
                    }
                    last = d;
                }
                named |= !word.is_empty();
            }
            ';' => finish(&mut word, &mut named, &mut keywords),
            c if c.is_ascii_alphabetic() || (c == '_' && !word.is_empty()) => {
                if !named {
                    word.push(c);
                }
            }
            _ => named |= !word.is_empty(),
        }
    }
    finish(&mut word, &mut named, &mut keywords);
    keywords
}

fn skip_line(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for c in chars.by_ref() {
        if c == '\n' {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_are_named_by_their_first_word() {
        assert_eq!(
            keywords(
                "-- setup\n insert into t values ('a;b', \"c\\\";\");\
                 /* x; */ UPDATE t SET a = 1 # why;\n WHERE b = `x;y`;\n\
                 select 1;  ;\n"
            ),
            ["INSERT", "UPDATE", "SELECT"]
        );
        assert_eq!(keywords("CREATE TABLE x (a int)"), ["CREATE"]);
        assert_eq!(keywords("delete from t where a = 'it''s'"), ["DELETE"]);
        assert!(keywords("  -- nothing\n").is_empty());
    }
}

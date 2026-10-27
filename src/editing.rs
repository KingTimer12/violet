//! Changes the SQL editor makes to its text: commenting lines out, and
//! finding text.

use std::ops::Range;

/// The text after an edit, and the byte range to select in it.
#[derive(Debug, PartialEq, Eq)]
pub struct Edited {
    pub text: String,
    pub selection: Range<usize>,
}

/// The stops left in an inserted snippet, which Tab visits in turn.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Snippet {
    /// Byte ranges in the text, ascending; empty for a bare caret.
    stops: Vec<Range<usize>>,
    /// The text's length when the stops were last placed.
    len: usize,
}

impl Snippet {
    /// The stops of an item inserted at byte `start` into a text now
    /// `len` long; none when the item has no more stops.
    pub fn new(start: usize, stops: &[Range<usize>], len: usize) -> Option<Self> {
        (!stops.is_empty()).then(|| Self {
            stops: stops
                .iter()
                .map(|stop| start + stop.start..start + stop.end)
                .collect(),
            len,
        })
    }

    /// Follows an edit that left the text `len` long with the cursor at
    /// `cursor`: the stops past the edit move with it. False when what was
    /// typed passed the next stop, so the snippet is over.
    pub fn follow(&mut self, len: usize, cursor: usize) -> bool {
        let delta = len as isize - self.len as isize;
        self.len = len;
        if delta == 0 {
            return true;
        }
        let at = cursor.saturating_sub(delta.max(0) as usize);
        for stop in &mut self.stops {
            if stop.start >= at {
                stop.start = stop.start.saturating_add_signed(delta);
                stop.end = stop.end.saturating_add_signed(delta);
            }
        }
        self.stops.first().is_some_and(|stop| stop.start >= cursor)
    }

    /// The next stop after `cursor`, which is dropped; `None` when the
    /// cursor is already past them all.
    pub fn next(&mut self, cursor: usize) -> Option<Range<usize>> {
        let index = self.stops.iter().position(|stop| stop.start > cursor)?;
        let stop = self.stops[index].clone();
        self.stops.drain(..=index);
        Some(stop)
    }

    /// Whether any stop is left.
    pub fn is_empty(&self) -> bool {
        self.stops.is_empty()
    }
}

/// Comments out the lines `selection` touches with `-- `, at their common
/// indentation, or uncomments them when every one that is not blank
/// already is. A selection ending at the start of a line leaves that line
/// alone. A caret stays where it was in its line; a selection grows to
/// the whole lines.
pub fn toggle_comment(text: &str, selection: Range<usize>) -> Edited {
    let start = text[..selection.start].rfind('\n').map_or(0, |at| at + 1);
    let last = if selection.end > selection.start && text[..selection.end].ends_with('\n') {
        selection.end - 1
    } else {
        selection.end
    };
    let end = text[last..].find('\n').map_or(text.len(), |at| last + at);
    let lines: Vec<&str> = text[start..end].split('\n').collect();
    let filled = || lines.iter().filter(|line| !line.trim().is_empty());
    let commented =
        filled().count() > 0 && filled().all(|line| line.trim_start().starts_with("--"));
    let indent = filled()
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);

    // Where each line's marker goes or went, and how long it is.
    let marker = |line: &str| {
        if commented {
            let at = line.len() - line.trim_start().len();
            (at, if line[at..].starts_with("-- ") { 3 } else { 2 })
        } else {
            (indent, 3)
        }
    };
    let block: Vec<String> = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                return (*line).to_owned();
            }
            let (at, length) = marker(line);
            if commented {
                format!("{}{}", &line[..at], &line[at + length..])
            } else {
                format!("{}-- {}", &line[..at], &line[at..])
            }
        })
        .collect();
    let block = block.join("\n");
    let selection = if selection.is_empty() {
        // The caret keeps its place in its text.
        let column = selection.start - start;
        let (at, length) = marker(lines[0]);
        let column = if lines[0].trim().is_empty() || column < at {
            column
        } else if commented {
            column.saturating_sub(length).max(at)
        } else {
            column + length
        };
        start + column..start + column
    } else {
        start..start + block.len()
    };
    Edited {
        text: format!("{}{block}{}", &text[..start], &text[end..]),
        selection,
    }
}

/// Where `needle` appears in `text`, without overlapping, as byte ranges.
/// Without `case`, letters match in either case.
pub fn find_all(text: &str, needle: &str, case: bool) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    if case {
        return text
            .match_indices(needle)
            .map(|(at, found)| at..at + found.len())
            .collect();
    }
    let needle: Vec<char> = needle.chars().collect();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let same = |a: char, b: char| a == b || a.to_lowercase().eq(b.to_lowercase());
    let mut found = Vec::new();
    let mut index = 0;
    while index + needle.len() <= chars.len() {
        let matches = needle
            .iter()
            .zip(&chars[index..])
            .all(|(want, (_, have))| same(*want, *have));
        if matches {
            let start = chars[index].0;
            let end = chars
                .get(index + needle.len())
                .map_or(text.len(), |(at, _)| *at);
            found.push(start..end);
            index += needle.len();
        } else {
            index += 1;
        }
    }
    found
}

/// `text` with every range in `found` replaced by `with`.
pub fn replace_all(text: &str, found: &[Range<usize>], with: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for range in found {
        out.push_str(&text[at..range.start]);
        out.push_str(with);
        at = range.end;
    }
    out.push_str(&text[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_uncomments_whole_lines() {
        let text = "select 1\n  from t\nwhere x\n";
        // From inside the first line to the start of the third: two lines.
        let edited = toggle_comment(text, 3..18);
        assert_eq!(edited.text, "-- select 1\n--   from t\nwhere x\n");
        assert_eq!(edited.selection, 0..23);
        let back = toggle_comment(&edited.text, edited.selection);
        assert_eq!(back.text, text);
    }

    #[test]
    fn a_caret_comments_its_line_and_keeps_its_place() {
        let text = "a\n    select 1\nb";
        let edited = toggle_comment(text, 10..10);
        assert_eq!(edited.text, "a\n    -- select 1\nb");
        assert_eq!(edited.selection, 13..13);
        let back = toggle_comment(&edited.text, 13..13);
        assert_eq!(back.text, text);
        assert_eq!(back.selection, 10..10);
    }

    #[test]
    fn blank_lines_are_left_alone() {
        let edited = toggle_comment("a\n\nb", 0..4);
        assert_eq!(edited.text, "-- a\n\n-- b");
    }

    #[test]
    fn finds_in_either_case() {
        let text = "Select nome FROM t WHERE nome = 'NOME'";
        assert_eq!(find_all(text, "nome", true), [7..11, 25..29]);
        assert_eq!(find_all(text, "NOME", false), [7..11, 25..29, 33..37]);
        assert_eq!(find_all("ção Ção", "ÇÃO", false), [0..5, 6..11]);
        assert!(find_all(text, "", false).is_empty());
        assert_eq!(
            replace_all(text, &find_all(text, "nome", true), "name"),
            "Select name FROM t WHERE name = 'NOME'"
        );
    }

    #[test]
    fn snippet_stops_are_visited_and_follow_typing() {
        // `(a, b) VALUES (|, |)` inserted at 10.
        let mut snippet = Snippet::new(10, &[4..4, 6..6], 20).expect("stops");
        assert_eq!(snippet.next(14), Some(16..16));
        assert_eq!(snippet.next(16), None);
        assert!(snippet.is_empty());
        assert!(Snippet::new(0, &[], 5).is_none());

        // Typing `42` at the first stop moves the second one along.
        let mut snippet = Snippet::new(10, &[4..4, 6..6], 20).expect("stops");
        assert!(snippet.follow(22, 16));
        assert_eq!(snippet.next(16), Some(18..18));
        // Moving the cursor past the stop with edits ends it.
        let mut snippet = Snippet::new(0, &[2..2], 10).expect("stops");
        assert!(!snippet.follow(14, 9));
    }
}

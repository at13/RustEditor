use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use regex::{NoExpand, Regex, RegexBuilder};
use ropey::Rope;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub case_sensitive: bool,
    pub regex: bool,
}

#[derive(Clone, Debug)]
pub struct SearchMatch {
    pub line_index: usize,
    pub start_char: usize,
    pub end_char: usize,
    pub global_start_char: usize,
    pub global_end_char: usize,
}

pub struct SearchResults {
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
}

/// Error returned when a search or replace was cancelled.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

fn check(cancel: Option<&AtomicBool>, line_index: usize) -> Result<()> {
    if line_index.is_multiple_of(1024) && cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Err(Cancelled.into());
    }
    Ok(())
}

pub fn find_all(rope: &Rope, query: &SearchQuery, max_matches: usize) -> Result<SearchResults> {
    find_all_cancellable(rope, query, max_matches, None)
}

/// Like [`find_all`]; stops with [`Cancelled`] once `cancel` is set.
pub fn find_all_cancellable(
    rope: &Rope,
    query: &SearchQuery,
    max_matches: usize,
    cancel: Option<&AtomicBool>,
) -> Result<SearchResults> {
    let regex = compile(query)?;
    let mut matches = Vec::new();
    let mut truncated = false;

    for (line_index, line) in rope.lines().enumerate() {
        check(cancel, line_index)?;
        let raw = line.to_string();
        let content = without_line_ending(&raw);
        let line_start = rope.line_to_char(line_index);
        let mut previous_byte = 0;
        let mut previous_char = 0;

        for found in regex.find_iter(content) {
            if matches.len() == max_matches {
                truncated = true;
                return Ok(SearchResults { matches, truncated });
            }

            let start_char = previous_char + content[previous_byte..found.start()].chars().count();
            let end_char = start_char + content[found.start()..found.end()].chars().count();
            previous_byte = found.end();
            previous_char = end_char;
            matches.push(SearchMatch {
                line_index,
                start_char,
                end_char,
                global_start_char: line_start + start_char,
                global_end_char: line_start + end_char,
            });
        }
    }

    Ok(SearchResults { matches, truncated })
}

pub fn replace_all(rope: &Rope, query: &SearchQuery, replacement: &str) -> Result<(Rope, usize)> {
    replace_all_cancellable(rope, query, replacement, None)
}

/// Like [`replace_all`]; stops with [`Cancelled`] once `cancel` is set.
pub fn replace_all_cancellable(
    rope: &Rope,
    query: &SearchQuery,
    replacement: &str,
    cancel: Option<&AtomicBool>,
) -> Result<(Rope, usize)> {
    let regex = compile(query)?;
    let mut output = Rope::new();
    let mut replacements = 0;

    for (line_index, line) in rope.lines().enumerate() {
        check(cancel, line_index)?;
        let raw = line.to_string();
        let content = without_line_ending(&raw);
        let ending = &raw[content.len()..];
        replacements += regex.find_iter(content).count();

        let replaced = if query.regex {
            regex.replace_all(content, replacement)
        } else {
            regex.replace_all(content, NoExpand(replacement))
        };
        output.insert(output.len_chars(), replaced.as_ref());
        output.insert(output.len_chars(), ending);
    }

    Ok((output, replacements))
}

pub fn replacement_for_match(
    matched_text: &str,
    query: &SearchQuery,
    replacement: &str,
) -> Result<String> {
    let regex = compile(query)?;
    let Some(captures) = regex.captures(matched_text) else {
        bail!("the current match no longer matches the search expression");
    };

    if query.regex {
        let mut expanded = String::new();
        captures.expand(replacement, &mut expanded);
        Ok(expanded)
    } else {
        Ok(replacement.to_owned())
    }
}

/// Searches `(line number, text without line ending)` pairs, e.g. streamed
/// from a huge file. Matches carry line and char columns; the `global_*`
/// fields are 0 (there is no in-memory document).
pub fn find_in_lines<E: std::fmt::Display>(
    lines: impl IntoIterator<Item = Result<(usize, String), E>>,
    query: &SearchQuery,
    max_matches: usize,
    cancel: Option<&AtomicBool>,
) -> Result<SearchResults> {
    let regex = compile(query)?;
    let mut matches = Vec::new();
    for item in lines {
        let (line_index, content) = item.map_err(|e| anyhow::anyhow!("cannot read: {e}"))?;
        check(cancel, line_index)?;
        let mut previous_byte = 0;
        let mut previous_char = 0;
        for found in regex.find_iter(&content) {
            if matches.len() == max_matches {
                return Ok(SearchResults {
                    matches,
                    truncated: true,
                });
            }
            let start_char = previous_char + content[previous_byte..found.start()].chars().count();
            let end_char = start_char + content[found.start()..found.end()].chars().count();
            previous_byte = found.end();
            previous_char = end_char;
            matches.push(SearchMatch {
                line_index,
                start_char,
                end_char,
                global_start_char: 0,
                global_end_char: 0,
            });
        }
    }
    Ok(SearchResults {
        matches,
        truncated: false,
    })
}

fn compile(query: &SearchQuery) -> Result<Regex> {
    if query.text.is_empty() {
        bail!("enter text to search for");
    }

    let pattern = if query.regex {
        query.text.clone()
    } else {
        regex::escape(&query.text)
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!query.case_sensitive)
        .build()
        .with_context(|| format!("invalid regular expression: {}", query.text))
}

fn without_line_ending(line: &str) -> &str {
    line.strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(line)
}

#[cfg(test)]
mod tests {
    use ropey::Rope;

    use super::{
        Cancelled, SearchQuery, find_all, find_all_cancellable, find_in_lines, replace_all,
        replace_all_cancellable, replacement_for_match,
    };

    #[test]
    fn line_streams_match_the_rope_search() {
        let text = "alpha beta\nbeta 😀 beta\r\n\nno\nBETA";
        let rope = Rope::from_str(text);
        let query = SearchQuery {
            text: "beta".into(),
            case_sensitive: false,
            regex: false,
        };
        let expected = find_all(&rope, &query, 100).unwrap();
        let lines = rope.lines().enumerate().map(|(i, l)| {
            let l = l.to_string();
            let l = l.trim_end_matches('\n').trim_end_matches('\r').to_owned();
            Ok::<_, std::io::Error>((i, l))
        });
        let got = find_in_lines(lines, &query, 100, None).unwrap();
        let pick = |r: &super::SearchResults| -> Vec<(usize, usize, usize)> {
            r.matches
                .iter()
                .map(|m| (m.line_index, m.start_char, m.end_char))
                .collect()
        };
        assert_eq!(pick(&got), pick(&expected));
        let capped = find_in_lines(
            (0..10).map(|i| Ok::<_, std::io::Error>((i, "beta".to_owned()))),
            &query,
            3,
            None,
        )
        .unwrap();
        assert!(capped.truncated);
        assert_eq!(capped.matches.len(), 3);
    }

    #[test]
    fn cancelled_searches_stop_with_an_error() {
        let rope = Rope::from_str(
            &"needle
"
            .repeat(5000),
        );
        let query = SearchQuery {
            text: "needle".into(),
            case_sensitive: true,
            regex: false,
        };
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let error = find_all_cancellable(&rope, &query, 10, Some(&cancel))
            .err()
            .unwrap();
        assert!(error.downcast_ref::<Cancelled>().is_some());
        assert!(replace_all_cancellable(&rope, &query, "x", Some(&cancel)).is_err());
        let open = std::sync::atomic::AtomicBool::new(false);
        let found = find_all_cancellable(&rope, &query, 10, Some(&open)).unwrap();
        assert_eq!(found.matches.len(), 10);
    }

    #[test]
    fn literal_search_can_ignore_case() {
        let rope = Rope::from_str("Alpha alpha\nALPHA");
        let query = SearchQuery {
            text: "alpha".to_owned(),
            case_sensitive: false,
            regex: false,
        };

        let results = find_all(&rope, &query, 100).expect("search should succeed");

        assert_eq!(results.matches.len(), 3);
        assert_eq!(results.matches[1].line_index, 0);
        assert_eq!(results.matches[1].start_char, 6);
    }

    #[test]
    fn regex_replace_supports_capture_groups() {
        let rope = Rope::from_str("Doe, Jane\r\nSmith, John");
        let query = SearchQuery {
            text: r"(\w+), (\w+)".to_owned(),
            case_sensitive: true,
            regex: true,
        };

        let (replaced, count) =
            replace_all(&rope, &query, "$2 $1").expect("replace should succeed");

        assert_eq!(count, 2);
        assert_eq!(replaced.to_string(), "Jane Doe\r\nJohn Smith");
        assert_eq!(
            replacement_for_match("Doe, Jane", &query, "$2 $1").expect("replacement should expand"),
            "Jane Doe"
        );
    }

    #[test]
    fn invalid_regex_is_reported() {
        let query = SearchQuery {
            text: "(".to_owned(),
            case_sensitive: true,
            regex: true,
        };

        assert!(find_all(&Rope::from_str("text"), &query, 10).is_err());
    }
}

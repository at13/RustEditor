//! Fuzzy matching for the command palette.

use crate::{CommandInfo, Registry};

/// Scores `candidate` against `query` (case-insensitive subsequence match per
/// whitespace-separated token). Higher is better; `None` means no match.
pub fn score(query: &str, candidate: &str) -> Option<i64> {
    let candidate: Vec<char> = candidate.to_lowercase().chars().collect();
    let mut total = 0;
    for token in query.split_whitespace() {
        total += score_token(
            &token.to_lowercase().chars().collect::<Vec<_>>(),
            &candidate,
        )?;
    }
    // Among equal matches, prefer shorter candidates ("Save" over "Save As").
    Some(total * 4 - candidate.len().min(40) as i64 / 4)
}

fn score_token(token: &[char], candidate: &[char]) -> Option<i64> {
    let mut score = 0i64;
    let mut position = 0;
    let mut previous: Option<usize> = None;
    for &wanted in token {
        let found = (position..candidate.len()).find(|&i| candidate[i] == wanted)?;
        let word_start = found == 0 || !candidate[found - 1].is_alphanumeric();
        score += 1;
        if word_start {
            score += 8;
        }
        match previous {
            Some(prev) if prev + 1 == found => score += 5,
            Some(prev) => score -= (found - prev - 1).min(5) as i64,
            None => score -= found.min(10) as i64,
        }
        previous = Some(found);
        position = found + 1;
    }
    Some(score)
}

/// Visible commands matching `query`, best first. An empty query lists all
/// visible commands by category and title.
pub fn filter<'a>(registry: &'a Registry, query: &str) -> Vec<&'a CommandInfo> {
    let mut matches: Vec<(i64, &CommandInfo)> = registry
        .iter()
        .filter(|command| !command.hidden)
        .filter_map(|command| {
            if query.trim().is_empty() {
                return Some((0, command));
            }
            let title = score(query, &command.title);
            let qualified =
                score(query, &format!("{}: {}", command.category, command.title)).map(|s| s - 3);
            title.max(qualified).map(|s| (s, command))
        })
        .collect();
    matches.sort_by(|(a, x), (b, y)| {
        b.cmp(a)
            .then_with(|| x.category.cmp(&y.category))
            .then_with(|| x.title.cmp(&y.title))
    });
    matches.into_iter().map(|(_, command)| command).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(query: &str) -> Vec<String> {
        filter(&Registry::builtin(), query)
            .into_iter()
            .map(|c| c.title.to_string())
            .collect()
    }

    #[test]
    fn matches_subsequences_case_insensitively() {
        assert!(score("sv", "Save").is_some());
        assert!(score("SAVE", "save as...").is_some());
        assert!(score("xyz", "Save").is_none());
        assert!(score("", "anything").is_some());
    }

    #[test]
    fn prefers_word_starts_and_contiguous_runs() {
        assert!(score("sa", "Save As") > score("sa", "Toggle Case Sensitive"));
        assert!(score("save", "Save") > score("save", "Save As..."));
    }

    #[test]
    fn filters_and_ranks_commands() {
        assert_eq!(titles("save")[0], "Save");
        assert_eq!(titles("undo")[0], "Undo");
        assert_eq!(titles("fmt json")[0], "Format JSON");
        // Category words help: "bookmark clear".
        assert_eq!(titles("bookmark clear")[0], "Clear Bookmarks");
        // Hidden editor commands never appear.
        assert!(!titles("").iter().any(|t| t == "Move Left"));
        assert!(titles("zzzz").is_empty());
    }
}

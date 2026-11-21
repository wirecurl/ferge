//! Fuzzy matching for the command palette: the query's characters must
//! appear in the text in order. Matches at the start of words, in a row and
//! early in the text rank higher.

/// Points for each matched character.
const MATCH: i32 = 16;
/// Extra points when a match follows the previous one directly.
const CONSECUTIVE: i32 = 24;
/// Extra points when a match starts a word.
const WORD_START: i32 = 30;
/// Extra points when the text starts with the match.
const PREFIX: i32 = 40;
/// Points lost per character skipped between matches.
const GAP: i32 = 2;
/// Points lost per character of text, so shorter texts win ties.
const LENGTH: i32 = 1;

/// A query, prepared once and matched against many texts.
#[derive(Clone, Debug)]
pub(crate) struct Query {
    chars: Vec<char>,
}

impl Query {
    pub(crate) fn new(query: &str) -> Self {
        Self {
            chars: query
                .chars()
                .filter(|c| !c.is_whitespace())
                .flat_map(char::to_lowercase)
                .collect(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// The score of `text`, or `None` if it does not match. Higher is better.
    pub(crate) fn score(&self, text: &str) -> Option<i32> {
        if self.chars.is_empty() {
            return Some(0);
        }
        let mut wanted = self.chars.iter().peekable();
        let mut score = 0;
        let mut previous: Option<char> = None;
        let mut last_match: Option<usize> = None;
        let mut length = 0_i32;
        for (index, original) in text.chars().enumerate() {
            length = length.saturating_add(1);
            let Some(&&next) = wanted.peek() else {
                continue;
            };
            let lower = original.to_lowercase().next().unwrap_or(original);
            if lower == next {
                score += MATCH;
                let word_start = previous.is_none_or(|previous| {
                    !previous.is_alphanumeric()
                        || (previous.is_lowercase() && original.is_uppercase())
                });
                if index == 0 {
                    score += PREFIX;
                }
                if word_start {
                    score += WORD_START;
                }
                match last_match {
                    Some(last) if last + 1 == index => score += CONSECUTIVE,
                    Some(last) => {
                        score -= GAP * i32::try_from(index - last - 1).unwrap_or(i32::MAX / 4);
                    }
                    None => {}
                }
                last_match = Some(index);
                wanted.next();
            }
            previous = Some(original);
        }
        wanted
            .peek()
            .is_none()
            .then(|| score - LENGTH * length.min(200))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, text: &str) -> Option<i32> {
        Query::new(query).score(text)
    }

    #[test]
    fn matches_characters_in_order() {
        assert!(score("nf", "New folder").is_some());
        assert!(score("NEWF", "new folder").is_some());
        assert!(score("fn", "New folder").is_none());
        assert!(score("xyz", "New folder").is_none());
        assert_eq!(score("", "anything"), Some(0));
        assert!(score("new folder", "New folder").is_some());
    }

    #[test]
    fn ranks_word_starts_and_runs_higher() {
        // Word starts beat scattered letters.
        assert!(score("nf", "New folder") > score("nf", "uninformed"));
        // A run beats a gap.
        assert!(score("set", "Settings") > score("set", "Show transfers"));
        // A prefix beats a match later on.
        assert!(score("tr", "Transfers") > score("tr", "Show transfers"));
        // CamelCase counts as word starts.
        assert!(score("gc", "GoCopy") > score("gc", "agency"));
        // Shorter texts win ties.
        assert!(score("readme", "README.md") > score("readme", "README.md.backup"));
    }
}

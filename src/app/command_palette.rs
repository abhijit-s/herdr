//! Native command palette: catalog assembly, fuzzy matching, and palette state.
//! Core capability (KTD-1) — not a plugin.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandSource {
    BuiltIn,
    Plugin,
    Custom,
}

impl CommandSource {
    /// Dim right-aligned tag shown on every row (all three sources).
    pub(crate) fn tag(self) -> &'static str {
        match self {
            CommandSource::BuiltIn => "built-in",
            CommandSource::Plugin => "plugin",
            CommandSource::Custom => "custom",
        }
    }
}

/// Case-insensitive subsequence match. Returns `None` when `name` does not
/// contain every char of `query` in order. Higher score = better: contiguous
/// runs and early matches score higher. Empty query returns `Some(0)`.
pub(crate) fn fuzzy_score(query: &str, name: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let name_lower = name.to_ascii_lowercase();
    let mut score: i32 = 0;
    let mut run: i32 = 0;
    let mut name_iter = name_lower.chars().enumerate();
    for qc in query.to_ascii_lowercase().chars() {
        let mut matched = false;
        for (idx, nc) in name_iter.by_ref() {
            if nc == qc {
                // reward early matches and contiguous runs
                score += 10 - (idx as i32).min(9);
                run += 1;
                score += run * 2;
                matched = true;
                break;
            }
            run = 0;
        }
        if !matched {
            return None;
        }
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_subsequence_and_ranks_prefix_higher() {
        assert!(fuzzy_score("rena", "rename-workspace").is_some());
        assert!(fuzzy_score("xyz", "rename-workspace").is_none());
        // empty query matches everything with a neutral score
        assert_eq!(fuzzy_score("", "zoom"), Some(0));
        // a contiguous prefix outranks a scattered subsequence
        let prefix = fuzzy_score("ren", "rename-tab").unwrap();
        let scattered = fuzzy_score("ren", "reset-pane-navigation").unwrap();
        assert!(prefix > scattered);
    }
}

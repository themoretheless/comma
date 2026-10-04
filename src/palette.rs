//! Command palette (Cmd+K): one fuzzy input over app actions, open tabs and
//! subdirectories of the active tab's cwd. Pure state and matching; the
//! window itself is drawn by `app`.

/// What a palette entry does when chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    NewTab,
    CloseTab,
    CopySelection,
    ScrollTop,
    ScrollBottom,
    Quit,
    SwitchTab(usize),
    Cd(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) label: String,
    /// Right-aligned hint (shortcut or kind), dimmed.
    pub(crate) hint: &'static str,
    pub(crate) action: Action,
}

#[derive(Debug, Default)]
pub(crate) struct Palette {
    pub(crate) query: String,
    pub(crate) selected: usize,
    items: Vec<Item>,
}

impl Palette {
    pub(crate) fn new(tabs: &[String], dirs: &[String]) -> Self {
        let mut items = Vec::new();
        for (index, label) in tabs.iter().enumerate() {
            items.push(Item { label: label.clone(), hint: "tab", action: Action::SwitchTab(index) });
        }
        for dir in dirs {
            items.push(Item { label: format!("{dir}/"), hint: "cd", action: Action::Cd(dir.clone()) });
        }
        for (label, hint, action) in [
            ("New tab", "⌘T", Action::NewTab),
            ("Close tab", "⌘W", Action::CloseTab),
            ("Copy selection", "⌘C", Action::CopySelection),
            ("Scroll to top", "", Action::ScrollTop),
            ("Scroll to bottom", "", Action::ScrollBottom),
            ("Quit", "⌘Q", Action::Quit),
        ] {
            items.push(Item { label: label.into(), hint, action });
        }
        Self { query: String::new(), selected: 0, items }
    }

    /// Items matching the query, best first (stable for equal scores).
    pub(crate) fn matches(&self) -> Vec<&Item> {
        let mut scored: Vec<(i32, &Item)> = self
            .items
            .iter()
            .filter_map(|item| fuzzy_score(&self.query, &item.label).map(|s| (s, item)))
            .collect();
        scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
        scored.into_iter().map(|(_, item)| item).collect()
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let len = self.matches().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
    }

    pub(crate) fn chosen(&self) -> Option<Action> {
        self.matches().get(self.selected).map(|item| item.action.clone())
    }
}

/// Case-insensitive subsequence match. Higher is better: consecutive runs
/// and matches at word starts score more; `None` when not a subsequence.
pub(crate) fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0;
    let mut pos = 0;
    let mut prev: Option<usize> = None;
    for q in query.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let found = (pos..text.len()).find(|&i| text[i] == q)?;
        score += 1;
        if prev.is_some_and(|p| p + 1 == found) {
            score += 3;
        }
        if found == 0 || !text[found - 1].is_alphanumeric() {
            score += 2;
        }
        prev = Some(found);
        pos = found + 1;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_subsequences() {
        assert!(fuzzy_score("nt", "New tab").is_some());
        assert!(fuzzy_score("xyz", "New tab").is_none());
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn fuzzy_prefers_word_starts_and_runs() {
        let start = fuzzy_score("src", "src/").unwrap();
        let scattered = fuzzy_score("src", "scroll recent").unwrap();
        assert!(start > scattered);
    }

    #[test]
    fn query_filters_and_selection_wraps() {
        let mut p = Palette::new(&["zsh".into()], &["src".into(), "target".into()]);
        p.query = "src".into();
        assert_eq!(p.chosen(), Some(Action::Cd("src".into())));
        p.query = "tab".into();
        let n = p.matches().len();
        p.move_selection(-1);
        assert_eq!(p.selected, n - 1);
    }
}

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

use crate::app::input::navigate::NavigateAction;
use crate::config::CustomCommandKeybind;

#[derive(Debug, Clone)]
pub(crate) enum CommandHandle {
    Navigate(NavigateAction),
    Plugin(String), // qualified_id, e.g. "myplugin.dosomething"
    Custom(Box<CustomCommandKeybind>),
}

#[derive(Debug, Clone)]
pub(crate) struct CommandEntry {
    pub name: String,
    pub description: Option<String>,
    pub source: CommandSource,
    pub handle: CommandHandle,
}

impl CommandEntry {
    /// Stable dedup key = handle identity (NOT display name). Distinct plugin
    /// actions that share a title get distinct keys via their qualified_id.
    pub(crate) fn identity_key(&self) -> String {
        match &self.handle {
            // NavigateAction derives Debug — variant identity suffices
            // (index-bearing variants already collapsed to a picker in the catalog).
            CommandHandle::Navigate(a) => format!("nav:{a:?}"),
            CommandHandle::Plugin(id) => format!("plugin:{id}"),
            CommandHandle::Custom(kb) => format!("custom:{}", kb.command),
        }
    }
}

/// How a NavigateAction variant appears in the palette.
pub(crate) enum BuiltinDisposition {
    /// A directly-invokable command with a display name + description.
    Entry {
        name: &'static str,
        description: &'static str,
    },
    /// An index-bearing action; the palette offers ONE entry that routes to the
    /// existing picker action instead of a fixed index.
    RouteToPicker(NavigateAction),
}

/// Single-source catalog macro (drift guard). One table generates BOTH the
/// exhaustive `builtin_disposition` match (NO wildcard — a new upstream
/// `NavigateAction` variant fails to compile until it gets a row here) AND
/// `all_builtin_actions()`. Because both expand from the same rows they can
/// never diverge.
macro_rules! builtin_catalog {
    // Base case: emit both functions from the accumulated tokens.
    (@munch [$($arm:tt)*] [$($ctor:tt)*] ) => {
        pub(crate) fn builtin_disposition(a: &NavigateAction) -> BuiltinDisposition {
            match a { $($arm)* }
        }
        fn all_builtin_actions() -> Vec<NavigateAction> {
            vec![ $($ctor)* ]
        }
    };

    // Directly-invokable row.
    (@munch [$($arm:tt)*] [$($ctor:tt)*] entry $v:ident $nm:literal $desc:literal ; $($rest:tt)* ) => {
        builtin_catalog!(@munch
            [$($arm)* NavigateAction::$v => BuiltinDisposition::Entry { name: $nm, description: $desc },]
            [$($ctor)* NavigateAction::$v,]
            $($rest)*);
    };

    // Index-bearing row → route to an existing picker (placeholder index; only identity matters).
    (@munch [$($arm:tt)*] [$($ctor:tt)*] picker $v:ident $target:ident ; $($rest:tt)* ) => {
        builtin_catalog!(@munch
            [$($arm)* NavigateAction::$v(_) => BuiltinDisposition::RouteToPicker(NavigateAction::$target),]
            [$($ctor)* NavigateAction::$v(0),]
            $($rest)*);
    };

    // Entry point: start the muncher with two empty accumulators.
    ( $($rows:tt)* ) => { builtin_catalog!(@munch [] [] $($rows)*); };
}

builtin_catalog! {
    entry NewWorkspace           "new-workspace"            "Create a new workspace";
    entry NewWorktree            "new-worktree"             "Create a new linked worktree";
    entry OpenWorktree           "open-worktree"            "Open an existing worktree";
    entry RemoveWorktree         "remove-worktree"          "Remove a worktree";
    entry RenameWorkspace        "rename-workspace"         "Rename the current workspace";
    entry CloseWorkspace         "close-workspace"          "Close the current workspace";
    entry WorkspacePicker        "workspace-picker"         "Pick a workspace";
    entry PreviousWorkspace      "previous-workspace"       "Switch to the previous workspace";
    entry NextWorkspace          "next-workspace"           "Switch to the next workspace";
    entry PreviousAgent          "previous-agent"           "Focus the previous agent";
    entry NextAgent              "next-agent"               "Focus the next agent";
    entry NewTab                 "new-tab"                  "Create a new tab";
    entry RenameTab              "rename-tab"               "Rename the current tab";
    entry PreviousTab            "previous-tab"             "Switch to the previous tab";
    entry NextTab                "next-tab"                 "Switch to the next tab";
    entry CloseTab               "close-tab"                "Close the current tab";
    entry RenamePane             "rename-pane"              "Rename the current pane";
    entry FocusPaneLeft          "focus-pane-left"          "Focus the pane to the left";
    entry FocusPaneDown          "focus-pane-down"          "Focus the pane below";
    entry FocusPaneUp            "focus-pane-up"            "Focus the pane above";
    entry FocusPaneRight         "focus-pane-right"         "Focus the pane to the right";
    entry SwapPaneLeft           "swap-pane-left"           "Swap with the pane to the left";
    entry SwapPaneDown           "swap-pane-down"           "Swap with the pane below";
    entry SwapPaneUp             "swap-pane-up"             "Swap with the pane above";
    entry SwapPaneRight          "swap-pane-right"          "Swap with the pane to the right";
    entry SplitVertical          "split-vertical"           "Split the pane vertically";
    entry SplitHorizontal        "split-horizontal"         "Split the pane horizontally";
    entry ClosePane              "close-pane"               "Close the current pane";
    entry EditScrollback         "edit-scrollback"          "Edit the scrollback buffer";
    entry CopyMode               "copy-mode"                "Enter copy mode";
    entry Zoom                   "zoom"                     "Toggle pane zoom";
    entry EnterResizeMode        "resize-mode"              "Enter pane resize mode";
    entry ToggleSidebar          "toggle-sidebar"           "Toggle the sidebar";
    entry CyclePaneNext          "cycle-pane-next"          "Cycle to the next pane";
    entry CyclePanePrevious      "cycle-pane-previous"      "Cycle to the previous pane";
    entry LastPane               "last-pane"                "Focus the last active pane";
    entry Help                   "help"                     "Open keybinding help";
    entry Settings               "settings"                 "Open settings";
    entry ReloadConfig           "reload-config"            "Reload configuration";
    entry OpenNotificationTarget "open-notification-target" "Jump to the notification target";
    entry Detach                 "detach"                   "Detach from the session";
    entry OpenNavigator          "navigator"                "Open the navigator";

    // index-bearing → route to an existing picker (never a fixed index):
    picker SwitchWorkspace WorkspacePicker;
    picker SwitchTab       OpenNavigator;
    picker FocusAgent      OpenNavigator;
}

pub(crate) fn builtin_entries() -> Vec<CommandEntry> {
    let mut entries: Vec<CommandEntry> = all_builtin_actions()
        .into_iter()
        .map(|action| match builtin_disposition(&action) {
            BuiltinDisposition::Entry { name, description } => CommandEntry {
                name: name.to_string(),
                description: Some(description.to_string()),
                source: CommandSource::BuiltIn,
                handle: CommandHandle::Navigate(action),
            },
            BuiltinDisposition::RouteToPicker(target) => CommandEntry {
                name: picker_entry_name(&target),
                description: Some(picker_entry_desc(&target)),
                source: CommandSource::BuiltIn,
                handle: CommandHandle::Navigate(target),
            },
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.dedup_by(|a, b| a.name == b.name); // RouteToPicker collapses duplicates
    entries
}

fn picker_entry_name(action: &NavigateAction) -> String {
    match builtin_disposition(action) {
        BuiltinDisposition::Entry { name, .. } => name.to_string(),
        _ => "picker".to_string(),
    }
}

fn picker_entry_desc(action: &NavigateAction) -> String {
    match builtin_disposition(action) {
        BuiltinDisposition::Entry { description, .. } => description.to_string(),
        _ => String::new(),
    }
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

    #[test]
    fn builtin_catalog_covers_actions_and_routes_index_variants() {
        let entries = builtin_entries();
        // every entry is a built-in, alphabetical, with a non-empty name
        assert!(entries.iter().all(|e| e.source == CommandSource::BuiltIn));
        assert!(entries.windows(2).all(|w| w[0].name <= w[1].name));
        assert!(entries.iter().all(|e| !e.name.is_empty()));
        // a representative no-arg action is present
        assert!(entries.iter().any(|e| e.name == "navigator"));
        // index-bearing variants route to a picker handle, not a fixed index
        assert!(matches!(
            builtin_disposition(&NavigateAction::SwitchWorkspace(0)),
            BuiltinDisposition::RouteToPicker(_)
        ));
    }
}

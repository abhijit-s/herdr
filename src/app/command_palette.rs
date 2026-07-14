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
use crate::config::{CustomCommandKeybind, Keybinds};

#[derive(Debug, Clone)]
pub(crate) enum CommandHandle {
    Navigate(NavigateAction),
    Plugin(String), // qualified_id, e.g. "myplugin.dosomething"
    Custom(Box<CustomCommandKeybind>),
}

#[derive(Debug, Clone)]
pub(crate) struct CommandEntry {
    pub name: String,
    /// Dimmed secondary text drawn between the name and the keybind/tag columns.
    pub description: Option<String>,
    /// Right-aligned shortcut, populated for built-ins only. Custom entries show
    /// their chord as the `name`, so their keybind column stays blank.
    pub keybinding: Option<String>,
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
    /// Deliberately not surfaced in the palette (internal/transient, e.g. the
    /// action that opens the palette itself).
    Exclude,
}

/// Single-source catalog macro (drift guard). One table generates BOTH the
/// exhaustive `builtin_disposition` match (NO wildcard — a new upstream
/// `NavigateAction` variant fails to compile until it gets a row here) AND
/// `all_builtin_actions()`. Because both expand from the same rows they can
/// never diverge.
macro_rules! builtin_catalog {
    // Base case: emit all three functions from the accumulated tokens.
    (@munch [$($arm:tt)*] [$($ctor:tt)*] [$($kb:tt)*] ) => {
        pub(crate) fn builtin_disposition(a: &NavigateAction) -> BuiltinDisposition {
            match a { $($arm)* }
        }
        fn all_builtin_actions() -> Vec<NavigateAction> {
            vec![ $($ctor)* ]
        }
        /// Maps a built-in action to the accessor that reads its bound chord.
        /// Same-source-of-truth as `builtin_disposition`: a new upstream
        /// `NavigateAction` variant fails to compile until it gets a catalog row,
        /// so the keybind column cannot silently drift from the entry table.
        #[allow(clippy::type_complexity)]
        fn builtin_keybind_accessor(a: &NavigateAction) -> Option<fn(&Keybinds) -> Option<String>> {
            match a { $($kb)* }
        }
        /// Human keybind label for a built-in action (e.g. `"prefix+z"`), read
        /// from the live parsed keybinds. `None` when the action is unbound.
        pub(crate) fn builtin_keybind_label(kb: &Keybinds, a: &NavigateAction) -> Option<String> {
            builtin_keybind_accessor(a).and_then(|read| read(kb))
        }
    };

    // Directly-invokable row. `$field` names the `Keybinds` field that binds it.
    (@munch [$($arm:tt)*] [$($ctor:tt)*] [$($kb:tt)*] entry $v:ident $field:ident $nm:literal $desc:literal ; $($rest:tt)* ) => {
        builtin_catalog!(@munch
            [$($arm)* NavigateAction::$v => BuiltinDisposition::Entry { name: $nm, description: $desc },]
            [$($ctor)* NavigateAction::$v,]
            [$($kb)* NavigateAction::$v => Some(|kb: &Keybinds| kb.$field.label()),]
            $($rest)*);
    };

    // Index-bearing row → route to an existing picker (placeholder index; only identity matters).
    (@munch [$($arm:tt)*] [$($ctor:tt)*] [$($kb:tt)*] picker $v:ident $target:ident ; $($rest:tt)* ) => {
        builtin_catalog!(@munch
            [$($arm)* NavigateAction::$v(_) => BuiltinDisposition::RouteToPicker(NavigateAction::$target),]
            [$($ctor)* NavigateAction::$v(0),]
            [$($kb)* NavigateAction::$v(_) => None,]
            $($rest)*);
    };

    // Deliberately hidden row (still enumerated so the match stays exhaustive).
    (@munch [$($arm:tt)*] [$($ctor:tt)*] [$($kb:tt)*] exclude $v:ident ; $($rest:tt)* ) => {
        builtin_catalog!(@munch
            [$($arm)* NavigateAction::$v => BuiltinDisposition::Exclude,]
            [$($ctor)* NavigateAction::$v,]
            [$($kb)* NavigateAction::$v => None,]
            $($rest)*);
    };

    // Entry point: start the muncher with three empty accumulators.
    ( $($rows:tt)* ) => { builtin_catalog!(@munch [] [] [] $($rows)*); };
}

builtin_catalog! {
    entry NewWorkspace           new_workspace            "new-workspace"            "Create a new workspace";
    entry NewWorktree            new_worktree             "new-worktree"             "Create a new linked worktree";
    entry OpenWorktree           open_worktree            "open-worktree"            "Open an existing worktree";
    entry RemoveWorktree         remove_worktree          "remove-worktree"          "Remove a worktree";
    entry RenameWorkspace        rename_workspace         "rename-workspace"         "Rename the current workspace";
    entry CloseWorkspace         close_workspace          "close-workspace"          "Close the current workspace";
    entry WorkspacePicker        workspace_picker         "workspace-picker"         "Pick a workspace";
    entry PreviousWorkspace      previous_workspace       "previous-workspace"       "Switch to the previous workspace";
    entry NextWorkspace          next_workspace           "next-workspace"           "Switch to the next workspace";
    entry PreviousAgent          previous_agent           "previous-agent"           "Focus the previous agent";
    entry NextAgent              next_agent               "next-agent"               "Focus the next agent";
    entry NewTab                 new_tab                  "new-tab"                  "Create a new tab";
    entry RenameTab              rename_tab               "rename-tab"               "Rename the current tab";
    entry PreviousTab            previous_tab             "previous-tab"             "Switch to the previous tab";
    entry NextTab                next_tab                 "next-tab"                 "Switch to the next tab";
    entry CloseTab               close_tab                "close-tab"                "Close the current tab";
    entry RenamePane             rename_pane              "rename-pane"              "Rename the current pane";
    entry FocusPaneLeft          focus_pane_left          "focus-pane-left"          "Focus the pane to the left";
    entry FocusPaneDown          focus_pane_down          "focus-pane-down"          "Focus the pane below";
    entry FocusPaneUp            focus_pane_up            "focus-pane-up"            "Focus the pane above";
    entry FocusPaneRight         focus_pane_right         "focus-pane-right"         "Focus the pane to the right";
    entry SwapPaneLeft           swap_pane_left           "swap-pane-left"           "Swap with the pane to the left";
    entry SwapPaneDown           swap_pane_down           "swap-pane-down"           "Swap with the pane below";
    entry SwapPaneUp             swap_pane_up             "swap-pane-up"             "Swap with the pane above";
    entry SwapPaneRight          swap_pane_right          "swap-pane-right"          "Swap with the pane to the right";
    entry SplitVertical          split_vertical           "split-vertical"           "Split the pane vertically";
    entry SplitHorizontal        split_horizontal         "split-horizontal"         "Split the pane horizontally";
    entry ClosePane              close_pane               "close-pane"               "Close the current pane";
    entry EditScrollback         edit_scrollback          "edit-scrollback"          "Edit the scrollback buffer";
    entry CopyMode               copy_mode                "copy-mode"                "Enter copy mode";
    entry Zoom                   zoom                     "zoom"                     "Toggle pane zoom";
    entry EnterResizeMode        resize_mode              "resize-mode"              "Enter pane resize mode";
    entry ToggleSidebar          toggle_sidebar           "toggle-sidebar"           "Toggle the sidebar";
    entry CyclePaneNext          cycle_pane_next          "cycle-pane-next"          "Cycle to the next pane";
    entry CyclePanePrevious      cycle_pane_previous      "cycle-pane-previous"      "Cycle to the previous pane";
    entry LastPane               last_pane                "last-pane"                "Focus the last active pane";
    entry Help                   help                     "help"                     "Open keybinding help";
    entry Settings               settings                 "settings"                 "Open settings";
    entry ReloadConfig           reload_config            "reload-config"            "Reload configuration";
    entry OpenNotificationTarget open_notification_target "open-notification-target" "Jump to the notification target";
    entry Detach                 detach                   "detach"                   "Detach from the session";
    entry OpenNavigator          goto                     "navigator"                "Open the navigator";

    // hidden: opening the palette from within the palette is meaningless.
    exclude OpenCommandPalette;

    // index-bearing → route to an existing picker (never a fixed index):
    picker SwitchWorkspace WorkspacePicker;
    picker SwitchTab       OpenNavigator;
    picker FocusAgent      OpenNavigator;
}

pub(crate) fn builtin_entries(keybinds: &Keybinds) -> Vec<CommandEntry> {
    let mut entries: Vec<CommandEntry> = all_builtin_actions()
        .into_iter()
        .filter_map(|action| match builtin_disposition(&action) {
            BuiltinDisposition::Entry { name, description } => Some(CommandEntry {
                name: name.to_string(),
                description: Some(description.to_string()),
                keybinding: builtin_keybind_label(keybinds, &action),
                source: CommandSource::BuiltIn,
                handle: CommandHandle::Navigate(action),
            }),
            BuiltinDisposition::RouteToPicker(target) => Some(CommandEntry {
                name: picker_entry_name(&target),
                description: Some(picker_entry_desc(&target)),
                keybinding: builtin_keybind_label(keybinds, &target),
                source: CommandSource::BuiltIn,
                handle: CommandHandle::Navigate(target),
            }),
            BuiltinDisposition::Exclude => None,
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceToggles {
    pub built_in: bool,
    pub plugin: bool,
    pub custom: bool,
}

impl SourceToggles {
    #[cfg(test)]
    pub(crate) fn all() -> Self {
        Self {
            built_in: true,
            plugin: true,
            custom: true,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct CommandPaletteState {
    pub query: String,
    pub replace_on_type: bool,
    pub entries: Vec<CommandEntry>,
    pub filtered: Vec<usize>, // indices into `entries`, ranked best-first
    pub selected: usize,
}

impl CommandPaletteState {
    /// Rebuild the full catalog from the three sources (respecting toggles),
    /// then reset query/selection and refilter. Called on open.
    pub(crate) fn assemble(
        &mut self,
        toggles: SourceToggles,
        keybinds: &Keybinds,
        plugin_entries: Vec<CommandEntry>,
        custom_entries: Vec<CommandEntry>,
    ) {
        let mut entries: Vec<CommandEntry> = Vec::new();
        if toggles.built_in {
            entries.extend(builtin_entries(keybinds));
        }
        if toggles.plugin {
            entries.extend(plugin_entries);
        }
        if toggles.custom {
            entries.extend(custom_entries);
        }
        // Identity dedup: same handle, NOT same display name. Two plugins that
        // both title an action "build" have distinct handles (different
        // qualified_ids) and both survive; only a genuine duplicate handle is
        // dropped. Order-independent seen-set (not adjacency-based `dedup_by`).
        let mut seen = std::collections::HashSet::new();
        entries.retain(|e| seen.insert(e.identity_key()));
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        self.entries = entries;
        self.query.clear();
        self.replace_on_type = false;
        self.selected = 0;
        self.refilter();
    }

    /// Recompute `filtered` from `query`; reset selection to row 0.
    pub(crate) fn refilter(&mut self) {
        let mut scored: Vec<(usize, i32)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| fuzzy_score(&self.query, &e.name).map(|s| (i, s)))
            .collect();
        // best score first; tie-break by name for determinism
        scored.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| self.entries[a.0].name.cmp(&self.entries[b.0].name))
        });
        self.filtered = scored.into_iter().map(|(i, _)| i).collect();
        self.selected = 0;
    }

    pub(crate) fn visible(&self) -> &[usize] {
        &self.filtered
    }

    pub(crate) fn selected_entry(&self) -> Option<&CommandEntry> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.entries.get(i))
    }

    /// Wrap-move selection by `delta` (single-step Up/Down/Ctrl+p/Ctrl+n): moving
    /// past the last row wraps to the first, and past the first wraps to the last.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let len = self.filtered.len() as i32;
        let next = (self.selected as i32 + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    /// Clamp-move selection by `delta` (half-/full-page jumps): clamps to the
    /// list bounds and never wraps.
    pub(crate) fn jump_clamped(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let max = self.filtered.len() - 1;
        let next = (self.selected as i32 + delta).clamp(0, max as i32);
        self.selected = next as usize;
    }

    /// Jump to the first row (Home).
    pub(crate) fn select_first(&mut self) {
        self.selected = 0;
    }

    /// Jump to the last row (End). Empty list stays at 0.
    pub(crate) fn select_last(&mut self) {
        self.selected = self.filtered.len().saturating_sub(1);
    }
}

/// Availability-filtered plugin actions as palette entries. Reuses the
/// `manifest_actions` availability filter (not the raw plugin map).
pub(crate) fn plugin_entries_from_registry(
    plugins: &crate::app::state::InstalledPluginRegistry,
) -> Vec<CommandEntry> {
    crate::app::api::plugins::palette_plugin_actions(plugins)
        .into_iter()
        .map(|info| CommandEntry {
            name: info.title.clone(),
            description: info.description.clone(),
            keybinding: None,
            source: CommandSource::Plugin,
            handle: CommandHandle::Plugin(info.qualified_id()),
        })
        .collect()
}

/// User-defined `[[keys.command]]` custom commands as palette entries.
pub(crate) fn custom_entries_from_config(customs: &[CustomCommandKeybind]) -> Vec<CommandEntry> {
    customs
        .iter()
        .map(|kb| CommandEntry {
            name: kb.label.clone(),
            description: kb.description.clone(),
            // A labeled custom shows its chord in the keybind column; a label-less
            // one keeps the chord as the `name` and leaves the column blank
            // (`keybind_display` is `None`) so it is not duplicated.
            keybinding: kb.keybind_display.clone(),
            source: CommandSource::Custom,
            handle: CommandHandle::Custom(Box::new(kb.clone())),
        })
        .collect()
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
        let entries = builtin_entries(&Keybinds::default());
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

    #[test]
    fn keybind_resolves_for_builtins_and_is_blank_for_other_sources() {
        let kb = Keybinds::default();
        // a representative built-in resolves to its configured chord
        assert_eq!(
            builtin_keybind_label(&kb, &NavigateAction::Zoom).as_deref(),
            Some("prefix+z")
        );
        // the palette entry for it carries the same string
        let zoom = builtin_entries(&kb)
            .into_iter()
            .find(|e| e.name == "zoom")
            .expect("zoom entry present");
        assert_eq!(zoom.keybinding.as_deref(), Some("prefix+z"));
        // an unbound-by-default built-in yields no keybind column
        assert!(builtin_keybind_label(&kb, &NavigateAction::OpenWorktree).is_none());
        // a label-less custom surfaces its chord as the name, blank keybind column;
        // a labeled custom shows the label as the name and its chord on the right.
        let customs = vec![
            CustomCommandKeybind {
                bindings: crate::config::ActionKeybinds::default(),
                label: "prefix+ctrl+j".to_string(),
                keybind_display: None,
                command: "swap-pane-down".to_string(),
                action: crate::config::CustomCommandAction::Pane,
                description: Some("swap pane down".to_string()),
            },
            CustomCommandKeybind {
                bindings: crate::config::ActionKeybinds::default(),
                label: "Deploy web".to_string(),
                keybind_display: Some("prefix+ctrl+d".to_string()),
                command: "deploy".to_string(),
                action: crate::config::CustomCommandAction::Shell,
                description: Some("ship it".to_string()),
            },
        ];
        let custom = custom_entries_from_config(&customs);
        assert_eq!(custom.len(), 2);
        assert_eq!(custom[0].name, "prefix+ctrl+j");
        assert!(custom[0].keybinding.is_none());
        assert_eq!(custom[1].name, "Deploy web");
        assert_eq!(custom[1].keybinding.as_deref(), Some("prefix+ctrl+d"));
    }

    fn plugin_entries_from(v: Vec<(String, String, Option<String>)>) -> Vec<CommandEntry> {
        v.into_iter()
            .map(|(id, title, desc)| CommandEntry {
                name: title,
                description: desc,
                keybinding: None,
                source: CommandSource::Plugin,
                handle: CommandHandle::Plugin(id),
            })
            .collect()
    }

    #[test]
    fn assemble_merges_sources_dedups_by_identity_and_sorts() {
        let mut state = CommandPaletteState::default();
        let plugin = vec![("acme.build".to_string(), "build".to_string(), None)];
        let custom: Vec<CommandEntry> = vec![CommandEntry {
            name: "deploy".to_string(),
            description: None,
            keybinding: None,
            source: CommandSource::Custom,
            handle: CommandHandle::Plugin("acme.deploy".to_string()),
        }];
        state.assemble(
            SourceToggles::all(),
            &Keybinds::default(),
            plugin_entries_from(plugin),
            custom,
        );
        // sorted alphabetically across all sources
        assert!(state.entries.windows(2).all(|w| w[0].name <= w[1].name));
        // a plugin entry survived and carries its source tag
        assert!(state
            .entries
            .iter()
            .any(|e| e.source == CommandSource::Plugin && e.name == "build"));
        // disabling built-ins drops them
        let mut only_plugins = CommandPaletteState::default();
        only_plugins.assemble(
            SourceToggles {
                built_in: false,
                plugin: true,
                custom: false,
            },
            &Keybinds::default(),
            plugin_entries_from(vec![("acme.build".to_string(), "build".to_string(), None)]),
            vec![],
        );
        assert!(only_plugins
            .entries
            .iter()
            .all(|e| e.source == CommandSource::Plugin));
        // `visible` mirrors `filtered`
        assert_eq!(only_plugins.visible().len(), only_plugins.filtered.len());
    }

    #[test]
    fn move_selection_wraps_around() {
        let mut state = CommandPaletteState::default();
        state.assemble(SourceToggles::all(), &Keybinds::default(), vec![], vec![]);
        let n = state.filtered.len();
        assert!(n > 1, "catalog should hold more than one entry");
        // from row 0, a single step up wraps to the last row
        state.move_selection(-1);
        assert_eq!(state.selected, n - 1);
        // from the last row, a single step down wraps back to row 0
        state.move_selection(1);
        assert_eq!(state.selected, 0);
        // empty list stays put
        let mut empty = CommandPaletteState::default();
        empty.move_selection(1);
        assert_eq!(empty.selected, 0);
    }

    #[test]
    fn jump_clamped_clamps_at_both_ends_without_wrap() {
        let mut state = CommandPaletteState::default();
        state.assemble(SourceToggles::all(), &Keybinds::default(), vec![], vec![]);
        let n = state.filtered.len();
        assert!(n > 4, "catalog should hold several entries");
        // from the middle, moves by the delta
        state.selected = 2;
        state.jump_clamped(1);
        assert_eq!(state.selected, 3);
        // near the top, a large negative jump clamps to 0 (does NOT wrap to the end)
        state.selected = 1;
        state.jump_clamped(-1000);
        assert_eq!(state.selected, 0);
        // near the bottom, a large positive jump clamps to the last row (no wrap)
        state.selected = n - 2;
        state.jump_clamped(1000);
        assert_eq!(state.selected, n - 1);
    }

    #[test]
    fn select_first_and_last_land_on_ends() {
        let mut state = CommandPaletteState::default();
        state.assemble(SourceToggles::all(), &Keybinds::default(), vec![], vec![]);
        let n = state.filtered.len();
        state.select_last();
        assert_eq!(state.selected, n - 1);
        state.select_first();
        assert_eq!(state.selected, 0);
        // empty list stays at 0 for both
        let mut empty = CommandPaletteState::default();
        empty.select_last();
        assert_eq!(empty.selected, 0);
        empty.select_first();
        assert_eq!(empty.selected, 0);
    }

    #[test]
    fn colliding_plugin_titles_are_not_deduped() {
        // two DIFFERENT plugin actions that share the display title "build"
        let plugins = plugin_entries_from(vec![
            ("acme.build".to_string(), "build".to_string(), None),
            ("other.build".to_string(), "build".to_string(), None),
        ]);
        let mut state = CommandPaletteState::default();
        state.assemble(
            SourceToggles {
                built_in: false,
                plugin: true,
                custom: false,
            },
            &Keybinds::default(),
            plugins,
            vec![],
        );
        // distinct qualified_ids → distinct identity keys → both survive
        assert_eq!(
            state.entries.iter().filter(|e| e.name == "build").count(),
            2
        );
    }
}

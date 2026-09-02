//! Client-side command palette: catalog assembly, fuzzy matching, selection,
//! and dispatch.
//!
//! The palette is pure client presentation. Its query, ranked results, and
//! selection never leave this process, so two clients attached to one session
//! each keep their own. Running a row is the only part that reaches the
//! endpoint, and it does so through dispatches the client already owns: a
//! built-in replays the exact `KeybindMatch` its chord would have produced, a
//! custom command replays its `KeybindMatch::Command`, and a plugin action uses
//! the public `plugin.action.invoke` method. No palette-specific server state,
//! API method, or snapshot field exists.

use super::*;

use crate::config::{CustomCommandKeybind, Keybinds};
use crate::input::{KeybindAction, KeybindMatch};

/// Case-insensitive subsequence match. Returns `None` when `name` does not
/// contain every char of `query` in order. Higher score = better: contiguous
/// runs and early matches score higher. An empty query returns `Some(0)`.
pub(super) fn fuzzy_score(query: &str, name: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let name_lower = name.to_ascii_lowercase();
    let mut score: i32 = 0;
    let mut run: i32 = 0;
    let mut name_iter = name_lower.chars().enumerate();
    for query_char in query.to_ascii_lowercase().chars() {
        let mut matched = false;
        for (index, name_char) in name_iter.by_ref() {
            if name_char == query_char {
                // reward early matches and contiguous runs
                score += 10 - (index as i32).min(9);
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

/// How a `KeybindAction` variant appears in the palette.
pub(super) enum BuiltinDisposition {
    /// A directly invokable command with a display name and description.
    Entry {
        name: &'static str,
        description: &'static str,
    },
    /// An index-bearing action. The palette offers one entry that opens the
    /// existing picker instead of binding a fixed index.
    RouteToPicker(KeybindAction),
    /// Deliberately not surfaced (opening the palette from inside it is a no-op).
    Exclude,
}

/// Single-source catalog (drift guard). One table generates the exhaustive
/// `builtin_disposition` match, `all_builtin_actions`, and the keybind-label
/// accessor. The match has no wildcard arm, so a new upstream `KeybindAction`
/// variant fails to compile until it gets a row here, and because all three
/// expand from the same rows they cannot diverge.
macro_rules! builtin_catalog {
    (@munch [$($arm:tt)*] [$($ctor:tt)*] [$($kb:tt)*] ) => {
        pub(super) fn builtin_disposition(action: &KeybindAction) -> BuiltinDisposition {
            match action { $($arm)* }
        }
        fn all_builtin_actions() -> Vec<KeybindAction> {
            vec![ $($ctor)* ]
        }
        #[allow(clippy::type_complexity)]
        fn builtin_keybind_accessor(
            action: &KeybindAction,
        ) -> Option<fn(&Keybinds) -> Option<String>> {
            match action { $($kb)* }
        }
        /// Human keybind label for a built-in (e.g. `"prefix+z"`), read from the
        /// live parsed keybinds. `None` when the action is unbound.
        pub(super) fn builtin_keybind_label(
            keybinds: &Keybinds,
            action: &KeybindAction,
        ) -> Option<String> {
            builtin_keybind_accessor(action).and_then(|read| read(keybinds))
        }
    };

    // Directly invokable row. `$field` names the `Keybinds` field that binds it.
    (@munch [$($arm:tt)*] [$($ctor:tt)*] [$($kb:tt)*]
        entry $variant:ident $field:ident $name:literal $description:literal ; $($rest:tt)* ) => {
        builtin_catalog!(@munch
            [$($arm)* KeybindAction::$variant =>
                BuiltinDisposition::Entry { name: $name, description: $description },]
            [$($ctor)* KeybindAction::$variant,]
            [$($kb)* KeybindAction::$variant => Some(|kb: &Keybinds| kb.$field.label()),]
            $($rest)*);
    };

    // Index-bearing row routed to an existing picker (placeholder index; only
    // the variant identity matters for the disposition lookup).
    (@munch [$($arm:tt)*] [$($ctor:tt)*] [$($kb:tt)*]
        picker $variant:ident $target:ident ; $($rest:tt)* ) => {
        builtin_catalog!(@munch
            [$($arm)* KeybindAction::$variant(_) =>
                BuiltinDisposition::RouteToPicker(KeybindAction::$target),]
            [$($ctor)* KeybindAction::$variant(0),]
            [$($kb)* KeybindAction::$variant(_) => None,]
            $($rest)*);
    };

    // Deliberately hidden row, still enumerated so the match stays exhaustive.
    (@munch [$($arm:tt)*] [$($ctor:tt)*] [$($kb:tt)*] exclude $variant:ident ; $($rest:tt)* ) => {
        builtin_catalog!(@munch
            [$($arm)* KeybindAction::$variant => BuiltinDisposition::Exclude,]
            [$($ctor)* KeybindAction::$variant,]
            [$($kb)* KeybindAction::$variant => None,]
            $($rest)*);
    };

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
    entry MoveTabPrevious        move_tab_previous        "move-tab-previous"        "Move the current tab left";
    entry MoveTabNext            move_tab_next            "move-tab-next"            "Move the current tab right";
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
    entry ResizePaneLeft         resize_pane_left         "resize-pane-left"         "Resize the pane leftward";
    entry ResizePaneDown         resize_pane_down         "resize-pane-down"         "Resize the pane downward";
    entry ResizePaneUp           resize_pane_up           "resize-pane-up"           "Resize the pane upward";
    entry ResizePaneRight        resize_pane_right        "resize-pane-right"        "Resize the pane rightward";
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

    // index-bearing, routed to an existing picker rather than a fixed index:
    picker SwitchWorkspace WorkspacePicker;
    picker SwitchTab       OpenNavigator;
    picker FocusAgent      OpenNavigator;
}

pub(super) fn builtin_entries(keybinds: &Keybinds) -> Vec<ClientPaletteEntry> {
    let mut entries = all_builtin_actions()
        .into_iter()
        .filter_map(|action| match builtin_disposition(&action) {
            BuiltinDisposition::Entry { name, description } => Some(ClientPaletteEntry {
                name: name.to_owned(),
                description: Some(description.to_owned()),
                keybinding: builtin_keybind_label(keybinds, &action),
                source: ClientPaletteSource::BuiltIn,
                handle: ClientPaletteHandle::Action(action),
            }),
            BuiltinDisposition::RouteToPicker(target) => Some(ClientPaletteEntry {
                name: picker_entry_text(&target).0.to_owned(),
                description: Some(picker_entry_text(&target).1.to_owned()),
                keybinding: builtin_keybind_label(keybinds, &target),
                source: ClientPaletteSource::BuiltIn,
                handle: ClientPaletteHandle::Action(target),
            }),
            BuiltinDisposition::Exclude => None,
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries.dedup_by(|left, right| left.name == right.name);
    entries
}

fn picker_entry_text(action: &KeybindAction) -> (&'static str, &'static str) {
    match builtin_disposition(action) {
        BuiltinDisposition::Entry { name, description } => (name, description),
        _ => ("picker", ""),
    }
}

/// User-defined `[[keys.command]]` entries as palette rows.
pub(super) fn custom_entries(commands: &[CustomCommandKeybind]) -> Vec<ClientPaletteEntry> {
    commands
        .iter()
        .map(|command| ClientPaletteEntry {
            name: command.label.clone(),
            description: command.description.clone(),
            // A labelled custom shows its chord in the keybind column; a
            // label-less one keeps the chord as its name and leaves the column
            // blank so it is not printed twice.
            keybinding: command.keybind_display.clone(),
            source: ClientPaletteSource::Custom,
            handle: ClientPaletteHandle::Command(Box::new(command.clone())),
        })
        .collect()
}

/// Installed plugin manifest actions as palette rows.
pub(super) fn plugin_entries(
    actions: Vec<crate::api::schema::PluginActionInfo>,
) -> Vec<ClientPaletteEntry> {
    actions
        .into_iter()
        .map(|action| ClientPaletteEntry {
            name: action.title,
            description: action.description,
            keybinding: None,
            source: ClientPaletteSource::Plugin,
            handle: ClientPaletteHandle::PluginAction {
                plugin_id: action.plugin_id,
                action_id: action.action_id,
            },
        })
        .collect()
}

/// Order and dedup a freshly merged catalog. Dedup is by handle identity, not
/// display name, so two plugins that both title an action "build" both survive.
fn normalize_entries(entries: &mut Vec<ClientPaletteEntry>) {
    let mut seen = HashSet::new();
    entries.retain(|entry| seen.insert(entry.identity_key()));
    entries.sort_by(|left, right| left.name.cmp(&right.name));
}

/// Rank `entries` against `query`, best score first, name-ordered on ties so the
/// list is stable across identical scores.
fn ranked_indices(entries: &[ClientPaletteEntry], query: &str) -> Vec<usize> {
    let mut scored = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| fuzzy_score(query, &entry.name).map(|score| (index, score)))
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| entries[left.0].name.cmp(&entries[right.0].name))
    });
    scored.into_iter().map(|(index, _)| index).collect()
}

impl ClientCommandPaletteOverlay {
    pub(super) fn selected_entry(&self) -> Option<&ClientPaletteEntry> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.entries.get(*index))
    }

    /// Recompute `filtered` from `query` and send the selection back to row 0.
    /// This is the query-edit path: a changed query invalidates the old ranking,
    /// so the best match should be selected.
    fn refilter(&mut self) {
        self.filtered = ranked_indices(&self.entries, &self.query);
        self.selected = 0;
    }

    /// Recompute `filtered` while keeping the highlighted command highlighted.
    /// Used when the catalog grows underneath the user (the plugin list landing
    /// after they have already started navigating), where resetting to row 0
    /// would move the selection out from under them.
    fn refilter_preserving_selection(&mut self) {
        let selected_key = self.selected_entry().map(ClientPaletteEntry::identity_key);
        self.filtered = ranked_indices(&self.entries, &self.query);
        self.selected = selected_key
            .and_then(|key| {
                self.filtered
                    .iter()
                    .position(|index| self.entries[*index].identity_key() == key)
            })
            .unwrap_or(0);
    }

    /// Wrap-move the selection by `delta` (single steps: arrows, ctrl+p/ctrl+n).
    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let len = self.filtered.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }

    /// Clamp-move the selection by `delta` (half- and full-page jumps). Page
    /// keys stop at the ends rather than wrapping.
    fn jump_clamped(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.filtered.len().saturating_sub(1);
        self.selected = (self.selected as isize + delta).clamp(0, last as isize) as usize;
    }
}

impl ClientShellState {
    /// Assemble the catalog from the enabled sources and open the palette.
    /// Built-ins and custom commands are already resolved client-side; plugin
    /// actions are requested from the endpoint and merged when they arrive.
    pub(super) fn open_command_palette(&mut self, outcome: &mut ClientShellInput) {
        let sources = self.config.command_palette_sources;
        let mut entries = Vec::new();
        if sources.built_in {
            entries.extend(builtin_entries(&self.config.keybinds.keybinds));
        }
        if sources.custom {
            entries.extend(custom_entries(
                &self.config.keybinds.keybinds.custom_commands,
            ));
        }
        normalize_entries(&mut entries);
        let mut palette = ClientCommandPaletteOverlay {
            query: String::new(),
            entries,
            filtered: Vec::new(),
            selected: 0,
            loading_plugin_actions: sources.plugin,
        };
        palette.refilter();
        self.overlay = Some(ClientShellOverlay::CommandPalette(palette));
        self.chrome_drag = None;
        if sources.plugin {
            self.push_endpoint_method_with_kind(
                crate::api::schema::Method::PluginActionList(
                    crate::api::schema::PluginActionListParams::default(),
                ),
                PendingEndpointKind::CommandPaletteActionList,
                outcome,
            );
        }
    }

    pub(super) fn edit_command_palette_query(&mut self, edit: impl FnOnce(&mut String)) -> bool {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return false;
        };
        edit(&mut palette.query);
        palette.refilter();
        true
    }

    pub(super) fn move_command_palette_selection(&mut self, delta: isize) {
        if let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() {
            palette.move_selection(delta);
        }
    }

    pub(super) fn jump_command_palette_selection(&mut self, delta: isize) {
        if let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() {
            palette.jump_clamped(delta);
        }
    }

    /// Jump to the last row (`last`) or the first. An empty list stays at 0.
    pub(super) fn select_command_palette_edge(&mut self, last: bool) {
        if let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() {
            palette.selected = if last {
                palette.filtered.len().saturating_sub(1)
            } else {
                0
            };
        }
    }

    pub(super) fn select_command_palette_row(&mut self, row: usize) {
        if let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() {
            if row < palette.filtered.len() {
                palette.selected = row;
            }
        }
    }

    /// Rows the palette list can show, from the last render. Drives the page and
    /// half-page jumps; `jump_clamped` stops at the ends, so an off-by-a-row
    /// estimate before the first render is harmless.
    pub(super) fn command_palette_page(&self) -> isize {
        self.hits.command_palette_list_height.max(1) as isize
    }

    /// Run the highlighted row through the client's normal dispatch.
    ///
    /// The palette closes first: a built-in such as `help` or `settings` opens
    /// its own overlay, and clearing afterwards would throw that overlay away.
    pub(super) fn dispatch_command_palette_entry(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_ref() else {
            return;
        };
        // An empty result list is a no-op; the palette stays open.
        let Some(handle) = palette.selected_entry().map(|entry| entry.handle.clone()) else {
            return;
        };
        self.overlay = None;
        outcome.repaint = true;
        match handle {
            ClientPaletteHandle::Action(action) => {
                self.record_binding(KeybindMatch::Action(action), outcome);
            }
            ClientPaletteHandle::Command(command) => {
                self.record_binding(KeybindMatch::Command(*command), outcome);
            }
            ClientPaletteHandle::PluginAction {
                plugin_id,
                action_id,
            } => {
                self.push_endpoint_method(
                    crate::api::schema::Method::PluginActionInvoke(
                        crate::api::schema::PluginActionInvokeParams {
                            action_id,
                            plugin_id: Some(plugin_id),
                            context: None,
                        },
                    ),
                    outcome,
                );
            }
        }
    }

    /// Palette keys. Movement and page jumps claim their chords first, so the
    /// query only ever receives what is left over: printable text, backspace,
    /// and the word/line deletes.
    pub(super) fn route_command_palette_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        use crossterm::event::KeyModifiers;

        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let control = modifiers.contains(KeyModifiers::CONTROL);
        let page = self.command_palette_page();
        let half_page = (page / 2).max(1);

        match code {
            KeyCode::Esc => {
                self.overlay = None;
                outcome.repaint = true;
                return;
            }
            KeyCode::Enter => {
                self.dispatch_command_palette_entry(outcome);
                return;
            }
            KeyCode::Up => self.move_command_palette_selection(-1),
            KeyCode::Down => self.move_command_palette_selection(1),
            KeyCode::Char('p') if control => self.move_command_palette_selection(-1),
            KeyCode::Char('n') if control => self.move_command_palette_selection(1),
            KeyCode::Char('d') if control => self.jump_command_palette_selection(half_page),
            KeyCode::Char('u') if control => self.jump_command_palette_selection(-half_page),
            KeyCode::PageDown => self.jump_command_palette_selection(page),
            KeyCode::PageUp => self.jump_command_palette_selection(-page),
            KeyCode::Home => self.select_command_palette_edge(false),
            KeyCode::End => self.select_command_palette_edge(true),
            KeyCode::Backspace if modifiers.contains(KeyModifiers::SUPER) => {
                self.edit_command_palette_query(String::clear);
            }
            KeyCode::Backspace if control || modifiers.contains(KeyModifiers::ALT) => {
                self.edit_command_palette_query(super::delete_trailing_word);
            }
            KeyCode::Char('h' | 'w') if control => {
                self.edit_command_palette_query(super::delete_trailing_word);
            }
            KeyCode::Backspace => {
                self.edit_command_palette_query(|query| {
                    query.pop();
                });
            }
            KeyCode::Char(character) if modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                let text = key
                    .generated_text
                    .clone()
                    .unwrap_or_else(|| character.to_string());
                self.edit_command_palette_query(|query| query.push_str(&text));
            }
            _ => return,
        }
        outcome.repaint = true;
    }

    pub(super) fn handle_command_palette_endpoint_result(
        &mut self,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        // The palette may already be closed; drop a late list rather than
        // reopening or stashing a catalog nothing will read.
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return false;
        };
        palette.loading_plugin_actions = false;
        match result {
            Ok(crate::api::schema::ResponseResult::PluginActionList { actions }) => {
                palette.entries.extend(plugin_entries(actions));
                normalize_entries(&mut palette.entries);
                palette.refilter_preserving_selection();
            }
            Ok(_) => {
                self.endpoint_error =
                    Some("endpoint returned an unexpected plugin action list result".to_owned());
            }
            Err(error) => self.endpoint_error = Some(error.message),
        }
        true
    }
}

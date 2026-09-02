//! Characterization of the client-shell command palette: its open/close
//! transitions, the fuzzy filter, and the mapping from a highlighted row to the
//! request the client actually sends.
//!
//! The palette's query, ranking, and selection are per-client presentation, so
//! these drive `ClientShellState` directly. Running a row is the only part that
//! crosses to the endpoint, and every assertion below pins it to a dispatch the
//! client already owned before the palette existed.

use super::*;
use crate::protocol::ClientShellCommand;
use crossterm::event::{KeyCode, KeyModifiers};

fn key_event(code: KeyCode, modifiers: KeyModifiers) -> RawInputEvent {
    RawInputEvent::Key(crate::input::TerminalKey::new(code, modifiers))
}

fn key(code: KeyCode) -> RawInputEvent {
    key_event(code, KeyModifiers::empty())
}

fn typed(character: char) -> RawInputEvent {
    key(KeyCode::Char(character))
}

fn ctrl(character: char) -> RawInputEvent {
    key_event(KeyCode::Char(character), KeyModifiers::CONTROL)
}

fn state_with(config: &Config) -> ClientShellState {
    let mut shell_config = ClientShellConfig::from_config(config);
    shell_config.mobile_width_threshold = 0;
    let mut state = ClientShellState::new(shell_config);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state
}

fn default_state() -> ClientShellState {
    state_with(&Config::default())
}

/// Drive the real configured trigger: prefix, then the palette chord.
fn press_palette_chord(state: &mut ClientShellState) -> ClientShellInput {
    let (code, modifiers) = state.config.keybinds.prefix;
    state.handle_raw_events(vec![key_event(code, modifiers)]);
    state.handle_raw_events(vec![typed(':')])
}

fn palette(state: &ClientShellState) -> &ClientCommandPaletteOverlay {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::CommandPalette(palette)) => palette,
        other => panic!("expected an open command palette, got {other:?}"),
    }
}

fn visible_names(state: &ClientShellState) -> Vec<String> {
    let palette = palette(state);
    palette
        .filtered
        .iter()
        .map(|index| palette.entries[*index].name.clone())
        .collect()
}

/// Open the palette and return the endpoint requests the open produced.
fn open(state: &mut ClientShellState) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.open_command_palette(&mut outcome);
    outcome
}

fn endpoint_methods(outcome: &ClientShellInput) -> Vec<crate::api::schema::Method> {
    outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => Some(request.method.clone()),
            _ => None,
        })
        .collect()
}

/// Mirror the endpoint's own projection: `keybind_display` is set only when the
/// author's `label` differs from the chord, so a fixture cannot claim a
/// combination the server would never send.
fn shell_command(command_id: &str, binding_label: &str, chords: &[&str]) -> ClientShellCommand {
    let binding_labels = chords
        .iter()
        .map(|chord| (*chord).to_string())
        .collect::<Vec<_>>();
    let chord = binding_labels.join(" / ");
    ClientShellCommand {
        command_id: command_id.into(),
        binding_label: binding_label.into(),
        keybind_display: (binding_label != chord).then_some(chord),
        binding_labels,
        action: crate::protocol::ClientShellCommandAction::Shell,
        description: Some("ship it".into()),
    }
}

fn plugin_action(
    plugin_id: &str,
    action_id: &str,
    title: &str,
) -> crate::api::schema::PluginActionInfo {
    crate::api::schema::PluginActionInfo {
        plugin_id: plugin_id.into(),
        action_id: action_id.into(),
        title: title.into(),
        description: None,
        contexts: Vec::new(),
        command: vec!["true".into()],
        platforms: None,
    }
}

#[test]
fn the_configured_chord_opens_the_palette_and_esc_closes_it() {
    let mut state = default_state();
    assert!(state.overlay.is_none());

    let opened = press_palette_chord(&mut state);
    assert!(opened.repaint);
    assert!(!palette(&state).entries.is_empty(), "catalog was empty");
    assert!(palette(&state).query.is_empty());
    assert_eq!(palette(&state).selected, 0);

    let closed = state.handle_raw_events(vec![key(KeyCode::Esc)]);
    assert!(closed.repaint);
    assert!(state.overlay.is_none());
}

#[test]
fn the_palette_never_offers_itself() {
    // Opening the palette from inside the palette is meaningless, so the
    // catalog excludes its own action rather than recursing.
    let mut state = default_state();
    open(&mut state);
    assert!(
        !palette(&state).entries.iter().any(|entry| matches!(
            entry.handle,
            ClientPaletteHandle::Action(crate::input::KeybindAction::OpenCommandPalette)
        )),
        "palette listed itself"
    );
}

#[test]
fn typing_filters_the_catalog_and_sends_the_selection_back_to_the_best_match() {
    let mut state = default_state();
    open(&mut state);
    let total = palette(&state).filtered.len();

    state.handle_raw_events(vec![key(KeyCode::Down), key(KeyCode::Down)]);
    assert_eq!(palette(&state).selected, 2);

    state.handle_raw_events(vec![typed('z'), typed('o'), typed('o')]);
    assert_eq!(palette(&state).query, "zoo");
    // A changed query invalidates the old ranking, so selection returns to the
    // new best match rather than staying on whatever row 2 now holds.
    assert_eq!(palette(&state).selected, 0);
    let filtered = visible_names(&state);
    assert!(filtered.len() < total, "query did not narrow the list");
    assert_eq!(filtered.first().map(String::as_str), Some("zoom"));

    // A subsequence that no command contains empties the list without closing.
    state.handle_raw_events(vec![typed('q'), typed('q'), typed('q')]);
    assert!(palette(&state).filtered.is_empty());
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::CommandPalette(_))
    ));

    // Backspacing back to a matching prefix restores the results.
    state.handle_raw_events((0..3).map(|_| key(KeyCode::Backspace)).collect());
    assert_eq!(palette(&state).query, "zoo");
    assert_eq!(
        visible_names(&state).first().map(String::as_str),
        Some("zoom")
    );
}

#[test]
fn enter_on_an_empty_result_list_is_a_no_op() {
    let mut state = default_state();
    open(&mut state);
    state.handle_raw_events(vec![typed('q'), typed('q'), typed('q')]);

    let outcome = state.handle_raw_events(vec![key(KeyCode::Enter)]);
    assert!(endpoint_methods(&outcome).is_empty());
    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::CommandPalette(_))),
        "an unrunnable enter closed the palette"
    );
}

#[test]
fn arrows_wrap_while_page_keys_clamp() {
    let mut state = default_state();
    open(&mut state);
    let last = palette(&state).filtered.len() - 1;
    assert!(last > 4, "catalog should hold several entries");

    // Single steps wrap around both ends.
    state.handle_raw_events(vec![key(KeyCode::Up)]);
    assert_eq!(palette(&state).selected, last);
    state.handle_raw_events(vec![key(KeyCode::Down)]);
    assert_eq!(palette(&state).selected, 0);
    // ctrl+p / ctrl+n are the same movement, not query text.
    state.handle_raw_events(vec![ctrl('p')]);
    assert_eq!(palette(&state).selected, last);
    state.handle_raw_events(vec![ctrl('n')]);
    assert_eq!(palette(&state).selected, 0);
    assert!(palette(&state).query.is_empty(), "movement chords typed");

    // Page keys stop at the ends instead of wrapping.
    state.handle_raw_events(vec![key(KeyCode::PageUp)]);
    assert_eq!(palette(&state).selected, 0);
    state.handle_raw_events(vec![key(KeyCode::End)]);
    assert_eq!(palette(&state).selected, last);
    state.handle_raw_events(vec![key(KeyCode::PageDown)]);
    assert_eq!(palette(&state).selected, last);
    state.handle_raw_events(vec![key(KeyCode::Home)]);
    assert_eq!(palette(&state).selected, 0);
}

#[test]
fn running_a_builtin_sends_the_same_request_its_chord_would_have() {
    let mut state = default_state();

    // The chord's own dispatch, for comparison.
    let mut direct = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::Zoom),
        &mut direct,
    );

    open(&mut state);
    state.handle_raw_events(vec![typed('z'), typed('o'), typed('o'), typed('m')]);
    assert_eq!(
        visible_names(&state).first().map(String::as_str),
        Some("zoom")
    );
    let outcome = state.handle_raw_events(vec![key(KeyCode::Enter)]);

    assert_eq!(endpoint_methods(&outcome), endpoint_methods(&direct));
    assert!(matches!(
        endpoint_methods(&outcome).as_slice(),
        [crate::api::schema::Method::PaneZoom(_)]
    ));
    assert!(state.overlay.is_none(), "palette stayed open after running");
}

#[test]
fn running_a_builtin_that_opens_an_overlay_leaves_that_overlay_open() {
    // The palette has to close *before* dispatch: `help` opens its own overlay,
    // and clearing afterwards would throw it away.
    let mut state = default_state();
    open(&mut state);
    state.handle_raw_events(vec![typed('h'), typed('e'), typed('l'), typed('p')]);
    state.handle_raw_events(vec![key(KeyCode::Enter)]);

    assert!(
        matches!(state.overlay, Some(ClientShellOverlay::Help(_))),
        "help overlay was clobbered by the palette closing"
    );
}

#[test]
fn an_index_bearing_builtin_routes_to_its_picker_instead_of_a_fixed_index() {
    let mut state = default_state();
    open(&mut state);
    // `switch-workspace 1` is not a palette row; the workspace picker is.
    let names = visible_names(&state);
    assert!(names.iter().any(|name| name == "workspace-picker"));
    assert!(!names.iter().any(|name| name.starts_with("switch-")));

    state.handle_raw_events(vec![
        typed('w'),
        typed('o'),
        typed('r'),
        typed('k'),
        typed('s'),
        typed('p'),
        typed('a'),
        typed('c'),
        typed('e'),
        typed('-'),
    ]);
    assert_eq!(
        visible_names(&state).first().map(String::as_str),
        Some("workspace-picker")
    );
    state.handle_raw_events(vec![key(KeyCode::Enter)]);
    assert_eq!(state.mode, ClientShellMode::Navigate);
}

#[test]
fn running_a_custom_command_invokes_only_the_opaque_endpoint_id() {
    let mut state = default_state();
    let mut projection = snapshot();
    projection.commands.push(shell_command(
        "cmd_deploy",
        "Deploy web",
        &["prefix+ctrl+d"],
    ));
    state.set_snapshot(Box::new(projection));

    open(&mut state);
    let deploy = palette(&state)
        .entries
        .iter()
        .find(|entry| entry.name == "Deploy web")
        .expect("labelled custom command in the catalog");
    assert_eq!(deploy.source, ClientPaletteSource::Custom);
    // A labelled custom shows its chord in the keybind column; the name is the
    // label, so the chord is not printed twice.
    assert_eq!(deploy.keybinding.as_deref(), Some("prefix+ctrl+d"));
    assert_eq!(deploy.description.as_deref(), Some("ship it"));

    state.handle_raw_events(vec![typed('D'), typed('e'), typed('p')]);
    let outcome = state.handle_raw_events(vec![key(KeyCode::Enter)]);
    let methods = endpoint_methods(&outcome);
    let [crate::api::schema::Method::CommandInvoke(params)] = methods.as_slice() else {
        panic!(
            "expected command.invoke, got {:?}",
            endpoint_methods(&outcome)
        );
    };
    assert_eq!(params.command_id, "cmd_deploy");
}

#[test]
fn both_keybinding_sources_recover_the_same_label_and_chord_split() {
    // The wire carries `binding_label` (label, else chord) and `binding_labels`
    // (the chords), never the original `Option<String>` label. Comparing them
    // recovers it, and both keybinding sources have to agree or the palette's
    // keybind column would be permanently blank under one of them.
    let labelled = shell_command("cmd_deploy", "Deploy web", &["prefix+ctrl+d"]);
    let unlabelled = shell_command("cmd_raw", "prefix+ctrl+j", &["prefix+ctrl+j"]);

    let mut endpoint_config = Config::default();
    endpoint_config.keys.prefix = "ctrl+b".into();
    for source in [
        ClientShellKeybindingSource::Local,
        ClientShellKeybindingSource::Endpoint,
    ] {
        let mut state = ClientShellState::new(
            ClientShellConfig::from_config(&Config::default()).with_keybinding_source(source),
        );
        let mut projection = snapshot();
        projection.server_keybindings_toml = endpoint_config.local_keybindings_profile_toml().ok();
        projection.commands = vec![labelled.clone(), unlabelled.clone()];
        state.set_snapshot(Box::new(projection));

        let commands = &state.config.keybinds.keybinds.custom_commands;
        let deploy = commands
            .iter()
            .find(|command| command.command == "cmd_deploy")
            .unwrap_or_else(|| panic!("{source:?}: labelled command survived the projection"));
        assert_eq!(deploy.label, "Deploy web", "{source:?}");
        assert_eq!(
            deploy.keybind_display.as_deref(),
            Some("prefix+ctrl+d"),
            "{source:?}"
        );

        let raw = commands
            .iter()
            .find(|command| command.command == "cmd_raw")
            .unwrap_or_else(|| panic!("{source:?}: label-less command survived the projection"));
        assert_eq!(raw.label, "prefix+ctrl+j", "{source:?}");
        assert!(raw.keybind_display.is_none(), "{source:?}");
    }
}

#[test]
fn a_label_less_custom_command_keeps_its_chord_as_the_name() {
    let mut state = default_state();
    let mut projection = snapshot();
    // The endpoint reports `binding_label == chord` when the config set no label.
    projection.commands.push(shell_command(
        "cmd_raw",
        "prefix+ctrl+j",
        &["prefix+ctrl+j"],
    ));
    state.set_snapshot(Box::new(projection));

    open(&mut state);
    let raw = palette(&state)
        .entries
        .iter()
        .find(|entry| entry.name == "prefix+ctrl+j")
        .expect("label-less custom command in the catalog");
    assert!(
        raw.keybinding.is_none(),
        "chord printed twice: {:?}",
        raw.keybinding
    );
}

#[test]
fn plugin_actions_are_fetched_on_open_and_merged_without_moving_the_selection() {
    let mut state = default_state();
    let opened = open(&mut state);
    assert!(palette(&state).loading_plugin_actions);
    let [ClientShellAction::Endpoint { request, .. }] = opened.actions.as_slice() else {
        panic!("expected one plugin.action.list request");
    };
    assert!(matches!(
        request.method,
        crate::api::schema::Method::PluginActionList(_)
    ));
    let request_id = request.id.clone();

    // Highlight a row, then let the catalog grow underneath it.
    state.handle_raw_events(vec![typed('z'), typed('o'), typed('o'), typed('m')]);
    let highlighted = palette(&state)
        .selected_entry()
        .expect("a highlighted row")
        .name
        .clone();

    let (repaint, _) = state.handle_endpoint_result(
        "boot-1",
        &request_id,
        Ok(crate::api::schema::ResponseResult::PluginActionList {
            actions: vec![
                plugin_action("acme", "build", "build"),
                plugin_action("other", "build", "build"),
            ],
        }),
    );
    assert!(repaint);
    assert!(!palette(&state).loading_plugin_actions);
    assert_eq!(
        palette(&state)
            .selected_entry()
            .expect("selection survived the merge")
            .name,
        highlighted,
        "the merge moved the selection out from under the user"
    );

    // Two different plugin actions sharing one title both survive: dedup keys on
    // handle identity, not display name.
    state.handle_raw_events((0..4).map(|_| key(KeyCode::Backspace)).collect());
    assert_eq!(
        palette(&state)
            .entries
            .iter()
            .filter(|entry| entry.name == "build")
            .count(),
        2
    );
}

#[test]
fn running_a_plugin_action_invokes_it_through_the_public_api() {
    let mut state = default_state();
    let opened = open(&mut state);
    let [ClientShellAction::Endpoint { request, .. }] = opened.actions.as_slice() else {
        panic!("expected one plugin.action.list request");
    };
    let request_id = request.id.clone();
    state.handle_endpoint_result(
        "boot-1",
        &request_id,
        Ok(crate::api::schema::ResponseResult::PluginActionList {
            actions: vec![plugin_action("acme", "deploy", "acme deploy")],
        }),
    );

    state.handle_raw_events(vec![typed('a'), typed('c'), typed('m'), typed('e')]);
    let outcome = state.handle_raw_events(vec![key(KeyCode::Enter)]);
    let methods = endpoint_methods(&outcome);
    let [crate::api::schema::Method::PluginActionInvoke(params)] = methods.as_slice() else {
        panic!(
            "expected plugin.action.invoke, got {:?}",
            endpoint_methods(&outcome)
        );
    };
    assert_eq!(params.plugin_id.as_deref(), Some("acme"));
    assert_eq!(params.action_id, "deploy");
}

#[test]
fn a_late_plugin_list_is_dropped_once_the_palette_has_closed() {
    let mut state = default_state();
    let opened = open(&mut state);
    let [ClientShellAction::Endpoint { request, .. }] = opened.actions.as_slice() else {
        panic!("expected one plugin.action.list request");
    };
    let request_id = request.id.clone();
    state.handle_raw_events(vec![key(KeyCode::Esc)]);

    let (repaint, actions) = state.handle_endpoint_result(
        "boot-1",
        &request_id,
        Ok(crate::api::schema::ResponseResult::PluginActionList {
            actions: vec![plugin_action("acme", "build", "build")],
        }),
    );
    assert!(!repaint);
    assert!(actions.is_empty());
    assert!(state.overlay.is_none(), "a late list reopened the palette");
}

#[test]
fn source_toggles_gate_the_catalog_and_the_plugin_request() {
    let mut config = Config::default();
    config.command_palette.sources.built_in = false;
    config.command_palette.sources.plugin = false;
    let mut state = state_with(&config);
    let mut projection = snapshot();
    projection.commands.push(shell_command(
        "cmd_deploy",
        "Deploy web",
        &["prefix+ctrl+d"],
    ));
    state.set_snapshot(Box::new(projection));

    let opened = open(&mut state);
    assert!(
        opened.actions.is_empty(),
        "plugin list requested with the plugin source off"
    );
    assert!(!palette(&state).loading_plugin_actions);
    assert!(palette(&state)
        .entries
        .iter()
        .all(|entry| entry.source == ClientPaletteSource::Custom));
    assert_eq!(visible_names(&state), ["Deploy web"]);
}

#[test]
fn source_toggles_follow_config_reload_rather_than_the_open_catalog() {
    // The gate is a config-driven flag, so a reload that turns a source off has
    // to change what the *next* open assembles even though the catalog it is
    // read from is rebuilt from scratch every time.
    let mut state = default_state();
    assert!(state.config.command_palette_sources.plugin);

    let mut next = Config::default();
    next.command_palette.sources.plugin = false;
    state.config.apply_live_config(&next, &[], &[]);
    assert!(!state.config.command_palette_sources.plugin);

    let opened = open(&mut state);
    assert!(opened.actions.is_empty());
    assert!(!palette(&state).loading_plugin_actions);
}

#[test]
fn the_palette_renders_its_query_rows_and_position_counter() {
    let mut state = default_state();
    let opened = open(&mut state);
    state.handle_raw_events(vec![typed('z'), typed('o'), typed('o'), typed('m')]);

    let composed_text = |state: &mut ClientShellState| {
        state
            .compose(120, 32)
            .expect("composed frame")
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>()
    };

    // While the plugin list is in flight the header says so rather than showing
    // a counter over a catalog that is still growing.
    let loading = composed_text(&mut state);
    assert!(loading.contains("> zoom"), "query line missing");
    assert!(loading.contains("Toggle pane zoom"), "description missing");
    assert!(loading.contains("built-in"), "source tag missing");
    assert!(loading.contains("loading plugins…"), "loading hint missing");

    let [ClientShellAction::Endpoint { request, .. }] = opened.actions.as_slice() else {
        panic!("expected one plugin.action.list request");
    };
    let request_id = request.id.clone();
    state.handle_endpoint_result(
        "boot-1",
        &request_id,
        Ok(crate::api::schema::ResponseResult::PluginActionList {
            actions: Vec::new(),
        }),
    );

    let settled = composed_text(&mut state);
    assert!(settled.contains("1/1"), "position counter missing");
    assert!(!settled.contains("loading plugins…"), "loading hint stuck");
    // The list viewport was measured, so page jumps have a real page size.
    assert!(state.hits.command_palette_list_height > 0);
    assert!(!state.hits.command_palette_rows.is_empty());
}

#[test]
fn clicking_a_row_runs_it_and_clicking_outside_closes_the_palette() {
    let mut state = default_state();
    open(&mut state);
    state.handle_raw_events(vec![typed('z'), typed('o'), typed('o'), typed('m')]);
    state.compose(120, 32).expect("composed frame");
    let (row, _) = state.hits.command_palette_rows[0];

    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: row.x + 1,
        row: row.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(matches!(
        endpoint_methods(&outcome).as_slice(),
        [crate::api::schema::Method::PaneZoom(_)]
    ));
    assert!(state.overlay.is_none());

    open(&mut state);
    state.compose(120, 32).expect("composed frame");
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 0,
        row: 0,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(state.overlay.is_none(), "click outside did not dismiss");
}

#[test]
fn fuzzy_score_ranks_contiguous_prefixes_above_scattered_subsequences() {
    use super::super::command_palette::fuzzy_score;

    assert!(fuzzy_score("rena", "rename-workspace").is_some());
    assert!(fuzzy_score("xyz", "rename-workspace").is_none());
    assert_eq!(fuzzy_score("", "zoom"), Some(0));
    let prefix = fuzzy_score("ren", "rename-tab").expect("prefix match");
    let scattered = fuzzy_score("ren", "reset-pane-navigation").expect("scattered match");
    assert!(
        prefix > scattered,
        "prefix {prefix} <= scattered {scattered}"
    );
}

#[test]
fn extreme_geometry_and_an_oversized_pasted_query_do_not_overflow() {
    // Every width the header derives is u16. A terminal past 10922 columns
    // overflows the 60%-of-screen popup sizing, and a pasted query wider than
    // u16::MAX overflows the counter-fit check and the cursor position. Both
    // are reachable, and both panic in a debug build.
    let mut state = default_state();
    open(&mut state);
    // Each dimension overflows independently, so probe them one at a time
    // rather than allocating a 20000x12000 cell buffer.
    state.compose(20000, 12).expect("wide composed frame");
    state.compose(60, 12000).expect("tall composed frame");

    state.handle_raw_events(vec![RawInputEvent::Paste("a".repeat(70_000))]);
    state
        .compose(120, 32)
        .expect("composed frame with a huge query");
    // The palette survives it as an ordinary empty result set.
    assert!(palette(&state).filtered.is_empty());
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::CommandPalette(_))
    ));

    // And it still degrades gracefully at the other extreme.
    for cols in [1u16, 2, 8, 20, 41] {
        for rows in [1u16, 2, 5, 11] {
            state.compose(cols, rows);
        }
    }
}

#[test]
fn unbinding_the_chord_disables_the_palette_entirely() {
    // The configuration docs promise `command_palette = ""` turns the feature
    // off. The chord is the only entrypoint — no menu item or mouse affordance
    // opens it — so an empty binding has to leave it unreachable.
    let mut config = Config::default();
    config.keys.command_palette = crate::config::BindingConfig::One(String::new());
    let mut state = state_with(&config);

    assert!(state
        .config
        .keybinds
        .keybinds
        .command_palette
        .label()
        .is_none());
    press_palette_chord(&mut state);
    assert!(
        state.overlay.is_none(),
        "unbound chord still opened the palette"
    );
}

#[test]
fn only_the_current_palettes_own_plugin_list_is_applied() {
    // Close and reopen fast enough and the first open's list is still in
    // flight. It must not land on the second open's palette: it would clear a
    // spinner whose own request has not returned, and the two opens can have
    // been assembled under different sources. Only the live request applies,
    // and it applies exactly once.
    let mut state = default_state();
    let first = open(&mut state);
    state.handle_raw_events(vec![key(KeyCode::Esc)]);
    let second = open(&mut state);

    let request_id = |outcome: &ClientShellInput| {
        let [ClientShellAction::Endpoint { request, .. }] = outcome.actions.as_slice() else {
            panic!("expected one plugin.action.list request");
        };
        request.id.clone()
    };
    let actions = || {
        Ok(crate::api::schema::ResponseResult::PluginActionList {
            actions: vec![
                plugin_action("acme", "build", "build"),
                plugin_action("acme", "deploy", "deploy"),
            ],
        })
    };

    let baseline = palette(&state).entries.len();
    let (repaint, _) = state.handle_endpoint_result("boot-1", &request_id(&first), actions());
    assert!(!repaint, "a superseded list was applied");
    assert_eq!(palette(&state).entries.len(), baseline);
    assert!(
        palette(&state).loading_plugin_actions,
        "a superseded list cleared the live request's spinner"
    );

    state.handle_endpoint_result("boot-1", &request_id(&second), actions());
    assert_eq!(palette(&state).entries.len(), baseline + 2);
    assert!(!palette(&state).loading_plugin_actions);
}
#[test]
fn a_palette_opened_before_the_first_snapshot_does_not_claim_to_be_loading() {
    // Endpoint requests are dropped while the client has no snapshot, so a
    // palette opened during attach latency would sit on "loading plugins…"
    // forever waiting for a response that was never sent.
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    assert!(state.snapshot.is_none());

    let opened = open(&mut state);
    assert!(opened.actions.is_empty(), "request sent without a snapshot");
    assert!(
        !palette(&state).loading_plugin_actions,
        "palette waits on a request that was never sent"
    );
    // Built-ins still resolve, so the palette is usable immediately.
    assert!(!palette(&state).filtered.is_empty());
}

#[test]
fn a_stale_plugin_list_cannot_smuggle_rows_past_a_disabled_source() {
    // Open A requests the list, then a config reload turns the plugin source
    // off before open B. A's in-flight response must not populate B, which was
    // deliberately assembled without plugin rows.
    let mut state = default_state();
    let first = open(&mut state);
    let [ClientShellAction::Endpoint { request, .. }] = first.actions.as_slice() else {
        panic!("expected one plugin.action.list request");
    };
    let stale_id = request.id.clone();
    state.handle_raw_events(vec![key(KeyCode::Esc)]);

    let mut next = Config::default();
    next.command_palette.sources.plugin = false;
    state.config.apply_live_config(&next, &[], &[]);
    let reopened = open(&mut state);
    assert!(reopened.actions.is_empty());

    let (repaint, _) = state.handle_endpoint_result(
        "boot-1",
        &stale_id,
        Ok(crate::api::schema::ResponseResult::PluginActionList {
            actions: vec![plugin_action("acme", "build", "build")],
        }),
    );
    assert!(!repaint, "a stale list was applied to a later palette");
    assert!(
        palette(&state)
            .entries
            .iter()
            .all(|entry| entry.source != ClientPaletteSource::Plugin),
        "plugin rows reached a palette with the plugin source disabled"
    );
}

#[test]
fn a_too_small_terminal_keeps_its_popup_clickable() {
    // `panel` draws the frame before the min-size guard returns, so a palette
    // that is too small to lay out still occupies screen. Reporting no popup
    // rect would make every click inside the visible box a dismiss.
    let mut state = default_state();
    open(&mut state);
    state.compose(30, 8).expect("composed frame");
    assert_ne!(
        state.hits.command_palette_popup,
        ratatui::layout::Rect::default(),
        "a drawn palette reported no clickable region"
    );
}

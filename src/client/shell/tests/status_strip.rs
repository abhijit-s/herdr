//! End-to-end characterization of the `[ui.status]` strip's client-shell seam:
//! it claims the tab bar's right edge, resolves endpoint-pushed directive values
//! against the *client's* palette, and truncates to the *client's* width.

use super::*;

use crate::protocol::ClientShellStatusSegment;

fn content(text: &str) -> ClientShellStatusSegment {
    ClientShellStatusSegment {
        text: text.to_string(),
        separator: false,
        fg: None,
        bg: None,
        modifiers: 0,
    }
}

fn separator(text: &str) -> ClientShellStatusSegment {
    ClientShellStatusSegment {
        separator: true,
        ..content(text)
    }
}

fn endpoint_status_entries() -> Vec<crate::protocol::ClientShellTabStatusSegment> {
    vec![crate::protocol::ClientShellTabStatusSegment {
        text: "host".into(),
        accent: false,
    }]
}

/// Compose one frame and return its top row (the tab bar) as text plus cells.
fn compose_tab_bar(
    projected: ClientShellSnapshot,
    cols: u16,
    config: &Config,
) -> (String, Vec<crate::protocol::CellData>) {
    let mut shell_config = ClientShellConfig::from_config(config);
    shell_config.mobile_width_threshold = 0;
    let mut state = ClientShellState::new(shell_config);
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    let frame = state.compose(cols, 20).expect("composed frame");
    let cells = frame.cells[..frame.width as usize].to_vec();
    let row = cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<String>();
    (row, cells)
}

fn tab_bar_row(projected: ClientShellSnapshot, cols: u16) -> String {
    compose_tab_bar(projected, cols, &Config::default()).0
}

/// Cell index (not byte offset) where `needle`'s symbols start on the bar.
fn cell_index(cells: &[crate::protocol::CellData], needle: &str) -> Option<usize> {
    let symbols = needle.chars().map(String::from).collect::<Vec<_>>();
    cells.windows(symbols.len()).position(|window| {
        window
            .iter()
            .zip(&symbols)
            .all(|(cell, s)| cell.symbol == *s)
    })
}

#[test]
fn strip_renders_right_aligned_and_leaves_the_tabs_their_own_zone() {
    let mut projected = snapshot();
    projected.status_strip = vec![content("main"), separator(" │ "), content("09:04")];
    projected.status_strip_budget = 28;

    let (row, cells) = compose_tab_bar(projected, 106, &Config::default());
    // Right-aligned: the strip is the last thing on the bar.
    assert!(row.trim_end().ends_with("main │ 09:04"), "row: {row:?}");
    // The tab zone survives to the strip's left: the new-tab control is still
    // drawn, and it sits before the strip rather than being pushed off the bar.
    let new_tab = cell_index(&cells, "+").expect("new-tab control on the bar");
    let strip = cell_index(&cells, "main").expect("strip on the bar");
    assert!(new_tab < strip, "row: {row:?}");
}

#[test]
fn strip_wins_the_right_edge_over_endpoint_tab_bar_entries() {
    // Both decorations target the same zone. The strip claims it, and
    // `tab_bar_right` must then reserve and draw nothing rather than stacking.
    let mut projected = snapshot();
    projected.tab_bar_right = endpoint_status_entries();
    projected.tab_bar_right_separator = " · ".into();
    projected.status_strip = vec![content("main")];
    projected.status_strip_budget = 28;

    let row = tab_bar_row(projected, 106);
    assert!(row.contains("main"), "row: {row:?}");
    assert!(
        !row.contains("host"),
        "tab_bar_right double-stacked: {row:?}"
    );
}

#[test]
fn a_configured_strip_keeps_the_edge_while_its_segments_resolve_empty() {
    // The endpoint drops segments that resolve empty, so a strip whose only
    // `#(command)` has not run yet projects no segments at all. Ownership of
    // the right edge must follow the configured budget rather than that list,
    // or both decorations flap every time the command output goes blank.
    let mut projected = snapshot();
    projected.tab_bar_right = endpoint_status_entries();
    projected.tab_bar_right_separator = " · ".into();
    projected.status_strip = Vec::new();
    projected.status_strip_budget = 28;

    let row = tab_bar_row(projected, 106);
    assert!(
        !row.contains("host"),
        "tab_bar_right reclaimed the edge from a blank strip: {row:?}"
    );
}

#[test]
fn endpoint_tab_bar_entries_keep_the_edge_when_no_strip_is_configured() {
    let mut projected = snapshot();
    projected.tab_bar_right = endpoint_status_entries();
    projected.tab_bar_right_separator = " · ".into();
    // A budget of zero is how the endpoint reports a disabled strip.
    projected.status_strip = vec![content("main")];
    projected.status_strip_budget = 0;

    let row = tab_bar_row(projected, 106);
    assert!(row.contains("host"), "row: {row:?}");
    assert!(!row.contains("main"), "disabled strip drew anyway: {row:?}");
}

#[test]
fn a_tight_budget_drops_leftmost_segments_and_their_dangling_separator() {
    let mut projected = snapshot();
    projected.status_strip = vec![content("main"), separator(" │ "), content("09:04")];
    // Only "09:04" fits, so "main" and the now-leading " │ " both go.
    projected.status_strip_budget = 6;

    let (row, cells) = compose_tab_bar(projected, 106, &Config::default());
    assert!(row.trim_end().ends_with("09:04"), "row: {row:?}");
    assert!(!row.contains("main"), "row: {row:?}");
    // The separator went with the segment it belonged to: the columns just
    // left of the clock are blank bar, not a dangling " │ ". (The sidebar draws
    // its own '│' elsewhere on this row, so check position, not the whole row.)
    let clock = cell_index(&cells, "09:04").expect("clock on the bar");
    let before = cells[clock
        .checked_sub(3)
        .expect("clock is not at the bar's left edge")..clock]
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<String>();
    assert_eq!(before, "   ", "dangling separator survived: {row:?}");
}

#[test]
fn a_lone_oversize_segment_is_ellipsis_truncated_to_the_budget() {
    let mut projected = snapshot();
    projected.status_strip = vec![content("2026-07-09")];
    projected.status_strip_budget = 5;

    let row = tab_bar_row(projected, 106);
    assert!(row.contains('…'), "row: {row:?}");
    assert!(!row.contains("2026-07-09"), "row: {row:?}");
}

#[test]
fn a_narrow_bar_clamps_the_strip_below_its_configured_budget() {
    // The budget is endpoint config, but the ceiling is this client's own
    // width: a narrow client truncates without the endpoint knowing.
    let mut projected = snapshot();
    projected.status_strip = vec![content("a-very-long-status-value")];
    projected.status_strip_budget = 60;

    let wide = tab_bar_row(projected.clone(), 200);
    assert!(wide.contains("a-very-long-status-value"), "wide: {wide:?}");

    let narrow = tab_bar_row(projected, 40);
    assert!(
        !narrow.contains("a-very-long-status-value"),
        "narrow bar did not clamp: {narrow:?}"
    );
}

#[test]
fn directive_colors_resolve_against_the_clients_own_palette() {
    // The endpoint sends raw directive values, never resolved colors, so the
    // same snapshot has to paint differently under two client themes.
    let mut projected = snapshot();
    projected.status_strip = vec![ClientShellStatusSegment {
        fg: Some("accent".into()),
        ..content("STATUS")
    }];
    projected.status_strip_budget = 28;

    let strip_fg = |theme: &str| {
        let mut config = Config::default();
        config.theme.name = Some(theme.to_string());
        let accent = crate::app::client_palette_from_config(&config).accent;
        let (_, cells) = compose_tab_bar(projected.clone(), 106, &config);
        let start = cell_index(&cells, "STATUS").expect("strip text on the bar");
        (cells[start].fg, crate::protocol::color_to_u32(accent))
    };

    let (catppuccin, catppuccin_accent) = strip_fg("catppuccin");
    assert_eq!(catppuccin, catppuccin_accent);
    let (tokyo, tokyo_accent) = strip_fg("tokyo-night");
    assert_eq!(tokyo, tokyo_accent);
    // The endpoint sent one snapshot; two themes painted two different colors.
    assert_ne!(catppuccin, tokyo);
}

//! Client-side presentation for the `[ui.status]` right strip.
//!
//! The endpoint resolves what the strip says (`src/app/status_strip.rs`); this
//! module decides how it looks and how much of it fits. Both halves are here
//! rather than on the endpoint because they depend on facts only the client
//! has: its own active [`Palette`] and its own terminal width. Two clients
//! attached to one session can therefore show the same strip in different
//! themes, and a narrow client truncates without affecting a wide one.
//!
//! Pure and allocation-light: fitting drops whole segments from the left, so
//! the survivors are always a suffix slice of the pushed segments, and the only
//! allocation is the ellipsis-truncated text of a lone oversize segment.

use super::*;

use ratatui::style::Color;

use crate::protocol::ClientShellStatusSegment;

/// Ellipsis appended when a lone oversize segment is truncated.
const ELLIPSIS: char = '…';

/// Resolve a herdr theme-token name to its color in the active [`Palette`].
///
/// Only the theme-only tokens are mapped. The names that overlap ANSI colors
/// (`green`/`yellow`/`red`/`blue`/`cyan`) deliberately return `None` so they
/// keep resolving to ANSI via `parse_color_opt`; the two sets stay disjoint.
/// An unknown name returns `None` and the caller leaves the style untouched.
fn theme_token(name: &str, palette: &Palette) -> Option<Color> {
    let color = match name {
        "accent" => palette.accent,
        "panel_bg" => palette.panel_bg,
        "surface0" => palette.surface0,
        "surface1" => palette.surface1,
        "surface_dim" => palette.surface_dim,
        "overlay0" => palette.overlay0,
        "overlay1" => palette.overlay1,
        "text" => palette.text,
        "subtext0" => palette.subtext0,
        "mauve" => palette.mauve,
        "teal" => palette.teal,
        "peach" => palette.peach,
        _ => return None,
    };
    Some(color)
}

/// Resolve one raw `#[fg=…]`/`#[bg=…]` value: a hex/rgb/ANSI color first, then
/// a theme token. Unparseable values resolve to `None` and are ignored.
fn resolve_color(value: &str, palette: &Palette) -> Option<Color> {
    crate::config::parse_color_opt(value).or_else(|| theme_token(value, palette))
}

/// Apply a segment's folded directive spec on top of the strip's base style.
/// The endpoint already collapsed `#[default]` resets, so an empty spec means
/// "use the base" and never "inherit from the previous segment".
fn segment_style(segment: &ClientShellStatusSegment, base: Style, palette: &Palette) -> Style {
    let mut style = base;
    if let Some(fg) = segment
        .fg
        .as_deref()
        .and_then(|fg| resolve_color(fg, palette))
    {
        style = style.fg(fg);
    }
    if let Some(bg) = segment
        .bg
        .as_deref()
        .and_then(|bg| resolve_color(bg, palette))
    {
        style = style.bg(bg);
    }
    style.add_modifier(Modifier::from_bits_truncate(segment.modifiers))
}

/// The strip's base style: the same muted foreground the `tab_bar_right`
/// entries use, so an unstyled strip blends into the rest of the bar.
fn base_style(palette: &Palette) -> Style {
    Style::default().fg(palette.overlay1).bg(palette.panel_bg)
}

fn segments_width(segments: &[ClientShellStatusSegment]) -> u16 {
    segments.iter().fold(0u16, |width, segment| {
        width.saturating_add(display_width(&segment.text))
    })
}

/// Truncate `text` to at most `budget` display columns, appending an ellipsis
/// when characters are dropped. Measures display columns, never bytes or chars,
/// so a wide glyph costs what it actually costs on screen.
fn truncate_to_columns(text: &str, budget: u16) -> String {
    if display_width(text) <= budget {
        return text.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    // Reserve one column for the ellipsis marker.
    let content_budget = usize::from(budget.saturating_sub(1));
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > content_budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push(ELLIPSIS);
    out
}

/// The part of the strip that fits into a budget.
///
/// Segments are dropped leftmost-first (along with a now-leading separator), so
/// what survives is always a suffix of the pushed list — hence a borrowed slice
/// rather than a rebuilt `Vec`.
pub(super) struct FittedStrip<'a> {
    segments: &'a [ClientShellStatusSegment],
    /// Replacement text for the first survivor when it alone still overflows.
    truncated_first: Option<String>,
    width: u16,
}

impl FittedStrip<'_> {
    pub(super) fn width(&self) -> u16 {
        self.width
    }

    /// Each surviving segment's text in draw order, with the lone oversize
    /// survivor's truncation substituted in.
    fn texts(&self) -> impl Iterator<Item = (&ClientShellStatusSegment, &str)> {
        self.segments.iter().enumerate().map(|(index, segment)| {
            let text = match (index, self.truncated_first.as_deref()) {
                (0, Some(truncated)) => truncated,
                _ => segment.text.as_str(),
            };
            (segment, text)
        })
    }
}

/// Fit the pushed segments into `budget` display columns: drop whole segments
/// leftmost-first until they fit, also dropping a separator left dangling at
/// the front; if a single surviving segment still overflows, truncate its text
/// with an ellipsis. Each survivor keeps the style it was resolved with, so
/// truncation can never leak a dropped segment's color onto a survivor.
pub(super) fn fit_strip(segments: &[ClientShellStatusSegment], budget: u16) -> FittedStrip<'_> {
    let mut remaining = segments;
    while remaining.len() > 1 && segments_width(remaining) > budget {
        remaining = &remaining[1..];
        // Drop a now-leading separator so no dangling " │ " remains.
        if remaining.len() > 1 && remaining[0].separator {
            remaining = &remaining[1..];
        }
    }

    let width = segments_width(remaining);
    if width <= budget {
        return FittedStrip {
            segments: remaining,
            truncated_first: None,
            width,
        };
    }

    // Lone oversize segment: the sole allowed mid-character truncation.
    let truncated = remaining
        .first()
        .map(|segment| truncate_to_columns(&segment.text, budget));
    let width = truncated.as_deref().map(display_width).unwrap_or(0);
    FittedStrip {
        segments: remaining,
        truncated_first: truncated,
        width,
    }
}

/// Whether the endpoint has a strip configured at all. Width-independent, so it
/// is a stable answer to "which decoration owns the tab bar's right edge?" even
/// on a terminal too narrow to actually show the strip.
pub(super) fn is_enabled(snapshot: &ClientShellSnapshot) -> bool {
    !snapshot.status_strip.is_empty() && snapshot.status_strip_budget > 0
}

/// Columns the strip wants on the tab bar, capped by both the endpoint's
/// configured `status_right_length` budget and this client's available width.
pub(super) fn status_strip_width(snapshot: &ClientShellSnapshot, available: u16) -> u16 {
    if !is_enabled(snapshot) {
        return 0;
    }
    fit_strip(
        &snapshot.status_strip,
        snapshot.status_strip_budget.min(available),
    )
    .width()
}

/// Draw the strip right-aligned into its reserved zone.
///
/// Reads only the pushed snapshot: no filesystem access, no process spawning,
/// no clock sampling. Every one of those happens on the endpoint's interval
/// tick, which is what keeps this safe to call from the tab-bar render path.
pub(super) fn render_status_strip(
    buffer: &mut Buffer,
    rect: Rect,
    snapshot: &ClientShellSnapshot,
    palette: &Palette,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let fitted = fit_strip(
        &snapshot.status_strip,
        snapshot.status_strip_budget.min(rect.width),
    );
    let base = base_style(palette);
    // Right-align within the reserved zone; `fit_strip` guarantees the content
    // is no wider than the zone, so this never underflows.
    let mut x = rect.right().saturating_sub(fitted.width());
    for (segment, text) in fitted.texts() {
        x = put_segment(
            buffer,
            x,
            rect.y,
            rect.right(),
            text,
            segment_style(segment, base, palette),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            text: text.to_string(),
            separator: true,
            ..content(text)
        }
    }

    fn styled(text: &str, fg: &str) -> ClientShellStatusSegment {
        ClientShellStatusSegment {
            fg: Some(fg.to_string()),
            ..content(text)
        }
    }

    fn joined(fitted: &FittedStrip<'_>) -> String {
        fitted.texts().map(|(_, text)| text).collect()
    }

    #[test]
    fn fit_keeps_everything_within_budget() {
        let segs = [content("main"), separator(" │ "), content("09:04")];
        let fitted = fit_strip(&segs, 40);
        assert_eq!(joined(&fitted), "main │ 09:04");
        assert_eq!(fitted.width(), 12);
    }

    #[test]
    fn fit_drops_leftmost_segment_and_its_dangling_separator() {
        let segs = [content("main"), separator(" │ "), content("09:04")];
        // Budget fits only "09:04": "main" AND the now-leading " │ " both go.
        let fitted = fit_strip(&segs, 6);
        assert_eq!(joined(&fitted), "09:04");
        assert_eq!(fitted.width(), 5);
    }

    #[test]
    fn fit_drops_until_only_the_rightmost_survives() {
        let segs = [
            content("aaaa"),
            separator(" │ "),
            content("bbbb"),
            separator(" │ "),
            content("cc"),
        ];
        assert_eq!(joined(&fit_strip(&segs, 3)), "cc");
    }

    #[test]
    fn fit_truncates_a_lone_oversize_segment_with_an_ellipsis() {
        let segs = [content("2026-07-09")];
        let fitted = fit_strip(&segs, 5);
        let out = joined(&fitted);
        assert!(out.ends_with(ELLIPSIS), "out: {out:?}");
        assert!(display_width(&out) <= 5, "out: {out:?}");
        assert_eq!(fitted.width(), display_width(&out));
    }

    #[test]
    fn fit_measures_display_columns_not_chars_for_wide_glyphs() {
        // Each CJK glyph is 2 columns, so four glyphs are 8 columns wide.
        let segs = [content("提交反馈")];
        assert_eq!(display_width("提交反馈"), 8);
        let out = joined(&fit_strip(&segs, 5));
        assert!(display_width(&out) <= 5, "out: {out:?}");
        assert!(out.ends_with(ELLIPSIS));
    }

    #[test]
    fn truncation_preserves_the_survivor_style() {
        let segs = [
            styled("aaaa", "red"),
            separator(" │ "),
            styled("bbbb", "blue"),
        ];
        let palette = Palette::catppuccin();
        let base = base_style(&palette);

        let full = fit_strip(&segs, 100);
        let dropped = fit_strip(&segs, 4);
        // The rightmost survivor keeps its blue fg whether or not the leftmost
        // was dropped, because each segment carries its own folded spec.
        for fitted in [&full, &dropped] {
            let (segment, text) = fitted.texts().last().expect("a survivor");
            assert_eq!(text, "bbbb");
            assert_eq!(segment_style(segment, base, &palette).fg, Some(Color::Blue));
        }
    }

    #[test]
    fn theme_tokens_resolve_against_the_client_palette() {
        let mut palette = Palette::catppuccin();
        palette.accent = Color::Rgb(1, 2, 3);
        let base = base_style(&palette);

        for (token, expected) in [
            ("accent", palette.accent),
            ("mauve", palette.mauve),
            ("teal", palette.teal),
            ("peach", palette.peach),
            ("text", palette.text),
            ("surface0", palette.surface0),
        ] {
            let style = segment_style(&styled("x", token), base, &palette);
            assert_eq!(style.fg, Some(expected), "token: {token}");
        }
    }

    #[test]
    fn ansi_names_hex_and_rgb_win_over_the_token_path() {
        let palette = Palette::catppuccin();
        let base = base_style(&palette);
        let fg = |value: &str| segment_style(&styled("x", value), base, &palette).fg;

        // These names overlap palette fields but must stay ANSI.
        assert_eq!(fg("blue"), Some(Color::Blue));
        assert_eq!(fg("green"), Some(Color::Green));
        assert_eq!(fg("red"), Some(Color::Red));
        assert_eq!(fg("yellow"), Some(Color::Yellow));
        assert_eq!(fg("cyan"), Some(Color::Cyan));
        assert_eq!(fg("#cba6f7"), Some(Color::Rgb(0xcb, 0xa6, 0xf7)));
        assert_eq!(fg("rgb(1,2,3)"), Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn an_unknown_color_value_leaves_the_base_style_untouched() {
        let palette = Palette::catppuccin();
        let base = base_style(&palette);
        assert_eq!(
            segment_style(&styled("x", "notacolor"), base, &palette).fg,
            base.fg
        );
    }

    #[test]
    fn modifier_bits_survive_the_wire_round_trip() {
        let palette = Palette::catppuccin();
        let base = base_style(&palette);
        let segment = ClientShellStatusSegment {
            modifiers: (Modifier::BOLD | Modifier::UNDERLINED).bits(),
            ..content("x")
        };
        let style = segment_style(&segment, base, &palette);
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(!style.add_modifier.contains(Modifier::ITALIC));
    }
}

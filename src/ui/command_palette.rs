use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::app::state::AppState;
use crate::ui::widgets::{modal_stack_areas, render_modal_shell};

/// `[start, end)` row window that keeps `selected` visible.
pub(crate) fn visible_window(total: usize, height: usize, selected: usize) -> (usize, usize) {
    if total <= height {
        return (0, total);
    }
    let half = height / 2;
    let start = selected.saturating_sub(half).min(total - height);
    (start, start + height)
}

/// Truncate to `max` columns with a trailing ellipsis (char-based).
pub(crate) fn truncate_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let keep: String = s.chars().take(max - 1).collect();
    format!("{keep}…")
}

/// 1-based `<selected+1>/<total>` position counter (e.g. `"12/63"`), so the user
/// can see the filtered list continues off-screen. `None` when the list is empty.
pub(crate) fn position_indicator(selected: usize, total: usize) -> Option<String> {
    if total == 0 {
        return None;
    }
    Some(format!("{}/{}", selected + 1, total))
}

/// Which column a rendered row segment belongs to (drives its style).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowSpanKind {
    Name,
    Description,
    Keybind,
    Tag,
    Pad,
}

/// Width-budget one palette row into styled segments that together occupy
/// exactly `width` columns (or fewer only when `width` cannot hold the name).
///
/// Priority when space is tight: the source `tag` and `name` are kept, the
/// `keybinding` is dropped next, and the `description` yields first. The right
/// cluster (keybind + tag) is pushed to the right edge with a `Pad` fill.
pub(crate) fn command_palette_row(
    name: &str,
    description: Option<&str>,
    keybinding: Option<&str>,
    tag: &str,
    width: usize,
) -> Vec<(RowSpanKind, String)> {
    let mut out: Vec<(RowSpanKind, String)> = Vec::new();
    if width == 0 {
        return out;
    }
    let lead = 1usize;
    let tag_w = tag.chars().count();
    let kb = keybinding.filter(|s| !s.is_empty());
    let kb_w = kb.map(|s| s.chars().count()).unwrap_or(0);

    // Decide which right-side columns fit. Reserve `lead + >=1 name col + gap`.
    let with_kb = kb_w + 2 + tag_w; // keybind + 2-col gap + tag
    let (show_kb, right_w) = if kb.is_some() && width >= lead + 2 + with_kb {
        (true, with_kb)
    } else if width >= lead + 2 + tag_w {
        (false, tag_w)
    } else {
        (false, 0) // too narrow even for the tag → name only
    };

    out.push((RowSpanKind::Pad, " ".repeat(lead)));

    if right_w == 0 {
        out.push((
            RowSpanKind::Name,
            truncate_ellipsis(name, width.saturating_sub(lead)),
        ));
        return out;
    }

    // Everything except the lead and the right cluster is the left region; the
    // guaranteed >=1 gap before the right cluster lives inside the middle Pad.
    let left_region = width - lead - right_w;
    let name_shown = truncate_ellipsis(name, left_region.saturating_sub(1).max(1));
    let name_len = name_shown.chars().count();
    out.push((RowSpanKind::Name, name_shown));

    // Description takes a leading gap + the remaining left-region columns.
    let remaining = left_region.saturating_sub(name_len);
    let mut desc_len = 0usize;
    if let Some(d) = description.filter(|s| !s.is_empty()) {
        if remaining >= 3 {
            let shown = truncate_ellipsis(d, remaining - 1);
            let seg = format!(" {shown}");
            desc_len = seg.chars().count();
            out.push((RowSpanKind::Description, seg));
        }
    }

    // Slack pushes the right cluster to the edge; totals sum to exactly `width`.
    let slack = width.saturating_sub(lead + name_len + desc_len + right_w);
    out.push((RowSpanKind::Pad, " ".repeat(slack)));

    if show_kb {
        if let Some(k) = kb {
            out.push((RowSpanKind::Keybind, k.to_string()));
            out.push((RowSpanKind::Pad, "  ".to_string()));
        }
    }
    out.push((RowSpanKind::Tag, tag.to_string()));
    out
}

pub(crate) fn render_command_palette_overlay(app: &AppState, frame: &mut Frame) {
    let area = frame.area();
    let p = &app.palette;
    let popup_w = area.width.saturating_mul(6) / 10; // ~60% width
    let popup_h = area.height.saturating_mul(6) / 10;
    // Minimum-size floor: filter line + at least a few rows. Refuse to render a
    // clipped box when the terminal is too small.
    let Some(inner) = render_modal_shell(frame, area, popup_w.max(20), popup_h.max(5), p) else {
        return;
    };
    let areas = modal_stack_areas(inner, 1, 0, 0, 1);
    let cp = &app.command_palette;

    // Filter line with the block cursor (mirrors the rename dialog input) plus a
    // right-aligned position counter. The query always wins: when the header is
    // too narrow to hold both with a 1-col gap, the counter is dropped.
    let header_style = Style::default().fg(p.text).bg(p.surface0);
    let query_text = format!(" {}█", cp.query);
    let query_w = query_text.chars().count();
    let mut spans: Vec<Span> = vec![Span::styled(query_text, header_style)];
    if let Some(indicator) = position_indicator(cp.selected, cp.visible().len()) {
        let header_w = areas.header.width as usize;
        let ind_w = indicator.chars().count();
        if header_w > query_w + ind_w {
            let pad = header_w - query_w - ind_w;
            spans.push(Span::styled(" ".repeat(pad), header_style));
            spans.push(Span::styled(
                indicator,
                header_style.fg(p.overlay0).add_modifier(Modifier::DIM),
            ));
        }
    }
    let filter = Paragraph::new(Line::from(spans)).style(header_style);
    frame.render_widget(Clear, areas.header);
    frame.render_widget(filter, areas.header);

    let list_area = areas.content;
    let height = list_area.height as usize;
    let visible = cp.visible();
    if visible.is_empty() {
        let msg = Paragraph::new(format!("  No commands match \"{}\"", cp.query))
            .style(Style::default().fg(p.overlay0).add_modifier(Modifier::DIM));
        frame.render_widget(msg, list_area);
        return;
    }
    if height == 0 {
        return;
    }

    let (start, end) = visible_window(visible.len(), height, cp.selected);
    for (row, vis_idx) in (start..end).enumerate() {
        let entry = &cp.entries[visible[vis_idx]];
        let selected = vis_idx == cp.selected;
        let row_area = Rect::new(list_area.x, list_area.y + row as u16, list_area.width, 1);
        let base = if selected {
            Style::default().fg(p.text).bg(p.surface1)
        } else {
            Style::default().fg(p.subtext0)
        };
        let dim = base.fg(p.overlay0).add_modifier(Modifier::DIM);
        let segments = command_palette_row(
            &entry.name,
            entry.description.as_deref(),
            entry.keybinding.as_deref(),
            entry.source.tag(),
            list_area.width as usize,
        );
        let spans: Vec<Span> = segments
            .into_iter()
            .map(|(kind, text)| match kind {
                RowSpanKind::Name | RowSpanKind::Pad => Span::styled(text, base),
                RowSpanKind::Description | RowSpanKind::Tag => Span::styled(text, dim),
                RowSpanKind::Keybind => Span::styled(text, base.fg(p.overlay1)),
            })
            .collect();
        frame.render_widget(Paragraph::new(Line::from(spans)).style(base), row_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_window_keeps_selection_in_view() {
        // 100 rows, viewport 10, selected near the end → window ends past selected
        let (start, end) = visible_window(100, 10, 95);
        assert!(start <= 95 && 95 < end);
        assert_eq!(end - start, 10);
        // selection at top
        assert_eq!(visible_window(100, 10, 0), (0, 10));
        // fewer rows than viewport
        assert_eq!(visible_window(3, 10, 0), (0, 3));
    }

    #[test]
    fn truncate_ellipsis_caps_width() {
        assert_eq!(truncate_ellipsis("rename-workspace", 8), "rename-…");
        assert_eq!(truncate_ellipsis("zoom", 8), "zoom");
    }

    fn row_width(segments: &[(RowSpanKind, String)]) -> usize {
        segments.iter().map(|(_, s)| s.chars().count()).sum()
    }

    fn kind_text(segments: &[(RowSpanKind, String)], kind: RowSpanKind) -> Option<&str> {
        segments
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, s)| s.as_str())
    }

    #[test]
    fn command_palette_row_fits_all_four_fields_when_wide() {
        let width = 60;
        let seg = command_palette_row("zoom", Some("Toggle pane zoom"), Some("prefix+z"), "built-in", width);
        // occupies exactly the available width
        assert_eq!(row_width(&seg), width);
        // every column is present and un-truncated at a comfortable width
        assert_eq!(kind_text(&seg, RowSpanKind::Name), Some("zoom"));
        assert_eq!(kind_text(&seg, RowSpanKind::Keybind), Some("prefix+z"));
        assert_eq!(kind_text(&seg, RowSpanKind::Tag), Some("built-in"));
        assert!(kind_text(&seg, RowSpanKind::Description)
            .unwrap()
            .contains("Toggle pane zoom"));
    }

    #[test]
    fn command_palette_row_description_yields_first_when_tight() {
        // enough for name + keybind + tag, but not the description
        let seg = command_palette_row(
            "rename-workspace",
            Some("Rename the current workspace"),
            Some("prefix+,"),
            "built-in",
            34,
        );
        assert!(row_width(&seg) <= 34);
        assert_eq!(kind_text(&seg, RowSpanKind::Tag), Some("built-in"));
        assert_eq!(kind_text(&seg, RowSpanKind::Keybind), Some("prefix+,"));
        // description dropped before name/keybind are sacrificed
        assert!(kind_text(&seg, RowSpanKind::Description).is_none());
    }

    #[test]
    fn command_palette_row_never_overflows_narrow_width() {
        // sweep narrow widths; the row must never exceed the budget
        for width in 1..=30usize {
            let seg = command_palette_row(
                "open-notification-target",
                Some("Jump to the notification target"),
                Some("prefix+n"),
                "built-in",
                width,
            );
            assert!(
                row_width(&seg) <= width,
                "row overflowed at width {width}: {}",
                row_width(&seg)
            );
        }
    }

    #[test]
    fn position_indicator_is_one_based_and_none_when_empty() {
        assert_eq!(position_indicator(0, 63).as_deref(), Some("1/63"));
        assert_eq!(position_indicator(11, 63).as_deref(), Some("12/63"));
        assert_eq!(position_indicator(0, 0), None);
    }

    #[test]
    fn command_palette_row_blank_keybind_shows_name_and_tag_only() {
        let seg = command_palette_row("build", None, None, "custom", 40);
        assert_eq!(row_width(&seg), 40);
        assert!(kind_text(&seg, RowSpanKind::Keybind).is_none());
        assert_eq!(kind_text(&seg, RowSpanKind::Name), Some("build"));
        assert_eq!(kind_text(&seg, RowSpanKind::Tag), Some("custom"));
    }
}

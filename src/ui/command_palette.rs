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

    // Filter line with the block cursor (mirrors the rename dialog input).
    let filter = Paragraph::new(format!(" {}█", cp.query))
        .style(Style::default().fg(p.text).bg(p.surface0));
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
        let tag = entry.source.tag();
        let name_w = (list_area.width as usize).saturating_sub(tag.len() + 3);
        let name = truncate_ellipsis(&entry.name, name_w);
        let line = Line::from(vec![
            Span::raw(format!(" {name:<name_w$} ")),
            Span::styled(
                tag,
                Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
            ),
        ]);
        let style = if selected {
            Style::default().fg(p.text).bg(p.surface1)
        } else {
            Style::default().fg(p.subtext0)
        };
        frame.render_widget(Paragraph::new(line).style(style), row_area);
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
}

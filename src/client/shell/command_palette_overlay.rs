use super::*;

/// `[start, end)` row window that keeps `selected` visible.
pub(super) fn visible_window(total: usize, height: usize, selected: usize) -> (usize, usize) {
    if height == 0 {
        return (0, 0);
    }
    if total <= height {
        return (0, total);
    }
    let start = selected.saturating_sub(height / 2).min(total - height);
    (start, start + height)
}

/// Truncate to `max` display columns, with a trailing ellipsis when anything
/// was dropped. Measured in columns rather than chars so a wide glyph cannot
/// overflow the cell budget the row was given.
pub(super) fn truncate_ellipsis(text: &str, max: u16) -> String {
    if display_width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max.saturating_sub(1);
    let mut used = 0u16;
    let mut kept = String::new();
    for character in text.chars() {
        let width = display_width(character.encode_utf8(&mut [0u8; 4]));
        if used + width > budget {
            break;
        }
        used += width;
        kept.push(character);
    }
    kept.push('…');
    kept
}

/// 1-based `<selected+1>/<total>` counter (e.g. `"12/63"`) so the user can see
/// the filtered list continues off-screen. `None` when the list is empty.
pub(super) fn position_indicator(selected: usize, total: usize) -> Option<String> {
    (total > 0).then(|| format!("{}/{}", selected + 1, total))
}

/// Which column a rendered row segment belongs to, which drives its style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowSpanKind {
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
/// cluster (keybind and tag) is pushed to the right edge with a `Pad` fill.
pub(super) fn command_palette_row(
    name: &str,
    description: Option<&str>,
    keybinding: Option<&str>,
    tag: &str,
    width: u16,
) -> Vec<(RowSpanKind, String)> {
    let mut out = Vec::new();
    if width == 0 {
        return out;
    }
    let lead = 1u16;
    let tag_width = display_width(tag);
    let keybind = keybinding.filter(|value| !value.is_empty());
    let keybind_width = keybind.map(display_width).unwrap_or(0);

    // Decide which right-side columns fit, reserving the lead, at least one
    // name column, and a gap before the right cluster. A chord label is joined
    // from every binding of one action, so it is config-sized rather than
    // screen-sized: these sums saturate instead of wrapping.
    let with_keybind = keybind_width.saturating_add(2).saturating_add(tag_width);
    let (show_keybind, right_width) =
        if keybind.is_some() && width >= lead.saturating_add(2).saturating_add(with_keybind) {
            (true, with_keybind)
        } else if width >= lead.saturating_add(2).saturating_add(tag_width) {
            (false, tag_width)
        } else {
            (false, 0)
        };

    out.push((RowSpanKind::Pad, " ".repeat(usize::from(lead))));

    if right_width == 0 {
        out.push((
            RowSpanKind::Name,
            truncate_ellipsis(name, width.saturating_sub(lead)),
        ));
        return out;
    }

    // Everything except the lead and the right cluster is the left region; the
    // guaranteed gap before the right cluster lives inside the middle Pad.
    let left_region = width - lead - right_width;
    let shown_name = truncate_ellipsis(name, left_region.saturating_sub(1).max(1));
    let name_width = display_width(&shown_name);
    out.push((RowSpanKind::Name, shown_name));

    let remaining = left_region.saturating_sub(name_width);
    let mut description_width = 0;
    if let Some(description) = description.filter(|value| !value.is_empty()) {
        if remaining >= 3 {
            let segment = format!(" {}", truncate_ellipsis(description, remaining - 1));
            description_width = display_width(&segment);
            out.push((RowSpanKind::Description, segment));
        }
    }

    // Slack pushes the right cluster to the edge; the totals sum to `width`.
    let slack = width.saturating_sub(lead + name_width + description_width + right_width);
    out.push((RowSpanKind::Pad, " ".repeat(usize::from(slack))));

    if let Some(keybind) = keybind.filter(|_| show_keybind) {
        out.push((RowSpanKind::Keybind, keybind.to_owned()));
        out.push((RowSpanKind::Pad, "  ".to_owned()));
    }
    out.push((RowSpanKind::Tag, tag.to_owned()));
    out
}

pub(super) fn render_command_palette_overlay(
    b: &mut Buffer,
    palette: &ClientCommandPaletteOverlay,
    p: &Palette,
    rounded: bool,
) -> Option<OverlayRender> {
    // Widened before scaling: `u16 * 6` overflows past 10922 columns or rows.
    let six_tenths =
        |available: u16| (u32::from(available) * 6 / 10).min(u32::from(u16::MAX)) as u16;
    let outer = popup(
        b.area,
        six_tenths(b.area.width).max(40),
        six_tenths(b.area.height).max(10),
    )?;
    let inner = panel(b, outer, p.accent, p.panel_bg, rounded)?;
    if inner.width < 12 || inner.height < 4 {
        // Too small for the header and a list row, but `panel` has already
        // drawn the frame. Report the popup so a click inside the visible box
        // is not treated as a click-outside dismiss.
        return Some(OverlayRender {
            command_palette_popup: outer,
            ..OverlayRender::default()
        });
    }

    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    let header = Rect::new(inner.x, inner.y, inner.width, 1);
    let query = format!(" > {}", palette.query);
    // A pasted query can be far wider than any terminal, so every width derived
    // from it saturates rather than wrapping.
    let query_width = display_width(&query);
    put_text(b, header.x, header.y, header.width, &query, base.fg(p.text));
    let counter = if palette.loading_plugin_actions {
        Some("loading plugins…".to_owned())
    } else {
        position_indicator(palette.selected, palette.filtered.len())
    };
    if let Some(counter) = counter {
        // The query always wins the header: drop the counter rather than let it
        // collide with a long query.
        if header.width > query_width.saturating_add(display_width(&counter)) {
            put_right_text(b, header, header.y, &counter, base.fg(p.overlay0));
        }
    }
    put_text(
        b,
        inner.x,
        inner.y + 1,
        inner.width,
        &"─".repeat(usize::from(inner.width)),
        base.fg(p.surface1),
    );

    let body = Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 3);
    let mut rows = Vec::new();
    if palette.filtered.is_empty() {
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            &format!(" no commands match \"{}\"", palette.query),
            base.fg(p.overlay0),
        );
    } else {
        let (start, end) = visible_window(
            palette.filtered.len(),
            usize::from(body.height),
            palette.selected,
        );
        for (offset, position) in (start..end).enumerate() {
            let Some(entry) = palette
                .filtered
                .get(position)
                .and_then(|index| palette.entries.get(*index))
            else {
                continue;
            };
            let rect = Rect::new(body.x, body.y + offset as u16, body.width, 1);
            rows.push((rect, position));
            let selected = position == palette.selected;
            let row_style = if selected {
                base.fg(panel_contrast_fg(p)).bg(p.accent)
            } else {
                base.fg(p.subtext0)
            };
            let dim_style = if selected {
                row_style
            } else {
                base.fg(p.overlay0)
            };
            b.set_style(rect, row_style);
            let mut x = rect.x;
            for (kind, text) in command_palette_row(
                &entry.name,
                entry.description.as_deref(),
                entry.keybinding.as_deref(),
                entry.source.tag(),
                rect.width,
            ) {
                let style = match kind {
                    RowSpanKind::Name | RowSpanKind::Pad => row_style,
                    RowSpanKind::Description | RowSpanKind::Tag => dim_style,
                    RowSpanKind::Keybind => {
                        if selected {
                            row_style
                        } else {
                            base.fg(p.overlay1)
                        }
                    }
                };
                x = put_segment(b, x, rect.y, rect.right(), &text, style);
            }
        }
    }

    put_text(
        b,
        inner.x,
        inner.bottom() - 1,
        inner.width,
        " filter type · move ↑↓/ctrl+n/p · page pgup/pgdn · run enter · close esc",
        base.fg(p.overlay0),
    );

    Some(OverlayRender {
        command_palette_popup: outer,
        command_palette_rows: rows,
        command_palette_list_height: usize::from(body.height),
        cursor: Some(crate::protocol::CursorState {
            x: header
                .x
                .saturating_add(query_width)
                .min(header.right().saturating_sub(1)),
            y: header.y,
            visible: true,
            shape: 0,
        }),
        ..OverlayRender::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_width(segments: &[(RowSpanKind, String)]) -> u16 {
        segments
            .iter()
            .map(|(_, text)| display_width(text))
            .sum::<u16>()
    }

    fn kind_text(segments: &[(RowSpanKind, String)], kind: RowSpanKind) -> Option<&str> {
        segments
            .iter()
            .find(|(candidate, _)| *candidate == kind)
            .map(|(_, text)| text.as_str())
    }

    #[test]
    fn visible_window_keeps_selection_in_view() {
        let (start, end) = visible_window(100, 10, 95);
        assert!(start <= 95 && 95 < end);
        assert_eq!(end - start, 10);
        assert_eq!(visible_window(100, 10, 0), (0, 10));
        assert_eq!(visible_window(3, 10, 0), (0, 3));
        assert_eq!(visible_window(3, 0, 0), (0, 0));
    }

    #[test]
    fn truncate_ellipsis_caps_display_width() {
        assert_eq!(truncate_ellipsis("rename-workspace", 8), "rename-…");
        assert_eq!(truncate_ellipsis("zoom", 8), "zoom");
        // a double-width glyph consumes two columns of the budget
        assert_eq!(display_width(&truncate_ellipsis("日本語テスト", 5)), 5);
    }

    #[test]
    fn command_palette_row_fits_all_four_fields_when_wide() {
        let segments = command_palette_row(
            "zoom",
            Some("Toggle pane zoom"),
            Some("prefix+z"),
            "built-in",
            60,
        );
        assert_eq!(row_width(&segments), 60);
        assert_eq!(kind_text(&segments, RowSpanKind::Name), Some("zoom"));
        assert_eq!(kind_text(&segments, RowSpanKind::Keybind), Some("prefix+z"));
        assert_eq!(kind_text(&segments, RowSpanKind::Tag), Some("built-in"));
        assert!(kind_text(&segments, RowSpanKind::Description)
            .expect("description column")
            .contains("Toggle pane zoom"));
    }

    #[test]
    fn command_palette_row_description_yields_before_keybind_and_tag() {
        let segments = command_palette_row(
            "rename-workspace",
            Some("Rename the current workspace"),
            Some("prefix+,"),
            "built-in",
            34,
        );
        assert!(row_width(&segments) <= 34);
        assert_eq!(kind_text(&segments, RowSpanKind::Tag), Some("built-in"));
        assert_eq!(kind_text(&segments, RowSpanKind::Keybind), Some("prefix+,"));
        assert!(kind_text(&segments, RowSpanKind::Description).is_none());
    }

    #[test]
    fn command_palette_row_never_overflows_narrow_width() {
        for width in 1..=30u16 {
            let segments = command_palette_row(
                "open-notification-target",
                Some("Jump to the notification target"),
                Some("prefix+n"),
                "built-in",
                width,
            );
            assert!(
                row_width(&segments) <= width,
                "row overflowed at width {width}: {}",
                row_width(&segments)
            );
        }
    }

    #[test]
    fn command_palette_row_blank_keybind_shows_name_and_tag_only() {
        let segments = command_palette_row("build", None, None, "custom", 40);
        assert_eq!(row_width(&segments), 40);
        assert!(kind_text(&segments, RowSpanKind::Keybind).is_none());
        assert_eq!(kind_text(&segments, RowSpanKind::Name), Some("build"));
        assert_eq!(kind_text(&segments, RowSpanKind::Tag), Some("custom"));
    }

    #[test]
    fn position_indicator_is_one_based_and_none_when_empty() {
        assert_eq!(position_indicator(0, 63).as_deref(), Some("1/63"));
        assert_eq!(position_indicator(11, 63).as_deref(), Some("12/63"));
        assert_eq!(position_indicator(0, 0), None);
    }
}

//! tmux-style right status strip: parsing, resolution, clock formatting, and
//! budget-aware fit/drop layout.
//!
//! This module holds all real logic for the strip so the high-churn tab-bar
//! and headless render loop only need thin call-outs (KTD7). Everything here is
//! pure except [`ClockTime::now_local`] (which samples the wall clock) and
//! [`spawn_status_command`] (which spawns a process) — neither is called from
//! `render`; both run on the server interval tick (KTD6).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Upper bound on characters kept from a `#(command)` line before display-width
/// budgeting. Mirrors the length cap in `normalize_custom_status`.
const MAX_SEGMENT_CHARS: usize = 256;

/// Ellipsis appended when a lone oversize segment is truncated (KTD4).
const ELLIPSIS: char = '…';

/// One parsed piece of a `status_right` format string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Literal text (also acts as a droppable separator between content).
    Literal(String),
    /// A `%`-strftime subset run, e.g. `%H:%M`.
    Clock(String),
    /// A `#(command)` whose stdout becomes the segment text.
    Command(String),
}

/// Whether a resolved segment carries content or is a droppable separator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegmentKind {
    Literal,
    Content,
}

/// A segment resolved to its current display string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedSegment {
    pub kind: SegmentKind,
    pub text: String,
}

fn is_clock_specifier(c: char) -> bool {
    matches!(c, 'H' | 'M' | 'S' | 'd' | 'b' | 'Y')
}

fn is_clock_connector(c: char) -> bool {
    matches!(c, ':' | ' ' | '-' | '/' | '.' | ',')
}

/// Parse a `status_right` format string into ordered segments without executing
/// anything. Malformed tokens (unclosed `#(`, unknown `%X`) degrade to literal
/// text rather than erroring.
pub(crate) fn parse_status_right(input: &str) -> Vec<Segment> {
    let chars: Vec<char> = input.chars().collect();
    let mut segments: Vec<Segment> = Vec::new();
    let mut literal = String::new();
    let mut i = 0;

    let flush = |literal: &mut String, segments: &mut Vec<Segment>| {
        if !literal.is_empty() {
            segments.push(Segment::Literal(std::mem::take(literal)));
        }
    };

    while i < chars.len() {
        let c = chars[i];

        if c == '#' && chars.get(i + 1) == Some(&'(') {
            if let Some(close) = (i + 2..chars.len()).find(|&j| chars[j] == ')') {
                flush(&mut literal, &mut segments);
                let inner: String = chars[i + 2..close].iter().collect();
                segments.push(Segment::Command(inner));
                i = close + 1;
                continue;
            }
            // Unclosed `#(` degrades to literal text.
            literal.push('#');
            i += 1;
            continue;
        }

        if c == '%' {
            match chars.get(i + 1) {
                Some('%') => {
                    literal.push('%');
                    i += 2;
                    continue;
                }
                Some(&next) if is_clock_specifier(next) => {
                    flush(&mut literal, &mut segments);
                    let (clock, next_i) = consume_clock_run(&chars, i);
                    segments.push(Segment::Clock(clock));
                    i = next_i;
                    continue;
                }
                // A lone `%` (e.g. a trailing percent sign) is literal.
                _ => {
                    literal.push('%');
                    i += 1;
                    continue;
                }
            }
        }

        literal.push(c);
        i += 1;
    }

    flush(&mut literal, &mut segments);
    segments
}

/// Consume a maximal clock run starting at `start` (a `%` followed by a valid
/// specifier). Connector characters are only absorbed when another clock token
/// follows, so trailing separators stay as literal text.
fn consume_clock_run(chars: &[char], start: usize) -> (String, usize) {
    let mut buf = String::new();
    buf.push('%');
    buf.push(chars[start + 1]);
    let mut i = start + 2;

    loop {
        let mut j = i;
        while j < chars.len() && is_clock_connector(chars[j]) {
            j += 1;
        }
        let follows_token =
            chars.get(j) == Some(&'%') && chars.get(j + 1).copied().is_some_and(is_clock_specifier);
        if follows_token {
            for &connector in &chars[i..j] {
                buf.push(connector);
            }
            buf.push('%');
            buf.push(chars[j + 1]);
            i = j + 2;
        } else {
            break;
        }
    }

    (buf, i)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Wall-clock fields needed by the strftime subset. Kept free of any time
/// library so [`format_clock`] is deterministic and unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClockTime {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub day: u8,
    pub month: u8,
    pub year: i16,
}

impl ClockTime {
    /// Sample the current local time. This is the single time-library
    /// touchpoint; it runs on the interval tick, never inside `render`.
    pub(crate) fn now_local() -> Self {
        let zoned = jiff::Zoned::now();
        Self {
            hour: zoned.hour().max(0) as u8,
            minute: zoned.minute().max(0) as u8,
            second: zoned.second().max(0) as u8,
            day: zoned.day().max(0) as u8,
            month: zoned.month().max(0) as u8,
            year: zoned.year(),
        }
    }
}

/// Format a `%`-strftime subset (`%H %M %S %d %b %Y %%`). Unknown tokens render
/// literally (the parser already downgrades most, but this stays robust).
fn format_clock(fmt: &str, t: &ClockTime) -> String {
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' {
            match chars.get(i + 1) {
                Some('H') => out.push_str(&format!("{:02}", t.hour)),
                Some('M') => out.push_str(&format!("{:02}", t.minute)),
                Some('S') => out.push_str(&format!("{:02}", t.second)),
                Some('d') => out.push_str(&format!("{:02}", t.day)),
                Some('b') => out.push_str(
                    MONTHS
                        .get((t.month as usize).wrapping_sub(1))
                        .copied()
                        .unwrap_or(""),
                ),
                Some('Y') => out.push_str(&t.year.to_string()),
                Some('%') => out.push('%'),
                Some(&other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Sanitize `#(command)` output before it reaches the tab-bar draw: keep the
/// first line, drop control/escape bytes (so a crafted git branch name cannot
/// inject escape sequences), and cap length. Mirrors `normalize_custom_status`.
pub(crate) fn sanitize_command_output(raw: &str) -> String {
    let first_line = raw.lines().next().unwrap_or("");
    first_line
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_SEGMENT_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate `text` to at most `budget` display columns, appending an ellipsis
/// when characters are dropped. Measures display columns, never bytes/chars.
fn truncate_to_columns(text: &str, budget: usize) -> String {
    if display_width(text) <= budget {
        return text.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    // Reserve one column for the ellipsis marker.
    let content_budget = budget.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > content_budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push(ELLIPSIS);
    out
}

/// Fit resolved segments into `budget` display columns (KTD4): drop whole
/// segments leftmost-first (also dropping the now-leading separator) until they
/// fit; if a single surviving segment still overflows, truncate it with `…`.
pub(crate) fn fit_segments(resolved: &[ResolvedSegment], budget: usize) -> String {
    let joined = |segs: &[ResolvedSegment]| -> String {
        segs.iter().map(|s| s.text.as_str()).collect::<String>()
    };

    if display_width(&joined(resolved)) <= budget {
        return joined(resolved);
    }

    let mut remaining: Vec<ResolvedSegment> = resolved.to_vec();
    while remaining.len() > 1 && display_width(&joined(&remaining)) > budget {
        remaining.remove(0);
        // Drop a now-leading separator so no dangling " │ " remains.
        if remaining.len() > 1 && remaining[0].kind == SegmentKind::Literal {
            remaining.remove(0);
        }
    }

    let line = joined(&remaining);
    if display_width(&line) > budget {
        // Lone oversize segment: the sole allowed mid-character truncation.
        return truncate_to_columns(&line, budget);
    }
    line
}

/// Per-`#(command)` scheduling and last-known value. Pure data on `AppState`;
/// nothing exposes it over the JSON API (KTD2/OQ3 — render-scheduling state).
#[derive(Debug, Clone, Default)]
pub(crate) struct CommandSlot {
    pub last_value: Option<String>,
    pub last_run: Option<Instant>,
    pub in_flight: bool,
}

/// Parsed strip config plus resolved caches. Lives on `AppState`; updated only
/// on the interval tick (clock sampling, command completions), read by
/// `compute_view`/`render`.
#[derive(Debug, Clone, Default)]
pub(crate) struct StatusStripState {
    raw: String,
    segments: Vec<Segment>,
    budget: usize,
    interval: Duration,
    clock_texts: HashMap<String, String>,
    commands: HashMap<String, CommandSlot>,
}

impl StatusStripState {
    pub(crate) fn from_config(cfg: &crate::config::StatusConfig) -> Self {
        let segments = parse_status_right(&cfg.status_right);
        let mut commands = HashMap::new();
        for seg in &segments {
            if let Segment::Command(cmd) = seg {
                commands.entry(cmd.clone()).or_default();
            }
        }
        Self {
            raw: cfg.status_right.clone(),
            segments,
            budget: cfg.status_right_length,
            interval: Duration::from_secs(cfg.effective_interval_seconds()),
            clock_texts: HashMap::new(),
            commands,
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        !self.raw.trim().is_empty() && self.budget > 0
    }

    fn has_clock(&self) -> bool {
        self.segments.iter().any(|s| matches!(s, Segment::Clock(_)))
    }

    fn has_seconds_field(&self) -> bool {
        self.segments.iter().any(|s| match s {
            Segment::Clock(fmt) => fmt.contains("%S"),
            _ => false,
        })
    }

    /// Repaint cadence for the clock: ~1s when a seconds field is present, else
    /// 60s; `None` when there is no clock segment (KTD5). Independent of the
    /// `#(command)` refresh interval.
    pub(crate) fn clock_period(&self) -> Option<Duration> {
        if !self.has_clock() {
            None
        } else if self.has_seconds_field() {
            Some(Duration::from_secs(1))
        } else {
            Some(Duration::from_secs(60))
        }
    }

    pub(crate) fn has_commands(&self) -> bool {
        !self.commands.is_empty()
    }

    pub(crate) fn command_interval(&self) -> Duration {
        self.interval
    }

    /// Format every distinct clock segment against `now` and cache the result.
    pub(crate) fn refresh_clock(&mut self, now: &ClockTime) {
        for seg in &self.segments {
            if let Segment::Clock(fmt) = seg {
                self.clock_texts.insert(fmt.clone(), format_clock(fmt, now));
            }
        }
    }

    /// Commands due to run: never-run, or `last_run + interval` elapsed, and not
    /// already in flight.
    pub(crate) fn due_commands(&self, now: Instant) -> Vec<String> {
        self.commands
            .iter()
            .filter(|(_, slot)| {
                !slot.in_flight
                    && slot
                        .last_run
                        .is_none_or(|last| now.saturating_duration_since(last) >= self.interval)
            })
            .map(|(cmd, _)| cmd.clone())
            .collect()
    }

    /// Mark a command spawned: record the start time and set the in-flight flag
    /// so it is not re-spawned while running (skip-if-in-flight, KTD5).
    pub(crate) fn mark_command_started(&mut self, command: &str, now: Instant) {
        if let Some(slot) = self.commands.get_mut(command) {
            slot.in_flight = true;
            slot.last_run = Some(now);
        }
    }

    /// Apply a completed command's result. On success with non-empty output the
    /// sanitized value replaces the cache; on error/empty the last good value is
    /// retained. Returns whether the displayed value changed.
    pub(crate) fn apply_command_result(
        &mut self,
        command: &str,
        result: Result<String, String>,
    ) -> bool {
        let Some(slot) = self.commands.get_mut(command) else {
            return false;
        };
        slot.in_flight = false;
        match result {
            Ok(output) => {
                let value = sanitize_command_output(&output);
                if value.is_empty() {
                    // Empty output: keep the last good value.
                    return false;
                }
                let changed = slot.last_value.as_deref() != Some(value.as_str());
                slot.last_value = Some(value);
                changed
            }
            Err(err) => {
                tracing::debug!(command, error = %err, "status command failed; keeping last value");
                false
            }
        }
    }

    fn resolve(&self) -> Vec<ResolvedSegment> {
        self.segments
            .iter()
            .map(|seg| match seg {
                Segment::Literal(text) => ResolvedSegment {
                    kind: SegmentKind::Literal,
                    text: text.clone(),
                },
                Segment::Clock(fmt) => ResolvedSegment {
                    kind: SegmentKind::Content,
                    text: self.clock_texts.get(fmt).cloned().unwrap_or_default(),
                },
                Segment::Command(cmd) => ResolvedSegment {
                    kind: SegmentKind::Content,
                    text: self
                        .commands
                        .get(cmd)
                        .and_then(|slot| slot.last_value.clone())
                        .unwrap_or_default(),
                },
            })
            .collect()
    }

    /// Compose the fitted strip line for the given available width. Pure: reads
    /// only cached values (no clock sampling, no spawning) so it is safe to call
    /// from `compute_view`/`render` (KTD6).
    pub(crate) fn render_line(&self, available_width: usize) -> String {
        if !self.is_enabled() {
            return String::new();
        }
        let budget = self.budget.min(available_width);
        fit_segments(&self.resolve(), budget)
    }
}

/// Spawn a `#(command)` on a background OS thread and deliver its captured
/// stdout back through the app event channel (the capturing spawn primitive
/// required by U5 — NOT the fire-and-forget, null-stdout keybind path). Runs off
/// the render thread so a slow command never stalls render (R8).
pub(crate) fn spawn_status_command(
    command: String,
    event_tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
) {
    std::thread::spawn(move || {
        let result = match crate::platform::detached_custom_command_process(&command).output() {
            Ok(output) if output.status.success() => {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            Ok(output) => Err(format!("exit status {:?}", output.status.code())),
            Err(err) => Err(err.to_string()),
        };
        let _ = event_tx
            .blocking_send(crate::events::AppEvent::StatusCommandFinished { command, result });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_time() -> ClockTime {
        ClockTime {
            hour: 9,
            minute: 4,
            second: 7,
            day: 3,
            month: 7,
            year: 2026,
        }
    }

    fn content(text: &str) -> ResolvedSegment {
        ResolvedSegment {
            kind: SegmentKind::Content,
            text: text.to_string(),
        }
    }

    fn sep(text: &str) -> ResolvedSegment {
        ResolvedSegment {
            kind: SegmentKind::Literal,
            text: text.to_string(),
        }
    }

    // --- U2: parser -------------------------------------------------------

    #[test]
    fn parses_literal_only() {
        assert_eq!(
            parse_status_right("hello"),
            vec![Segment::Literal("hello".into())]
        );
    }

    #[test]
    fn parses_empty_string_to_empty_vec() {
        assert!(parse_status_right("").is_empty());
    }

    #[test]
    fn parses_clock_and_splits_surrounding_literals() {
        assert_eq!(
            parse_status_right("at %H:%M now"),
            vec![
                Segment::Literal("at ".into()),
                Segment::Clock("%H:%M".into()),
                Segment::Literal(" now".into()),
            ]
        );
    }

    #[test]
    fn parses_single_command() {
        assert_eq!(
            parse_status_right("#(cpu.sh)"),
            vec![Segment::Command("cpu.sh".into())]
        );
    }

    #[test]
    fn parses_mixed_segments_with_literal_separators() {
        assert_eq!(
            parse_status_right("#(gitmux) │ CPU #(cpu.sh)% │ %H:%M"),
            vec![
                Segment::Command("gitmux".into()),
                Segment::Literal(" │ CPU ".into()),
                Segment::Command("cpu.sh".into()),
                Segment::Literal("% │ ".into()),
                Segment::Clock("%H:%M".into()),
            ]
        );
    }

    #[test]
    fn malformed_tokens_degrade_to_literal_without_panic() {
        assert_eq!(
            parse_status_right("#(unclosed"),
            vec![Segment::Literal("#(unclosed".into())]
        );
        // Unknown specifier is kept literally.
        assert_eq!(
            parse_status_right("%Q"),
            vec![Segment::Literal("%Q".into())]
        );
    }

    // --- U3: clock + resolve + sanitize ----------------------------------

    #[test]
    fn clock_tokens_format_expected_strings() {
        let t = fixed_time();
        assert_eq!(format_clock("%H", &t), "09");
        assert_eq!(format_clock("%M", &t), "04");
        assert_eq!(format_clock("%S", &t), "07");
        assert_eq!(format_clock("%d", &t), "03");
        assert_eq!(format_clock("%b", &t), "Jul");
        assert_eq!(format_clock("%Y", &t), "2026");
        assert_eq!(format_clock("%H:%M:%S", &t), "09:04:07");
        assert_eq!(format_clock("%%", &t), "%");
    }

    #[test]
    fn stray_unknown_token_renders_literally_in_formatter() {
        let t = fixed_time();
        assert_eq!(format_clock("%Q", &t), "%Q");
    }

    #[test]
    fn command_segment_reads_cache_missing_key_is_empty() {
        let mut strip = StatusStripState::from_config(&crate::config::StatusConfig {
            status_right: "#(cpu.sh)".into(),
            status_right_length: 20,
            status_interval: 5,
        });
        // Before any run, the cache is empty and the segment renders empty.
        assert_eq!(strip.render_line(20), "");
        assert!(strip.apply_command_result("cpu.sh", Ok("42%".into())));
        assert_eq!(strip.render_line(20), "42%");
    }

    #[test]
    fn sanitize_strips_control_and_escape_bytes_and_caps_length() {
        let dirty = "\x1b[31mbranch\x07\nsecond line";
        let clean = sanitize_command_output(dirty);
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains('\u{7}'));
        assert!(!clean.contains("second line")); // only first line kept
        assert!(clean.contains("branch"));

        let long = "x".repeat(MAX_SEGMENT_CHARS + 50);
        assert_eq!(
            sanitize_command_output(&long).chars().count(),
            MAX_SEGMENT_CHARS
        );
    }

    #[test]
    fn command_error_retains_last_good_value() {
        let mut strip = StatusStripState::from_config(&crate::config::StatusConfig {
            status_right: "#(cpu.sh)".into(),
            status_right_length: 20,
            status_interval: 5,
        });
        assert!(strip.apply_command_result("cpu.sh", Ok("42%".into())));
        // An error keeps the previous value.
        assert!(!strip.apply_command_result("cpu.sh", Err("boom".into())));
        assert_eq!(strip.render_line(20), "42%");
    }

    #[test]
    fn command_that_never_succeeded_and_errors_is_blank() {
        let mut strip = StatusStripState::from_config(&crate::config::StatusConfig {
            status_right: "#(cpu.sh)".into(),
            status_right_length: 20,
            status_interval: 5,
        });
        assert!(!strip.apply_command_result("cpu.sh", Err("boom".into())));
        assert_eq!(strip.render_line(20), "");
    }

    #[test]
    fn mixed_segment_list_resolves_in_order() {
        let mut strip = StatusStripState::from_config(&crate::config::StatusConfig {
            status_right: "#(gitmux) │ %H:%M".into(),
            status_right_length: 40,
            status_interval: 5,
        });
        strip.apply_command_result("gitmux", Ok("main".into()));
        strip.refresh_clock(&fixed_time());
        assert_eq!(strip.render_line(40), "main │ 09:04");
    }

    // --- U4: fit / drop / truncate ---------------------------------------

    #[test]
    fn fit_keeps_all_when_within_budget() {
        let segs = vec![content("main"), sep(" │ "), content("09:04")];
        assert_eq!(fit_segments(&segs, 40), "main │ 09:04");
    }

    #[test]
    fn fit_drops_leftmost_segment_and_adjacent_separator() {
        let segs = vec![content("main"), sep(" │ "), content("09:04")];
        // Budget fits only "09:04": drop "main" AND the dangling " │ ".
        assert_eq!(fit_segments(&segs, 6), "09:04");
    }

    #[test]
    fn fit_drops_until_only_rightmost_survives() {
        let segs = vec![
            content("aaaa"),
            sep(" │ "),
            content("bbbb"),
            sep(" │ "),
            content("cc"),
        ];
        assert_eq!(fit_segments(&segs, 3), "cc");
    }

    #[test]
    fn fit_truncates_lone_oversize_segment_with_ellipsis() {
        let segs = vec![content("2026-07-09")];
        let out = fit_segments(&segs, 5);
        assert!(out.ends_with('…'), "out: {out:?}");
        assert!(display_width(&out) <= 5, "out width: {out:?}");
    }

    #[test]
    fn fit_measures_display_width_for_wide_glyphs() {
        // Each CJK glyph is 2 columns; four glyphs = 8 columns.
        let segs = vec![content("提交反馈")];
        assert_eq!(display_width("提交反馈"), 8);
        let out = fit_segments(&segs, 5);
        // Truncated by display columns (with the ellipsis) rather than chars.
        assert!(
            display_width(&out) <= 5,
            "out: {out:?} width {}",
            display_width(&out)
        );
        assert!(out.ends_with('…'));
    }

    #[test]
    fn render_line_empty_when_disabled() {
        let strip = StatusStripState::from_config(&crate::config::StatusConfig {
            status_right: String::new(),
            status_right_length: 20,
            status_interval: 5,
        });
        assert_eq!(strip.render_line(20), "");
    }

    // --- U5-adjacent: scheduling state (pure) ----------------------------

    #[test]
    fn clock_period_tracks_finest_field() {
        let seconds = StatusStripState::from_config(&crate::config::StatusConfig {
            status_right: "%H:%M:%S".into(),
            status_right_length: 20,
            status_interval: 30,
        });
        assert_eq!(seconds.clock_period(), Some(Duration::from_secs(1)));

        let minutes = StatusStripState::from_config(&crate::config::StatusConfig {
            status_right: "%H:%M".into(),
            status_right_length: 20,
            status_interval: 30,
        });
        assert_eq!(minutes.clock_period(), Some(Duration::from_secs(60)));

        let none = StatusStripState::from_config(&crate::config::StatusConfig {
            status_right: "#(cpu.sh)".into(),
            status_right_length: 20,
            status_interval: 30,
        });
        assert_eq!(none.clock_period(), None);
    }

    #[test]
    fn due_and_in_flight_scheduling() {
        let now = Instant::now();
        let mut strip = StatusStripState::from_config(&crate::config::StatusConfig {
            status_right: "#(cpu.sh)".into(),
            status_right_length: 20,
            status_interval: 5,
        });
        // Never run → due immediately (warm-on-arm).
        assert_eq!(strip.due_commands(now), vec!["cpu.sh".to_string()]);

        strip.mark_command_started("cpu.sh", now);
        // In flight → not re-spawned.
        assert!(strip.due_commands(now).is_empty());
        // Even past the interval, an in-flight command is skipped.
        assert!(strip.due_commands(now + Duration::from_secs(10)).is_empty());

        // Completion clears in-flight; not yet due again within the interval.
        strip.apply_command_result("cpu.sh", Ok("42%".into()));
        assert!(strip.due_commands(now + Duration::from_secs(1)).is_empty());
        // Due again once the interval elapses.
        assert_eq!(
            strip.due_commands(now + Duration::from_secs(6)),
            vec!["cpu.sh".to_string()]
        );
    }

    #[test]
    fn interval_floor_applied_via_config_accessor() {
        let strip = StatusStripState::from_config(&crate::config::StatusConfig {
            status_right: "#(cpu.sh)".into(),
            status_right_length: 20,
            status_interval: 0,
        });
        // Floored to MIN_STATUS_INTERVAL_SECONDS (1s), so a command run at t is
        // not due at t but is due 1s later.
        let now = Instant::now();
        strip.render_line(20); // no-op, keeps `strip` used before mutation
        let mut strip = strip;
        strip.mark_command_started("cpu.sh", now);
        strip.apply_command_result("cpu.sh", Ok("x".into()));
        assert!(strip.due_commands(now).is_empty());
        assert_eq!(
            strip.due_commands(now + Duration::from_secs(1)),
            vec!["cpu.sh".to_string()]
        );
    }
}

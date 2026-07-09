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

use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::state::Palette;
use crate::config::parse_color_opt;

/// Upper bound on characters kept from a `#(command)` line before display-width
/// budgeting. Mirrors the length cap in `normalize_custom_status`.
const MAX_SEGMENT_CHARS: usize = 256;

/// Ellipsis appended when a lone oversize segment is truncated (KTD4).
const ELLIPSIS: char = '…';

/// A tmux-style `#[…]` style directive: the fg/bg/modifiers it sets for the
/// segments that follow it, plus a `reset` flag for `#[default]`/`#[none]`.
/// Zero display width (KTD4). Colors come only from the trusted format string
/// (KTD1); `#(command)` output stays sanitized plain text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StyleSpec {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    /// A theme-token fg name (e.g. `accent`, `mauve`) deferred to render time
    /// so it tracks the active `Palette`. Set only when the value is not a
    /// hex/rgb/ANSI color (those resolve to `fg` at parse time).
    pub fg_token: Option<String>,
    /// A theme-token bg name, deferred to render time (mirrors `fg_token`).
    pub bg_token: Option<String>,
    pub add_modifier: Modifier,
    /// `#[default]`/`#[none]`: reset the current style back to the strip base.
    pub reset: bool,
}

impl StyleSpec {
    /// Fold this directive onto the current style (KTD3, stateful): `reset`
    /// snaps back to `base`, then fg/bg/modifiers layer on top cumulatively.
    /// A deferred theme token resolves against `palette` here (KTD1/KTD2); the
    /// width-only path passes `None`, so tokens contribute no color there.
    fn apply(&self, base: Style, current: Style, palette: Option<&Palette>) -> Style {
        let mut next = if self.reset { base } else { current };
        if let Some(fg) = self.resolved_fg(palette) {
            next = next.fg(fg);
        }
        if let Some(bg) = self.resolved_bg(palette) {
            next = next.bg(bg);
        }
        next.add_modifier(self.add_modifier)
    }

    fn resolved_fg(&self, palette: Option<&Palette>) -> Option<Color> {
        self.fg.or_else(|| {
            self.fg_token
                .as_deref()
                .and_then(|token| palette.and_then(|p| theme_token(token, p)))
        })
    }

    fn resolved_bg(&self, palette: Option<&Palette>) -> Option<Color> {
        self.bg.or_else(|| {
            self.bg_token
                .as_deref()
                .and_then(|token| palette.and_then(|p| theme_token(token, p)))
        })
    }
}

/// Resolve a herdr theme-token name to its color in the active [`Palette`].
/// Maps only the theme-only tokens that have no ANSI equivalent; the
/// overlapping names (`green`/`yellow`/`red`/`blue`/`cyan`) intentionally
/// return `None` so they keep resolving to ANSI via `parse_color_opt` (KTD2).
/// Unknown names return `None` and are ignored by the caller (graceful degrade).
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

/// Split a `#[…]` body on attribute commas, but not on commas inside a
/// parenthesized color value such as `rgb(1,2,3)`.
fn split_style_attrs(inner: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut buf = String::new();
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => {
                depth += 1;
                buf.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                buf.push(c);
            }
            ',' if depth == 0 => tokens.push(std::mem::take(&mut buf)),
            _ => buf.push(c),
        }
    }
    tokens.push(buf);
    tokens
}

/// Parse the inside of a `#[…]` directive into a [`StyleSpec`]. Attributes are
/// comma-separated (commas inside `rgb(…)` are preserved); unknown keys and
/// unparseable colors are ignored (KTD4, graceful degrade) rather than erroring.
fn parse_style_spec(inner: &str) -> StyleSpec {
    let mut spec = StyleSpec::default();
    for token in split_style_attrs(inner) {
        let token = token.trim().to_ascii_lowercase();
        if token.is_empty() {
            continue;
        }
        match token.as_str() {
            "default" | "none" => spec.reset = true,
            "bold" => spec.add_modifier |= Modifier::BOLD,
            "dim" => spec.add_modifier |= Modifier::DIM,
            "italic" => spec.add_modifier |= Modifier::ITALIC,
            "underline" => spec.add_modifier |= Modifier::UNDERLINED,
            "reverse" => spec.add_modifier |= Modifier::REVERSED,
            _ => {
                if let Some(value) = token.strip_prefix("fg=") {
                    // hex/rgb/ANSI resolve now; other names defer to render time
                    // as a theme token (KTD2 — the two sets are disjoint).
                    match parse_color_opt(value) {
                        Some(color) => spec.fg = Some(color),
                        None => spec.fg_token = Some(value.to_string()),
                    }
                } else if let Some(value) = token.strip_prefix("bg=") {
                    match parse_color_opt(value) {
                        Some(color) => spec.bg = Some(color),
                        None => spec.bg_token = Some(value.to_string()),
                    }
                }
                // Unknown attribute: ignored.
            }
        }
    }
    spec
}

/// One parsed piece of a `status_right` format string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Literal text (also acts as a droppable separator between content).
    Literal(String),
    /// A `%`-strftime subset run, e.g. `%H:%M`.
    Clock(String),
    /// A `#(command)` whose stdout becomes the segment text.
    Command(String),
    /// A `#{slot:NAME}` push slot whose text is the latest value pushed for
    /// `NAME` over the API socket (the push lane, KTD1). Empty when unset or
    /// expired, so it drops its adjacent separator like an empty command (R3).
    Slot(String),
    /// A `#[…]` style directive; contributes no text and no display width.
    Style(StyleSpec),
}

/// Whether a resolved segment carries content or is a droppable separator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SegmentKind {
    Literal,
    Content,
}

/// A segment resolved to its current display string plus the ratatui [`Style`]
/// captured at its original position in the directive stream (KTD3). Capturing
/// per-segment makes truncation style-safe: dropping leftmost segments never
/// leaks a style onto a survivor (R4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedSegment {
    pub kind: SegmentKind,
    pub text: String,
    pub style: Style,
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

        if c == '#' && chars.get(i + 1) == Some(&'[') {
            if let Some(close) = (i + 2..chars.len()).find(|&j| chars[j] == ']') {
                flush(&mut literal, &mut segments);
                let inner: String = chars[i + 2..close].iter().collect();
                segments.push(Segment::Style(parse_style_spec(&inner)));
                i = close + 1;
                continue;
            }
            // Unclosed `#[` degrades to literal text (mirrors `#(` handling).
            literal.push('#');
            i += 1;
            continue;
        }

        if c == '#' && chars.get(i + 1) == Some(&'{') {
            if let Some(close) = (i + 2..chars.len()).find(|&j| chars[j] == '}') {
                let inner: String = chars[i + 2..close].iter().collect();
                // Only `#{slot:NAME}` is recognized; any other `#{…}` body
                // degrades to literal so the token space stays reserved (KTD1).
                if let Some(name) = inner.strip_prefix("slot:") {
                    flush(&mut literal, &mut segments);
                    segments.push(Segment::Slot(name.to_string()));
                    i = close + 1;
                    continue;
                }
            }
            // Unclosed or unrecognized `#{` degrades to literal text.
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

/// Concatenate resolved segment text (ignoring style) — the display string and
/// width basis for the strip.
pub(crate) fn joined(segs: &[ResolvedSegment]) -> String {
    segs.iter().map(|s| s.text.as_str()).collect()
}

/// Fit resolved segments into `budget` display columns (KTD4): drop whole
/// segments leftmost-first (also dropping the now-leading separator) until they
/// fit; if a single surviving segment still overflows, truncate its text with
/// `…`. Each survivor keeps the [`Style`] it was resolved with (R4).
pub(crate) fn fit_segments(resolved: &[ResolvedSegment], budget: usize) -> Vec<ResolvedSegment> {
    if display_width(&joined(resolved)) <= budget {
        return resolved.to_vec();
    }

    let mut remaining: Vec<ResolvedSegment> = resolved.to_vec();
    while remaining.len() > 1 && display_width(&joined(&remaining)) > budget {
        remaining.remove(0);
        // Drop a now-leading separator so no dangling " │ " remains.
        if remaining.len() > 1 && remaining[0].kind == SegmentKind::Literal {
            remaining.remove(0);
        }
    }

    if display_width(&joined(&remaining)) > budget {
        // Lone oversize segment: the sole allowed mid-character truncation.
        // Truncate its text but preserve its captured style/kind.
        if let Some(first) = remaining.first_mut() {
            first.text = truncate_to_columns(&first.text, budget);
        }
    }
    remaining
}

/// Drop segments that resolved to empty content, along with one adjacent
/// separator literal, so an empty `#(command)`/clock leaves no dangling ` │ `
/// (R5). Prefers dropping the following separator; falls back to the preceding
/// one for a trailing empty segment.
fn drop_empty_segments(segs: Vec<ResolvedSegment>) -> Vec<ResolvedSegment> {
    let mut out: Vec<ResolvedSegment> = Vec::with_capacity(segs.len());
    let mut i = 0;
    while i < segs.len() {
        let seg = &segs[i];
        if seg.kind == SegmentKind::Content && seg.text.is_empty() {
            if segs
                .get(i + 1)
                .is_some_and(|s| s.kind == SegmentKind::Literal)
            {
                i += 2; // Drop the empty segment and its following separator.
            } else {
                if out.last().is_some_and(|s| s.kind == SegmentKind::Literal) {
                    out.pop(); // Trailing empty: drop the preceding separator.
                }
                i += 1;
            }
            continue;
        }
        out.push(seg.clone());
        i += 1;
    }
    out
}

/// Per-`#(command)` scheduling and last-known value. Pure data on `AppState`;
/// nothing exposes it over the JSON API (KTD2/OQ3 — render-scheduling state).
#[derive(Debug, Clone, Default)]
pub(crate) struct CommandSlot {
    pub last_value: Option<String>,
    pub last_run: Option<Instant>,
    pub in_flight: bool,
}

/// One pushed slot value: the sanitized text, when it was reported, and an
/// optional TTL evaluated lazily at read time (KTD4).
#[derive(Debug, Clone)]
struct Slot {
    text: String,
    reported_at: Instant,
    ttl: Option<Duration>,
}

impl Slot {
    fn is_expired(&self, now: Instant) -> bool {
        self.ttl.is_some_and(|ttl| {
            let deadline = self
                .reported_at
                .checked_add(ttl)
                .unwrap_or(self.reported_at);
            now >= deadline
        })
    }
}

/// Host-scoped store for the push lane: source-keyed status values written over
/// the API socket and rendered wherever a matching `#{slot:NAME}` token appears
/// (KTD2). Modeled on `AgentMetadata`'s seq/ttl rules but host-scoped (keyed by
/// `source` only) with no pane-id key and no agent-lifecycle guard — the strip
/// is chrome, not agent state. Lives on `AppState`, separate from
/// `StatusStripState`, so it survives a config reload.
#[derive(Debug, Clone, Default)]
pub(crate) struct SlotStore {
    slots: HashMap<String, Slot>,
    /// Last accepted `seq` per source for last-writer-wins (mirrors
    /// `AgentMetadata`'s `metadata_report_sequences`).
    seqs: HashMap<String, u64>,
}

impl SlotStore {
    /// Accept a report only when its `seq` advances the last seen one for the
    /// source. A `None` seq is always accepted (unsequenced writers).
    fn accept_seq(&mut self, source: &str, seq: Option<u64>) -> bool {
        let Some(seq) = seq else {
            return true;
        };
        if self.seqs.get(source).is_some_and(|last| seq <= *last) {
            return false;
        }
        self.seqs.insert(source.to_string(), seq);
        true
    }

    /// Set the value for `source`. Returns whether the currently displayed
    /// value changed (so the caller can decide whether to repaint). An older or
    /// equal `seq` is ignored and returns `false`.
    pub(crate) fn set(
        &mut self,
        source: String,
        text: String,
        seq: Option<u64>,
        ttl: Option<Duration>,
        now: Instant,
    ) -> bool {
        if !self.accept_seq(&source, seq) {
            return false;
        }
        let previous = self.get(&source, now).map(str::to_string);
        self.slots.insert(
            source,
            Slot {
                text: text.clone(),
                reported_at: now,
                ttl,
            },
        );
        previous.as_deref() != Some(text.as_str())
    }

    /// Remove the value for `source`. Returns whether a visible value was
    /// dropped (so the caller can decide whether to repaint). Also clears the
    /// seq watermark so a fresh writer starts clean.
    pub(crate) fn clear(&mut self, source: &str, now: Instant) -> bool {
        let was_visible = self.get(source, now).is_some();
        self.slots.remove(source);
        self.seqs.remove(source);
        was_visible
    }

    /// Current value for `source`, or `None` when unset or expired (lazy TTL).
    fn get(&self, source: &str, now: Instant) -> Option<&str> {
        self.slots
            .get(source)
            .filter(|slot| !slot.is_expired(now))
            .map(|slot| slot.text.as_str())
    }

    /// Resolve a slot to its display text (empty when unset or expired).
    fn resolve(&self, name: &str, now: Instant) -> String {
        self.get(name, now).unwrap_or_default().to_string()
    }

    /// Current value for a source at "now", for handler tests.
    #[cfg(test)]
    pub(crate) fn get_for_test(&self, source: &str) -> Option<String> {
        self.get(source, Instant::now()).map(str::to_string)
    }
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

    /// Resolve segments to their display text, walking the `#[…]` directive
    /// stream left-to-right and capturing the effective [`Style`] at each
    /// content/literal position (KTD3). Style directives contribute no output.
    fn resolve(
        &self,
        base: Style,
        palette: Option<&Palette>,
        slots: &SlotStore,
        now: Instant,
    ) -> Vec<ResolvedSegment> {
        let mut current = base;
        let mut out = Vec::with_capacity(self.segments.len());
        for seg in &self.segments {
            match seg {
                Segment::Style(spec) => current = spec.apply(base, current, palette),
                Segment::Literal(text) => out.push(ResolvedSegment {
                    kind: SegmentKind::Literal,
                    text: text.clone(),
                    style: current,
                }),
                Segment::Clock(fmt) => out.push(ResolvedSegment {
                    kind: SegmentKind::Content,
                    text: self.clock_texts.get(fmt).cloned().unwrap_or_default(),
                    style: current,
                }),
                Segment::Command(cmd) => out.push(ResolvedSegment {
                    kind: SegmentKind::Content,
                    text: self
                        .commands
                        .get(cmd)
                        .and_then(|slot| slot.last_value.clone())
                        .unwrap_or_default(),
                    style: current,
                }),
                Segment::Slot(name) => out.push(ResolvedSegment {
                    kind: SegmentKind::Content,
                    text: slots.resolve(name, now),
                    style: current,
                }),
            }
        }
        out
    }

    /// Compose the fitted, styled strip segments for the given available width,
    /// relative to `base` (the strip's default style; `#[default]` resets to
    /// it). Empty-resolved content drops its adjacent separator (R5). Pure:
    /// reads only cached values (no clock sampling, no spawning) so it is safe
    /// to call from `compute_view`/`render` (KTD6).
    pub(crate) fn render_segments(
        &self,
        available_width: usize,
        base: Style,
        palette: Option<&Palette>,
        slots: &SlotStore,
    ) -> Vec<ResolvedSegment> {
        if !self.is_enabled() {
            return Vec::new();
        }
        // Sample the monotonic clock once so slot TTLs expire lazily at compose
        // time (KTD4). This reads the clock but performs no mutation/spawn, so
        // it stays safe to call from `compute_view`/`render` (KTD6).
        let now = Instant::now();
        let budget = self.budget.min(available_width);
        let resolved = drop_empty_segments(self.resolve(base, palette, slots, now));
        fit_segments(&resolved, budget)
    }

    /// Compose the fitted strip line as plain text (style-agnostic) including
    /// pushed slot values, used for width budgeting. Delegates to
    /// [`Self::render_segments`]; colors never affect segment text/width, so no
    /// palette is needed here.
    pub(crate) fn render_line_with_slots(
        &self,
        available_width: usize,
        slots: &SlotStore,
    ) -> String {
        joined(&self.render_segments(available_width, Style::default(), None, slots))
    }

    /// Compose the fitted strip line as plain text with no pushed slots. Used by
    /// tests that exercise commands/clock/styling without the push lane.
    #[cfg(test)]
    pub(crate) fn render_line(&self, available_width: usize) -> String {
        self.render_line_with_slots(available_width, &SlotStore::default())
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
            style: Style::default(),
        }
    }

    fn sep(text: &str) -> ResolvedSegment {
        ResolvedSegment {
            kind: SegmentKind::Literal,
            text: text.to_string(),
            style: Style::default(),
        }
    }

    fn styled_content(text: &str, style: Style) -> ResolvedSegment {
        ResolvedSegment {
            kind: SegmentKind::Content,
            text: text.to_string(),
            style,
        }
    }

    fn build(status_right: &str) -> StatusStripState {
        StatusStripState::from_config(&crate::config::StatusConfig {
            status_right: status_right.into(),
            status_right_length: 40,
            status_interval: 5,
        })
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
        assert_eq!(joined(&fit_segments(&segs, 40)), "main │ 09:04");
    }

    #[test]
    fn fit_drops_leftmost_segment_and_adjacent_separator() {
        let segs = vec![content("main"), sep(" │ "), content("09:04")];
        // Budget fits only "09:04": drop "main" AND the dangling " │ ".
        assert_eq!(joined(&fit_segments(&segs, 6)), "09:04");
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
        assert_eq!(joined(&fit_segments(&segs, 3)), "cc");
    }

    #[test]
    fn fit_truncates_lone_oversize_segment_with_ellipsis() {
        let segs = vec![content("2026-07-09")];
        let out = joined(&fit_segments(&segs, 5));
        assert!(out.ends_with('…'), "out: {out:?}");
        assert!(display_width(&out) <= 5, "out width: {out:?}");
    }

    #[test]
    fn fit_measures_display_width_for_wide_glyphs() {
        // Each CJK glyph is 2 columns; four glyphs = 8 columns.
        let segs = vec![content("提交反馈")];
        assert_eq!(display_width("提交反馈"), 8);
        let out = joined(&fit_segments(&segs, 5));
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

    // --- U1: `#[…]` directive parsing ------------------------------------

    #[test]
    fn parses_single_fg_directive() {
        assert_eq!(
            parse_status_right("#[fg=green]"),
            vec![Segment::Style(StyleSpec {
                fg: Some(Color::Green),
                ..Default::default()
            })]
        );
    }

    #[test]
    fn parses_combined_fg_bg_bold_directive() {
        let spec = parse_style_spec("fg=black,bg=#1e1e2e,bold");
        assert_eq!(spec.fg, Some(Color::Black));
        assert_eq!(spec.bg, Some(Color::Rgb(0x1e, 0x1e, 0x2e)));
        assert!(spec.add_modifier.contains(Modifier::BOLD));
        assert!(!spec.reset);
    }

    #[test]
    fn default_and_none_directives_are_reset() {
        assert!(parse_style_spec("default").reset);
        assert!(parse_style_spec("none").reset);
    }

    #[test]
    fn unknown_attr_and_bad_color_are_ignored() {
        let spec = parse_style_spec("wat=1,fg=notacolor,bold");
        assert_eq!(spec.fg, None);
        assert_eq!(spec.bg, None);
        assert!(spec.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn unclosed_style_bracket_degrades_to_literal() {
        assert_eq!(
            parse_status_right("#[fg=green"),
            vec![Segment::Literal("#[fg=green".into())]
        );
    }

    #[test]
    fn style_directives_contribute_zero_resolved_segments() {
        let strip = build("#[fg=green]#[bold]");
        assert!(strip
            .resolve(
                Style::default(),
                None,
                &SlotStore::default(),
                Instant::now()
            )
            .is_empty());
        assert_eq!(strip.render_line(40), "");
    }

    // --- U2: style resolution -> ratatui `Style` --------------------------

    #[test]
    fn directive_maps_to_ratatui_style() {
        let spec = parse_style_spec("fg=green,bg=blue,underline");
        let base = Style::default();
        let out = spec.apply(base, base, None);
        assert_eq!(out.fg, Some(Color::Green));
        assert_eq!(out.bg, Some(Color::Blue));
        assert!(out.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn segment_captures_current_style_and_default_resets_to_base() {
        let base = Style::default().fg(Color::White);
        let strip = build("#[fg=green]hi#[default]bye");
        let segs = strip.resolve(base, None, &SlotStore::default(), Instant::now());
        assert_eq!(segs[0].text, "hi");
        assert_eq!(segs[0].style.fg, Some(Color::Green));
        assert_eq!(segs[1].text, "bye");
        assert_eq!(segs[1].style.fg, Some(Color::White));
    }

    #[test]
    fn truncation_preserves_survivor_style() {
        let a = styled_content("aaaa", Style::default().fg(Color::Red));
        let separator = sep(" │ ");
        let b = styled_content("bbbb", Style::default().fg(Color::Blue));
        let full = fit_segments(&[a.clone(), separator.clone(), b.clone()], 100);
        let dropped = fit_segments(&[a, separator, b], 4);
        // The rightmost survivor keeps its blue fg whether or not the leftmost
        // was dropped during truncation.
        assert_eq!(full.last().unwrap().style.fg, Some(Color::Blue));
        assert_eq!(dropped.last().unwrap().text, "bbbb");
        assert_eq!(dropped.last().unwrap().style.fg, Some(Color::Blue));
    }

    #[test]
    fn named_hex_and_rgb_colors_all_resolve() {
        assert_eq!(parse_style_spec("fg=green").fg, Some(Color::Green));
        assert_eq!(
            parse_style_spec("fg=#ff0000").fg,
            Some(Color::Rgb(255, 0, 0))
        );
        assert_eq!(
            parse_style_spec("fg=rgb(1,2,3)").fg,
            Some(Color::Rgb(1, 2, 3))
        );
    }

    // --- U3: styled draw + empty-separator drop ---------------------------

    #[test]
    fn empty_leading_command_drops_following_separator() {
        let mut strip = build("#(git) │ %H:%M");
        strip.refresh_clock(&fixed_time());
        // git never produced output → empty → its trailing separator drops.
        assert_eq!(strip.render_line(40), "09:04");
    }

    #[test]
    fn empty_trailing_command_drops_preceding_separator() {
        let mut strip = build("%H:%M │ #(git)");
        strip.refresh_clock(&fixed_time());
        assert_eq!(strip.render_line(40), "09:04");
    }

    #[test]
    fn nonempty_neighbor_keeps_its_separator() {
        let mut strip = build("#(git) │ %H:%M");
        strip.apply_command_result("git", Ok("main".into()));
        strip.refresh_clock(&fixed_time());
        assert_eq!(strip.render_line(40), "main │ 09:04");
    }

    #[test]
    fn styles_do_not_change_display_width() {
        let styled = build("#[fg=green,bg=blue]abc#[default] │ #[bold]xy");
        let plain = build("abc │ xy");
        assert_eq!(styled.render_line(40), plain.render_line(40));
        assert_eq!(
            display_width(&styled.render_line(40)),
            display_width(&plain.render_line(40))
        );
    }

    #[test]
    fn powerline_example_composes_without_width_drift() {
        let styled = build("#[fg=black,bg=green] A #[fg=green,bg=blue] B ");
        let plain = build(" A  B ");
        assert_eq!(styled.render_line(40), plain.render_line(40));
    }

    #[test]
    fn styled_content_segment_carries_resolved_style() {
        let base = Style::default().fg(Color::White).bg(Color::Black);
        let strip = build("#[fg=red,bg=green]HI");
        let segs = strip.render_segments(40, base, None, &SlotStore::default());
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "HI");
        assert_eq!(segs[0].style.fg, Some(Color::Red));
        assert_eq!(segs[0].style.bg, Some(Color::Green));
    }

    // --- theme-token resolution -----------------------------------------

    /// Resolve `#[fg=<value>]X` against `palette` and return the survivor style.
    fn fg_style(value: &str, palette: &Palette) -> Style {
        let strip = build(&format!("#[fg={value}]X"));
        let segs =
            strip.render_segments(40, Style::default(), Some(palette), &SlotStore::default());
        segs[0].style
    }

    #[test]
    fn every_theme_token_resolves_to_its_palette_field() {
        let p = Palette::catppuccin();
        for (name, expected) in [
            ("accent", p.accent),
            ("panel_bg", p.panel_bg),
            ("surface0", p.surface0),
            ("surface1", p.surface1),
            ("surface_dim", p.surface_dim),
            ("overlay0", p.overlay0),
            ("overlay1", p.overlay1),
            ("text", p.text),
            ("subtext0", p.subtext0),
            ("mauve", p.mauve),
            ("teal", p.teal),
            ("peach", p.peach),
        ] {
            assert_eq!(fg_style(name, &p).fg, Some(expected), "token: {name}");
        }
    }

    #[test]
    fn non_ansi_tokens_now_render_instead_of_being_ignored() {
        let p = Palette::catppuccin();
        // These have no ANSI equivalent, so `parse_color_opt` returns None and
        // they were previously dropped; the token path now resolves them.
        for name in ["mauve", "teal", "peach"] {
            assert!(fg_style(name, &p).fg.is_some(), "token: {name}");
        }
        assert_eq!(fg_style("mauve", &p).fg, Some(p.mauve));
    }

    #[test]
    fn overlapping_names_still_resolve_to_ansi_not_palette_rgb() {
        let p = Palette::catppuccin();
        assert_eq!(fg_style("blue", &p).fg, Some(Color::Blue));
        assert_eq!(fg_style("green", &p).fg, Some(Color::Green));
        assert_eq!(fg_style("yellow", &p).fg, Some(Color::Yellow));
        assert_eq!(fg_style("red", &p).fg, Some(Color::Red));
        assert_eq!(fg_style("cyan", &p).fg, Some(Color::Cyan));
    }

    #[test]
    fn hex_rgb_and_reset_are_unchanged_by_token_path() {
        let p = Palette::catppuccin();
        assert_eq!(
            fg_style("#cba6f7", &p).fg,
            Some(Color::Rgb(0xcb, 0xa6, 0xf7))
        );
        assert_eq!(fg_style("rgb(1,2,3)", &p).fg, Some(Color::Rgb(1, 2, 3)));
        // `default` as a value resolves via `parse_color_opt` to Reset (not a token).
        assert_eq!(fg_style("default", &p).fg, Some(Color::Reset));
    }

    #[test]
    fn unknown_token_is_ignored_without_panic() {
        let p = Palette::catppuccin();
        // Unknown name defers as a token, then resolves to None → style unchanged.
        assert_eq!(fg_style("wat", &p).fg, None);
    }

    #[test]
    fn custom_override_is_reflected_in_token_resolution() {
        let mut p = Palette::catppuccin();
        p.accent = Color::Rgb(1, 2, 3);
        assert_eq!(fg_style("accent", &p).fg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn theme_token_resolves_on_both_fg_and_bg() {
        let p = Palette::catppuccin();
        let strip = build("#[fg=text,bg=surface0]X");
        let segs = strip.render_segments(40, Style::default(), Some(&p), &SlotStore::default());
        assert_eq!(segs[0].style.fg, Some(p.text));
        assert_eq!(segs[0].style.bg, Some(p.surface0));
    }

    // --- U1: host-scoped push store --------------------------------------

    #[test]
    fn slot_store_set_and_get_by_source() {
        let mut store = SlotStore::default();
        let now = Instant::now();
        assert!(store.set("git".into(), "main".into(), None, None, now));
        assert_eq!(store.get("git", now), Some("main"));
        // A missing source has no value.
        assert_eq!(store.get("cwd", now), None);
    }

    #[test]
    fn slot_store_newer_seq_overwrites_older_and_equal_ignored() {
        let mut store = SlotStore::default();
        let now = Instant::now();
        assert!(store.set("git".into(), "main".into(), Some(5), None, now));
        // Equal seq is ignored (returns false, value unchanged).
        assert!(!store.set("git".into(), "feature".into(), Some(5), None, now));
        assert_eq!(store.get("git", now), Some("main"));
        // Older seq is ignored.
        assert!(!store.set("git".into(), "feature".into(), Some(4), None, now));
        assert_eq!(store.get("git", now), Some("main"));
        // Newer seq wins.
        assert!(store.set("git".into(), "feature".into(), Some(6), None, now));
        assert_eq!(store.get("git", now), Some("feature"));
    }

    #[test]
    fn slot_store_ttl_expiry_hides_value_at_read_time() {
        let mut store = SlotStore::default();
        let now = Instant::now();
        assert!(store.set(
            "t".into(),
            "hi".into(),
            None,
            Some(Duration::from_millis(10)),
            now
        ));
        // Still visible before the deadline.
        assert_eq!(store.get("t", now + Duration::from_millis(5)), Some("hi"));
        // Hidden once the TTL elapses (lazy, evaluated at read).
        assert_eq!(store.get("t", now + Duration::from_millis(10)), None);
        assert_eq!(store.get("t", now + Duration::from_millis(50)), None);
    }

    #[test]
    fn slot_store_clear_removes_value() {
        let mut store = SlotStore::default();
        let now = Instant::now();
        store.set("git".into(), "main".into(), None, None, now);
        assert!(store.clear("git", now));
        assert_eq!(store.get("git", now), None);
        // Clearing an already-empty source reports no visible change.
        assert!(!store.clear("git", now));
        // A fresh writer after clear starts clean even with a low seq.
        assert!(store.set("git".into(), "dev".into(), Some(1), None, now));
        assert_eq!(store.get("git", now), Some("dev"));
    }

    #[test]
    fn slot_store_set_reports_visible_change() {
        let mut store = SlotStore::default();
        let now = Instant::now();
        // First set makes it visible.
        assert!(store.set("s".into(), "a".into(), None, None, now));
        // Same text is not a visible change.
        assert!(!store.set("s".into(), "a".into(), None, None, now));
        // Different text is a visible change.
        assert!(store.set("s".into(), "b".into(), None, None, now));
    }

    #[test]
    fn empty_store_renders_no_slot_text() {
        let strip = build("#{slot:git}");
        assert_eq!(strip.render_line_with_slots(40, &SlotStore::default()), "");
    }

    // --- U3: `#{slot:NAME}` parse + render -------------------------------

    #[test]
    fn parses_slot_token() {
        assert_eq!(
            parse_status_right("#{slot:git}"),
            vec![Segment::Slot("git".into())]
        );
    }

    #[test]
    fn unclosed_slot_token_degrades_to_literal() {
        assert_eq!(
            parse_status_right("#{slot:git"),
            vec![Segment::Literal("#{slot:git".into())]
        );
    }

    #[test]
    fn unrecognized_hash_brace_body_degrades_to_literal() {
        // A closed `#{…}` that is not `slot:` stays literal (no token created).
        assert_eq!(
            parse_status_right("#{foo}"),
            vec![Segment::Literal("#{foo}".into())]
        );
    }

    #[test]
    fn slot_renders_pushed_value() {
        let strip = build("#{slot:git}");
        let mut slots = SlotStore::default();
        slots.set("git".into(), "main".into(), None, None, Instant::now());
        assert_eq!(strip.render_line_with_slots(40, &slots), "main");
    }

    #[test]
    fn unset_slot_drops_its_separator() {
        let mut strip = build("#{slot:git} │ %H:%M");
        strip.refresh_clock(&fixed_time());
        // git slot is unset → empty → its trailing separator drops.
        assert_eq!(
            strip.render_line_with_slots(40, &SlotStore::default()),
            "09:04"
        );
    }

    #[test]
    fn set_slot_keeps_its_separator() {
        let mut strip = build("#{slot:git} │ %H:%M");
        strip.refresh_clock(&fixed_time());
        let mut slots = SlotStore::default();
        slots.set("git".into(), "main".into(), None, None, Instant::now());
        assert_eq!(strip.render_line_with_slots(40, &slots), "main │ 09:04");
    }

    #[test]
    fn slot_composes_with_command_and_clock() {
        let mut strip = build("#{slot:git} │ #(cpu.sh)% │ %H:%M");
        strip.apply_command_result("cpu.sh", Ok("42".into()));
        strip.refresh_clock(&fixed_time());
        let mut slots = SlotStore::default();
        slots.set("git".into(), "main".into(), None, None, Instant::now());
        assert_eq!(
            strip.render_line_with_slots(40, &slots),
            "main │ 42% │ 09:04"
        );
    }

    #[test]
    fn styled_pill_around_slot_keeps_style_when_untruncated() {
        let strip = build("#[fg=red,bg=green]#{slot:git}#[default] │ %H:%M");
        let mut slots = SlotStore::default();
        slots.set("git".into(), "main".into(), None, None, Instant::now());
        let base = Style::default();
        let segs = strip.render_segments(40, base, None, &slots);
        let slot_seg = segs.iter().find(|s| s.text == "main").unwrap();
        assert_eq!(slot_seg.style.fg, Some(Color::Red));
        assert_eq!(slot_seg.style.bg, Some(Color::Green));
    }

    #[test]
    fn styled_slot_keeps_style_as_sole_survivor_under_truncation() {
        // The slot sits rightmost so it survives when the budget forces the
        // clock and its separator to drop; its captured pill style is retained.
        let mut strip = build("%H:%M │ #[fg=red,bg=green]#{slot:git}");
        strip.refresh_clock(&fixed_time());
        let mut slots = SlotStore::default();
        slots.set("git".into(), "main".into(), None, None, Instant::now());
        let segs = strip.render_segments(4, Style::default(), None, &slots);
        assert_eq!(joined(&segs), "main");
        assert_eq!(segs.last().unwrap().style.fg, Some(Color::Red));
        assert_eq!(segs.last().unwrap().style.bg, Some(Color::Green));
    }
}

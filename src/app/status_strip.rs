//! tmux-style right status strip: format parsing, `#(command)` scheduling,
//! clock sampling, and the `#{slot:NAME}` push lane.
//!
//! This module owns the strip's *content*. `status_right` is one string mixing
//! runtime (`#(command)`) with presentation (`#[fg=…]`), and a command has to
//! run where the session lives, so the endpoint parses the format string and
//! resolves every segment's text. Only palette resolution and width fitting
//! stay with the client (`src/client/shell/status_strip.rs`): both depend on
//! facts the endpoint does not have, namely the client's active theme and its
//! terminal width.
//!
//! Everything here is pure except [`ClockTime::now_local`], which samples the
//! wall clock, and the command spawn. Neither is reachable from a render path;
//! both run on the endpoint's interval tick.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ratatui::style::Modifier;

use super::App;
use crate::config::StatusConfig;

/// Hard cap on how long a status `#(command)` may run before it is killed and
/// abandoned. Without it a hung command leaves the slot's `in_flight` flag
/// stuck, so the command never re-runs. Kept a constant rather than a config
/// field: 2s matches the `tab_bar_right` command default, and a fixed short
/// bound is also what lets the strip skip retaining task handles (see
/// [`App::handle_status_strip_tasks`]).
const STATUS_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

/// Distinct `#{slot:NAME}` sources the push lane keeps at once. Mirrors
/// `MAX_METADATA_TOKEN_KEYS_PER_RESOURCE`: the lane is host-scoped and writers
/// pick their own key, so it needs a ceiling to stay bounded.
const MAX_STATUS_SLOT_SOURCES: usize = 32;

/// A tmux-style `#[…]` style directive: the fg/bg/modifiers it sets for the
/// segments that follow it, plus a `reset` flag for `#[default]`/`#[none]`.
/// Contributes zero display width. Colors come only from the trusted format
/// string; `#(command)` output stays sanitized plain text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StyleSpec {
    /// Raw `#[fg=…]` value, left unparsed on purpose. The client resolves it
    /// against its own palette so a theme token (`accent`, `mauve`) tracks the
    /// theme that client is actually rendering with.
    pub fg: Option<String>,
    /// Raw `#[bg=…]` value; mirrors `fg`.
    pub bg: Option<String>,
    pub add_modifier: Modifier,
    /// `#[default]`/`#[none]`: reset the running style back to the strip base.
    pub reset: bool,
}

impl StyleSpec {
    /// Fold this directive onto the running style: `reset` clears back to the
    /// strip base (an empty spec), then fg/bg/modifiers layer on cumulatively.
    /// Folding specs rather than resolved styles keeps the whole directive
    /// stream palette-free on the endpoint.
    fn fold_onto(&self, current: &mut Self) {
        if self.reset {
            *current = Self::default();
        }
        if self.fg.is_some() {
            current.fg.clone_from(&self.fg);
        }
        if self.bg.is_some() {
            current.bg.clone_from(&self.bg);
        }
        current.add_modifier |= self.add_modifier;
    }
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
/// comma-separated (commas inside `rgb(…)` are preserved); unknown keys are
/// ignored rather than erroring, so a typo degrades gracefully.
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
                    spec.fg = Some(value.to_string());
                } else if let Some(value) = token.strip_prefix("bg=") {
                    spec.bg = Some(value.to_string());
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
    /// `NAME` over the API socket. Empty when unset or expired, so it drops its
    /// adjacent separator like an empty command.
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

/// A segment resolved to its current display text plus the folded [`StyleSpec`]
/// in force at its position in the directive stream. Folding per segment is
/// what makes client-side truncation style-safe: dropping leftmost segments can
/// never leak a style onto a survivor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedSegment {
    pub kind: SegmentKind,
    pub text: String,
    pub style: StyleSpec,
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
                // degrades to literal so the token space stays reserved.
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
    pub year: i32,
}

impl ClockTime {
    /// Sample the current local time. This is the module's single time-source
    /// touchpoint; it runs on the endpoint tick, never inside a render path.
    /// `None` when the platform cannot resolve local time, which leaves the
    /// cached clock text in place rather than blanking the strip.
    fn now_local() -> Option<Self> {
        let now = crate::platform::local_datetime()?;
        Some(Self {
            hour: now.hour(),
            minute: now.minute(),
            second: now.second(),
            day: now.day(),
            month: u8::from(now.month()),
            year: now.year(),
        })
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

/// Drop segments that resolved to empty content, along with one adjacent
/// separator literal, so an empty `#(command)`/clock/slot leaves no dangling
/// ` │ `. Prefers dropping the following separator; falls back to the preceding
/// one for a trailing empty segment.
/// Consumes `segs` and moves survivors across rather than cloning them: this
/// runs once per client-shell snapshot, so copying every segment's text would
/// ride the frame-fanout path for no reason.
fn drop_empty_segments(segs: Vec<ResolvedSegment>) -> Vec<ResolvedSegment> {
    let mut out: Vec<ResolvedSegment> = Vec::with_capacity(segs.len());
    let mut segs = segs.into_iter().peekable();
    while let Some(seg) = segs.next() {
        if seg.kind == SegmentKind::Content && seg.text.is_empty() {
            if segs.peek().is_some_and(|s| s.kind == SegmentKind::Literal) {
                segs.next(); // Drop the empty segment's following separator.
            } else if out.last().is_some_and(|s| s.kind == SegmentKind::Literal) {
                out.pop(); // Trailing empty: drop the preceding separator.
            }
            continue;
        }
        out.push(seg);
    }
    out
}

/// Per-`#(command)` scheduling and last-known value. Pure data on `AppState`.
#[derive(Debug, Clone, Default)]
pub(crate) struct CommandSlot {
    pub last_value: Option<String>,
    pub last_run: Option<Instant>,
    pub in_flight: bool,
}

/// One pushed slot value: the sanitized text, when it was reported, and an
/// optional TTL evaluated lazily at read time.
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
/// the API socket and rendered wherever a matching `#{slot:NAME}` token appears.
/// Modeled on `AgentMetadata`'s seq/ttl rules but keyed by `source` alone, with
/// no pane id and no agent-lifecycle guard — the strip is chrome, not agent
/// state. Lives on `AppState` separately from [`StatusStripState`] so pushed
/// values survive a config reload.
#[derive(Debug, Clone, Default)]
pub(crate) struct SlotStore {
    slots: HashMap<String, Slot>,
    /// Last accepted `seq` per source for last-writer-wins.
    seqs: HashMap<String, u64>,
}

impl SlotStore {
    /// Drop values whose TTL has elapsed, along with their seq watermarks.
    ///
    /// TTL expiry is lazy at read time, which hides a stale value but keeps its
    /// entry resident forever. Reclaiming here keeps a writer that pushes with
    /// a TTL from growing the store without bound. Dropping the watermark with
    /// the value matches [`Self::clear`]: once nothing is displayed for a
    /// source, a fresh writer starts clean.
    fn purge_expired(&mut self, now: Instant) {
        self.slots.retain(|_, slot| !slot.is_expired(now));
        let slots = &self.slots;
        self.seqs.retain(|source, _| slots.contains_key(source));
    }

    /// Whether accepting `source` would push the store past its ceiling.
    ///
    /// Callers choose their own `source`, so a writer keying by timestamp, run
    /// id, or pane would otherwise grow the store for the life of the endpoint.
    /// Expired entries are reclaimed first, so a store full of dead TTLs still
    /// admits a new writer.
    pub(crate) fn at_capacity_for(&mut self, source: &str, now: Instant) -> bool {
        self.purge_expired(now);
        !self.slots.contains_key(source) && self.slots.len() >= MAX_STATUS_SLOT_SOURCES
    }

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
    /// value changed, so the caller can decide whether to repaint. An older or
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
    /// dropped. Also clears the seq watermark so a fresh writer starts clean.
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

/// Parsed strip config plus resolved caches. Lives on `AppState`; mutated only
/// on the endpoint tick (clock sampling, command completions) and read when the
/// client-shell snapshot is projected.
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
    pub(crate) fn from_config(cfg: &StatusConfig) -> Self {
        let segments = parse_status_right(&cfg.status_right);
        let mut commands = HashMap::new();
        // Registering no commands on a platform that cannot run them is the
        // same gate `configure_tab_bar_status` applies, and matters more here:
        // an unregistered command resolves to empty text and drops out of the
        // strip, whereas a registered one would fail to spawn every interval
        // forever. Literals, the clock, and pushed slots keep working.
        if crate::platform::status_commands_supported() {
            for seg in &segments {
                if let Segment::Command(cmd) = seg {
                    commands.entry(cmd.clone()).or_default();
                }
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

    /// Whether the current format string actually renders `source`.
    ///
    /// The push lane accepts any source, but only a referenced one can change
    /// what is on screen. Repainting for an unreferenced push would rebuild the
    /// snapshot and re-render for every attached client on a lane a caller can
    /// drive at will.
    pub(crate) fn references_slot(&self, source: &str) -> bool {
        self.is_enabled()
            && self
                .segments
                .iter()
                .any(|segment| matches!(segment, Segment::Slot(name) if name == source))
    }

    /// Column budget the client should fit the strip into, clamped to `u16` for
    /// the wire. Zero when the strip is disabled.
    pub(crate) fn budget(&self) -> u16 {
        if self.is_enabled() {
            u16::try_from(self.budget).unwrap_or(u16::MAX)
        } else {
            0
        }
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
    /// 60s; `None` when there is no clock segment. Independent of the
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

    /// Format every distinct clock segment against `now`, caching the result.
    /// Returns whether any cached text changed.
    pub(crate) fn refresh_clock(&mut self, now: &ClockTime) -> bool {
        let mut changed = false;
        for seg in &self.segments {
            if let Segment::Clock(fmt) = seg {
                let text = format_clock(fmt, now);
                match self.clock_texts.get(fmt) {
                    Some(current) if *current == text => {}
                    _ => {
                        changed = true;
                        self.clock_texts.insert(fmt.clone(), text);
                    }
                }
            }
        }
        changed
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

    /// Earliest instant at which some `#(command)` becomes due, or `None` when
    /// every command is already in flight (or there are none).
    pub(crate) fn next_command_deadline(&self, now: Instant) -> Option<Instant> {
        self.commands
            .values()
            .filter(|slot| !slot.in_flight)
            .map(|slot| {
                slot.last_run
                    .and_then(|last| last.checked_add(self.interval))
                    .unwrap_or(now)
            })
            .min()
    }

    /// Mark a command spawned: record the start time and set the in-flight flag
    /// so it is not re-spawned while running.
    pub(crate) fn mark_command_started(&mut self, command: &str, now: Instant) {
        if let Some(slot) = self.commands.get_mut(command) {
            slot.in_flight = true;
            slot.last_run = Some(now);
        }
    }

    /// Apply a completed command's result. On success with non-empty output the
    /// sanitized value replaces the cache; on error or empty output the last
    /// good value is retained. Returns whether the displayed value changed.
    pub(crate) fn apply_command_result(
        &mut self,
        command: &str,
        result: Result<Option<String>, String>,
    ) -> bool {
        let Some(slot) = self.commands.get_mut(command) else {
            return false;
        };
        slot.in_flight = false;
        match result {
            Ok(Some(value)) => {
                let changed = slot.last_value.as_deref() != Some(value.as_str());
                slot.last_value = Some(value);
                changed
            }
            // Empty output keeps the last good value rather than blanking.
            Ok(None) => false,
            Err(err) => {
                tracing::debug!(command, error = %err, "status command failed; keeping last value");
                false
            }
        }
    }

    /// Resolve segments to their display text, walking the `#[…]` directive
    /// stream left to right and folding the style in force at each content or
    /// literal position. Style directives contribute no output.
    fn resolve(&self, slots: &SlotStore, now: Instant) -> Vec<ResolvedSegment> {
        let mut current = StyleSpec::default();
        let mut out = Vec::with_capacity(self.segments.len());
        for seg in &self.segments {
            let (kind, text) = match seg {
                Segment::Style(spec) => {
                    spec.fold_onto(&mut current);
                    continue;
                }
                Segment::Literal(text) => (SegmentKind::Literal, text.clone()),
                Segment::Clock(fmt) => (
                    SegmentKind::Content,
                    self.clock_texts.get(fmt).cloned().unwrap_or_default(),
                ),
                Segment::Command(cmd) => (
                    SegmentKind::Content,
                    self.commands
                        .get(cmd)
                        .and_then(|slot| slot.last_value.clone())
                        .unwrap_or_default(),
                ),
                Segment::Slot(name) => (SegmentKind::Content, slots.resolve(name, now)),
            };
            out.push(ResolvedSegment {
                kind,
                text,
                style: current.clone(),
            });
        }
        out
    }

    /// The strip's resolved, separator-compacted segments in draw order. Pure:
    /// reads only cached values, so no clock sampling and no spawning happen
    /// here. Width fitting is deliberately absent — that is the client's job,
    /// because the budget depends on the client's own terminal width.
    pub(crate) fn resolved_segments(&self, slots: &SlotStore) -> Vec<ResolvedSegment> {
        if !self.is_enabled() {
            return Vec::new();
        }
        // Sample the monotonic clock once so slot TTLs expire lazily here. This
        // reads the clock but mutates nothing and spawns nothing.
        drop_empty_segments(self.resolve(slots, Instant::now()))
    }

    /// Resolved segment text joined into one line, for tests that assert on the
    /// composed strip without going through the wire projection.
    #[cfg(test)]
    fn resolved_line(&self, slots: &SlotStore) -> String {
        self.resolved_segments(slots)
            .iter()
            .map(|seg| seg.text.as_str())
            .collect()
    }
}

impl App {
    /// (Re)build the strip from config. Bumps the generation so results from
    /// commands spawned under the previous config are discarded on arrival.
    pub(crate) fn configure_status_strip(&mut self, config: &StatusConfig) {
        self.status_strip_generation = self.status_strip_generation.wrapping_add(1);
        self.state.status_strip = StatusStripState::from_config(config);
        // Arm the clock immediately so the strip paints a time on first tick
        // rather than after a full period.
        self.next_status_clock_refresh = self
            .state
            .status_strip
            .clock_period()
            .map(|_| Instant::now());
    }

    /// Next instant the strip needs servicing, for the endpoint's deadline
    /// aggregation. `has_client` gates the whole lane: a detached session must
    /// not keep spawning `#(command)` processes with nobody watching.
    pub(crate) fn next_status_strip_deadline(&self, has_client: bool) -> Option<Instant> {
        if !has_client || !self.state.status_strip.is_enabled() {
            return None;
        }
        let now = Instant::now();
        self.state
            .status_strip
            .next_command_deadline(now)
            .into_iter()
            .chain(self.next_status_clock_refresh)
            .min()
    }

    /// Refresh the clock cache and spawn any due `#(command)`s. Returns whether
    /// anything the strip displays changed.
    pub(crate) fn handle_status_strip_tasks(&mut self, now: Instant, has_client: bool) -> bool {
        if !has_client || !self.state.status_strip.is_enabled() {
            return false;
        }
        let mut changed = false;

        if self
            .next_status_clock_refresh
            .is_some_and(|deadline| now >= deadline)
        {
            if let Some(sampled) = ClockTime::now_local() {
                changed |= self.state.status_strip.refresh_clock(&sampled);
            }
            self.next_status_clock_refresh = self
                .state
                .status_strip
                .clock_period()
                .and_then(|period| now.checked_add(period));
        }

        let due = self.state.status_strip.due_commands(now);
        if due.is_empty() {
            return changed;
        }
        let generation = self.status_strip_generation;
        let (environment, cwd) = self.custom_command_env();
        for command in due {
            self.state.status_strip.mark_command_started(&command, now);
            let finished_command = command.clone();
            // The task handle is deliberately dropped rather than retained.
            // `TabBarCommandRuntime` keeps its handle so a reload can abort the
            // task and kill the process group immediately (see its `Drop` and
            // `reload_aborts_an_in_flight_command_task_and_its_descendants`);
            // the strip settles for a weaker guarantee because its timeout is a
            // fixed 2s rather than configurable, so a reloaded-away command and
            // its descendants outlive the reload by at most that, and the
            // generation guard in `handle_status_command_finished` already
            // discards the result either way.
            drop(super::tab_bar_status::spawn_command_task(
                self.event_tx.clone(),
                command,
                STATUS_COMMAND_TIMEOUT,
                environment.clone(),
                cwd.clone(),
                move |result| crate::events::AppEvent::StatusCommandFinished {
                    generation,
                    command: finished_command,
                    result,
                },
            ));
        }
        changed
    }

    /// Apply a finished `#(command)`, ignoring results from a superseded config.
    pub(crate) fn handle_status_command_finished(
        &mut self,
        generation: u64,
        command: String,
        result: Result<Option<String>, String>,
    ) -> bool {
        if generation != self.status_strip_generation {
            return false;
        }
        self.state
            .status_strip
            .apply_command_result(&command, result)
    }
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

    fn build(status_right: &str) -> StatusStripState {
        StatusStripState::from_config(&StatusConfig {
            status_right: status_right.into(),
            status_right_length: 40,
            status_interval: 5,
        })
    }

    fn strip(status_right: &str, length: usize, interval: u64) -> StatusStripState {
        StatusStripState::from_config(&StatusConfig {
            status_right: status_right.into(),
            status_right_length: length,
            status_interval: interval,
        })
    }

    fn line(strip: &StatusStripState) -> String {
        strip.resolved_line(&SlotStore::default())
    }

    // --- parser ----------------------------------------------------------

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
        assert_eq!(
            parse_status_right("%Q"),
            vec![Segment::Literal("%Q".into())]
        );
        assert_eq!(
            parse_status_right("#[fg=green"),
            vec![Segment::Literal("#[fg=green".into())]
        );
        assert_eq!(
            parse_status_right("#{slot:git"),
            vec![Segment::Literal("#{slot:git".into())]
        );
        // A closed `#{…}` that is not `slot:` stays literal.
        assert_eq!(
            parse_status_right("#{foo}"),
            vec![Segment::Literal("#{foo}".into())]
        );
    }

    #[test]
    fn parses_slot_token() {
        assert_eq!(
            parse_status_right("#{slot:git}"),
            vec![Segment::Slot("git".into())]
        );
    }

    // --- `#[…]` directives -----------------------------------------------

    #[test]
    fn directive_values_stay_raw_for_client_side_palette_resolution() {
        // The endpoint must not resolve colors: a theme token has to reach the
        // client verbatim so it tracks that client's active palette.
        let spec = parse_style_spec("fg=black,bg=#1e1e2e,bold");
        assert_eq!(spec.fg.as_deref(), Some("black"));
        assert_eq!(spec.bg.as_deref(), Some("#1e1e2e"));
        assert!(spec.add_modifier.contains(Modifier::BOLD));
        assert!(!spec.reset);

        // A comma inside `rgb(…)` is not an attribute separator.
        assert_eq!(
            parse_style_spec("fg=rgb(1,2,3)").fg.as_deref(),
            Some("rgb(1,2,3)")
        );
    }

    #[test]
    fn default_and_none_directives_are_reset() {
        assert!(parse_style_spec("default").reset);
        assert!(parse_style_spec("none").reset);
    }

    #[test]
    fn unknown_attr_is_ignored_without_dropping_known_ones() {
        let spec = parse_style_spec("wat=1,bold");
        assert_eq!(spec.fg, None);
        assert_eq!(spec.bg, None);
        assert!(spec.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn style_directives_contribute_zero_resolved_segments() {
        let strip = build("#[fg=green]#[bold]");
        assert!(strip.resolved_segments(&SlotStore::default()).is_empty());
    }

    #[test]
    fn segment_captures_folded_style_and_default_resets_to_base() {
        let strip = build("#[fg=green]hi#[default]bye");
        let segs = strip.resolved_segments(&SlotStore::default());
        assert_eq!(segs[0].text, "hi");
        assert_eq!(segs[0].style.fg.as_deref(), Some("green"));
        assert_eq!(segs[1].text, "bye");
        // `#[default]` folds back to the empty base spec, so the client applies
        // its own base style rather than inheriting green.
        assert_eq!(segs[1].style.fg, None);
    }

    #[test]
    fn directives_layer_cumulatively_until_reset() {
        let strip = build("#[fg=green]#[bold]a#[bg=blue]b");
        let segs = strip.resolved_segments(&SlotStore::default());
        assert_eq!(segs[0].style.fg.as_deref(), Some("green"));
        assert!(segs[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(segs[0].style.bg, None);
        // `b` keeps the earlier fg/bold and adds the new bg.
        assert_eq!(segs[1].style.fg.as_deref(), Some("green"));
        assert!(segs[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(segs[1].style.bg.as_deref(), Some("blue"));
    }

    #[test]
    fn styles_do_not_change_resolved_text() {
        let styled = build("#[fg=green,bg=blue]abc#[default] │ #[bold]xy");
        let plain = build("abc │ xy");
        assert_eq!(line(&styled), line(&plain));
    }

    // --- clock ------------------------------------------------------------

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
        // Unknown tokens survive the formatter literally.
        assert_eq!(format_clock("%Q", &t), "%Q");
    }

    #[test]
    fn clock_period_tracks_finest_field() {
        assert_eq!(
            strip("%H:%M:%S", 20, 30).clock_period(),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            strip("%H:%M", 20, 30).clock_period(),
            Some(Duration::from_secs(60))
        );
        assert_eq!(strip("#(cpu.sh)", 20, 30).clock_period(), None);
    }

    #[test]
    fn refresh_clock_reports_only_real_changes() {
        let mut strip = build("%H:%M");
        assert!(strip.refresh_clock(&fixed_time()));
        // Re-formatting the same instant must not request a repaint.
        assert!(!strip.refresh_clock(&fixed_time()));
        let mut later = fixed_time();
        later.minute = 5;
        assert!(strip.refresh_clock(&later));
    }

    // --- command cache ----------------------------------------------------

    #[test]
    fn command_segment_reads_cache_missing_key_is_empty() {
        let mut strip = strip("#(cpu.sh)", 20, 5);
        assert_eq!(line(&strip), "");
        assert!(strip.apply_command_result("cpu.sh", Ok(Some("42%".into()))));
        assert_eq!(line(&strip), "42%");
    }

    #[test]
    fn command_error_or_empty_output_retains_last_good_value() {
        let mut strip = strip("#(cpu.sh)", 20, 5);
        assert!(strip.apply_command_result("cpu.sh", Ok(Some("42%".into()))));
        assert!(!strip.apply_command_result("cpu.sh", Err("boom".into())));
        assert!(!strip.apply_command_result("cpu.sh", Ok(None)));
        assert_eq!(line(&strip), "42%");
    }

    #[test]
    fn command_that_never_succeeded_and_errors_is_blank() {
        let mut strip = strip("#(cpu.sh)", 20, 5);
        assert!(!strip.apply_command_result("cpu.sh", Err("boom".into())));
        assert_eq!(line(&strip), "");
    }

    #[test]
    fn mixed_segment_list_resolves_in_order() {
        let mut strip = build("#(gitmux) │ %H:%M");
        strip.apply_command_result("gitmux", Ok(Some("main".into())));
        strip.refresh_clock(&fixed_time());
        assert_eq!(line(&strip), "main │ 09:04");
    }

    // --- scheduling -------------------------------------------------------

    #[test]
    fn due_and_in_flight_scheduling() {
        let now = Instant::now();
        let mut strip = strip("#(cpu.sh)", 20, 5);
        // Never run: due immediately (warm on arm).
        assert_eq!(strip.due_commands(now), vec!["cpu.sh".to_string()]);

        strip.mark_command_started("cpu.sh", now);
        assert!(strip.due_commands(now).is_empty());
        // Even past the interval, an in-flight command is skipped, and it
        // contributes no deadline while it runs.
        assert!(strip.due_commands(now + Duration::from_secs(10)).is_empty());
        assert_eq!(strip.next_command_deadline(now), None);

        strip.apply_command_result("cpu.sh", Ok(Some("42%".into())));
        assert!(strip.due_commands(now + Duration::from_secs(1)).is_empty());
        assert_eq!(
            strip.due_commands(now + Duration::from_secs(6)),
            vec!["cpu.sh".to_string()]
        );
    }

    #[test]
    fn timed_out_command_clears_in_flight_and_rearms() {
        // A timeout surfaces as an Err. It must clear the in-flight flag so the
        // command is not wedged, and become due again once the interval passes.
        let now = Instant::now();
        let mut strip = build("#(cpu.sh)");
        strip.mark_command_started("cpu.sh", now);
        assert!(strip.due_commands(now).is_empty(), "should be in flight");
        assert!(!strip.apply_command_result("cpu.sh", Err("timed out after 2s".into())));
        assert!(strip.due_commands(now + Duration::from_secs(1)).is_empty());
        assert_eq!(
            strip.due_commands(now + Duration::from_secs(6)),
            vec!["cpu.sh".to_string()]
        );
    }

    #[test]
    fn interval_floor_applied_via_config_accessor() {
        // status_interval = 0 is floored to MIN_STATUS_INTERVAL_SECONDS, so a
        // command run at t is not due at t but is due 1s later.
        let now = Instant::now();
        let mut strip = strip("#(cpu.sh)", 20, 0);
        strip.mark_command_started("cpu.sh", now);
        strip.apply_command_result("cpu.sh", Ok(Some("x".into())));
        assert!(strip.due_commands(now).is_empty());
        assert_eq!(
            strip.due_commands(now + Duration::from_secs(1)),
            vec!["cpu.sh".to_string()]
        );
    }

    // --- enable gate + budget --------------------------------------------

    #[test]
    fn empty_format_or_zero_length_disables_the_strip() {
        assert!(!strip("", 20, 5).is_enabled());
        assert!(!strip("   ", 20, 5).is_enabled());
        assert!(!strip("%H:%M", 0, 5).is_enabled());
        assert!(strip("%H:%M", 20, 5).is_enabled());
        // A disabled strip reserves no budget and resolves to nothing.
        assert_eq!(strip("%H:%M", 0, 5).budget(), 0);
        assert_eq!(strip("%H:%M", 20, 5).budget(), 20);
        assert_eq!(line(&strip("", 20, 5)), "");
    }

    // --- empty-segment separator drop -------------------------------------

    #[test]
    fn empty_leading_command_drops_following_separator() {
        let mut strip = build("#(git) │ %H:%M");
        strip.refresh_clock(&fixed_time());
        assert_eq!(line(&strip), "09:04");
    }

    #[test]
    fn empty_trailing_command_drops_preceding_separator() {
        let mut strip = build("%H:%M │ #(git)");
        strip.refresh_clock(&fixed_time());
        assert_eq!(line(&strip), "09:04");
    }

    #[test]
    fn nonempty_neighbor_keeps_its_separator() {
        let mut strip = build("#(git) │ %H:%M");
        strip.apply_command_result("git", Ok(Some("main".into())));
        strip.refresh_clock(&fixed_time());
        assert_eq!(line(&strip), "main │ 09:04");
    }

    // --- push lane --------------------------------------------------------

    #[test]
    fn slot_store_set_and_get_by_source() {
        let mut store = SlotStore::default();
        let now = Instant::now();
        assert!(store.set("git".into(), "main".into(), None, None, now));
        assert_eq!(store.get("git", now), Some("main"));
        assert_eq!(store.get("cwd", now), None);
    }

    #[test]
    fn slot_store_newer_seq_overwrites_older_and_equal_ignored() {
        let mut store = SlotStore::default();
        let now = Instant::now();
        assert!(store.set("git".into(), "main".into(), Some(5), None, now));
        assert!(!store.set("git".into(), "feature".into(), Some(5), None, now));
        assert_eq!(store.get("git", now), Some("main"));
        assert!(!store.set("git".into(), "feature".into(), Some(4), None, now));
        assert_eq!(store.get("git", now), Some("main"));
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
        assert_eq!(store.get("t", now + Duration::from_millis(5)), Some("hi"));
        assert_eq!(store.get("t", now + Duration::from_millis(10)), None);
        assert_eq!(store.get("t", now + Duration::from_millis(50)), None);
    }

    #[test]
    fn slot_store_clear_removes_value_and_seq_watermark() {
        let mut store = SlotStore::default();
        let now = Instant::now();
        store.set("git".into(), "main".into(), Some(9), None, now);
        assert!(store.clear("git", now));
        assert_eq!(store.get("git", now), None);
        assert!(!store.clear("git", now));
        // A fresh writer after clear starts clean even with a low seq.
        assert!(store.set("git".into(), "dev".into(), Some(1), None, now));
        assert_eq!(store.get("git", now), Some("dev"));
    }

    #[test]
    fn slot_store_caps_live_sources_but_reclaims_expired_ones() {
        let mut store = SlotStore::default();
        let now = Instant::now();
        for index in 0..MAX_STATUS_SLOT_SOURCES {
            let source = format!("s{index}");
            assert!(!store.at_capacity_for(&source, now), "source {index}");
            store.set(source, "x".into(), None, None, now);
        }
        // A new source beyond the cap is refused; an existing one still writes.
        assert!(store.at_capacity_for("overflow", now));
        assert!(!store.at_capacity_for("s0", now));

        // Expired entries are reclaimed before the cap is applied, so a store
        // full of lapsed TTLs still admits a new writer.
        let mut store = SlotStore::default();
        for index in 0..MAX_STATUS_SLOT_SOURCES {
            store.set(
                format!("s{index}"),
                "x".into(),
                None,
                Some(Duration::from_millis(10)),
                now,
            );
        }
        assert!(store.at_capacity_for("overflow", now));
        let later = now + Duration::from_millis(50);
        assert!(!store.at_capacity_for("overflow", later));
        // The reclaimed sources took their seq watermarks with them, so a fresh
        // writer on a recycled key starts clean.
        assert!(store.set("s0".into(), "new".into(), Some(1), None, later));
    }

    #[test]
    fn references_slot_only_matches_tokens_the_format_string_renders() {
        let configured = build("#{slot:git} │ %H:%M");
        assert!(configured.references_slot("git"));
        assert!(!configured.references_slot("app"));
        // A disabled strip renders nothing, so it references nothing.
        assert!(!strip("#{slot:git}", 0, 5).references_slot("git"));
    }

    #[test]
    fn slot_store_set_reports_visible_change() {
        let mut store = SlotStore::default();
        let now = Instant::now();
        assert!(store.set("s".into(), "a".into(), None, None, now));
        assert!(!store.set("s".into(), "a".into(), None, None, now));
        assert!(store.set("s".into(), "b".into(), None, None, now));
    }

    #[test]
    fn slot_renders_pushed_value_and_drops_its_separator_when_unset() {
        let mut strip = build("#{slot:git} │ %H:%M");
        strip.refresh_clock(&fixed_time());
        assert_eq!(line(&strip), "09:04");

        let mut slots = SlotStore::default();
        slots.set("git".into(), "main".into(), None, None, Instant::now());
        assert_eq!(strip.resolved_line(&slots), "main │ 09:04");
    }

    #[test]
    fn slot_composes_with_command_and_clock() {
        let mut strip = build("#{slot:git} │ #(cpu.sh)% │ %H:%M");
        strip.apply_command_result("cpu.sh", Ok(Some("42".into())));
        strip.refresh_clock(&fixed_time());
        let mut slots = SlotStore::default();
        slots.set("git".into(), "main".into(), None, None, Instant::now());
        assert_eq!(strip.resolved_line(&slots), "main │ 42% │ 09:04");
    }

    #[test]
    fn styled_pill_around_slot_keeps_its_directive_values() {
        let strip = build("#[fg=red,bg=green]#{slot:git}#[default] │ %H:%M");
        let mut slots = SlotStore::default();
        slots.set("git".into(), "main".into(), None, None, Instant::now());
        let segs = strip.resolved_segments(&slots);
        let slot_seg = segs
            .iter()
            .find(|seg| seg.text == "main")
            .expect("slot segment");
        assert_eq!(slot_seg.style.fg.as_deref(), Some("red"));
        assert_eq!(slot_seg.style.bg.as_deref(), Some("green"));
    }
}

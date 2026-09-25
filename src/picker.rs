use crate::model::{is_related, AgentState, Kind, Row};
use crate::refresh::{Message as RefreshMessage, ProjectSourceStatus, Snapshot, Updates};
use crossterm::cursor;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::{execute, terminal};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
use std::collections::HashSet;
use std::io::{self, stdout};
use std::path::Path;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub enum Outcome {
    Cancel,
    Jump(Row),
    ForceNew(Row),
    Close(Row),
}

pub struct Session {
    pub outcome: Outcome,
    pub live_workspace_ids: Option<HashSet<String>>,
    pub state: PickerState,
    /// Workspace the picker was opened from, when it could be identified.
    pub origin_workspace: Option<String>,
}

// --- Herdr's default Catppuccin Mocha palette (semantic tokens) ---
const BASE: Color = Color::Rgb(0x1e, 0x1e, 0x2e);
const MANTLE: Color = Color::Rgb(0x18, 0x18, 0x25);
const SURFACE0: Color = Color::Rgb(0x31, 0x32, 0x44);
const SURFACE1: Color = Color::Rgb(0x45, 0x47, 0x5a);
const FG: Color = Color::Rgb(0xcd, 0xd6, 0xf4);
const SUBTEXT0: Color = Color::Rgb(0xa6, 0xad, 0xc8);
const MUTED: Color = Color::Rgb(0x6c, 0x70, 0x86);
const OVERLAY1: Color = Color::Rgb(0x7f, 0x84, 0x9c);
const ACCENT: Color = Color::Rgb(0x89, 0xb4, 0xfa);
const MAUVE: Color = Color::Rgb(0xcb, 0xa6, 0xf7);
const SAPPHIRE: Color = Color::Rgb(0x74, 0xc7, 0xec);
const SKY: Color = Color::Rgb(0x89, 0xdc, 0xeb);
const TEAL: Color = Color::Rgb(0x94, 0xe2, 0xd5);
const GREEN: Color = Color::Rgb(0xa6, 0xe3, 0xa1);
const PEACH: Color = Color::Rgb(0xfa, 0xb3, 0x87);
const YELLOW: Color = Color::Rgb(0xf9, 0xe2, 0xaf);
const RED: Color = Color::Rgb(0xf3, 0x8b, 0xa8);
const SEL_BG: Color = SURFACE1;
const RELATED_BG: Color = SURFACE0;
const PATH_COLOR: Color = MUTED;
const FAINT: Color = BASE;

const NAME_W: usize = 20;
const GLYPH_W: usize = 2;
const HL_W: usize = 2;
const EVENT_POLL: Duration = Duration::from_millis(50);

struct TerminalGuard {
    raw_mode: bool,
    alternate_screen: bool,
    mouse_capture: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut guard = TerminalGuard {
            raw_mode: false,
            alternate_screen: false,
            mouse_capture: false,
        };
        terminal::enable_raw_mode()?;
        guard.raw_mode = true;
        if let Err(error) = execute!(stdout(), terminal::EnterAlternateScreen) {
            let _ = guard.restore();
            return Err(error);
        }
        guard.alternate_screen = true;
        guard.mouse_capture = true;
        if let Err(error) = execute!(stdout(), EnableMouseCapture) {
            let _ = guard.restore();
            return Err(error);
        }
        if let Err(error) = execute!(stdout(), cursor::Hide) {
            let _ = guard.restore();
            return Err(error);
        }
        Ok(guard)
    }

    fn restore(&mut self) -> io::Result<()> {
        let mut failure = None;
        if self.mouse_capture {
            if let Err(error) = execute!(stdout(), DisableMouseCapture) {
                failure = Some(error);
            }
            self.mouse_capture = false;
        }
        if self.alternate_screen {
            if let Err(error) = execute!(stdout(), cursor::Show, terminal::LeaveAlternateScreen) {
                if failure.is_none() {
                    failure = Some(error);
                }
            }
            self.alternate_screen = false;
        } else if let Err(error) = execute!(stdout(), cursor::Show) {
            if failure.is_none() {
                failure = Some(error);
            }
        }
        if self.raw_mode {
            if let Err(error) = terminal::disable_raw_mode() {
                if failure.is_none() {
                    failure = Some(error);
                }
            }
            self.raw_mode = false;
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Default)]
pub struct PickerState {
    rows: Vec<Row>,
    query: String,
    selected: usize,
}

impl PickerState {
    fn filtered(&self, matcher: &mut Matcher) -> Vec<usize> {
        filter(&self.rows, &self.query, matcher)
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot, matcher: &mut Matcher) {
        let old_filtered = self.filtered(matcher);
        let selected_id = old_filtered
            .get(self.selected)
            .map(|index| self.rows[*index].id());

        self.rows = snapshot.rows;
        let filtered = self.filtered(matcher);
        self.selected = selected_id
            .and_then(|id| {
                filtered
                    .iter()
                    .position(|index| self.rows[*index].id() == id)
            })
            .unwrap_or(0);
        if self.selected >= filtered.len() {
            self.selected = filtered.len().saturating_sub(1);
        }
    }

    fn apply_partial(&mut self, mut snapshot: Snapshot, matcher: &mut Matcher) {
        // Project discovery has not completed yet. Retain the last known
        // dormant rows so a loading refresh cannot make them disappear.
        let refreshed_ids: HashSet<_> = snapshot.rows.iter().map(Row::id).collect();
        snapshot.rows.extend(
            self.rows
                .iter()
                .filter(|row| matches!(row.kind, Kind::Dormant))
                .filter(|row| !refreshed_ids.contains(&row.id()))
                .cloned(),
        );
        self.apply_snapshot(snapshot, matcher);
    }

    /// Absolute paths for live workspaces observed in this picker snapshot.
    pub fn open_workspace_paths(&self) -> impl Iterator<Item = &Path> {
        self.rows
            .iter()
            .filter(|row| matches!(row.kind, Kind::Open { .. }))
            .map(|row| row.path.as_path())
    }

    pub fn remove(&mut self, id: &crate::model::RowId) {
        self.rows.retain(|row| row.id() != *id);
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let filtered = self.filtered(&mut matcher);
        self.selected = self.selected.min(filtered.len().saturating_sub(1));
    }

    fn selected_row(&self, filtered: &[usize]) -> Option<Row> {
        filtered
            .get(self.selected)
            .map(|index| self.rows[*index].clone())
    }

    /// Move to the first actionable row in the next populated section, wrapping
    /// in OPEN → TABS → PANES → PROJECTS order. Empty sections are skipped.
    fn next_section(&mut self, filtered: &[usize]) {
        let Some(current_index) = filtered.get(self.selected) else {
            return;
        };
        let current_section = section(&self.rows[*current_index].kind);
        for offset in 1..=4 {
            let target = (current_section + offset) % 4;
            if let Some(position) = filtered
                .iter()
                .position(|index| section(&self.rows[*index].kind) == target)
            {
                self.selected = position;
                return;
            }
        }
    }
}

/// Map an Alt+digit key to a row index, where 1 is the most recently used
/// open workspace and 0 the tenth. Returns `None` for non-digit keys.
fn alt_digit_index(digit: char) -> Option<usize> {
    digit.to_digit(10).map(|digit| match digit {
        0 => 9,
        digit => digit as usize - 1,
    })
}

/// macOS US-layout Option+0..9 glyphs, for terminals that send the symbol
/// instead of an ESC-prefixed digit (Option-as-Meta disabled).
const MAC_OPTION_DIGITS: &[(char, char)] = &[
    ('º', '0'),
    ('¡', '1'),
    ('™', '2'),
    ('£', '3'),
    ('¢', '4'),
    ('∞', '5'),
    ('§', '6'),
    ('¶', '7'),
    ('•', '8'),
    ('ª', '9'),
];

/// Map a macOS Option+digit glyph to a row index, when the terminal sent the
/// symbol rather than an ESC-prefixed digit.
fn mac_option_digit(character: char) -> Option<usize> {
    MAC_OPTION_DIGITS
        .iter()
        .find(|(symbol, _)| *symbol == character)
        .and_then(|(_, digit)| alt_digit_index(*digit))
}

/// Select and return the row of the Nth open workspace in the filtered list,
/// matching the quick-jump number shown in front of open rows. `None` when
/// fewer than N+1 open rows match. Dormant rows are skipped.
fn nth_open_jump(state: &mut PickerState, filtered: &[usize], n: usize) -> Option<Row> {
    let position = filtered
        .iter()
        .enumerate()
        .filter(|(_, &row_index)| matches!(state.rows[row_index].kind, Kind::Open { .. }))
        .nth(n)
        .map(|(position, _)| position)?;
    state.selected = position;
    state.selected_row(filtered)
}

fn state_color(state: AgentState) -> Color {
    match state {
        AgentState::Blocked => RED,
        AgentState::Working => GREEN,
        AgentState::Done => GREEN,
        AgentState::Idle => YELLOW,
        AgentState::Unknown => MUTED,
    }
}

struct SearchDocument(Vec<String>);

impl SearchDocument {
    fn for_row(row: &Row) -> Self {
        let mut fields = Vec::new();
        push_unique(&mut fields, row.name.clone());
        push_unique(&mut fields, row.display.clone());
        if let Some(space) = &row.space {
            push_unique(&mut fields, space.label.clone());
            if let Some(disambiguator) = &space.disambiguator {
                push_unique(&mut fields, disambiguator.clone());
            }
        }

        match &row.kind {
            Kind::Open { state, agent, .. } => {
                push_unique(&mut fields, row.path.display().to_string());
                push_unique(&mut fields, state.word().to_string());
                if let Some(agent) = agent {
                    push_unique(&mut fields, agent.clone());
                }
                for pane_name in &row.pane_names {
                    push_unique(&mut fields, pane_name.clone());
                }
            }
            Kind::Tab {
                state,
                agent,
                tab_id,
                ..
            } => {
                push_unique(&mut fields, row.path.display().to_string());
                push_unique(&mut fields, tab_id.clone());
                push_unique(&mut fields, state.word().to_string());
                if let Some(agent) = agent {
                    push_unique(&mut fields, agent.clone());
                }
            }
            Kind::Pane {
                state,
                agent,
                pane_id,
                tab_id,
                ..
            } => {
                push_unique(&mut fields, row.path.display().to_string());
                push_unique(&mut fields, pane_id.clone());
                if let Some(tab_id) = tab_id {
                    push_unique(&mut fields, tab_id.clone());
                }
                push_unique(&mut fields, state.word().to_string());
                if let Some(agent) = agent {
                    push_unique(&mut fields, agent.clone());
                }
            }
            Kind::Dormant => {}
        }

        SearchDocument(fields)
    }
}

/// Section order: live workspaces, their tabs, renamed panes, then projects.
/// Only the first section participates in the Alt+digit quick jumps.
fn section(kind: &Kind) -> u8 {
    match kind {
        Kind::Open { .. } => 0,
        Kind::Tab { .. } => 1,
        Kind::Pane { .. } => 2,
        Kind::Dormant => 3,
    }
}

fn group_indices_by_space(rows: &[Row], indices: Vec<usize>, alphabetical: bool) -> Vec<usize> {
    let mut groups: Vec<(Option<(String, Option<String>)>, Vec<usize>)> = Vec::new();
    for index in indices {
        let row = &rows[index];
        let key = if matches!(&row.kind, Kind::Tab { .. } | Kind::Pane { .. }) {
            row.space
                .as_ref()
                .map(|space| (space.label.clone(), space.disambiguator.clone()))
        } else {
            None
        };
        if let Some(key) = key {
            if let Some((_, members)) = groups
                .iter_mut()
                .find(|(existing, _)| existing.as_ref() == Some(&key))
            {
                members.push(index);
            } else {
                groups.push((Some(key), vec![index]));
            }
        } else {
            // Unknown context is not enough evidence to merge unrelated rows.
            groups.push((None, vec![index]));
        }
    }
    if alphabetical {
        groups.sort_by(|(left, _), (right, _)| match (left, right) {
            (Some(left), Some(right)) => left.cmp(right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });
    }
    groups
        .into_iter()
        .flat_map(|(_, members)| members)
        .collect()
}

fn push_unique(fields: &mut Vec<String>, value: String) {
    if !value.is_empty() && !fields.iter().any(|existing| existing == &value) {
        fields.push(value);
    }
}

/// Return original row indices, ranked within each section. Sections always
/// appear in `section()` order, including when the query is empty.
fn filter(rows: &[Row], query: &str, matcher: &mut Matcher) -> Vec<usize> {
    if query.is_empty() {
        let mut sections: Vec<Vec<usize>> = vec![Vec::new(); 4];
        for index in 0..rows.len() {
            sections[section(&rows[index].kind) as usize].push(index);
        }
        for group in [1, 2] {
            sections[group] =
                group_indices_by_space(rows, std::mem::take(&mut sections[group]), true);
        }
        return sections.into_iter().flatten().collect();
    }

    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buffer = Vec::new();
    let mut sections: Vec<Vec<(u32, usize)>> = vec![Vec::new(); 4];

    for (index, row) in rows.iter().enumerate() {
        let document = SearchDocument::for_row(row);
        // Match each field independently and keep its strongest score. Joining
        // fields lets a fuzzy subsequence cross metadata boundaries, creating
        // results that do not actually match any name, path, or pane label.
        let score = document
            .0
            .iter()
            .filter_map(|field| {
                let haystack = Utf32Str::new(field, &mut buffer);
                pattern.score(haystack, matcher)
            })
            .max();
        let Some(score) = score else { continue };
        sections[section(&row.kind) as usize].push((score, index));
    }

    let rank = |a: &(u32, usize), b: &(u32, usize)| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1));
    for bucket in &mut sections {
        bucket.sort_by(rank);
    }
    let mut sections: Vec<Vec<usize>> = sections
        .into_iter()
        .map(|bucket| bucket.into_iter().map(|(_, index)| index).collect())
        .collect();
    for group in [1, 2] {
        sections[group] = group_indices_by_space(rows, std::mem::take(&mut sections[group]), false);
    }
    sections.into_iter().flatten().collect()
}

fn display_width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

fn truncate_to_width(value: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if display_width(value) <= max_width {
        return value.to_string();
    }

    let ellipsis = "…";
    let content_width = max_width.saturating_sub(display_width(ellipsis));
    let mut result = String::new();
    let mut used = 0;
    for grapheme in UnicodeSegmentation::graphemes(value, true) {
        let width = display_width(grapheme);
        if used + width > content_width {
            break;
        }
        result.push_str(grapheme);
        used += width;
    }
    result.push_str(ellipsis);
    result
}

fn pad_to_width(value: &str, width: usize) -> String {
    let value = truncate_to_width(value, width);
    let padding = width.saturating_sub(display_width(&value));
    format!("{value}{}", " ".repeat(padding))
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn truncate_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Span<'static>> {
    let mut remaining = max_width;
    let mut output = Vec::new();
    for span in spans {
        if remaining == 0 {
            break;
        }
        let original_width = display_width(span.content.as_ref());
        let content = truncate_to_width(span.content.as_ref(), remaining);
        let width = display_width(&content);
        output.push(Span::styled(content, span.style));
        remaining = remaining.saturating_sub(width);
        if original_width > width {
            break;
        }
    }
    output
}

fn pane_summary(row: &Row) -> Option<String> {
    let mut seen = HashSet::new();
    let unique: Vec<&str> = row
        .pane_names
        .iter()
        .map(String::as_str)
        .filter(|name| seen.insert(*name))
        .collect();
    match unique.as_slice() {
        [] => None,
        [name] => Some((*name).to_string()),
        [first, second] => Some(format!("{first} · {second}")),
        [first, second, rest @ ..] => Some(format!("{first} · {second} +{}", rest.len())),
    }
}

/// Compact, deduplicated summary of the workspace's tab labels.
fn tab_summary(row: &Row) -> Option<String> {
    let mut seen = HashSet::new();
    let unique: Vec<&str> = row
        .tab_names
        .iter()
        .map(String::as_str)
        .filter(|name| seen.insert(*name))
        .collect();
    match unique.as_slice() {
        [] => None,
        [only] => Some((*only).to_string()),
        [first, second] => Some(format!("{first} · {second}")),
        [first, second, rest @ ..] => Some(format!("{first} · {second} +{}", rest.len())),
    }
}

/// Muted context keeps the selected space's contents while its human-facing
/// name is promoted to the primary open-row label.
fn secondary_context(row: &Row) -> String {
    if matches!(&row.kind, Kind::Open { .. }) {
        let mut parts = Vec::new();
        if let Some(space) = &row.space {
            if let Some(disambiguator) = &space.disambiguator {
                parts.push(disambiguator.clone());
            }
        }
        if let Some(panes) = pane_summary(row) {
            parts.push(format!("panes: {panes}"));
        }
        if !row.display.is_empty() {
            parts.push(row.display.clone());
        }
        if let Some(tabs) = tab_summary(row) {
            parts.push(format!("tabs: {tabs}"));
        }
        return parts.join(" · ");
    }

    match tab_summary(row) {
        Some(tabs) if row.display.is_empty() => format!("tabs: {tabs}"),
        Some(tabs) => format!("{} · tabs: {tabs}", row.display),
        None => row.display.clone(),
    }
}

fn context_spans(text: &str, style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let next_label = ["panes:", "tabs:", "folder:"]
            .iter()
            .filter_map(|label| rest.find(label).map(|index| (index, *label)))
            .min_by_key(|(index, _)| *index);
        let Some((index, label)) = next_label else {
            spans.push(Span::styled(rest.to_string(), style));
            break;
        };
        if index > 0 {
            spans.push(Span::styled(rest[..index].to_string(), style));
        }
        spans.push(Span::styled(
            label.to_string(),
            Style::default().fg(OVERLAY1),
        ));
        rest = &rest[index + label.len()..];
    }
    spans
}

fn body_spans(
    name: &str,
    path: &str,
    budget: usize,
    name_style: Style,
    path_style: Style,
) -> Vec<Span<'static>> {
    if budget == 0 {
        return Vec::new();
    }
    if path.is_empty() || budget < 14 {
        return vec![Span::styled(truncate_to_width(name, budget), name_style)];
    }

    let name_budget = NAME_W.min(budget.saturating_sub(5));
    let path_budget = budget.saturating_sub(name_budget + 1);
    let mut spans = vec![
        Span::styled(pad_to_width(name, name_budget), name_style),
        Span::raw(" "),
    ];
    spans.extend(context_spans(
        &truncate_to_width(path, path_budget),
        path_style,
    ));
    spans
}

/// One row constrained to `width` terminal display columns. `number` is the
/// Alt+digit quick-jump number (1-9, 0) shown in front of open rows; dormant
/// rows pass `None`.
/// One stateful row: optional Alt+digit prefix, state glyph, bold name, muted
/// context, and agent/state meta on the right.
fn stateful_line(
    row: &Row,
    width: usize,
    number: Option<usize>,
    state: AgentState,
    agent: Option<&str>,
    name: &str,
    related: bool,
    pinned: bool,
) -> Line<'static> {
    let number_prefix = number.map_or(String::new(), |digit| format!("{digit} "));
    let number_width = display_width(&number_prefix);
    let color = state_color(state);
    let glyph = truncate_to_width(&format!("{} ", state.glyph()), width.min(GLYPH_W));
    let glyph_width = display_width(&glyph);
    let relation_marker = if pinned {
        "● "
    } else if related {
        "↳ "
    } else {
        ""
    };
    let relation_width = display_width(relation_marker);
    let remaining = width.saturating_sub(number_width + glyph_width + relation_width);

    let full_meta = match agent {
        Some(agent) => format!("{agent} · {}", state.word()),
        None => state.word().to_string(),
    };
    let meta = if width >= 30 {
        truncate_to_width(&full_meta, (width / 3).min(20))
    } else {
        String::new()
    };
    let meta_width = display_width(&meta);
    let meta_gap = usize::from(!meta.is_empty() && remaining > meta_width);
    let body_budget = remaining.saturating_sub(meta_width + meta_gap);
    let mut body = body_spans(
        name,
        &secondary_context(row),
        body_budget,
        Style::default()
            .fg(if pinned {
                MAUVE
            } else if related {
                SKY
            } else {
                FG
            })
            .add_modifier(Modifier::BOLD),
        Style::default().fg(if related { SAPPHIRE } else { PATH_COLOR }),
    );
    let body_width = spans_width(&body);
    if body_width < body_budget {
        body.push(Span::raw(" ".repeat(body_budget - body_width)));
    }

    let mut spans = Vec::new();
    if !number_prefix.is_empty() {
        spans.push(Span::styled(
            number_prefix,
            Style::default().fg(SUBTEXT0).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(glyph, Style::default().fg(color)));
    if pinned || related {
        spans.push(Span::styled(
            relation_marker,
            Style::default().fg(if pinned { MAUVE } else { SKY }),
        ));
    }
    spans.extend(body);
    if meta_gap > 0 {
        spans.push(Span::raw(" "));
    }
    if !meta.is_empty() {
        if let Some((agent_tag, status)) = meta.split_once(" · ") {
            spans.push(Span::styled(
                agent_tag.to_string(),
                Style::default().fg(MAUVE),
            ));
            spans.push(Span::styled(" · ", Style::default().fg(OVERLAY1)));
            spans.push(Span::styled(status.to_string(), Style::default().fg(color)));
        } else {
            spans.push(Span::styled(
                meta,
                Style::default().fg(if agent.is_some() { MAUVE } else { color }),
            ));
        }
    }
    Line::from(truncate_spans(spans, width))
}

fn row_line(row: &Row, width: usize, number: Option<usize>) -> Line<'static> {
    row_line_with_relation(row, width, number, false, false)
}

fn row_line_with_relation(
    row: &Row,
    width: usize,
    number: Option<usize>,
    related: bool,
    pinned: bool,
) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }

    match &row.kind {
        Kind::Open { state, agent, .. } => stateful_line(
            row,
            width,
            number,
            *state,
            agent.as_deref(),
            row.space
                .as_ref()
                .map_or(row.name.as_str(), |space| space.label.as_str()),
            related,
            pinned,
        ),
        // Tabs and renamed panes are ordinary rows too; they just never carry
        // an Alt+digit number because that numbering is workspace-only.
        Kind::Tab { state, agent, .. } | Kind::Pane { state, agent, .. } => stateful_line(
            row,
            width,
            number,
            *state,
            agent.as_deref(),
            &row.name,
            related,
            pinned,
        ),
        Kind::Dormant => {
            let marker = if pinned {
                "● "
            } else if related {
                "↳ "
            } else {
                "  "
            };
            let glyph = truncate_to_width(marker, width.min(GLYPH_W));
            let body_budget = width.saturating_sub(display_width(&glyph));
            let glyph_style = if pinned {
                Style::default().fg(MAUVE)
            } else if related {
                Style::default().fg(SKY)
            } else {
                Style::default()
            };
            let mut spans = vec![Span::styled(glyph, glyph_style)];
            spans.extend(body_spans(
                &row.name,
                &row.display,
                body_budget,
                Style::default().fg(if related { SKY } else { FG }),
                Style::default().fg(if related { SAPPHIRE } else { PATH_COLOR }),
            ));
            Line::from(truncate_spans(spans, width))
        }
    }
}

fn header_color(label: &str) -> Color {
    match label {
        "OPEN" => MAUVE,
        "TABS" => PEACH,
        "PANES" => TEAL,
        "PROJECTS" => SKY,
        _ => SUBTEXT0,
    }
}

fn header_item(label: &str, suffix: &str, width: usize) -> ListItem<'static> {
    let spans = vec![
        Span::styled(
            format!("── {label} "),
            Style::default()
                .fg(header_color(label))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("— {suffix}"), Style::default().fg(OVERLAY1)),
    ];
    ListItem::new(Line::from(truncate_spans(spans, width)))
}

fn section_spaces(rows: &[Row], filtered: &[usize], group: u8) -> Vec<crate::model::SpaceContext> {
    let mut spaces = Vec::new();
    for row_index in filtered.iter().copied() {
        let row = &rows[row_index];
        if section(&row.kind) != group {
            continue;
        }
        let Some(space) = &row.space else { continue };
        if !spaces.iter().any(|existing: &crate::model::SpaceContext| {
            existing.label == space.label && existing.disambiguator == space.disambiguator
        }) {
            spaces.push(space.clone());
        }
    }
    spaces
}

fn space_header_item(space: &crate::model::SpaceContext, width: usize) -> ListItem<'static> {
    let mut spans = vec![
        Span::styled(
            "  ↳ SPACE · ",
            Style::default().fg(SAPPHIRE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            space.label.clone(),
            Style::default().fg(FG).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(disambiguator) = &space.disambiguator {
        spans.push(Span::styled(
            format!(" · {disambiguator}"),
            Style::default().fg(FAINT),
        ));
    }
    ListItem::new(Line::from(truncate_spans(spans, width)))
}

fn build(
    rows: &[Row],
    filtered: &[usize],
    selected: usize,
    width: usize,
) -> (Vec<ListItem<'static>>, usize, Vec<Option<usize>>) {
    build_with_anchor(rows, filtered, selected, width, None)
}

fn build_with_anchor(
    rows: &[Row],
    filtered: &[usize],
    selected: usize,
    width: usize,
    anchor: Option<&crate::model::RowId>,
) -> (Vec<ListItem<'static>>, usize, Vec<Option<usize>>) {
    let tab_spaces = section_spaces(rows, filtered, 1);
    let pane_spaces = section_spaces(rows, filtered, 2);
    let active_item = anchor
        .and_then(|id| rows.iter().find(|row| row.id() == *id))
        .or_else(|| filtered.get(selected).and_then(|index| rows.get(*index)));
    let mut items = Vec::new();
    let mut row_positions = Vec::new();
    let mut selected_position = 0;
    let mut last_group = None;
    let mut last_space: Option<(String, Option<String>)> = None;
    let mut open_index = 0usize;
    for (filtered_index, row_index) in filtered.iter().copied().enumerate() {
        let row = &rows[row_index];
        let group = section(&row.kind);
        let spaces: &[crate::model::SpaceContext] = match group {
            1 => &tab_spaces,
            2 => &pane_spaces,
            _ => &[],
        };
        if last_group != Some(group) {
            let (label, default_suffix) = match group {
                0 => ("OPEN", "LIVE WORKSPACES"),
                1 => ("TABS", "OPEN WORKSPACES"),
                2 => ("PANES", "RENAMED"),
                _ => ("PROJECTS", "NOT OPEN YET"),
            };
            let suffix = if spaces.len() == 1 {
                let space = &spaces[0];
                match &space.disambiguator {
                    Some(disambiguator) => format!("IN {} · {disambiguator}", space.label),
                    None => format!("IN SPACE {}", space.label),
                }
            } else {
                default_suffix.to_string()
            };
            items.push(header_item(label, &suffix, width));
            row_positions.push(None);
            last_group = Some(group);
            last_space = None;
        }
        if spaces.len() > 1 && matches!(group, 1 | 2) {
            if let Some(space) = &row.space {
                let key = (space.label.clone(), space.disambiguator.clone());
                if last_space.as_ref() != Some(&key) {
                    items.push(space_header_item(space, width));
                    row_positions.push(None);
                    last_space = Some(key);
                }
            }
        }
        if filtered_index == selected {
            selected_position = items.len();
        }
        // Open rows carry their Alt+digit quick-jump number (1-9, 0) up front.
        let number = if group == 0 {
            let number = open_index;
            open_index += 1;
            (number <= 9).then_some((number + 1) % 10)
        } else {
            None
        };
        let pinned = anchor.is_some_and(|id| row.id() == *id);
        let related = !pinned && active_item.is_some_and(|active| is_related(active, row));
        let item = ListItem::new(row_line_with_relation(row, width, number, related, pinned));
        items.push(if pinned {
            item.style(Style::default().fg(MAUVE).bg(RELATED_BG))
        } else if related {
            item.style(Style::default().fg(SKY).bg(RELATED_BG))
        } else {
            item
        });
        row_positions.push(Some(filtered_index));
    }
    (items, selected_position, row_positions)
}

fn spread(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let right = truncate_spans(right, width / 3);
    let right_width = spans_width(&right);
    let gap = usize::from(right_width > 0 && width > right_width);
    let left_budget = width.saturating_sub(right_width + gap);
    let mut left = truncate_spans(left, left_budget);
    let left_width = spans_width(&left);
    left.push(Span::raw(
        " ".repeat(width.saturating_sub(left_width + right_width)),
    ));
    left.extend(right);
    Line::from(truncate_spans(left, width))
}

fn mouse_in_area(column: u16, row: u16, area: Rect) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn mouse_row_index(
    column: u16,
    row: u16,
    area: Rect,
    list_offset: usize,
    row_positions: &[Option<usize>],
) -> Option<usize> {
    if !mouse_in_area(column, row, area) {
        return None;
    }
    let relative_row = row - area.y;
    row_positions
        .get(list_offset + usize::from(relative_row))
        .copied()
        .flatten()
}

fn mouse_scroll_target(selected: usize, count: usize, kind: MouseEventKind) -> Option<usize> {
    match kind {
        MouseEventKind::ScrollUp if selected > 0 => Some(selected - 1),
        MouseEventKind::ScrollDown if selected.saturating_add(1) < count => Some(selected + 1),
        _ => None,
    }
}

fn enter_action(kind: &Kind) -> &'static str {
    match kind {
        Kind::Open { .. } | Kind::Tab { .. } | Kind::Pane { .. } => "focus",
        Kind::Dormant => "open project",
    }
}

fn keycap(key: &str, label: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(FG)
                .bg(SURFACE1)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label}   "), Style::default().fg(SUBTEXT0)),
    ]
}

fn empty_item(
    loading: bool,
    project_search_started: bool,
    project_source_status: ProjectSourceStatus,
    error: Option<&str>,
    query: &str,
    width: usize,
) -> ListItem<'static> {
    let (message, color) = if loading {
        (
            if project_search_started {
                if query.is_empty() {
                    "Searching projects…"
                } else {
                    "No matches yet — searching projects…"
                }
            } else if query.is_empty() {
                "Loading workspaces…"
            } else {
                "No matches yet — still loading…"
            },
            MUTED,
        )
    } else if let Some(error) = error {
        (error, RED)
    } else if !query.is_empty() {
        ("No matching workspaces or projects", MUTED)
    } else if project_source_status == ProjectSourceStatus::Unavailable {
        ("No projects found; zoxide suggestions unavailable", MUTED)
    } else {
        ("No projects found", MUTED)
    };
    ListItem::new(Line::from(vec![Span::styled(
        truncate_to_width(message, width),
        Style::default().fg(color),
    )]))
}

pub fn run(mut state: PickerState, updates: Updates) -> io::Result<Session> {
    let mut terminal_guard = TerminalGuard::enter()?;
    let mut terminal = match Terminal::new(CrosstermBackend::new(stdout())) {
        Ok(terminal) => terminal,
        Err(error) => {
            if let Err(restore_error) = terminal_guard.restore() {
                eprintln!("herdr-muster: restore terminal: {restore_error}");
            }
            return Err(error);
        }
    };
    let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
    let mut loading = true;
    let mut project_search_started = false;
    let mut project_source_status = ProjectSourceStatus::Searching;
    let mut refresh_error: Option<String> = None;
    let mut live_workspace_ids = None;
    let mut origin_workspace = None;
    let mut channel_open = true;
    let mut highlight_anchor: Option<crate::model::RowId> = None;
    let mut outcome = Outcome::Cancel;

    let result = (|| -> io::Result<Session> {
        loop {
            while channel_open {
                match updates.try_recv() {
                    Ok(RefreshMessage::Partial(snapshot)) => {
                        project_source_status = snapshot.project_source_status;
                        project_search_started = true;
                        live_workspace_ids = snapshot.live_workspace_ids.clone();
                        origin_workspace = snapshot.origin_workspace.clone();
                        state.apply_partial(snapshot, &mut matcher);
                        loading = true;
                        refresh_error = None;
                    }
                    Ok(RefreshMessage::Ready(snapshot)) => {
                        project_source_status = snapshot.project_source_status;
                        live_workspace_ids = snapshot.live_workspace_ids.clone();
                        origin_workspace = snapshot.origin_workspace.clone();
                        state.apply_snapshot(snapshot, &mut matcher);
                        loading = false;
                        refresh_error = None;
                    }
                    Ok(RefreshMessage::Failed(error)) => {
                        loading = false;
                        project_search_started = false;
                        refresh_error = Some(format!("Refresh failed: {error}"));
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        channel_open = false;
                        if loading {
                            loading = false;
                            project_search_started = false;
                            refresh_error = Some("Refresh stopped before completion".into());
                        }
                    }
                }
            }

            let filtered = state.filtered(&mut matcher);
            if highlight_anchor
                .as_ref()
                .is_some_and(|anchor| !state.rows.iter().any(|row| row.id() == *anchor))
            {
                highlight_anchor = None;
            }
            if state.selected >= filtered.len() {
                state.selected = filtered.len().saturating_sub(1);
            }
            let open_count = state
                .rows
                .iter()
                .filter(|row| matches!(row.kind, Kind::Open { .. }))
                .count();
            let project_count = state
                .rows
                .iter()
                .filter(|row| matches!(row.kind, Kind::Dormant))
                .count();

            let mut list_state = ListState::default();
            let mut list_area = Rect::default();
            let mut row_positions = Vec::new();
            terminal.draw(|frame| {
                let area = frame.area();
                let title_left = Line::from(vec![Span::styled(
                    " one terminal for the whole herd ",
                    Style::default().fg(MUTED),
                )])
                .left_aligned();
                let mut title_spans = vec![
                    Span::styled(format!(" {open_count} open"), Style::default().fg(GREEN)),
                    Span::styled(" · ", Style::default().fg(OVERLAY1)),
                    Span::styled(
                        format!("{project_count} projects"),
                        Style::default().fg(PEACH),
                    ),
                ];
                if loading {
                    let status = if project_search_started {
                        " · searching projects "
                    } else {
                        " · loading workspaces "
                    };
                    title_spans.push(Span::styled(status, Style::default().fg(YELLOW)));
                } else if refresh_error.is_some() {
                    title_spans.push(Span::styled(" · refresh failed ", Style::default().fg(RED)));
                } else if project_source_status == ProjectSourceStatus::Unavailable {
                    title_spans.push(Span::styled(
                        " · zoxide unavailable ",
                        Style::default().fg(YELLOW),
                    ));
                } else {
                    title_spans.push(Span::raw(" "));
                }
                let title_right = Line::from(title_spans).right_aligned();
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(SURFACE1))
                    .style(Style::default().bg(MANTLE))
                    .title_top(title_left)
                    .title_top(title_right);
                let inner = block.inner(area);
                frame.render_widget(block, area);

                let vertical = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ])
                .horizontal_margin(1)
                .split(inner);
                let width = vertical[2].width as usize;

                let query_span = if state.query.is_empty() {
                    Span::styled("type to fuzzy-filter…", Style::default().fg(FAINT))
                } else {
                    Span::styled(state.query.clone(), Style::default().fg(FG))
                };
                let prompt = spread(
                    vec![
                        Span::styled(
                            "› ",
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                        ),
                        query_span,
                    ],
                    vec![Span::styled(
                        format!("{} matches", filtered.len()),
                        Style::default().fg(SUBTEXT0),
                    )],
                    width,
                );
                frame.render_widget(Paragraph::new(prompt), vertical[0]);

                let rule = "─".repeat(width);
                let rule_style = Style::default().fg(FAINT);
                frame.render_widget(Paragraph::new(rule.clone()).style(rule_style), vertical[1]);
                frame.render_widget(Paragraph::new(rule).style(rule_style), vertical[3]);

                let list_width = width.saturating_sub(HL_W);
                let (items, selected_position, rendered_row_positions) = if filtered.is_empty() {
                    (
                        vec![empty_item(
                            loading,
                            project_search_started,
                            project_source_status,
                            refresh_error.as_deref(),
                            &state.query,
                            list_width,
                        )],
                        0,
                        vec![None],
                    )
                } else {
                    build_with_anchor(
                        &state.rows,
                        &filtered,
                        state.selected,
                        list_width,
                        highlight_anchor.as_ref(),
                    )
                };
                row_positions = rendered_row_positions;
                if !filtered.is_empty() {
                    list_state.select(Some(selected_position));
                }
                let list = List::new(items)
                    .highlight_style(
                        Style::default()
                            .fg(MAUVE)
                            .bg(SEL_BG)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("▸ ");
                list_area = vertical[2];
                frame.render_stateful_widget(list, list_area, &mut list_state);

                let mut footer = Vec::new();
                if let Some(row) = state.selected_row(&filtered) {
                    footer.extend(keycap("↵", enter_action(&row.kind)));
                }
                footer.extend(keycap("tab", "next section"));
                footer.extend(keycap("⌥0-9", "recents"));
                footer.extend(keycap("^n", "force new"));
                footer.extend(keycap("^x", "close"));
                footer.extend(keycap("esc", "cancel"));
                frame.render_widget(
                    Paragraph::new(Line::from(truncate_spans(footer, width))),
                    vertical[4],
                );
            })?;
            let list_offset = list_state.offset();

            if !event::poll(EVENT_POLL)? {
                continue;
            }
            let key = match event::read()? {
                Event::Key(key) => key,
                Event::Mouse(mouse) => {
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Some(index) = mouse_row_index(
                                mouse.column,
                                mouse.row,
                                list_area,
                                list_offset,
                                &row_positions,
                            ) {
                                highlight_anchor = None;
                                state.selected = index;
                            }
                        }
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                            if mouse_in_area(mouse.column, mouse.row, list_area) =>
                        {
                            if let Some(index) =
                                mouse_scroll_target(state.selected, filtered.len(), mouse.kind)
                            {
                                highlight_anchor = None;
                                state.selected = index;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                _ => continue,
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            match key.code {
                KeyCode::Esc => break,
                KeyCode::Char('c') if control => break,
                KeyCode::Enter => {
                    if let Some(row) = state.selected_row(&filtered) {
                        outcome = Outcome::Jump(row);
                        break;
                    }
                }
                // Alt+1..9,0 jumps to the Nth open workspace: 1 is the
                // workspace the picker was opened from, 2 the one before that,
                // and 0 the tenth.
                KeyCode::Char(digit) if alt => {
                    if let Some(index) = alt_digit_index(digit) {
                        if let Some(row) = nth_open_jump(&mut state, &filtered, index) {
                            outcome = Outcome::Jump(row);
                            break;
                        }
                    }
                }
                KeyCode::Char('n') if control => {
                    if let Some(row) = state.selected_row(&filtered) {
                        // Force-new only means something for a directory row.
                        if matches!(row.kind, Kind::Open { .. } | Kind::Dormant) {
                            outcome = Outcome::ForceNew(row);
                            break;
                        }
                    }
                }
                KeyCode::Char('x') if control => {
                    if let Some(row) = state.selected_row(&filtered) {
                        if matches!(row.kind, Kind::Open { .. }) {
                            outcome = Outcome::Close(row);
                            break;
                        }
                    }
                }
                KeyCode::Up => {
                    let next = state.selected.saturating_sub(1);
                    if next != state.selected {
                        highlight_anchor = None;
                        state.selected = next;
                    }
                }
                KeyCode::Down => {
                    if state.selected + 1 < filtered.len() {
                        highlight_anchor = None;
                        state.selected += 1;
                    }
                }
                KeyCode::Tab => {
                    if highlight_anchor.is_none() {
                        highlight_anchor = state.selected_row(&filtered).map(|row| row.id());
                    }
                    state.next_section(&filtered);
                }
                KeyCode::Backspace => {
                    highlight_anchor = None;
                    state.query.pop();
                    state.selected = 0;
                }
                KeyCode::Char(character) if !control => {
                    // Some macOS terminals send the Option glyph (º¡™…) rather
                    // than ESC+digit; treat those as quick jumps too.
                    if let Some(index) = mac_option_digit(character) {
                        if let Some(row) = nth_open_jump(&mut state, &filtered, index) {
                            outcome = Outcome::Jump(row);
                            break;
                        }
                    }
                    highlight_anchor = None;
                    state.query.push(character);
                    state.selected = 0;
                }
                _ => {}
            }
        }

        Ok(Session {
            outcome,
            live_workspace_ids,
            state,
            origin_workspace,
        })
    })();
    match terminal_guard.restore() {
        Ok(()) => result,
        Err(error) if result.is_ok() => Err(error),
        Err(error) => {
            eprintln!("herdr-muster: restore terminal: {error}");
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RowId;
    use std::path::PathBuf;

    fn open(name: &str, display: &str, path: &str, pane_names: &[&str]) -> Row {
        Row {
            name: name.into(),
            path: PathBuf::from(path),
            display: display.into(),
            pane_names: pane_names.iter().map(|name| (*name).into()).collect(),
            tab_names: Vec::new(),
            space: Some(crate::model::SpaceContext {
                label: name.into(),
                disambiguator: None,
                number: None,
            }),
            kind: Kind::Open {
                workspace_id: name.into(),
                state: AgentState::Working,
                agent: Some("claude".into()),
            },
        }
    }

    fn open_with_tabs(
        name: &str,
        display: &str,
        path: &str,
        pane_names: &[&str],
        tab_names: &[&str],
    ) -> Row {
        let mut row = open(name, display, path, pane_names);
        row.tab_names = tab_names.iter().map(|name| (*name).into()).collect();
        row
    }

    fn project(name: &str, display: &str, path: &str) -> Row {
        Row {
            name: name.into(),
            path: PathBuf::from(path),
            display: display.into(),
            pane_names: Vec::new(),
            tab_names: Vec::new(),
            space: None,
            kind: Kind::Dormant,
        }
    }

    #[test]
    fn unnamed_pane_names_stay_searchable_on_the_workspace_row() {
        let rows = vec![
            open("workspace", "~/work", "/Users/me/work", &["shell"]),
            open_with_tabs(
                "labeled",
                "~/labeled",
                "/Users/me/labeled",
                &[],
                &["api server"],
            ),
        ];
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);

        // Unnamed panes have no row of their own, so the workspace row keeps
        // them searchable. Tab names are owned by tab rows instead.
        assert_eq!(filter(&rows, "shell", &mut matcher), vec![0]);
        assert!(filter(&rows, "api server", &mut matcher).is_empty());
    }

    #[test]
    fn open_row_shows_tab_names_only_when_present() {
        let labeled = open_with_tabs(
            "workspace",
            "~/work",
            "/Users/me/work",
            &["editor"],
            &["api server", "logs"],
        );
        let line = row_line(&labeled, 120, None).to_string();
        assert!(line.contains("workspace"), "{line}");
        assert!(line.contains("panes: editor"), "{line}");
        assert!(line.contains("tabs: api server · logs"), "{line}");

        let plain = open("workspace", "~/work", "/Users/me/work", &["editor"]);
        let line = row_line(&plain, 120, None).to_string();
        assert!(!line.contains("tabs:"), "{line}");
    }

    #[test]
    fn filtering_groups_sections_even_when_project_scores_higher() {
        let rows = vec![
            project("api", "~/api", "/Users/me/api"),
            open("workspace", "~/work", "/Users/me/work", &["api-dashboard"]),
        ];
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);

        assert_eq!(filter(&rows, "api", &mut matcher), vec![1, 0]);
        assert_eq!(filter(&rows, "", &mut matcher), vec![1, 0]);
    }

    #[test]
    fn searches_full_open_paths_and_open_only_metadata() {
        let rows = vec![
            open("web", "~/web", "/Users/me/web", &["backend"]),
            project("web", "~/web", "/Users/me/web"),
        ];
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);

        assert_eq!(filter(&rows, "/Users/me/web", &mut matcher), vec![0]);
        assert_eq!(filter(&rows, "backend", &mut matcher), vec![0]);
        assert_eq!(filter(&rows, "claude", &mut matcher), vec![0]);
        assert_eq!(filter(&rows, "working", &mut matcher), vec![0]);
    }

    #[test]
    fn fuzzy_matches_do_not_cross_metadata_fields() {
        let rows = vec![
            open(
                "instructional-design-agent",
                "~/Projects/00-in-progress/instructional-design-agent",
                "/Users/me/Projects/00-in-progress/instructional-design-agent",
                &[],
            ),
            project(
                "deadpotatodotcom",
                "~/Projects/00-in-progress/deadpotatodotcom",
                "/Users/me/Projects/00-in-progress/deadpotatodotcom",
            ),
        ];
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);

        assert_eq!(filter(&rows, "deadpo", &mut matcher), vec![1]);
    }

    #[test]
    fn snapshot_updates_keep_query_and_selection_identity() {
        let mut state = PickerState {
            rows: vec![
                open("one", "~/one", "/one", &[]),
                open("two", "~/two", "/two", &[]),
            ],
            query: "o".into(),
            selected: 1,
        };
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let snapshot = Snapshot {
            rows: vec![
                open("two", "~/two", "/two", &[]),
                open("one", "~/one", "/one", &[]),
            ],
            project_source_status: ProjectSourceStatus::Available,
            live_workspace_ids: Some(HashSet::new()),
            origin_workspace: None,
        };

        state.apply_snapshot(snapshot, &mut matcher);

        assert_eq!(state.query, "o");
        let filtered = state.filtered(&mut matcher);
        assert_eq!(
            state.selected_row(&filtered).unwrap().id(),
            RowId::Open("two".into())
        );
    }

    #[test]
    fn partial_refresh_retains_filtered_dormant_selection() {
        let mut state = PickerState {
            rows: vec![
                open("open", "~/open", "/open", &[]),
                project("dormant", "~/dormant", "/dormant"),
            ],
            query: "dorm".into(),
            selected: 0,
        };
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        state.apply_partial(
            Snapshot {
                rows: vec![open("open", "~/open", "/open", &[])],
                project_source_status: ProjectSourceStatus::Searching,
                live_workspace_ids: Some(HashSet::from(["open".into()])),

                origin_workspace: None,
            },
            &mut matcher,
        );

        let filtered = state.filtered(&mut matcher);
        assert_eq!(state.query, "dorm");
        assert_eq!(
            state.selected_row(&filtered).unwrap().id(),
            RowId::Project(PathBuf::from("/dormant"))
        );
    }

    #[test]
    fn row_rendering_never_exceeds_available_width() {
        let open_row = open(
            "api",
            "~/a/very/long/path",
            "/a/very/long/path",
            &["界界界界界界界界界界", "editor", "editor", "tests"],
        );
        let project_row = project("project", "~/a/very/long/project/path", "/project");

        for width in 0..80 {
            assert!(
                row_line(&open_row, width, Some(9)).width() <= width,
                "open width {width}"
            );
            assert!(
                row_line(&open_row, width, None).width() <= width,
                "open without number width {width}"
            );
            assert!(
                row_line(&project_row, width, None).width() <= width,
                "project width {width}"
            );
        }
    }

    #[test]
    fn open_rows_show_their_quick_jump_number() {
        let rows = vec![
            open("api", "~/api", "/api", &[]),
            project("dormant", "~/dormant", "/dormant"),
            open("web", "~/web", "/web", &[]),
        ];
        let state = PickerState {
            rows,
            query: String::new(),
            selected: 0,
        };
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let filtered = state.filtered(&mut matcher);
        assert_eq!(filtered, vec![0, 2, 1]);

        // Open rows are numbered 1..9, 0 by their position among open
        // matches; dormant rows get no number.
        let first = row_line(&state.rows[filtered[0]], 60, Some(1)).to_string();
        assert!(first.contains("1 "), "{first:?}");
        assert!(first.contains("api"));
        let second = row_line(&state.rows[filtered[1]], 60, Some(2)).to_string();
        assert!(second.contains("2 "), "{second:?}");
        assert!(second.contains("web"));
        let dormant = row_line(&state.rows[filtered[2]], 60, None).to_string();
        assert!(dormant.contains("dormant"));
        assert!(!dormant.contains("0 "), "{dormant:?}");
    }

    #[test]
    fn tenth_open_row_uses_zero_for_quick_jump_number() {
        let rows: Vec<_> = (0..10)
            .map(|index| open(&format!("project-{index}"), "~/project", "/project", &[]))
            .collect();
        let state = PickerState {
            rows,
            query: String::new(),
            selected: 0,
        };
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let filtered = state.filtered(&mut matcher);

        let first = row_line(&state.rows[filtered[0]], 60, Some(1)).to_string();
        let tenth = row_line(&state.rows[filtered[9]], 60, Some(0)).to_string();
        assert!(first.contains("1 "), "{first:?}");
        assert!(tenth.contains("0 "), "{tenth:?}");
    }

    #[test]
    fn truncation_is_grapheme_and_display_width_aware() {
        assert_eq!(display_width(&truncate_to_width("界界界", 5)), 5);
        assert_eq!(truncate_to_width("e\u{301}clair", 2), "e\u{301}…");
        assert_eq!(truncate_to_width("hello", 0), "");
    }

    #[test]
    fn display_deduplicates_names_and_reports_more() {
        let row = open(
            "api",
            "~/api",
            "/api",
            &["editor", "editor", "tests", "shell"],
        );
        assert_eq!(pane_summary(&row).as_deref(), Some("editor · tests +1"));
    }

    #[test]
    fn alt_digit_maps_to_row_indexes() {
        assert_eq!(alt_digit_index('1'), Some(0));
        assert_eq!(alt_digit_index('2'), Some(1));
        assert_eq!(alt_digit_index('9'), Some(8));
        assert_eq!(alt_digit_index('0'), Some(9));
        assert_eq!(alt_digit_index('a'), None);
    }

    #[test]
    fn mac_option_glyphs_map_to_row_indexes() {
        assert_eq!(mac_option_digit('¡'), Some(0));
        assert_eq!(mac_option_digit('™'), Some(1));
        assert_eq!(mac_option_digit('∞'), Some(4));
        assert_eq!(mac_option_digit('º'), Some(9));
        assert_eq!(mac_option_digit('x'), None);
    }

    #[test]
    fn nth_open_jump_skips_dormant_rows_and_selects() {
        let mut state = PickerState {
            rows: vec![
                open("three", "~/three", "/three", &[]),
                project("dormant", "~/dormant", "/dormant"),
                open("two", "~/two", "/two", &[]),
                project("dormant2", "~/dormant2", "/dormant2"),
                open("one", "~/one", "/one", &[]),
            ],
            query: String::new(),
            selected: 0,
        };
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let filtered = state.filtered(&mut matcher);
        assert_eq!(filtered, vec![0, 2, 4, 1, 3]);

        // The internal indexes skip dormant rows: 0 -> first open, 1 ->
        // second open, etc. `selected` tracks the filtered position of the
        // found row.
        assert_eq!(
            nth_open_jump(&mut state, &filtered, 0).unwrap().name,
            "three"
        );
        assert_eq!(state.selected, 0);
        assert_eq!(nth_open_jump(&mut state, &filtered, 1).unwrap().name, "two");
        assert_eq!(state.selected, 1);
        assert_eq!(nth_open_jump(&mut state, &filtered, 2).unwrap().name, "one");
        assert_eq!(state.selected, 2);
        assert!(nth_open_jump(&mut state, &filtered, 99).is_none());
    }

    fn tab_row(name: &str, workspace: &str, tab_id: &str, display: &str) -> Row {
        Row {
            name: name.into(),
            path: PathBuf::from("/Users/me/work"),
            display: display.into(),
            pane_names: Vec::new(),
            tab_names: Vec::new(),
            space: Some(crate::model::SpaceContext {
                label: workspace.into(),
                disambiguator: None,
                number: None,
            }),
            kind: Kind::Tab {
                workspace_id: workspace.into(),
                tab_id: tab_id.into(),
                state: AgentState::Working,
                agent: Some("claude".into()),
            },
        }
    }

    fn pane_row(name: &str, workspace: &str, pane_id: &str, display: &str) -> Row {
        Row {
            name: name.into(),
            path: PathBuf::from("/Users/me/work"),
            display: display.into(),
            pane_names: Vec::new(),
            tab_names: Vec::new(),
            space: Some(crate::model::SpaceContext {
                label: workspace.into(),
                disambiguator: None,
                number: None,
            }),
            kind: Kind::Pane {
                workspace_id: workspace.into(),
                tab_id: Some("w1:t1".into()),
                pane_id: pane_id.into(),
                state: AgentState::Idle,
                agent: None,
            },
        }
    }

    #[test]
    fn parent_space_labels_match_and_group_child_rows() {
        let mut first = tab_row(
            "Research: MTF bull-run rules audit",
            "w1",
            "w1:t1",
            "~/work · w1:t1",
        );
        first.space.as_mut().unwrap().label = "newsletter".into();
        let mut second = tab_row("Research: onset day1/day2", "w1", "w1:t2", "~/work · w1:t2");
        second.space.as_mut().unwrap().label = "newsletter".into();
        let mut third = tab_row("api logs", "w2", "w2:t1", "~/api · w2:t1");
        third.space.as_mut().unwrap().label = "pi-planning-profile".into();
        let rows = vec![
            open("workspace", "~/work", "/Users/me/work", &["shell"]),
            first,
            second,
            third,
        ];
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);

        assert_eq!(filter(&rows, "newsletter", &mut matcher), vec![1, 2]);
        let filtered = vec![0, 1, 2, 3];
        let (items, selected_position, _) = build(&rows, &filtered, 3, 100);
        let debug: Vec<String> = items.iter().map(|item| format!("{item:?}")).collect();
        assert!(debug
            .iter()
            .any(|item| item.contains("SPACE") && item.contains("newsletter")));
        assert!(debug
            .iter()
            .any(|item| item.contains("SPACE") && item.contains("pi-planning-profile")));
        assert_eq!(selected_position, 7);
    }

    #[test]
    fn tab_and_pane_rows_are_searchable_in_their_own_sections() {
        let rows = vec![
            open("workspace", "~/work", "/Users/me/work", &["shell"]),
            tab_row("api server", "w1", "w1:t2", "~/work · w1:t2"),
            pane_row("editor", "w1", "w1:p1", "~/work · w1:p1"),
            project("dormant", "~/dormant", "/Users/me/dormant"),
        ];
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);

        // Empty query keeps section order: workspaces, tabs, panes, projects.
        assert_eq!(filter(&rows, "", &mut matcher), vec![0, 1, 2, 3]);
        assert_eq!(filter(&rows, "api server", &mut matcher), vec![1]);
        assert_eq!(filter(&rows, "editor", &mut matcher), vec![2]);
        assert_eq!(filter(&rows, "w1:t2", &mut matcher), vec![1]);
        assert_eq!(filter(&rows, "w1:p1", &mut matcher), vec![2]);
        assert_eq!(filter(&rows, "dormant", &mut matcher), vec![3]);
    }

    #[test]
    fn tab_cycles_to_first_row_of_each_populated_section_and_wraps() {
        let rows = vec![
            open("workspace", "common", "/Users/me/work", &[]),
            tab_row("first tab", "w1", "w1:t1", "common"),
            tab_row("second tab", "w1", "w1:t2", "common"),
            pane_row("editor", "w1", "w1:p1", "common"),
            project("project", "common", "/Users/me/project"),
        ];
        let mut state = PickerState {
            rows,
            query: String::new(),
            selected: 2,
        };
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let filtered = state.filtered(&mut matcher);

        // From within TABS, jump to the first actionable PANES row.
        state.next_section(&filtered);
        assert_eq!(state.selected, 3);
        // Continue in section order, then wrap to OPEN and TABS.
        state.next_section(&filtered);
        assert_eq!(state.selected, 4);
        state.next_section(&filtered);
        assert_eq!(state.selected, 0);
        state.next_section(&filtered);
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn tab_skips_sections_removed_by_filtering_and_stays_put_if_alone() {
        let rows = vec![
            open("workspace", "common", "/Users/me/work", &[]),
            tab_row("api", "w1", "w1:t1", "unmatched"),
            pane_row("editor", "w1", "w1:p1", "unmatched"),
            project("project", "common", "/Users/me/project"),
        ];
        let mut state = PickerState {
            rows,
            query: "common".into(),
            selected: 0,
        };
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let filtered = state.filtered(&mut matcher);
        assert_eq!(filtered, vec![0, 3]);

        // TABS and PANES are empty in this filtered view, so jump directly to
        // PROJECTS rather than a section header or a non-matching row.
        state.next_section(&filtered);
        assert_eq!(state.selected, 1);
        state.next_section(&filtered);
        assert_eq!(state.selected, 0);

        state.query = "editor".into();
        state.selected = 0;
        let filtered = state.filtered(&mut matcher);
        assert_eq!(filtered, vec![2]);
        state.next_section(&filtered);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn tab_and_pane_rows_render_like_rows_without_quick_jump_numbers() {
        let tab = tab_row("api server", "w1", "w1:t2", "~/work · w1:t2");
        let text = row_line(&tab, 120, None).to_string();
        assert!(text.contains("api server"), "{text}");
        assert!(text.contains("w1:t2"), "{text}");
        assert!(!text.starts_with("1 "), "{text}");

        let pane = pane_row("editor", "w1", "w1:p1", "~/work · w1:p1");
        let text = row_line(&pane, 120, None).to_string();
        assert!(text.contains("editor"), "{text}");
        assert!(text.contains("w1:p1"), "{text}");
    }

    #[test]
    fn selected_workspace_marks_related_rows_across_sections() {
        let rows = vec![
            open("w1", "~/work", "/work", &[]),
            tab_row("api", "w1", "w1:t1", "~/work · w1:t1"),
            pane_row("editor", "w1", "w1:p1", "~/work · w1:p1"),
            project("work", "~/work", "/work"),
            project("other", "~/other", "/other"),
        ];
        assert!(is_related(&rows[0], &rows[1]));
        assert!(is_related(&rows[0], &rows[2]));
        assert!(is_related(&rows[0], &rows[3]));
        assert!(!is_related(&rows[0], &rows[4]));

        let linked = row_line_with_relation(&rows[1], 60, None, true, false).to_string();
        assert!(linked.contains("↳"), "{linked}");

        let anchor_id = rows[0].id();
        let filtered: Vec<usize> = (0..rows.len()).collect();
        let (items, selected_position, _) =
            build_with_anchor(&rows, &filtered, 1, 60, Some(&anchor_id));
        assert_eq!(selected_position, 3);
        assert!(format!("{:?}", items[1]).contains("●"));
        assert!(format!("{:?}", items[3]).contains("↳"));
    }

    #[test]
    fn build_adds_headers_for_every_section() {
        let rows = vec![
            open("workspace", "~/work", "/Users/me/work", &["shell"]),
            tab_row("api server", "w1", "w1:t2", "~/work · w1:t2"),
            pane_row("editor", "w1", "w1:p1", "~/work · w1:p1"),
            project("dormant", "~/dormant", "/Users/me/dormant"),
        ];
        let filtered: Vec<usize> = (0..rows.len()).collect();
        let (items, selected_position, row_positions) = build(&rows, &filtered, 0, 100);

        // Four sections, so four headers plus one item per row.
        assert_eq!(items.len(), rows.len() + 4);
        assert_eq!(selected_position, 1);
        assert_eq!(
            row_positions,
            vec![None, Some(0), None, Some(1), None, Some(2), None, Some(3)]
        );
        let debug: Vec<String> = items.iter().map(|item| format!("{item:?}")).collect();
        assert!(debug[0].contains("OPEN"), "{}", debug[0]);
        assert!(debug[2].contains("TABS"), "{}", debug[2]);
        assert!(debug[4].contains("PANES"), "{}", debug[4]);
        assert!(debug[6].contains("PROJECTS"), "{}", debug[6]);
    }

    #[test]
    fn mouse_click_maps_visible_list_rows_and_ignores_headers_and_bounds() {
        let area = Rect::new(10, 5, 20, 3);
        let row_positions = [None, Some(0), None, Some(1), Some(2)];

        assert_eq!(mouse_row_index(10, 5, area, 1, &row_positions), Some(0));
        assert_eq!(mouse_row_index(10, 6, area, 1, &row_positions), None);
        assert_eq!(mouse_row_index(10, 7, area, 1, &row_positions), Some(1));
        assert_eq!(mouse_row_index(30, 5, area, 1, &row_positions), None);
        assert_eq!(mouse_row_index(10, 8, area, 1, &row_positions), None);
        assert!(!mouse_in_area(30, 5, area));
        assert!(!mouse_in_area(10, 8, area));
    }

    #[test]
    fn mouse_scroll_moves_selection_within_filtered_rows() {
        assert_eq!(mouse_scroll_target(1, 3, MouseEventKind::ScrollUp), Some(0));
        assert_eq!(
            mouse_scroll_target(1, 3, MouseEventKind::ScrollDown),
            Some(2)
        );
        assert_eq!(mouse_scroll_target(0, 3, MouseEventKind::ScrollUp), None);
        assert_eq!(mouse_scroll_target(2, 3, MouseEventKind::ScrollDown), None);
        assert_eq!(mouse_scroll_target(0, 0, MouseEventKind::ScrollDown), None);
    }

    #[test]
    fn open_workspace_paths_excludes_tabs_panes_and_dormant_projects() {
        let state = PickerState {
            rows: vec![
                open("w1", "~/work", "/work", &[]),
                tab_row("api", "w1", "w1:t1", "~/work"),
                pane_row("editor", "w1", "w1:p1", "~/work"),
                project("other", "~/other", "/other"),
            ],
            query: String::new(),
            selected: 0,
        };

        assert_eq!(
            state.open_workspace_paths().collect::<Vec<_>>(),
            vec![std::path::Path::new("/work")]
        );
    }

    #[test]
    fn enter_action_distinguishes_focus_from_opening_a_project() {
        let workspace = open("w1", "~/work", "/work", &[]);
        let tab = tab_row("api", "w1", "w1:t1", "~/work");
        let pane = pane_row("editor", "w1", "w1:p1", "~/work");
        let project = project("other", "~/other", "/other");

        assert_eq!(enter_action(&workspace.kind), "focus");
        assert_eq!(enter_action(&tab.kind), "focus");
        assert_eq!(enter_action(&pane.kind), "focus");
        assert_eq!(enter_action(&project.kind), "open project");
    }

    #[test]
    fn empty_picker_messages_explain_project_discovery_state() {
        let searching = format!(
            "{:?}",
            empty_item(true, true, ProjectSourceStatus::Searching, None, "", 80)
        );
        assert!(searching.contains("Searching projects"), "{searching}");

        let unavailable = format!(
            "{:?}",
            empty_item(false, false, ProjectSourceStatus::Unavailable, None, "", 80)
        );
        assert!(
            unavailable.contains("zoxide suggestions unavailable"),
            "{unavailable}"
        );

        let no_matches = format!(
            "{:?}",
            empty_item(
                false,
                false,
                ProjectSourceStatus::Available,
                None,
                "missing",
                80
            )
        );
        assert!(
            no_matches.contains("No matching workspaces or projects"),
            "{no_matches}"
        );
    }

    #[test]
    fn alt_digit_quick_jump_ignores_tab_and_pane_rows() {
        let mut state = PickerState {
            rows: vec![
                open("one", "~/one", "/one", &[]),
                tab_row("api server", "w1", "w1:t1", "~/one · w1:t1"),
                pane_row("editor", "w1", "w1:p1", "~/one · w1:p1"),
                open("two", "~/two", "/two", &[]),
            ],
            query: String::new(),
            selected: 0,
        };
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let filtered = state.filtered(&mut matcher);

        assert_eq!(filtered, vec![0, 3, 1, 2]);
        assert_eq!(nth_open_jump(&mut state, &filtered, 0).unwrap().name, "one");
        assert_eq!(nth_open_jump(&mut state, &filtered, 1).unwrap().name, "two");
        assert!(nth_open_jump(&mut state, &filtered, 2).is_none());
    }
}

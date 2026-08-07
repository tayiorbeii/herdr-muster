use crate::model::{AgentState, Kind, Row};
use crate::refresh::{Message as RefreshMessage, Snapshot, Updates};
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::{execute, terminal};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
use std::collections::HashSet;
use std::io::{self, stdout};
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

// --- tokyo-night palette ---
const AMBER: Color = Color::Rgb(0xe0, 0xaf, 0x68);
const FG: Color = Color::Rgb(0xc0, 0xca, 0xf5);
const MUTED: Color = Color::Rgb(0x56, 0x5f, 0x89);
const FAINT: Color = Color::Rgb(0x3b, 0x42, 0x61);
const SEL_BG: Color = Color::Rgb(0x2a, 0x27, 0x1c);
const RED: Color = Color::Rgb(0xf7, 0x76, 0x8e);
const CYAN: Color = Color::Rgb(0x7d, 0xcf, 0xff);
const GREEN: Color = Color::Rgb(0x9e, 0xce, 0x6a);

const NAME_W: usize = 20;
const GLYPH_W: usize = 2;
const HL_W: usize = 2;
const EVENT_POLL: Duration = Duration::from_millis(50);

struct TerminalGuard {
    raw_mode: bool,
    alternate_screen: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut guard = TerminalGuard {
            raw_mode: false,
            alternate_screen: false,
        };
        terminal::enable_raw_mode()?;
        guard.raw_mode = true;
        if let Err(error) = execute!(stdout(), terminal::EnterAlternateScreen) {
            let _ = guard.restore();
            return Err(error);
        }
        guard.alternate_screen = true;
        if let Err(error) = execute!(stdout(), cursor::Hide) {
            let _ = guard.restore();
            return Err(error);
        }
        Ok(guard)
    }

    fn restore(&mut self) -> io::Result<()> {
        let mut failure = None;
        if self.alternate_screen {
            if let Err(error) = execute!(stdout(), cursor::Show, terminal::LeaveAlternateScreen) {
                failure = Some(error);
            }
            self.alternate_screen = false;
        } else if let Err(error) = execute!(stdout(), cursor::Show) {
            failure = Some(error);
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
}

/// Map an Alt+digit key to a row index, where 0 is the most recently used
/// open workspace and 9 the tenth. Returns `None` for non-digit keys.
fn alt_digit_index(digit: char) -> Option<usize> {
    digit.to_digit(10).map(|digit| digit as usize)
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
        .and_then(|(_, digit)| digit.to_digit(10).map(|digit| digit as usize))
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
        AgentState::Working => CYAN,
        AgentState::Done => GREEN,
        AgentState::Idle | AgentState::Unknown => MUTED,
    }
}

struct SearchDocument(Vec<String>);

impl SearchDocument {
    fn for_row(row: &Row) -> Self {
        let mut fields = Vec::new();
        push_unique(&mut fields, row.name.clone());
        push_unique(&mut fields, row.display.clone());

        if let Kind::Open { state, agent, .. } = &row.kind {
            push_unique(&mut fields, row.path.display().to_string());
            push_unique(&mut fields, state.word().to_string());
            if let Some(agent) = agent {
                push_unique(&mut fields, agent.clone());
            }
            for pane_name in &row.pane_names {
                push_unique(&mut fields, pane_name.clone());
            }
        }

        SearchDocument(fields)
    }
}

fn push_unique(fields: &mut Vec<String>, value: String) {
    if !value.is_empty() && !fields.iter().any(|existing| existing == &value) {
        fields.push(value);
    }
}

/// Return original row indices, ranked within each section. Open rows always
/// precede project rows, including when the query is empty.
fn filter(rows: &[Row], query: &str, matcher: &mut Matcher) -> Vec<usize> {
    if query.is_empty() {
        let (open, projects): (Vec<_>, Vec<_>) =
            (0..rows.len()).partition(|index| matches!(rows[*index].kind, Kind::Open { .. }));
        return open.into_iter().chain(projects).collect();
    }

    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buffer = Vec::new();
    let mut open = Vec::new();
    let mut projects = Vec::new();

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
        if matches!(row.kind, Kind::Open { .. }) {
            open.push((score, index));
        } else {
            projects.push((score, index));
        }
    }

    let rank = |a: &(u32, usize), b: &(u32, usize)| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1));
    open.sort_by(rank);
    projects.sort_by(rank);
    open.into_iter()
        .chain(projects)
        .map(|(_, index)| index)
        .collect()
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

fn pane_label(row: &Row) -> String {
    let mut seen = HashSet::new();
    let unique: Vec<&str> = row
        .pane_names
        .iter()
        .map(String::as_str)
        .filter(|name| seen.insert(*name))
        .collect();
    match unique.as_slice() {
        [] => row.name.clone(),
        [name] => (*name).to_string(),
        [first, second] => format!("{first} · {second}"),
        [first, second, rest @ ..] => format!("{first} · {second} +{}", rest.len()),
    }
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
    vec![
        Span::styled(pad_to_width(name, name_budget), name_style),
        Span::raw(" "),
        Span::styled(truncate_to_width(path, path_budget), path_style),
    ]
}

/// One row constrained to `width` terminal display columns. `number` is the
/// Alt+digit quick-jump index shown in front of open rows; dormant rows pass
/// `None`.
fn row_line(row: &Row, width: usize, number: Option<usize>) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }

    match &row.kind {
        Kind::Open { state, agent, .. } => {
            let number_prefix = number.map_or(String::new(), |digit| format!("{digit} "));
            let number_width = display_width(&number_prefix);
            let color = state_color(*state);
            let glyph = truncate_to_width(&format!("{} ", state.glyph()), width.min(GLYPH_W));
            let glyph_width = display_width(&glyph);
            let remaining = width.saturating_sub(number_width + glyph_width);

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
                &pane_label(row),
                &row.display,
                body_budget,
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
                Style::default().fg(MUTED),
            );
            let body_width = spans_width(&body);
            if body_width < body_budget {
                body.push(Span::raw(" ".repeat(body_budget - body_width)));
            }

            let mut spans = Vec::new();
            if !number_prefix.is_empty() {
                spans.push(Span::styled(
                    number_prefix,
                    Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(glyph, Style::default().fg(color)));
            spans.extend(body);
            if meta_gap > 0 {
                spans.push(Span::raw(" "));
            }
            if !meta.is_empty() {
                spans.push(Span::styled(meta, Style::default().fg(color)));
            }
            Line::from(truncate_spans(spans, width))
        }
        Kind::Dormant => {
            let glyph = " ".repeat(width.min(GLYPH_W));
            let body_budget = width.saturating_sub(display_width(&glyph));
            let mut spans = vec![Span::raw(glyph)];
            spans.extend(body_spans(
                &row.name,
                &row.display,
                body_budget,
                Style::default().fg(FG),
                Style::default().fg(FAINT),
            ));
            Line::from(truncate_spans(spans, width))
        }
    }
}

fn header_item(label: &str, suffix: &str, width: usize) -> ListItem<'static> {
    let spans = vec![
        Span::styled(
            format!("▸ {label} "),
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("— {suffix}"), Style::default().fg(FAINT)),
    ];
    ListItem::new(Line::from(truncate_spans(spans, width)))
}

fn build(
    rows: &[Row],
    filtered: &[usize],
    selected: usize,
    width: usize,
) -> (Vec<ListItem<'static>>, usize) {
    let mut items = Vec::new();
    let mut selected_position = 0;
    let mut last_group = None;
    let mut open_index = 0usize;
    for (filtered_index, row_index) in filtered.iter().copied().enumerate() {
        let row = &rows[row_index];
        let group = if matches!(row.kind, Kind::Open { .. }) {
            0u8
        } else {
            1u8
        };
        if last_group != Some(group) {
            items.push(header_item(
                if group == 0 { "OPEN" } else { "PROJECTS" },
                if group == 0 {
                    "LIVE WORKSPACES"
                } else {
                    "NOT OPEN YET"
                },
                width,
            ));
            last_group = Some(group);
        }
        if filtered_index == selected {
            selected_position = items.len();
        }
        // Open rows carry their Alt+digit quick-jump number (0-9) up front.
        let number = if group == 0 {
            let number = open_index;
            open_index += 1;
            (number <= 9).then_some(number)
        } else {
            None
        };
        items.push(ListItem::new(row_line(row, width, number)));
    }
    (items, selected_position)
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

fn keycap(key: &str, label: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!(" {key} "),
            Style::default().fg(Color::Black).bg(MUTED),
        ),
        Span::styled(format!(" {label}   "), Style::default().fg(MUTED)),
    ]
}

fn empty_item(loading: bool, error: Option<&str>, query: &str, width: usize) -> ListItem<'static> {
    let (message, color) = if loading {
        (
            if query.is_empty() {
                "Loading workspaces and projects…"
            } else {
                "No matches yet — still loading…"
            },
            MUTED,
        )
    } else if let Some(error) = error {
        (error, RED)
    } else if query.is_empty() {
        ("No projects configured", MUTED)
    } else {
        ("No matches", MUTED)
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
    let mut refresh_error: Option<String> = None;
    let mut live_workspace_ids = None;
    let mut origin_workspace = None;
    let mut channel_open = true;
    let mut outcome = Outcome::Cancel;

    let result = (|| -> io::Result<Session> {
        loop {
            while channel_open {
                match updates.try_recv() {
                    Ok(RefreshMessage::Partial(snapshot)) => {
                        live_workspace_ids = snapshot.live_workspace_ids.clone();
                        origin_workspace = snapshot.origin_workspace.clone();
                        state.apply_partial(snapshot, &mut matcher);
                        loading = true;
                        refresh_error = None;
                    }
                    Ok(RefreshMessage::Ready(snapshot)) => {
                        live_workspace_ids = snapshot.live_workspace_ids.clone();
                        origin_workspace = snapshot.origin_workspace.clone();
                        state.apply_snapshot(snapshot, &mut matcher);
                        loading = false;
                        refresh_error = None;
                    }
                    Ok(RefreshMessage::Failed(error)) => {
                        loading = false;
                        refresh_error = Some(format!("Refresh failed: {error}"));
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        channel_open = false;
                        if loading {
                            loading = false;
                            refresh_error = Some("Refresh stopped before completion".into());
                        }
                    }
                }
            }

            let filtered = state.filtered(&mut matcher);
            if state.selected >= filtered.len() {
                state.selected = filtered.len().saturating_sub(1);
            }
            let open_count = state
                .rows
                .iter()
                .filter(|row| matches!(row.kind, Kind::Open { .. }))
                .count();
            let dormant_count = state.rows.len() - open_count;

            terminal.draw(|frame| {
                let area = frame.area();
                let title_left = Line::from(vec![Span::styled(
                    " one terminal for the whole herd ",
                    Style::default().fg(MUTED),
                )])
                .left_aligned();
                let mut title_spans = vec![
                    Span::styled(format!(" {open_count} open"), Style::default().fg(GREEN)),
                    Span::styled(" · ", Style::default().fg(FAINT)),
                    Span::styled(format!("{dormant_count} idle"), Style::default().fg(MUTED)),
                ];
                if loading {
                    title_spans.push(Span::styled(" · loading ", Style::default().fg(AMBER)));
                } else if refresh_error.is_some() {
                    title_spans.push(Span::styled(" · refresh failed ", Style::default().fg(RED)));
                } else {
                    title_spans.push(Span::raw(" "));
                }
                let title_right = Line::from(title_spans).right_aligned();
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(FAINT))
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
                            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
                        ),
                        query_span,
                    ],
                    vec![Span::styled(
                        format!("{} matches", filtered.len()),
                        Style::default().fg(MUTED),
                    )],
                    width,
                );
                frame.render_widget(Paragraph::new(prompt), vertical[0]);

                let rule = "─".repeat(width);
                let rule_style = Style::default().fg(FAINT);
                frame.render_widget(Paragraph::new(rule.clone()).style(rule_style), vertical[1]);
                frame.render_widget(Paragraph::new(rule).style(rule_style), vertical[3]);

                let list_width = width.saturating_sub(HL_W);
                let (items, selected_position) = if filtered.is_empty() {
                    (
                        vec![empty_item(
                            loading,
                            refresh_error.as_deref(),
                            &state.query,
                            list_width,
                        )],
                        0,
                    )
                } else {
                    build(&state.rows, &filtered, state.selected, list_width)
                };
                let mut list_state = ListState::default();
                if !filtered.is_empty() {
                    list_state.select(Some(selected_position));
                }
                let list = List::new(items)
                    .highlight_style(Style::default().bg(SEL_BG))
                    .highlight_symbol("▌ ");
                frame.render_stateful_widget(list, vertical[2], &mut list_state);

                let mut footer = Vec::new();
                footer.extend(keycap("↵", "jump/create"));
                footer.extend(keycap("⌥0-9", "recents"));
                footer.extend(keycap("^n", "force new"));
                footer.extend(keycap("^x", "close"));
                footer.extend(keycap("esc", "cancel"));
                frame.render_widget(
                    Paragraph::new(Line::from(truncate_spans(footer, width))),
                    vertical[4],
                );
            })?;

            if !event::poll(EVENT_POLL)? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
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
                // Alt+0..9 jumps to the Nth open workspace: 0 is the workspace
                // the picker was opened from, 1 the one before that, etc.
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
                        outcome = Outcome::ForceNew(row);
                        break;
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
                KeyCode::Up => state.selected = state.selected.saturating_sub(1),
                KeyCode::Down => {
                    if state.selected + 1 < filtered.len() {
                        state.selected += 1;
                    }
                }
                KeyCode::Backspace => {
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
            kind: Kind::Open {
                workspace_id: name.into(),
                state: AgentState::Working,
                agent: Some("claude".into()),
            },
        }
    }

    fn project(name: &str, display: &str, path: &str) -> Row {
        Row {
            name: name.into(),
            path: PathBuf::from(path),
            display: display.into(),
            pane_names: Vec::new(),
            kind: Kind::Dormant,
        }
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

        // Open rows are numbered 0.. by their position among open matches;
        // dormant rows get no number.
        let first = row_line(&state.rows[filtered[0]], 60, Some(0)).to_string();
        assert!(first.contains("0 "), "{first:?}");
        assert!(first.contains("api"));
        let second = row_line(&state.rows[filtered[1]], 60, Some(1)).to_string();
        assert!(second.contains("1 "), "{second:?}");
        assert!(second.contains("web"));
        let dormant = row_line(&state.rows[filtered[2]], 60, None).to_string();
        assert!(dormant.contains("dormant"));
        assert!(!dormant.contains("0 "), "{dormant:?}");
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
        assert_eq!(pane_label(&row), "editor · tests +1");
    }

    #[test]
    fn alt_digit_maps_to_row_indexes() {
        assert_eq!(alt_digit_index('0'), Some(0));
        assert_eq!(alt_digit_index('1'), Some(1));
        assert_eq!(alt_digit_index('9'), Some(9));
        assert_eq!(alt_digit_index('a'), None);
    }

    #[test]
    fn mac_option_glyphs_map_to_row_indexes() {
        assert_eq!(mac_option_digit('º'), Some(0));
        assert_eq!(mac_option_digit('™'), Some(2));
        assert_eq!(mac_option_digit('∞'), Some(5));
        assert_eq!(mac_option_digit('ª'), Some(9));
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

        // Numbers skip dormant rows: 0 -> first open, 1 -> second open, etc.
        // `selected` tracks the filtered position of the found row.
        assert_eq!(nth_open_jump(&mut state, &filtered, 0).unwrap().name, "three");
        assert_eq!(state.selected, 0);
        assert_eq!(nth_open_jump(&mut state, &filtered, 1).unwrap().name, "two");
        assert_eq!(state.selected, 1);
        assert_eq!(nth_open_jump(&mut state, &filtered, 2).unwrap().name, "one");
        assert_eq!(state.selected, 2);
        assert!(nth_open_jump(&mut state, &filtered, 99).is_none());
    }
}

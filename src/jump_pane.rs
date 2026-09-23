//! Dedicated Jump Pane mode. No project discovery, workspace creation, binding,
//! directional focus, or canonicalization is reachable from this module.
use crate::config::{JumpPaneConfig, JumpPaneFilter, JumpPaneSession};
use crate::herdr::{Herdr, Pane, Result};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as NucleoConfig, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

const MAX_PANE_RECENCY: usize = 100;
const RECENCY_VERSION: u8 = 1;
const MAX_RENDER_ROWS: usize = 64;
const MAX_ENRICH_CANDIDATES: usize = 32;
const MAX_ENRICH_ACTIVE: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct PaneIdentity {
    pub runtime_id: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTarget {
    pub id: String,
    pub name: String,
    /// Sanitized display/search context for a remote runtime, when configured.
    pub remote_host: Option<String>,
    pub socket: Option<String>,
    /// Executable path, not a shell command.
    pub command: String,
    /// Argument prefix used for every list/process invocation.
    pub arg_prefix: Vec<String>,
    /// False for configured targets without an explicit focus socket.
    pub focus_supported: bool,
    pub filter: JumpPaneFilter,
}

impl RuntimeTarget {
    pub fn default(bin: String) -> Self {
        Self {
            id: "default".into(),
            name: "default".into(),
            remote_host: None,
            socket: None,
            command: bin,
            arg_prefix: Vec::new(),
            focus_supported: true,
            filter: JumpPaneFilter::default(),
        }
    }
    fn from_session(session: &JumpPaneSession, default_bin: &str, index: usize) -> Self {
        let name = if session.name.trim().is_empty() {
            format!("session-{index}")
        } else {
            sanitize(&session.name)
        };
        let mut args = session.args.clone();
        if let Some(remote) = session.remote.as_deref().filter(|v| !v.trim().is_empty()) {
            args.extend(["--remote".into(), remote.into()]);
        }
        if !session.name.trim().is_empty() {
            args.extend(["--session".into(), session.name.clone()]);
        }
        Self {
            id: format!("session:{name}:{index}"),
            name,
            remote_host: session.remote.as_deref().map(sanitize),
            socket: session.socket.as_deref().map(|s| {
                crate::config::expand_tilde(s)
                    .to_string_lossy()
                    .into_owned()
            }),
            command: session
                .command
                .clone()
                .unwrap_or_else(|| default_bin.into()),
            arg_prefix: args,
            focus_supported: session
                .socket
                .as_deref()
                .is_some_and(|socket| !socket.trim().is_empty()),
            filter: session.filter.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PaneEntry {
    pub identity: PaneIdentity,
    pub target: RuntimeTarget,
    pub workspace_id: String,
    pub tab_id: Option<String>,
    /// Human-readable label of the parent tab, when the runtime can provide it.
    /// Presentation/search metadata only: identity stays (runtime_id, pane_id).
    pub tab_label: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub label: Option<String>,
    pub title: Option<String>,
    pub terminal_title: Option<String>,
    pub agent: Option<String>,
    pub agent_status: Option<String>,
    pub focused: bool,
    pub application: Option<String>,
    pub hidden: bool,
    pub plugin: bool,
    pub floating: bool,
    pub suppressed: bool,
    /// Rows from a runtime whose backend failed remain visible but disabled so
    /// the user can see why a previously discovered pane cannot be activated.
    pub runtime_available: bool,
    /// Configured targets without an exact focus socket are visible but disabled.
    pub focus_supported: bool,
}

impl PaneEntry {
    pub fn from_pane(mut target: RuntimeTarget, pane: Pane) -> Self {
        target.remote_host = target.remote_host.take().map(|host| sanitize(&host));
        let focus_supported = target.focus_supported;
        Self {
            identity: PaneIdentity {
                runtime_id: target.id.clone(),
                pane_id: pane.pane_id,
            },
            target,
            workspace_id: sanitize(&pane.workspace_id),
            tab_id: pane.tab_id.map(|s| sanitize(&s)),
            tab_label: None,
            cwd: pane.cwd.map(|s| sanitize(&s)),
            foreground_cwd: pane.foreground_cwd.map(|s| sanitize(&s)),
            label: pane.label.map(|s| sanitize(&s)),
            title: pane.title.map(|s| sanitize(&s)),
            terminal_title: pane.terminal_title.map(|s| sanitize(&s)),
            agent: pane.agent.map(|s| sanitize(&s)),
            agent_status: pane.agent_status.map(|s| sanitize(&s)),
            focused: pane.focused,
            application: None,
            hidden: pane.hidden,
            plugin: pane.plugin,
            floating: pane.floating,
            suppressed: pane.suppressed,
            runtime_available: true,
            focus_supported,
        }
    }
    /// Attach the parent tab's display label. Additive builder so every existing
    /// `from_pane` call site keeps its arity.
    pub fn with_tab_label(mut self, label: Option<String>) -> Self {
        self.tab_label = label;
        self
    }
    pub fn path(&self) -> Option<&str> {
        self.foreground_cwd.as_deref().or(self.cwd.as_deref())
    }
    pub fn search_fields(&self) -> Vec<&str> {
        let mut fields = Vec::new();
        for value in [
            self.identity.pane_id.as_str(),
            self.workspace_id.as_str(),
            self.tab_id.as_deref().unwrap_or(""),
            self.tab_label.as_deref().unwrap_or(""),
            self.cwd.as_deref().unwrap_or(""),
            self.foreground_cwd.as_deref().unwrap_or(""),
            self.label.as_deref().unwrap_or(""),
            self.title.as_deref().unwrap_or(""),
            self.terminal_title.as_deref().unwrap_or(""),
            self.agent.as_deref().unwrap_or(""),
            self.agent_status.as_deref().unwrap_or(""),
            self.application.as_deref().unwrap_or(""),
            self.target.name.as_str(),
            self.target.remote_host.as_deref().unwrap_or(""),
            if self.focused { "focused" } else { "" },
        ] {
            if !value.is_empty() && !fields.contains(&value) {
                fields.push(value);
            }
        }
        fields
    }
    pub fn primary_display(&self) -> String {
        let base = self
            .label
            .clone()
            .or_else(|| self.title.clone())
            .or_else(|| self.terminal_title.clone())
            .or_else(|| self.path().and_then(path_basename))
            .unwrap_or_else(|| short_id(&self.identity.pane_id));
        // Always append the short identity: equal labels/paths must never be
        // indistinguishable, even when no runtime collision exists.
        format!("{} [{}]", base, short_id(&self.identity.pane_id))
    }
    pub fn secondary_display(&self) -> String {
        let path = self.path().map(collapse_path).unwrap_or_default();
        let mut context = format!("{} · {}", self.target.name, self.workspace_id);
        if let Some(host) = &self.target.remote_host {
            context.push_str(" @ ");
            context.push_str(host);
        }
        if let Some(tab) = self.tab_label.as_deref().or(self.tab_id.as_deref()) {
            context.push_str(" / ");
            context.push_str(tab);
        }
        if let Some(agent) = &self.agent {
            context.push_str(" · ");
            context.push_str(agent);
        }
        if let Some(status) = &self.agent_status {
            context.push_str(" · ");
            context.push_str(status);
        }
        if let Some(application) = &self.application {
            context.push_str(" · ");
            context.push_str(application);
        }
        if path.is_empty() {
            context
        } else {
            format!("{path} · {context}")
        }
    }
}

fn sanitize(value: &str) -> String {
    crate::herdr::sanitize_text(value).trim().to_string()
}
fn short_id(value: &str) -> String {
    let value = sanitize(value);
    let chars: Vec<_> = value.chars().collect();
    chars[chars.len().saturating_sub(8)..].iter().collect()
}
fn path_basename(value: &str) -> Option<String> {
    value
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .map(sanitize)
        .filter(|s| !s.is_empty())
}
fn collapse_path(value: &str) -> String {
    let home = std::env::var("HOME").ok();
    match home.as_deref() {
        Some(home) if value == home => "~".into(),
        Some(home) if value.starts_with(&(home.to_string() + "/")) => {
            format!("~{}", &value[home.len()..])
        }
        _ => value.into(),
    }
}

#[derive(Debug, Default, Clone)]
pub struct PaneRecency(pub Vec<PaneIdentity>);
impl PaneRecency {
    pub fn touch(&mut self, identity: PaneIdentity) {
        self.0.retain(|current| current != &identity);
        self.0.insert(0, identity);
        self.0.truncate(MAX_PANE_RECENCY);
    }
}
#[derive(Debug, Serialize, Deserialize)]
struct RecencyFile {
    version: u8,
    identities: Vec<PaneIdentity>,
}
pub fn load_recency(path: &std::path::Path) -> PaneRecency {
    let Ok(text) = std::fs::read_to_string(path) else {
        return PaneRecency::default();
    };
    let Ok(file) = serde_json::from_str::<RecencyFile>(&text) else {
        return PaneRecency::default();
    };
    if file.version != RECENCY_VERSION {
        return PaneRecency::default();
    }
    let mut out = PaneRecency::default();
    for identity in file.identities.into_iter().take(MAX_PANE_RECENCY) {
        if !identity.runtime_id.is_empty() && !identity.pane_id.is_empty() {
            out.touch(identity);
        }
    }
    // The file stores newest first; touch above reverses it, so restore order.
    out.0.reverse();
    out
}
pub fn save_recency(path: &std::path::Path, recency: &PaneRecency) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = RecencyFile {
        version: RECENCY_VERSION,
        identities: recency.0.iter().take(MAX_PANE_RECENCY).cloned().collect(),
    };
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec(&file).expect("recency serialization cannot fail"),
    )?;
    std::fs::rename(tmp, path)
}

#[derive(Debug, Default, Clone)]
pub struct JumpPaneState {
    pub entries: Vec<PaneEntry>,
    pub query: String,
    pub selected: usize,
    pub recency: Vec<PaneIdentity>,
    pub current_context: Option<(String, Option<String>)>,
}
impl JumpPaneState {
    pub fn filtered(&self) -> Vec<usize> {
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        filter(
            &self.entries,
            &self.query,
            &mut matcher,
            &self.recency,
            self.current_context
                .as_ref()
                .map(|(w, t)| (w.as_str(), t.as_deref())),
        )
    }
    pub fn replace(&mut self, mut entries: Vec<PaneEntry>) {
        let selected_id = self
            .filtered()
            .get(self.selected)
            .map(|i| self.entries[*i].identity.clone());
        entries.sort_by(|a, b| a.identity.cmp(&b.identity));
        self.entries = entries;
        let filtered = self.filtered();
        self.selected = selected_id
            .and_then(|id| {
                filtered
                    .iter()
                    .position(|i| self.entries[*i].identity == id)
            })
            .unwrap_or(0)
            .min(filtered.len().saturating_sub(1));
    }
    #[allow(dead_code)]
    pub fn selected(&self) -> Option<&PaneEntry> {
        self.filtered()
            .get(self.selected)
            .and_then(|i| self.entries.get(*i))
    }
}

fn score_field(
    pattern: &Pattern,
    value: &str,
    matcher: &mut Matcher,
    buffer: &mut Vec<char>,
) -> Option<u32> {
    pattern.score(Utf32Str::new(value, buffer), matcher)
}
pub fn filter(
    entries: &[PaneEntry],
    query: &str,
    matcher: &mut Matcher,
    recency: &[PaneIdentity],
    current: Option<(&str, Option<&str>)>,
) -> Vec<usize> {
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut scored = Vec::new();
    let mut buffer = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let fields = entry.search_fields();
        let exact = !query.is_empty() && fields.iter().any(|f| f.eq_ignore_ascii_case(query));
        let foreground_exact = !query.is_empty()
            && entry
                .foreground_cwd
                .as_deref()
                .is_some_and(|f| f.eq_ignore_ascii_case(query));
        let foreground_fuzzy_score = if query.is_empty() {
            None
        } else {
            entry
                .foreground_cwd
                .as_deref()
                .and_then(|f| score_field(&pattern, f, matcher, &mut buffer))
        };
        let foreground_match = foreground_fuzzy_score.is_some();
        let foreground_fuzzy = foreground_fuzzy_score.unwrap_or(0);
        let fuzzy = if query.is_empty() {
            Some(0)
        } else {
            fields
                .iter()
                .filter_map(|f| score_field(&pattern, f, matcher, &mut buffer))
                .max()
        };
        let Some(fuzzy) = fuzzy else { continue };
        let recent = recency
            .iter()
            .position(|id| id == &entry.identity)
            .map(|p| usize::MAX - p)
            .unwrap_or(0);
        let proximity = current
            .map(|(w, t)| {
                usize::from(entry.workspace_id == w) * 2
                    + usize::from(t.is_some_and(|v| entry.tab_id.as_deref() == Some(v)))
            })
            .unwrap_or(0);
        let status = match entry.agent_status.as_deref() {
            Some("blocked") => 3u8,
            Some("working") => 2,
            Some("done") => 1,
            _ => 0,
        };
        scored.push((
            foreground_match,
            foreground_exact,
            exact,
            foreground_fuzzy,
            fuzzy,
            recent,
            proximity,
            status,
            entry.identity.clone(),
            index,
        ));
    }
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| b.3.cmp(&a.3))
            .then_with(|| b.4.cmp(&a.4))
            .then_with(|| b.5.cmp(&a.5))
            .then_with(|| b.6.cmp(&a.6))
            .then_with(|| a.7.cmp(&b.7))
            .then_with(|| a.8.cmp(&b.8))
    });
    scored.into_iter().map(|s| s.9).collect()
}

/// Return the first filtered row to render. The selected index is always in
/// the returned window, while the window remains bounded for large snapshots.
pub fn viewport_start(total: usize, selected: usize, capacity: usize) -> usize {
    if total <= capacity || capacity == 0 {
        return 0;
    }
    let selected = selected.min(total - 1);
    let centered = selected.saturating_sub(capacity / 2);
    centered.min(total - capacity)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusResult {
    Focused,
    Stale(String),
    Failed(String),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiState {
    Loading,
    Ready,
    EmptyRuntime,
    ZeroMatches,
    Stale(String),
    BackendUnavailable(String),
    FocusFailure(String),
    Success,
    Cancelled,
}
impl UiState {
    pub fn message(&self) -> String {
        match self {
            Self::Loading => "loading panes…".into(),
            Self::Ready => "ready".into(),
            Self::EmptyRuntime => "no panes available".into(),
            Self::ZeroMatches => "no matches".into(),
            Self::Stale(s) | Self::BackendUnavailable(s) | Self::FocusFailure(s) => s.clone(),
            Self::Success => "focused".into(),
            Self::Cancelled => "cancelled".into(),
        }
    }
}

pub fn focus_selected<H: Herdr>(client: &H, entry: &PaneEntry) -> FocusResult {
    if !entry.runtime_available {
        return FocusResult::Failed("backend unavailable for this runtime".into());
    }
    if !entry.focus_supported {
        return FocusResult::Failed("absolute pane focus is unsupported for this runtime".into());
    }
    let live = match client.list_panes() {
        Ok(panes) => panes,
        Err(e) => {
            return FocusResult::Failed(format!(
                "backend unavailable: {}",
                sanitize(&e.to_string())
            ))
        }
    };
    let Some(pane) = live
        .iter()
        .find(|pane| pane.pane_id == entry.identity.pane_id)
    else {
        return FocusResult::Stale("pane is stale; results refreshed".into());
    };
    // cwd is intentionally not part of identity: a pane may cd. Workspace/tab
    // checks prevent a reused pane id from activating another pane.
    if crate::herdr::sanitize_text(&pane.workspace_id) != entry.workspace_id
        || (entry.tab_id.is_some()
            && pane.tab_id.is_some()
            && pane.tab_id.as_deref() != entry.tab_id.as_deref())
    {
        return FocusResult::Stale("pane identity changed; results refreshed".into());
    }
    client
        .focus_pane(&entry.identity.pane_id)
        .map(|_| FocusResult::Focused)
        .unwrap_or_else(|e| FocusResult::Failed(sanitize(&format!("focus failed: {e}"))))
}

fn allowed(target: &RuntimeTarget, entry: &PaneEntry) -> bool {
    let f = &target.filter;
    let listed = |values: &Option<Vec<String>>, value: &str| {
        values
            .as_ref()
            .is_none_or(|items| items.iter().any(|item| item == value))
    };
    f.runtime.as_ref().is_none_or(|items| {
        items
            .iter()
            .any(|item| item == &target.id || item == &target.name)
    }) && listed(&f.host, target.remote_host.as_deref().unwrap_or("local"))
        && listed(&f.workspace, &entry.workspace_id)
        && listed(
            &f.status,
            entry.agent_status.as_deref().unwrap_or("unknown"),
        )
        && (f.include_hidden || !entry.hidden)
        && (f.include_plugin || !entry.plugin)
        && (f.include_floating || !entry.floating)
        && (f.include_suppressed || !entry.suppressed)
}

/// Best-effort parent-tab labels keyed by exact `tab_id` → (workspace_id, label).
/// Never fails the snapshot: any tab-list error degrades to "no labels".
fn tab_labels<H: Herdr>(client: &H, panes: &[Pane]) -> HashMap<String, (String, String)> {
    if panes.is_empty() {
        return HashMap::new();
    }
    match client.list_tabs() {
        Ok(tabs) => tabs
            .into_iter()
            .filter_map(|tab| {
                tab.label
                    .map(|label| (tab.tab_id, (tab.workspace_id, label)))
            })
            .collect(),
        Err(_) => HashMap::new(),
    }
}

pub fn snapshot<H: Herdr>(client: &H, target: RuntimeTarget) -> Result<Vec<PaneEntry>> {
    // Panes first: a tab-list failure must never hide or block a pane row.
    let panes = client.list_panes()?;
    let tabs = tab_labels(client, &panes);
    Ok(panes
        .into_iter()
        .map(|pane| {
            let label = pane
                .tab_id
                .as_deref()
                .and_then(|id| tabs.get(id))
                .filter(|(workspace, _)| {
                    workspace.is_empty() || workspace.as_str() == pane.workspace_id
                })
                .map(|(_, label)| label.clone());
            PaneEntry::from_pane(target.clone(), pane).with_tab_label(label)
        })
        .filter(|entry| allowed(&target, entry))
        .collect())
}
pub fn merge_runtime(
    state: &mut JumpPaneState,
    runtime_id: &str,
    result: Result<Vec<PaneEntry>>,
) -> Option<String> {
    match result {
        Ok(rows) => {
            let selected = state.selected().map(|entry| entry.identity.clone());
            state
                .entries
                .retain(|e| e.identity.runtime_id != runtime_id);
            state.entries.extend(rows);
            state.entries.sort_by(|a, b| a.identity.cmp(&b.identity));
            let filtered = state.filtered();
            state.selected = selected
                .and_then(|id| {
                    filtered
                        .iter()
                        .position(|i| state.entries[*i].identity == id)
                })
                .unwrap_or_else(|| state.selected.min(filtered.len().saturating_sub(1)));
            None
        }
        Err(e) => {
            for row in &mut state.entries {
                if row.identity.runtime_id == runtime_id {
                    row.runtime_available = false;
                }
            }
            Some(sanitize(&e.to_string()))
        }
    }
}

/// Lazy enrichment is intentionally event based. It starts only after the
/// initial pane snapshot is ready, examines at most 32 candidates, and has at
/// most four process-info calls in flight. Only a safe application name crosses
/// the channel; cancellation is propagated to every worker.
type EnrichmentEvent = (PaneIdentity, Option<String>);

fn enrichment_candidates(
    entries: &[PaneEntry],
    visible: &[usize],
    recency: &[PaneIdentity],
    current: Option<(&str, Option<&str>)>,
    matcher: &mut Matcher,
) -> Vec<usize> {
    let fallback;
    let ranked = if visible.is_empty() {
        fallback = filter(entries, "", matcher, recency, current);
        &fallback
    } else {
        visible
    };
    let mut seen = std::collections::HashSet::new();
    ranked
        .iter()
        .copied()
        .filter(|index| seen.insert(*index))
        .take(MAX_ENRICH_CANDIDATES)
        .collect()
}

struct EnrichmentHandle {
    cancelled: Arc<AtomicBool>,
    receiver: mpsc::Receiver<EnrichmentEvent>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Drop for EnrichmentHandle {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn start_enrichment(
    clients: &[(crate::herdr::CliHerdr, RuntimeTarget)],
    entries: &[PaneEntry],
    visible: &[usize],
) -> EnrichmentHandle {
    let (tx, rx) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let stop = cancelled.clone();
    let jobs: Vec<_> = visible
        .iter()
        .filter_map(|i| entries.get(*i))
        .filter_map(|entry| {
            clients
                .iter()
                .find(|(_, target)| target.id == entry.identity.runtime_id)
                .map(|(client, _)| (entry.identity.clone(), client.clone()))
        })
        .collect();
    let worker = thread::spawn(move || {
        for chunk in jobs.chunks(MAX_ENRICH_ACTIVE) {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let mut handles = Vec::new();
            for (identity, client) in chunk {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let tx = tx.clone();
                let identity = identity.clone();
                let client = client.with_cancellation(stop.clone());
                handles.push(thread::spawn(move || {
                    let name = client.process_name(&identity.pane_id).ok().flatten();
                    let _ = tx.send((identity, name));
                }));
            }
            for handle in handles {
                let _ = handle.join();
            }
        }
    });
    EnrichmentHandle {
        cancelled,
        receiver: rx,
        worker: Some(worker),
    }
}

pub fn targets(config: &JumpPaneConfig, bin: &str) -> Vec<RuntimeTarget> {
    let mut out = vec![RuntimeTarget::default(bin.into())];
    out.extend(
        config
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| RuntimeTarget::from_session(s, bin, i)),
    );
    out
}

fn render(
    state: &JumpPaneState,
    filtered: &[usize],
    status: &UiState,
    errors: &[String],
) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout();
    writeln!(
        out,
        "\x1b[2J\x1b[HJump Pane · {} · query: {}",
        status.message(),
        sanitize(&state.query)
    )?;
    let start = viewport_start(filtered.len(), state.selected, MAX_RENDER_ROWS);
    for (row, index) in filtered
        .iter()
        .skip(start)
        .take(MAX_RENDER_ROWS)
        .enumerate()
    {
        let entry = &state.entries[*index];
        let marker = if start + row == state.selected {
            ">"
        } else {
            " "
        };
        let disabled = if !entry.runtime_available {
            " [unavailable]"
        } else if !entry.focus_supported {
            " [focus unsupported]"
        } else {
            ""
        };
        writeln!(out, "{marker} {}{disabled}", entry.primary_display())?;
        writeln!(out, "  {}", entry.secondary_display())?;
    }
    if filtered.len() > MAX_RENDER_ROWS {
        writeln!(out, "… {} more", filtered.len() - MAX_RENDER_ROWS)?;
    }
    for error in errors {
        writeln!(out, "! {}", sanitize(error))?;
    }
    out.flush()
}

/// Interactive frontend: Enter only calls exact pane focus; Escape never
/// focuses or creates/binds anything. Raw mode is guarded on every terminal
/// error path.
pub fn run_interactive(
    mut state: JumpPaneState,
    clients: &[(crate::herdr::CliHerdr, RuntimeTarget)],
) -> std::io::Result<FocusResult> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    struct RawGuard;
    impl Drop for RawGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
            println!();
        }
    }
    let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
    let mut errors = Vec::new();
    let mut all = Vec::new();
    for (client, target) in clients {
        match snapshot(client, target.clone()) {
            Ok(mut rows) => all.append(&mut rows),
            Err(e) => errors.push(format!(
                "{}: {}",
                sanitize(&target.name),
                sanitize(&e.to_string())
            )),
        }
    }
    state.replace(all);
    if let Some(current) = state.entries.iter().find(|e| e.focused) {
        state.current_context = Some((current.workspace_id.clone(), current.tab_id.clone()));
    }
    let mut recency = PaneRecency(state.recency.clone());
    let mut ui = if !errors.is_empty() {
        UiState::BackendUnavailable(errors.join("; "))
    } else if state.entries.is_empty() {
        UiState::EmptyRuntime
    } else {
        UiState::Loading
    };
    crossterm::terminal::enable_raw_mode()?;
    let _guard = RawGuard;
    let mut enrichment: Option<EnrichmentHandle> = None;
    let mut ready = false;
    let mut filtered = Vec::new();
    let mut filter_dirty = true;
    loop {
        // Do not fuzzy re-score on every poll tick. Query, snapshot, and
        // enrichment changes explicitly invalidate this cache.
        if filter_dirty {
            filtered = filter(
                &state.entries,
                &state.query,
                &mut matcher,
                &state.recency,
                state
                    .current_context
                    .as_ref()
                    .map(|(w, t)| (w.as_str(), t.as_deref())),
            );
            filter_dirty = false;
        }
        state.selected = state.selected.min(filtered.len().saturating_sub(1));
        if filtered.is_empty() && errors.is_empty() {
            ui = if state.entries.is_empty() {
                UiState::EmptyRuntime
            } else {
                UiState::ZeroMatches
            };
        }
        render(&state, &filtered, &ui, &errors)?;
        if ready && enrichment.is_none() {
            // Application-only queries can have zero matches before enrichment.
            // Fall back to the prior/all ranking so app names can still arrive.
            let candidates = enrichment_candidates(
                &state.entries,
                &filtered,
                &state.recency,
                state
                    .current_context
                    .as_ref()
                    .map(|(w, t)| (w.as_str(), t.as_deref())),
                &mut matcher,
            );
            if !candidates.is_empty() {
                ui = UiState::Ready;
                enrichment = Some(start_enrichment(clients, &state.entries, &candidates));
            }
        }
        if !ready {
            ready = true;
            let candidates = enrichment_candidates(
                &state.entries,
                &filtered,
                &state.recency,
                state
                    .current_context
                    .as_ref()
                    .map(|(w, t)| (w.as_str(), t.as_deref())),
                &mut matcher,
            );
            if !candidates.is_empty() {
                ui = UiState::Ready;
                enrichment = Some(start_enrichment(clients, &state.entries, &candidates));
            }
        }
        if let Some(task) = &enrichment {
            while let Ok((identity, name)) = task.receiver.try_recv() {
                if let Some(entry) = state.entries.iter_mut().find(|e| e.identity == identity) {
                    entry.application = name;
                    filter_dirty = true;
                }
            }
        }
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        // Some PTYs report carriage-return/newline as Ctrl-J. Treat that
        // terminal-standard equivalent exactly like Enter.
        let code = if matches!(key.code, KeyCode::Char('j'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            KeyCode::Enter
        } else {
            key.code
        };
        match code {
            KeyCode::Esc => {
                ui = UiState::Cancelled;
                let _ = ui.message();
                return Ok(FocusResult::Stale("cancelled".into()));
            }
            KeyCode::Up => state.selected = state.selected.saturating_sub(1),
            KeyCode::Down => {
                state.selected = (state.selected + 1).min(filtered.len().saturating_sub(1))
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.selected = 0;
                filter_dirty = true;
            }
            KeyCode::Char(c) => {
                state.query.push(c);
                state.selected = 0;
                filter_dirty = true;
            }
            KeyCode::Enter => {
                let Some(index) = filtered.get(state.selected) else {
                    continue;
                };
                let entry = state.entries[*index].clone();
                let Some((client, target)) = clients
                    .iter()
                    .find(|(_, t)| t.id == entry.identity.runtime_id)
                else {
                    ui = UiState::BackendUnavailable("runtime target unavailable".into());
                    continue;
                };
                if target.id != entry.target.id {
                    ui = UiState::Stale("runtime identity changed; refresh required".into());
                    continue;
                }
                match focus_selected(client, &entry) {
                    FocusResult::Focused => {
                        recency.touch(entry.identity);
                        let path = std::env::var("HERDR_PLUGIN_STATE_DIR")
                            .map(std::path::PathBuf::from)
                            .unwrap_or_else(|_| std::path::PathBuf::from("."))
                            .join("jump-pane-state-v1.json");
                        // Focus succeeded; persistence is best effort and never
                        // changes the already-completed focus operation.
                        let _ = save_recency(&path, &recency);
                        ui = UiState::Success;
                        let _ = ui.message();
                        return Ok(FocusResult::Focused);
                    }
                    FocusResult::Stale(message) => {
                        ui = UiState::Stale(message);
                        // Refresh invalidates application metadata; drop the
                        // old workers so refreshed identities are enriched.
                        enrichment.take();
                        let refreshed = snapshot(client, target.clone());
                        if let Some(error) = merge_runtime(&mut state, &target.id, refreshed) {
                            errors.push(format!("{}: {}", target.name, error));
                            ui = UiState::BackendUnavailable(
                                errors.last().cloned().unwrap_or_default(),
                            );
                        }
                        filter_dirty = true;
                    }
                    FocusResult::Failed(message) => {
                        ui = if message.contains("backend unavailable") {
                            UiState::BackendUnavailable(message)
                        } else {
                            UiState::FocusFailure(message)
                        };
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pane(id: &str, workspace: &str, cwd: Option<&str>) -> Pane {
        Pane {
            pane_id: id.into(),
            workspace_id: workspace.into(),
            tab_id: Some("tab".into()),
            cwd: cwd.map(str::to_owned),
            foreground_cwd: None,
            agent: Some("agent".into()),
            agent_status: Some("working".into()),
            label: Some("label".into()),
            title: None,
            terminal_title: None,
            focused: false,
            hidden: false,
            plugin: false,
            floating: false,
            suppressed: false,
        }
    }
    fn entry(id: &str) -> PaneEntry {
        PaneEntry::from_pane(
            RuntimeTarget::default("herdr".into()),
            pane(id, "w", Some("/Users/test/Downloads")),
        )
    }

    #[cfg(unix)]
    struct FakeProcessInfo {
        _directory: tempfile::TempDir,
        bin: std::path::PathBuf,
        state: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl FakeProcessInfo {
        fn new(pane_ids: &[&str], delay_seconds: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;

            let directory = tempfile::tempdir().unwrap();
            let state = directory.path().join("state");
            std::fs::create_dir(&state).unwrap();
            let pane_json = serde_json::json!({
                "result": {
                    "panes": pane_ids.iter().map(|id| serde_json::json!({
                        "pane_id": id,
                        "workspace_id": "w",
                        "tab_id": "tab",
                        "cwd": "/old",
                        "focused": false,
                        "hidden": false,
                        "plugin": false,
                        "floating": false,
                        "suppressed": false
                    })).collect::<Vec<_>>()
                }
            });
            let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\\"'\\\"'"));
            let script = directory.path().join("fake-herdr.sh");
            let contents = format!(
                "#!/bin/sh\nstate={}\ndelay={}\nacquire() {{ while ! mkdir \"$state/lock\" 2>/dev/null; do sleep 0.001; done; }}\nrelease() {{ rmdir \"$state/lock\"; }}\nif [ \"$1\" = pane ] && [ \"$2\" = list ]; then printf '%s\\n' {}; exit 0; fi\nif [ \"$1\" = pane ] && [ \"$2\" = process-info ]; then\n  id=$4\n  acquire\n  calls=$(cat \"$state/calls\" 2>/dev/null || printf 0)\n  echo $((calls + 1)) > \"$state/calls\"\n  active=$(cat \"$state/active\" 2>/dev/null || printf 0)\n  active=$((active + 1))\n  echo $active > \"$state/active\"\n  max=$(cat \"$state/max\" 2>/dev/null || printf 0)\n  if [ $active -gt $max ]; then echo $active > \"$state/max\"; fi\n  release\n  if [ -e \"$state/fail-$id\" ]; then\n    sleep $delay\n    acquire; active=$(cat \"$state/active\"); echo $((active - 1)) > \"$state/active\"; release\n    printf 'intentional failure for %s\\n' \"$id\" >&2\n    exit 7\n  fi\n  sleep $delay\n  acquire; active=$(cat \"$state/active\"); echo $((active - 1)) > \"$state/active\"; release\n  name=$(cat \"$state/name-$id\" 2>/dev/null || printf 'app-%s' \"$id\")\n  printf '{{\"result\":{{\"processes\":[{{\"name\":\"%s\"}}]}}}}\\n' \"$name\"\n  exit 0\nfi\nexit 2\n",
                quote(state.to_str().unwrap()),
                delay_seconds,
                quote(&pane_json.to_string()),
            );
            std::fs::write(&script, contents).unwrap();
            let mut permissions = std::fs::metadata(&script).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script, permissions).unwrap();
            Self {
                _directory: directory,
                bin: script,
                state,
            }
        }
        fn client(&self) -> crate::herdr::CliHerdr {
            crate::herdr::CliHerdr::new(self.bin.to_string_lossy().into_owned())
        }
        fn calls(&self) -> usize {
            std::fs::read_to_string(self.state.join("calls"))
                .ok()
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0)
        }
        fn max_active(&self) -> usize {
            std::fs::read_to_string(self.state.join("max"))
                .ok()
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0)
        }
        fn set_name(&self, pane_id: &str, name: &str) {
            std::fs::write(self.state.join(format!("name-{pane_id}")), name).unwrap();
        }
        fn fail(&self, pane_id: &str) {
            std::fs::write(self.state.join(format!("fail-{pane_id}")), "fail").unwrap();
        }
        fn wait_for_calls(&self, expected: usize) {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while self.calls() < expected && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                self.calls() >= expected,
                "expected {expected} process calls"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn initial_snapshot_and_filter_do_not_call_process_info_until_enrichment_starts() {
        let fake = FakeProcessInfo::new(&["pane-1"], "0.01");
        let target = RuntimeTarget::default("unused".into());
        let entries = snapshot(&fake.client(), target.clone()).unwrap();
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let visible = filter(&entries, "", &mut matcher, &[], None);
        assert_eq!(visible, vec![0]);
        assert_eq!(fake.calls(), 0, "snapshot/filter must be process-info free");

        let candidates = enrichment_candidates(&entries, &visible, &[], None, &mut matcher);
        let handle = start_enrichment(&[(fake.client(), target)], &entries, &candidates);
        let (identity, application) = handle
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(identity.pane_id, "pane-1");
        assert_eq!(application.as_deref(), Some("app-pane-1"));
        assert_eq!(fake.calls(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn enrichment_schedules_at_most_32_candidates_with_four_active_lookups() {
        let ids: Vec<_> = (0..40).map(|i| format!("pane-{i}")).collect();
        let id_refs: Vec<_> = ids.iter().map(String::as_str).collect();
        let fake = FakeProcessInfo::new(&id_refs, "0.04");
        let entries: Vec<_> = ids.iter().map(|id| entry(id)).collect();
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let candidates = enrichment_candidates(
            &entries,
            &(0..entries.len()).collect::<Vec<_>>(),
            &[],
            None,
            &mut matcher,
        );
        assert_eq!(candidates.len(), 32);
        let target = RuntimeTarget::default("unused".into());
        let handle = start_enrichment(&[(fake.client(), target)], &entries, &candidates);
        for _ in &candidates {
            handle
                .receiver
                .recv_timeout(Duration::from_secs(3))
                .unwrap();
        }
        drop(handle);
        assert_eq!(fake.calls(), 32);
        assert!(fake.max_active() <= MAX_ENRICH_ACTIVE);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_enrichment_handle_cancels_slow_process_lookups_promptly() {
        let ids: Vec<_> = (0..4).map(|i| format!("slow-{i}")).collect();
        let id_refs: Vec<_> = ids.iter().map(String::as_str).collect();
        let fake = FakeProcessInfo::new(&id_refs, "10");
        let entries: Vec<_> = ids.iter().map(|id| entry(id)).collect();
        let target = RuntimeTarget::default("unused".into());
        let candidates: Vec<_> = (0..entries.len()).collect();
        let handle = start_enrichment(&[(fake.client(), target)], &entries, &candidates);
        fake.wait_for_calls(4);
        let started = std::time::Instant::now();
        drop(handle);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancelling slow process-info calls took too long"
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_enrichment_is_partial_and_successful_applications_remain_searchable() {
        let fake = FakeProcessInfo::new(&["ok-a", "failed", "ok-b"], "0.01");
        fake.fail("failed");
        let entries: Vec<_> = ["ok-a", "failed", "ok-b"].into_iter().map(entry).collect();
        let target = RuntimeTarget::default("unused".into());
        let candidates: Vec<_> = (0..entries.len()).collect();
        let handle = start_enrichment(&[(fake.client(), target)], &entries, &candidates);
        let mut enriched = entries;
        for _ in 0..candidates.len() {
            let (identity, application) = handle
                .receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            enriched
                .iter_mut()
                .find(|entry| entry.identity == identity)
                .unwrap()
                .application = application;
        }
        drop(handle);
        assert_eq!(
            enriched
                .iter()
                .find(|e| e.identity.pane_id == "failed")
                .unwrap()
                .application,
            None
        );
        assert_eq!(
            enriched
                .iter()
                .filter_map(|entry| entry.application.as_deref())
                .collect::<Vec<_>>(),
            vec!["app-ok-a", "app-ok-b"]
        );
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        assert_eq!(
            filter(&enriched, "app-ok-b", &mut matcher, &[], None),
            vec![2]
        );
    }

    #[cfg(unix)]
    #[test]
    fn re_enrichment_updates_application_after_refresh() {
        let fake = FakeProcessInfo::new(&["changing"], "0.01");
        let target = RuntimeTarget::default("unused".into());
        let entries = vec![entry("changing")];
        fake.set_name("changing", "old-app");
        let handle = start_enrichment(&[(fake.client(), target.clone())], &entries, &[0]);
        let (identity, application) = handle
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        drop(handle);
        let mut refreshed = entries;
        refreshed[0].application = application;
        assert_eq!(refreshed[0].application.as_deref(), Some("old-app"));

        fake.set_name("changing", "new-app");
        let handle = start_enrichment(&[(fake.client(), target)], &refreshed, &[0]);
        let (identity_again, application_again) = handle
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        drop(handle);
        assert_eq!(identity_again, identity);
        refreshed[0].application = application_again;
        assert_eq!(refreshed[0].application.as_deref(), Some("new-app"));
    }

    #[test]
    fn rows_have_all_fields_and_short_id() {
        let e = entry("w:pabcdefghi");
        assert!(e.primary_display().contains("efghi"));
        assert!(
            e.secondary_display().contains("default") && e.secondary_display().contains("working")
        );
    }
    #[test]
    fn duplicate_directories_keep_identity() {
        assert_ne!(entry("a").identity, entry("b").identity);
    }
    #[test]
    fn deterministic_order_is_input_independent() {
        let a = entry("a");
        let b = entry("b");
        let mut x = JumpPaneState::default();
        x.replace(vec![b.clone(), a.clone()]);
        let mut y = JumpPaneState::default();
        y.replace(vec![a, b]);
        assert_eq!(
            x.entries.iter().map(|e| &e.identity).collect::<Vec<_>>(),
            y.entries.iter().map(|e| &e.identity).collect::<Vec<_>>()
        );
    }
    #[test]
    fn selection_survives_refresh_and_query() {
        let mut s = JumpPaneState {
            query: "label".into(),
            ..Default::default()
        };
        s.replace(vec![entry("b"), entry("a")]);
        s.selected = 1;
        let selected = s.selected().unwrap().identity.clone();
        s.replace(vec![entry("a"), entry("b")]);
        assert_eq!(s.query, "label");
        assert_eq!(s.selected().unwrap().identity, selected);
    }
    #[test]
    fn recency_is_bounded_and_persisted_identity_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jump.json");
        let mut r = PaneRecency::default();
        for i in 0..150 {
            r.touch(PaneIdentity {
                runtime_id: "r".into(),
                pane_id: i.to_string(),
            });
        }
        save_recency(&path, &r).unwrap();
        let got = load_recency(&path);
        assert_eq!(got.0.len(), 100);
        let text = std::fs::read_to_string(path).unwrap();
        assert!(!text.contains("cwd") && !text.contains("title"));
    }
    #[test]
    fn corrupt_or_old_recency_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jump.json");
        std::fs::write(&path, "not-json").unwrap();
        assert!(load_recency(&path).0.is_empty());
        std::fs::write(&path, r#"{"version":0,"identities":[]}"#).unwrap();
        assert!(load_recency(&path).0.is_empty());
    }
    #[test]
    fn scoped_filter_excludes_hidden_and_unlisted_workspace() {
        let mut target = RuntimeTarget::default("herdr".into());
        target.filter.workspace = Some(vec!["w1".into()]);
        let mut hidden = entry("hidden");
        hidden.hidden = true;
        assert!(!allowed(&target, &hidden));
        let mut visible = entry("visible");
        visible.workspace_id = "w1".into();
        assert!(allowed(&target, &visible));
    }
    #[test]
    fn large_snapshot_filter_is_bounded_without_process_lookup() {
        let entries: Vec<_> = (0..5_000)
            .map(|i| entry(&format!("large-pane-{i}")))
            .collect();
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let rows = filter(&entries, "large-pane-4999", &mut matcher, &[], None);
        assert_eq!(rows.len(), 1);
        assert_eq!(entries[rows[0]].identity.pane_id, "large-pane-4999");
    }
    #[test]
    fn application_only_query_uses_all_ranked_candidates_and_caps_at_32() {
        let entries: Vec<_> = (0..40).map(|i| entry(&format!("pane-{i}"))).collect();
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let visible = filter(&entries, "not-an-application-yet", &mut matcher, &[], None);
        assert!(visible.is_empty());
        let candidates = enrichment_candidates(&entries, &visible, &[], None, &mut matcher);
        assert_eq!(candidates.len(), MAX_ENRICH_CANDIDATES);
        assert_eq!(candidates[0], 0);
    }
    #[test]
    fn viewport_keeps_selected_row_visible_beyond_render_cap() {
        assert_eq!(viewport_start(100, 0, MAX_RENDER_ROWS), 0);
        assert_eq!(viewport_start(100, 99, MAX_RENDER_ROWS), 36);
        for selected in 0..100 {
            let start = viewport_start(100, selected, MAX_RENDER_ROWS);
            assert!(selected >= start && selected < start + MAX_RENDER_ROWS);
        }
    }
    #[test]
    fn foreground_cwd_out_ranks_stale_base_cwd() {
        let mut stale = entry("stale");
        stale.cwd = Some("/Users/test/Downloads".into());
        stale.foreground_cwd = Some("/Users/test/project".into());
        let mut current = entry("current");
        current.cwd = Some("/Users/test/other".into());
        current.foreground_cwd = Some("/Users/test/Downloads".into());
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        let rows = filter(
            &[stale, current],
            "/Users/test/Downloads",
            &mut matcher,
            &[],
            None,
        );
        assert_eq!(rows, vec![1, 0]);
    }
    #[test]
    fn unsupported_focus_rows_are_visible_but_non_activatable() {
        let mut target = RuntimeTarget::default("herdr".into());
        target.focus_supported = false;
        let row = PaneEntry::from_pane(target, pane("unsupported", "w", None));
        assert_eq!(
            filter(
                std::slice::from_ref(&row),
                "",
                &mut Matcher::new(NucleoConfig::DEFAULT),
                &[],
                None,
            ),
            vec![0]
        );
        assert!(matches!(
            focus_selected(&NoopHerdr, &row),
            FocusResult::Failed(_)
        ));
    }
    struct NoopHerdr;
    impl Herdr for NoopHerdr {
        fn list_workspaces(&self) -> Result<Vec<crate::herdr::Workspace>> {
            Ok(Vec::new())
        }
        fn list_panes(&self) -> Result<Vec<Pane>> {
            Ok(Vec::new())
        }
        fn create_workspace(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
        fn focus_workspace(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn close_workspace(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn close_pane(&self, _: &str) -> Result<()> {
            unreachable!()
        }
    }
    struct CwdChangingHerdr {
        live_pane: Pane,
        focused: Arc<AtomicBool>,
    }
    impl Herdr for CwdChangingHerdr {
        fn list_workspaces(&self) -> Result<Vec<crate::herdr::Workspace>> {
            Ok(Vec::new())
        }
        fn list_panes(&self) -> Result<Vec<Pane>> {
            Ok(vec![self.live_pane.clone()])
        }
        fn focus_pane(&self, _: &str) -> Result<()> {
            self.focused.store(true, Ordering::Relaxed);
            Ok(())
        }
        fn create_workspace(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
        fn focus_workspace(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn close_workspace(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn close_pane(&self, _: &str) -> Result<()> {
            unreachable!()
        }
    }

    #[test]
    fn cwd_change_with_same_workspace_and_tab_revalidates_and_focuses() {
        let original = pane("cwd-changing", "workspace", Some("/old/path"));
        let mut live = original.clone();
        live.cwd = Some("/new/path".into());
        let focused = Arc::new(AtomicBool::new(false));
        let client = CwdChangingHerdr {
            live_pane: live,
            focused: focused.clone(),
        };
        let entry = PaneEntry::from_pane(RuntimeTarget::default("unused".into()), original);
        assert_ne!(entry.cwd.as_deref(), Some("/new/path"));
        assert_eq!(focus_selected(&client, &entry), FocusResult::Focused);
        assert!(focused.load(Ordering::Relaxed));
    }

    #[test]
    fn runtime_host_is_sanitized_and_searchable() {
        let mut target = RuntimeTarget::default("herdr".into());
        target.remote_host = Some("remote\u{1b}[31m.example".into());
        let row = PaneEntry::from_pane(target, pane("host", "w", None));
        assert_eq!(
            row.target.remote_host.as_deref(),
            Some("remote[31m.example")
        );
        assert!(row.search_fields().contains(&"remote[31m.example"));
        assert!(row.secondary_display().contains("remote"));
    }
    #[test]
    fn stale_close_before_focus_retains_query_and_other_runtime() {
        let mut live = entry("live");
        live.identity.runtime_id = "live".into();
        live.target.id = "live".into();
        let mut closed = entry("closed");
        closed.identity.runtime_id = "closed".into();
        closed.target.id = "closed".into();
        let mut state = JumpPaneState {
            query: "keep-me".into(),
            ..Default::default()
        };
        state.replace(vec![live.clone(), closed]);
        assert!(merge_runtime(&mut state, "closed", Ok(Vec::new())).is_none());
        assert_eq!(state.query, "keep-me");
        assert!(state.entries.iter().any(|e| e.identity == live.identity));
    }
    #[test]
    fn backend_loss_disables_only_failed_runtime() {
        let mut one = entry("one");
        one.identity.runtime_id = "one".into();
        one.target.id = "one".into();
        let mut two = entry("two");
        two.identity.runtime_id = "two".into();
        two.target.id = "two".into();
        let mut state = JumpPaneState {
            entries: vec![one, two],
            ..Default::default()
        };
        assert!(merge_runtime(
            &mut state,
            "one",
            Err(crate::herdr::HerdrError::Command("lost".into()))
        )
        .is_some());
        assert!(!state.entries[0].runtime_available);
        assert!(state.entries[1].runtime_available);
        assert_eq!(state.filtered().len(), 2);
    }
    #[test]
    fn runtime_refresh_preserves_other_runtime_and_nearest_selection() {
        let mut a = entry("a");
        let mut b = entry("b");
        a.identity.runtime_id = "one".into();
        a.target.id = "one".into();
        b.identity.runtime_id = "two".into();
        b.target.id = "two".into();
        let mut state = JumpPaneState {
            query: "label".into(),
            ..Default::default()
        };
        state.replace(vec![a.clone(), b.clone()]);
        state.selected = state
            .filtered()
            .iter()
            .position(|i| state.entries[*i].identity == b.identity)
            .unwrap();
        let mut replacement = b.clone();
        replacement.cwd = Some("/Users/test/Downloads/new".into());
        merge_runtime(&mut state, "two", Ok(vec![replacement])).unwrap_or_default();
        assert_eq!(state.query, "label");
        assert!(state.entries.iter().any(|e| e.identity.runtime_id == "one"));
        assert_eq!(state.selected().unwrap().identity.runtime_id, "two");
    }

    fn tab_info(tab_id: &str, workspace: &str, label: Option<&str>) -> crate::herdr::TabInfo {
        crate::herdr::TabInfo {
            tab_id: tab_id.into(),
            workspace_id: workspace.into(),
            label: label.map(str::to_owned),
            number: None,
            agent_status: None,
        }
    }

    /// Pane/tab double whose tab list is configurable, so every degradation path
    /// (error, timeout, cancellation, empty) is exercised without a CLI.
    struct TabHerdr {
        panes: Vec<Pane>,
        tabs: std::result::Result<Vec<crate::herdr::TabInfo>, crate::herdr::HerdrError>,
        tab_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TabHerdr {
        fn new(panes: Vec<Pane>, tabs: Vec<crate::herdr::TabInfo>) -> Self {
            Self {
                panes,
                tabs: Ok(tabs),
                tab_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
        fn failing(panes: Vec<Pane>, error: crate::herdr::HerdrError) -> Self {
            Self {
                panes,
                tabs: Err(error),
                tab_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
        fn tab_calls(&self) -> usize {
            self.tab_calls.load(Ordering::Relaxed)
        }
    }

    impl Herdr for TabHerdr {
        fn list_workspaces(&self) -> Result<Vec<crate::herdr::Workspace>> {
            Ok(Vec::new())
        }
        fn list_panes(&self) -> Result<Vec<Pane>> {
            Ok(self.panes.clone())
        }
        fn list_tabs(&self) -> Result<Vec<crate::herdr::TabInfo>> {
            self.tab_calls.fetch_add(1, Ordering::Relaxed);
            self.tabs.clone()
        }
        fn focus_pane(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn create_workspace(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
        fn focus_workspace(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn close_workspace(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn close_pane(&self, _: &str) -> Result<()> {
            unreachable!()
        }
    }

    fn target() -> RuntimeTarget {
        RuntimeTarget::default("herdr".into())
    }

    #[test]
    fn tab_label_is_joined_by_exact_tab_id() {
        let client = TabHerdr::new(
            vec![pane("p1", "w", None)],
            vec![tab_info("tab", "w", Some("api server"))],
        );
        let rows = snapshot(&client, target()).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tab_id.as_deref(), Some("tab"));
        assert_eq!(rows[0].tab_label.as_deref(), Some("api server"));
    }

    #[test]
    fn tab_label_requires_matching_workspace_id() {
        let client = TabHerdr::new(
            vec![pane("p1", "w", None)],
            vec![tab_info("tab", "other-workspace", Some("wrong tab"))],
        );
        assert_eq!(snapshot(&client, target()).unwrap()[0].tab_label, None);

        // Older payloads may omit workspace_id; an empty value still joins.
        let client = TabHerdr::new(
            vec![pane("p1", "w", None)],
            vec![tab_info("tab", "", Some("legacy"))],
        );
        assert_eq!(
            snapshot(&client, target()).unwrap()[0].tab_label.as_deref(),
            Some("legacy")
        );
    }

    #[test]
    fn missing_or_blank_tab_metadata_leaves_pane_searchable() {
        let mut no_tab = pane("no-tab", "w", None);
        no_tab.tab_id = None;
        let mut unknown_tab = pane("unknown-tab", "w", None);
        unknown_tab.tab_id = Some("not-listed".into());
        let client = TabHerdr::new(
            vec![no_tab, pane("blank-label", "w", None), unknown_tab],
            vec![tab_info("tab", "w", None)],
        );
        let rows = snapshot(&client, target()).unwrap();

        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.tab_label.is_none()));
        assert!(rows.iter().all(|row| !row.search_fields().is_empty()));
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
        assert_eq!(filter(&rows, "label", &mut matcher, &[], None).len(), 3);
    }

    #[test]
    fn multiple_panes_in_one_tab_share_the_tab_label() {
        let client = TabHerdr::new(
            vec![pane("p1", "w", None), pane("p2", "w", None)],
            vec![tab_info("tab", "w", Some("shared"))],
        );
        let rows = snapshot(&client, target()).unwrap();

        assert_eq!(rows[0].tab_label.as_deref(), Some("shared"));
        assert_eq!(rows[1].tab_label.as_deref(), Some("shared"));
    }

    #[test]
    fn duplicate_tab_labels_keep_distinct_pane_identity() {
        let mut first = pane("p1", "w", None);
        first.tab_id = Some("tab-a".into());
        let mut second = pane("p2", "w", None);
        second.tab_id = Some("tab-b".into());
        let client = TabHerdr::new(
            vec![first, second],
            vec![
                tab_info("tab-a", "w", Some("same")),
                tab_info("tab-b", "w", Some("same")),
            ],
        );
        let rows = snapshot(&client, target()).unwrap();

        assert_eq!(rows[0].tab_label.as_deref(), Some("same"));
        assert_eq!(rows[1].tab_label.as_deref(), Some("same"));
        assert_ne!(rows[0].identity, rows[1].identity);
    }

    #[test]
    fn tab_list_failure_preserves_pane_navigation() {
        let client = TabHerdr::failing(
            vec![pane("p1", "w", None)],
            crate::herdr::HerdrError::Command("tab list exploded".into()),
        );
        let rows = snapshot(&client, target()).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tab_label, None);
        assert!(rows[0].runtime_available);
        assert!(rows[0].focus_supported);
        assert_eq!(focus_selected(&client, &rows[0]), FocusResult::Focused);
    }

    #[test]
    fn tab_labels_do_not_leak_across_runtimes() {
        let mut first = target();
        first.id = "first".into();
        let mut second = target();
        second.id = "second".into();
        let one = TabHerdr::new(
            vec![pane("p1", "w", None)],
            vec![tab_info("tab", "w", Some("from-first"))],
        );
        let two = TabHerdr::new(
            vec![pane("p1", "w", None)],
            vec![tab_info("tab", "w", Some("from-second"))],
        );
        let rows_one = snapshot(&one, first).unwrap();
        let rows_two = snapshot(&two, second).unwrap();

        assert_eq!(rows_one[0].tab_label.as_deref(), Some("from-first"));
        assert_eq!(rows_two[0].tab_label.as_deref(), Some("from-second"));
        assert_ne!(rows_one[0].identity, rows_two[0].identity);
    }

    #[test]
    fn empty_pane_list_skips_tab_lookup() {
        let client = TabHerdr::new(Vec::new(), vec![tab_info("tab", "w", Some("unused"))]);
        assert!(snapshot(&client, target()).unwrap().is_empty());
        assert_eq!(client.tab_calls(), 0);
    }

    #[test]
    fn tab_label_is_searchable_without_replacing_pane_name() {
        let mut row = entry("p1");
        row.tab_label = Some("release checklist".into());
        let mut sibling = entry("p2");
        sibling.tab_label = Some("release checklist".into());
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);

        // One tab label matches every pane in that tab, as an ordinary field.
        assert_eq!(
            filter(
                &[row.clone(), sibling],
                "checklist",
                &mut matcher,
                &[],
                None
            ),
            vec![0, 1]
        );
        assert_eq!(
            filter(std::slice::from_ref(&row), "label", &mut matcher, &[], None),
            vec![0]
        );
        assert_eq!(row.primary_display(), "label [p1]");
    }

    #[test]
    fn tab_label_does_not_enable_cross_field_fuzzy_matches() {
        let mut row = entry("p1");
        row.tab_label = Some("alpha".into());
        row.title = Some("beta".into());
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);

        assert!(filter(
            std::slice::from_ref(&row),
            "alphabeta",
            &mut matcher,
            &[],
            None
        )
        .is_empty());
    }

    #[test]
    fn secondary_display_prefers_tab_label_then_tab_id() {
        let mut labeled = entry("p1");
        labeled.tab_label = Some("api server".into());
        let context = labeled.secondary_display();
        assert!(context.contains(" / api server"), "{context}");
        assert!(!context.contains(" / tab"), "{context}");

        let plain = entry("p1");
        assert!(plain.secondary_display().contains(" / tab"));
    }

    #[test]
    fn secondary_display_omits_tab_context_when_absent() {
        let mut row = entry("p1");
        row.tab_id = None;
        row.tab_label = None;
        row.cwd = None;
        row.foreground_cwd = None;

        assert!(!row.secondary_display().contains(" / "));
    }

    #[test]
    fn tab_id_remains_searchable_alongside_tab_label() {
        let mut row = entry("p1");
        row.tab_label = Some("human name".into());
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);

        assert!(row.search_fields().contains(&"tab"));
        assert_eq!(
            filter(std::slice::from_ref(&row), "tab", &mut matcher, &[], None),
            vec![0]
        );
    }

    #[test]
    fn duplicate_pane_names_stay_distinguishable_with_tab_labels() {
        let mut first = entry("p1");
        first.tab_label = Some("tab one".into());
        let mut second = entry("p2");
        second.tab_label = Some("tab two".into());

        assert_ne!(first.primary_display(), second.primary_display());
        assert_ne!(first.secondary_display(), second.secondary_display());
    }

    #[test]
    fn unnamed_pane_is_discoverable_by_tab_label_and_shows_short_id() {
        let mut unnamed = pane("p7", "w", None);
        unnamed.label = None;
        unnamed.title = None;
        unnamed.terminal_title = None;
        unnamed.cwd = None;
        let mut row = PaneEntry::from_pane(target(), unnamed);
        row.tab_label = Some("build logs".into());
        let mut matcher = Matcher::new(NucleoConfig::DEFAULT);

        assert_eq!(
            filter(std::slice::from_ref(&row), "build", &mut matcher, &[], None),
            vec![0]
        );
        assert!(row.primary_display().contains(&short_id("p7")));
    }

    #[test]
    fn refresh_removes_stale_tab_labels() {
        let target = target();
        let runtime_id = target.id.clone();
        let labeled = TabHerdr::new(
            vec![pane("p1", "w", None)],
            vec![tab_info("tab", "w", Some("api"))],
        );
        let mut state = JumpPaneState::default();
        merge_runtime(&mut state, &runtime_id, snapshot(&labeled, target.clone()));
        assert_eq!(state.entries[0].tab_label.as_deref(), Some("api"));

        let unlabeled = TabHerdr::new(vec![pane("p1", "w", None)], Vec::new());
        merge_runtime(&mut state, &runtime_id, snapshot(&unlabeled, target));

        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].tab_label, None);
    }

    #[test]
    fn selection_survives_refresh_with_tab_labels() {
        let target = target();
        let runtime_id = target.id.clone();
        let initial = TabHerdr::new(
            vec![pane("p1", "w", None), pane("p2", "w", None)],
            vec![tab_info("tab", "w", Some("api"))],
        );
        let mut state = JumpPaneState::default();
        merge_runtime(&mut state, &runtime_id, snapshot(&initial, target.clone()));
        state.selected = 1;
        let selected = state.selected().unwrap().identity.clone();

        let refreshed = TabHerdr::new(
            vec![pane("p1", "w", None), pane("p2", "w", None)],
            vec![tab_info("tab", "w", Some("api renamed"))],
        );
        merge_runtime(
            &mut state,
            &runtime_id,
            snapshot(&refreshed, target.clone()),
        );

        assert_eq!(state.selected().unwrap().identity, selected);
        assert_eq!(
            state.selected().unwrap().tab_label.as_deref(),
            Some("api renamed")
        );

        // The selected pane disappearing falls back to the nearest valid row.
        let shrunken = TabHerdr::new(
            vec![pane("p1", "w", None)],
            vec![tab_info("tab", "w", Some("api renamed"))],
        );
        merge_runtime(&mut state, &runtime_id, snapshot(&shrunken, target));

        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.selected, 0);
        assert_eq!(
            state.selected().unwrap().tab_label.as_deref(),
            Some("api renamed")
        );
    }

    #[test]
    fn tab_list_timeout_degrades_to_unlabeled_panes() {
        let client = TabHerdr::failing(
            vec![pane("p1", "w", None)],
            crate::herdr::HerdrError::Timeout("tab list timed out".into()),
        );
        let rows = snapshot(&client, target()).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tab_label, None);
        assert!(rows[0].runtime_available);
    }

    #[test]
    fn tab_list_cancellation_degrades_to_unlabeled_panes() {
        let client = TabHerdr::failing(
            vec![pane("p1", "w", None)],
            crate::herdr::HerdrError::Cancelled("cancelled".into()),
        );
        let rows = snapshot(&client, target()).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tab_label, None);
        assert_eq!(focus_selected(&client, &rows[0]), FocusResult::Focused);
    }

    #[test]
    fn failed_runtime_tab_list_does_not_disable_other_runtimes() {
        let mut broken_target = target();
        broken_target.id = "broken".into();
        let mut good_target = target();
        good_target.id = "good".into();
        let broken = TabHerdr::failing(
            vec![pane("p1", "w", None)],
            crate::herdr::HerdrError::Command("boom".into()),
        );
        let good = TabHerdr::new(
            vec![pane("p2", "w", None)],
            vec![tab_info("tab", "w", Some("api"))],
        );
        let mut state = JumpPaneState::default();
        merge_runtime(
            &mut state,
            "broken",
            snapshot(&broken, broken_target.clone()),
        );
        merge_runtime(&mut state, "good", snapshot(&good, good_target.clone()));

        assert_eq!(state.entries.len(), 2);
        assert!(state.entries.iter().all(|row| row.runtime_available));
        let labeled = state
            .entries
            .iter()
            .find(|row| row.identity.runtime_id == "good")
            .unwrap();
        assert_eq!(labeled.tab_label.as_deref(), Some("api"));
        assert!(state.entries.iter().any(|row| row.tab_label.is_none()));
    }

    #[test]
    fn named_pane_focus_unchanged_with_tab_labels() {
        let original = pane("focus-me", "workspace", Some("/old/path"));
        let mut live = original.clone();
        live.cwd = Some("/new/path".into());
        let focused = Arc::new(AtomicBool::new(false));
        let client = CwdChangingHerdr {
            live_pane: live,
            focused: focused.clone(),
        };
        let mut row = PaneEntry::from_pane(target(), original);
        row.tab_label = Some("renamed tab".into());

        assert_eq!(focus_selected(&client, &row), FocusResult::Focused);
        assert!(focused.load(Ordering::Relaxed));
    }
}

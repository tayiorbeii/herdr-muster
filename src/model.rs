use crate::herdr::{Pane, TabInfo, Workspace};
use crate::sources::{basename, collapse_home, Candidate};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Blocked,
    Working,
    Done,
    Idle,
    Unknown,
}

impl AgentState {
    pub fn from_str(value: &str) -> Self {
        match value {
            "blocked" => AgentState::Blocked,
            "working" => AgentState::Working,
            "done" => AgentState::Done,
            "idle" => AgentState::Idle,
            _ => AgentState::Unknown,
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            AgentState::Blocked => 0,
            AgentState::Working => 1,
            AgentState::Done => 2,
            AgentState::Idle => 3,
            AgentState::Unknown => 4,
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            AgentState::Blocked => "●",
            AgentState::Working => "◐",
            AgentState::Done => "✓",
            AgentState::Idle => "○",
            AgentState::Unknown => "·",
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            AgentState::Blocked => "blocked",
            AgentState::Working => "working",
            AgentState::Done => "done",
            AgentState::Idle => "idle",
            AgentState::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Open {
        workspace_id: String,
        state: AgentState,
        agent: Option<String>,
    },
    /// A tab inside an open workspace. Enter focuses this exact tab.
    Tab {
        workspace_id: String,
        tab_id: String,
        state: AgentState,
        agent: Option<String>,
    },
    /// A pane the user renamed. Enter focuses this exact pane.
    Pane {
        workspace_id: String,
        tab_id: Option<String>,
        pane_id: String,
        state: AgentState,
        agent: Option<String>,
    },
    Dormant,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowId {
    Open(String),
    Tab(String),
    Pane(String),
    Project(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub path: PathBuf,
    pub display: String,
    /// Names of panes belonging to an open workspace, in Herdr response order.
    pub pane_names: Vec<String>,
    /// Human-readable labels of the workspace's tabs, in Herdr response order.
    pub tab_names: Vec<String>,
    pub kind: Kind,
}

impl Row {
    pub fn id(&self) -> RowId {
        match &self.kind {
            Kind::Open { workspace_id, .. } => RowId::Open(workspace_id.clone()),
            Kind::Tab { tab_id, .. } => RowId::Tab(tab_id.clone()),
            Kind::Pane { pane_id, .. } => RowId::Pane(pane_id.clone()),
            Kind::Dormant => RowId::Project(self.path.clone()),
        }
    }
}

/// Canonicalize a workspace cwd for identity/dedup; fall back to the raw path
/// when it no longer exists on disk.
fn canon(dir: &str) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir))
}

/// Control-stripped basename for display. Directory names reach Ratatui raw,
/// so remove terminal control characters before rendering; a pathological
/// control-char-only name falls back to a printable placeholder while the
/// path itself stays untouched for identity and workspace creation.
fn display_name_for(path: &Path) -> String {
    let name = crate::herdr::sanitize_text(&basename(path));
    if name.trim().is_empty() {
        "project".to_string()
    } else {
        name
    }
}

/// The display name of a tab, or `None` when it carries no useful name.
/// Herdr labels an unrenamed tab with its own position number, so a label equal
/// to the tab number is treated as "not renamed" and hidden from rows and
/// inline context. Payloads without a `number` cannot be judged, so their label
/// is kept.
fn tab_display_label(tab: &TabInfo) -> Option<String> {
    let label = tab.label.as_deref()?;
    let label = crate::herdr::sanitize_text(label);
    let label = label.trim();
    if label.is_empty() {
        return None;
    }
    if tab.number.is_some_and(|number| label == number.to_string()) {
        return None;
    }
    Some(label.to_string())
}

fn root_key(pane: &Pane) -> (usize, &str) {
    (pane.number().unwrap_or(usize::MAX), pane.pane_id.as_str())
}

/// One OPEN row per live workspace. Directory identity: the registry binding
/// (muster-created, survives `cd`) wins; otherwise the workspace's root-pane
/// cwd. Dormant projects that resolve to an open dir are dropped.
pub fn assemble(
    bound: &HashMap<PathBuf, String>,
    workspaces: &[Workspace],
    panes: &[Pane],
    tabs: &[TabInfo],
    dormant: &[Candidate],
    mru: &[String],
    origin_workspace: Option<&str>,
) -> Vec<Row> {
    let workspace_bindings: HashMap<&str, &Path> = bound
        .iter()
        .map(|(directory, workspace)| (workspace.as_str(), directory.as_path()))
        .collect();

    let mut root_panes: HashMap<&str, &Pane> = HashMap::new();
    let mut pane_names: HashMap<&str, Vec<String>> = HashMap::new();
    for pane in panes {
        root_panes
            .entry(pane.workspace_id.as_str())
            .and_modify(|current| {
                if root_key(pane) < root_key(current) {
                    *current = pane;
                }
            })
            .or_insert(pane);

        // Only unnamed panes feed the workspace row's pane list: named panes
        // are represented by their own rows, so a pane-name query resolves to
        // the pane row instead of the workspace that contains it.
        if pane.label.is_none() {
            if let Some(name) = pane.display_name() {
                pane_names
                    .entry(pane.workspace_id.as_str())
                    .or_default()
                    .push(name.to_string());
            }
        }
    }

    // Tab labels are optional display/search metadata: a missing or failed tab
    // list simply leaves every workspace row without tab context.
    let mut tab_names: HashMap<&str, Vec<String>> = HashMap::new();
    for tab in tabs {
        let Some(label) = tab_display_label(tab) else {
            continue;
        };
        let entry = tab_names.entry(tab.workspace_id.as_str()).or_default();
        if !entry.iter().any(|existing| existing == &label) {
            entry.push(label);
        }
    }

    let mut rows = Vec::new();
    let mut open_directories = HashSet::new();
    for workspace in workspaces {
        let id = workspace.workspace_id.as_str();
        let root = root_panes.get(id);
        let directory = workspace_bindings
            .get(id)
            .map(|path| path.to_path_buf())
            .or_else(|| root.and_then(|pane| pane.cwd.as_deref()).map(canon));
        let agent = root.and_then(|pane| pane.agent.clone());
        let state = AgentState::from_str(&workspace.agent_status);

        let (name, display, path) = match directory {
            Some(directory) => (
                display_name_for(&directory),
                crate::herdr::sanitize_text(&collapse_home(&directory)),
                directory,
            ),
            None => {
                let fallback = if workspace.label.trim().is_empty() {
                    workspace.workspace_id.clone()
                } else {
                    workspace.label.clone()
                };
                (fallback.clone(), fallback.clone(), PathBuf::from(fallback))
            }
        };
        open_directories.insert(path.clone());

        // Tabs and renamed panes are rows in their own right so they are
        // searchable and jumpable next to workspaces and projects. Only panes
        // the user actually named get a row: unnamed panes stay discoverable
        // through their workspace row's pane names.
        for tab in tabs.iter().filter(|tab| tab.workspace_id == id) {
            let Some(label) = tab_display_label(tab) else {
                continue;
            };
            let agent = panes
                .iter()
                .find(|pane| {
                    pane.tab_id.as_deref() == Some(tab.tab_id.as_str()) && pane.agent.is_some()
                })
                .and_then(|pane| pane.agent.clone());
            rows.push(Row {
                name: label,
                display: format!("{display} · {}", tab.tab_id),
                path: path.clone(),
                pane_names: Vec::new(),
                tab_names: Vec::new(),
                kind: Kind::Tab {
                    workspace_id: workspace.workspace_id.clone(),
                    tab_id: tab.tab_id.clone(),
                    state: AgentState::from_str(
                        tab.agent_status
                            .as_deref()
                            .unwrap_or(workspace.agent_status.as_str()),
                    ),
                    agent,
                },
            });
        }
        for pane in panes.iter().filter(|pane| pane.workspace_id == id) {
            let Some(label) = pane.label.as_deref() else {
                continue;
            };
            let label = crate::herdr::sanitize_text(label);
            let label = label.trim();
            if label.is_empty() {
                continue;
            }
            rows.push(Row {
                name: label.to_string(),
                display: format!("{display} · {}", pane.pane_id),
                path: path.clone(),
                pane_names: Vec::new(),
                tab_names: Vec::new(),
                kind: Kind::Pane {
                    workspace_id: workspace.workspace_id.clone(),
                    tab_id: pane.tab_id.clone(),
                    pane_id: pane.pane_id.clone(),
                    state: AgentState::from_str(pane.agent_status.as_deref().unwrap_or("")),
                    agent: pane.agent.clone(),
                },
            });
        }

        rows.push(Row {
            name,
            display,
            path,
            pane_names: pane_names.get(id).cloned().unwrap_or_default(),
            tab_names: tab_names.get(id).cloned().unwrap_or_default(),
            kind: Kind::Open {
                workspace_id: workspace.workspace_id.clone(),
                state,
                agent,
            },
        });
    }

    for candidate in dormant {
        if open_directories.contains(&candidate.path) {
            continue;
        }
        rows.push(Row {
            name: display_name_for(&candidate.path),
            display: candidate.display.clone(),
            path: candidate.path.clone(),
            pane_names: Vec::new(),
            tab_names: Vec::new(),
            kind: Kind::Dormant,
        });
    }

    // `sort_by` is stable, so project source order is preserved within the
    // lower-priority project section (recent history, configured paths, roots,
    // then zoxide frecency order).
    rows.sort_by(|left, right| match (&left.kind, &right.kind) {
        (Kind::Dormant, Kind::Dormant) => Ordering::Equal,
        _ => sort_key(left, mru, origin_workspace).cmp(&sort_key(right, mru, origin_workspace)),
    });
    rows
}

fn sort_key(row: &Row, mru: &[String], origin_workspace: Option<&str>) -> (u8, usize, u8, String) {
    match &row.kind {
        Kind::Open {
            workspace_id,
            state,
            ..
        } => {
            // Open workspaces rank by recency: the persisted
            // most-recently-used order first, then workspaces never focused
            // through muster by agent-state priority and name. The workspace
            // the picker was opened from sorts last: Escape already returns
            // there, so the top row fast-tracks to where you were before it.
            let position = if Some(workspace_id.as_str()) == origin_workspace {
                usize::MAX
            } else {
                mru.iter()
                    .position(|id| id == workspace_id)
                    .unwrap_or(usize::MAX - 1)
            };
            (0, position, state.rank(), row.name.to_lowercase())
        }
        Kind::Tab { state, .. } => (1, 0, state.rank(), row.name.to_lowercase()),
        Kind::Pane { state, .. } => (2, 0, state.rank(), row.name.to_lowercase()),
        Kind::Dormant => (3, 0, 0, row.name.to_lowercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(id: &str, label: &str, status: &str) -> Workspace {
        Workspace {
            workspace_id: id.into(),
            label: label.into(),
            agent_status: status.into(),
        }
    }

    fn candidate(path: &str) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            display: path.into(),
        }
    }

    fn pane(id: &str, workspace: &str, cwd: Option<&str>, agent: Option<&str>) -> Pane {
        pane_with_name(id, workspace, cwd, agent, None)
    }

    fn pane_with_name(
        id: &str,
        workspace: &str,
        cwd: Option<&str>,
        agent: Option<&str>,
        name: Option<&str>,
    ) -> Pane {
        Pane {
            pane_id: id.into(),
            workspace_id: workspace.into(),
            tab_id: None,
            cwd: cwd.map(Into::into),
            foreground_cwd: None,
            agent: agent.map(Into::into),
            agent_status: None,
            label: name.map(Into::into),
            title: None,
            terminal_title: None,
            focused: false,
            hidden: false,
            plugin: false,
            floating: false,
            suppressed: false,
        }
    }

    #[test]
    fn open_from_registry_and_from_pane_cwd_with_sort() {
        let mut bound = HashMap::new();
        bound.insert(PathBuf::from("/dev/web"), "w1".to_string());
        let workspaces = vec![
            workspace("w1", "web", "working"),
            workspace("w2", "api", "blocked"),
        ];
        let panes = vec![
            pane_with_name(
                "w1:p1",
                "w1",
                Some("/tmp/moved"),
                Some("codex"),
                Some("editor"),
            ),
            pane_with_name("w1:p2", "w1", Some("/tmp/moved"), None, Some("tests")),
            pane("w2:p2", "w2", Some("/other"), None),
            pane("w2:p1", "w2", Some("/dev/api"), None),
        ];
        let dormant = vec![
            candidate("/dev/api"),
            candidate("/dev/zeta"),
            candidate("/dev/alpha"),
        ];

        let rows = assemble(&bound, &workspaces, &panes, &[], &dormant, &[], None);

        assert_eq!(rows[0].name, "api");
        assert!(matches!(
            rows[0].kind,
            Kind::Open {
                state: AgentState::Blocked,
                ..
            }
        ));
        assert_eq!(rows[1].name, "web");
        assert_eq!(rows[1].path, PathBuf::from("/dev/web"));
        // Named panes are not listed on the workspace row; they are rows.
        assert!(rows[1].pane_names.is_empty());
        match &rows[1].kind {
            Kind::Open { agent, .. } => assert_eq!(agent.as_deref(), Some("codex")),
            _ => panic!("expected open row"),
        }
        // Renamed panes get their own rows, after every open workspace.
        assert_eq!(rows[2].name, "editor");
        assert!(matches!(rows[2].kind, Kind::Pane { .. }));
        assert_eq!(rows[3].name, "tests");
        assert!(matches!(rows[3].kind, Kind::Pane { .. }));
        assert_eq!(rows[4].name, "zeta");
        assert!(matches!(rows[4].kind, Kind::Dormant));
        assert_eq!(rows[5].name, "alpha");
        assert_eq!(rows.len(), 6);
    }

    #[test]
    fn open_rows_rank_by_most_recently_used_with_origin_last() {
        let mut bound = HashMap::new();
        bound.insert(PathBuf::from("/dev/web"), "w1".to_string());
        bound.insert(PathBuf::from("/dev/api"), "w2".to_string());
        bound.insert(PathBuf::from("/dev/other"), "w3".to_string());
        bound.insert(PathBuf::from("/dev/never"), "w4".to_string());
        let workspaces = vec![
            workspace("w1", "web", "working"),
            workspace("w2", "api", "blocked"),
            workspace("w3", "other", "idle"),
            workspace("w4", "never", "idle"),
        ];
        let panes = vec![
            pane("w1:p1", "w1", Some("/dev/web"), None),
            pane("w2:p1", "w2", Some("/dev/api"), None),
            pane("w3:p1", "w3", Some("/dev/other"), None),
            pane("w4:p1", "w4", Some("/dev/never"), None),
        ];
        let dormant = vec![candidate("/dev/zeta")];
        let mru = vec!["w3".to_string(), "w1".to_string()];

        // The workspace the picker was opened from sorts last — after the
        // MRU order and after never-focused workspaces — because Escape
        // already returns to it; the previous MRU front leads instead.
        let rows = assemble(&bound, &workspaces, &panes, &[], &dormant, &mru, Some("w2"));
        assert_eq!(rows[0].name, "other");
        assert_eq!(rows[1].name, "web");
        assert_eq!(rows[2].name, "never");
        assert_eq!(rows[3].name, "api");
        assert_eq!(rows[4].name, "zeta");
        assert_eq!(rows.len(), 5);

        // Without an origin, persisted MRU order applies; never-focused
        // workspaces sort after it by agent-state priority, then name.
        let rows = assemble(&bound, &workspaces, &panes, &[], &dormant, &mru, None);
        assert_eq!(rows[0].name, "other");
        assert_eq!(rows[1].name, "web");
        assert_eq!(rows[2].name, "api");
        assert_eq!(rows[3].name, "never");
        assert_eq!(rows[4].name, "zeta");
        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn alphabetic_pane_numbers_select_the_lowest_root() {
        let workspaces = vec![workspace("wJ", "workspace", "idle")];
        let panes = vec![
            pane("wJ:pT", "wJ", Some("/wrong"), None),
            pane("wJ:pJ", "wJ", Some("/right"), Some("claude")),
        ];

        let rows = assemble(&HashMap::new(), &workspaces, &panes, &[], &[], &[], None);

        assert_eq!(rows[0].path, PathBuf::from("/right"));
        assert!(matches!(
            &rows[0].kind,
            Kind::Open { agent, .. } if agent.as_deref() == Some("claude")
        ));
    }

    #[test]
    fn missing_cwd_uses_workspace_label_and_malformed_ids_tie_break() {
        let workspaces = vec![workspace("w1", "fallback", "idle")];
        let panes = vec![
            pane("w1:p?", "w1", Some("/z"), None),
            pane("w1:p!", "w1", None, None),
        ];

        let rows = assemble(&HashMap::new(), &workspaces, &panes, &[], &[], &[], None);

        assert_eq!(rows[0].name, "fallback");
        assert_eq!(rows[0].path, PathBuf::from("fallback"));
    }

    #[test]
    fn missing_cwd_uses_a_sanitized_workspace_label() {
        let workspaces = crate::herdr::parse_workspaces(
            r#"{"result":{"workspaces":[{"workspace_id":"w1","label":"\u001b[2Jfallback","agent_status":"idle"}]}}"#,
        )
        .unwrap();

        let rows = assemble(&HashMap::new(), &workspaces, &[], &[], &[], &[], None);

        assert_eq!(rows[0].name, "[2Jfallback");
        assert_eq!(rows[0].display, "[2Jfallback");
    }

    #[test]
    fn row_identity_is_stable() {
        let open = Row {
            name: "api".into(),
            path: PathBuf::from("/api"),
            display: "/api".into(),
            pane_names: Vec::new(),
            tab_names: Vec::new(),
            kind: Kind::Open {
                workspace_id: "w1".into(),
                state: AgentState::Idle,
                agent: None,
            },
        };
        assert_eq!(open.id(), RowId::Open("w1".into()));
    }

    fn tab(id: &str, workspace: &str, label: Option<&str>) -> TabInfo {
        TabInfo {
            tab_id: id.into(),
            workspace_id: workspace.into(),
            label: label.map(str::to_owned),
            number: None,
            agent_status: None,
        }
    }

    fn numbered_tab(id: &str, workspace: &str, label: Option<&str>, number: usize) -> TabInfo {
        let mut tab = tab(id, workspace, label);
        tab.number = Some(number);
        tab
    }

    #[test]
    fn assemble_joins_tab_labels_by_workspace_id() {
        let workspaces = vec![
            workspace("w1", "api", "idle"),
            workspace("w2", "web", "idle"),
        ];
        let panes = vec![
            pane("w1:p1", "w1", Some("/api"), None),
            pane("w2:p1", "w2", Some("/web"), None),
        ];
        let tabs = vec![
            tab("w1:t1", "w1", Some("api server")),
            tab("w1:t2", "w1", Some("logs")),
            tab("w2:t1", "w2", Some("web ui")),
            tab("w9:t1", "w9", Some("orphan")),
        ];

        let rows = assemble(&HashMap::new(), &workspaces, &panes, &tabs, &[], &[], None);

        assert_eq!(rows[0].tab_names, vec!["api server", "logs"]);
        assert_eq!(rows[1].tab_names, vec!["web ui"]);
    }

    #[test]
    fn tab_labels_are_optional_sanitized_and_deduplicated() {
        let workspaces = vec![workspace("w1", "api", "idle")];
        let panes = vec![pane("w1:p1", "w1", Some("/api"), None)];
        let tabs = vec![
            tab("w1:t1", "w1", Some("\u{1b}[2Japi")),
            tab("w1:t2", "w1", Some("api")),
            tab("w1:t3", "w1", None),
            tab("w1:t4", "w1", Some("   ")),
        ];

        let rows = assemble(&HashMap::new(), &workspaces, &panes, &tabs, &[], &[], None);
        assert_eq!(rows[0].tab_names, vec!["[2Japi", "api"]);

        // No tab list at all: rows stay intact and simply carry no tab names.
        let rows = assemble(&HashMap::new(), &workspaces, &panes, &[], &[], &[], None);
        assert!(rows[0].tab_names.is_empty());
        assert_eq!(rows[0].name, "api");
    }

    #[test]
    fn tab_rows_and_renamed_pane_rows_are_listed_and_jumpable() {
        let workspaces = vec![workspace("w1", "api", "working")];
        let mut editor =
            pane_with_name("w1:p1", "w1", Some("/api"), Some("claude"), Some("editor"));
        editor.tab_id = Some("w1:t1".into());
        let mut unnamed = pane_with_name("w1:p2", "w1", Some("/api"), None, None);
        unnamed.tab_id = Some("w1:t1".into());
        let panes = vec![editor, unnamed];
        let tabs = vec![
            tab("w1:t1", "w1", Some("api server")),
            tab("w1:t2", "w1", Some("logs")),
        ];

        let rows = assemble(&HashMap::new(), &workspaces, &panes, &tabs, &[], &[], None);
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();

        // Workspaces first, then tabs, then renamed panes.
        assert_eq!(names, vec!["api", "api server", "logs", "editor"]);
        assert_eq!(rows[1].id(), RowId::Tab("w1:t1".into()));
        assert_eq!(rows[3].id(), RowId::Pane("w1:p1".into()));
        assert!(rows[1].display.contains("w1:t1"), "{}", rows[1].display);
        assert!(rows[3].display.contains("w1:p1"), "{}", rows[3].display);
        match &rows[1].kind {
            Kind::Tab {
                workspace_id,
                tab_id,
                agent,
                ..
            } => {
                assert_eq!(workspace_id, "w1");
                assert_eq!(tab_id, "w1:t1");
                assert_eq!(agent.as_deref(), Some("claude"));
            }
            _ => panic!("expected tab row"),
        }
        match &rows[3].kind {
            Kind::Pane {
                workspace_id,
                pane_id,
                ..
            } => {
                assert_eq!(workspace_id, "w1");
                assert_eq!(pane_id, "w1:p1");
            }
            _ => panic!("expected pane row"),
        }
        // The unnamed pane is not a row; it stays visible via the open row.
        assert!(!names.contains(&"w1:p2"));
        assert!(rows[0].pane_names.is_empty());
    }

    #[test]
    fn tab_rows_use_the_tab_agent_state() {
        let workspaces = vec![workspace("w1", "api", "idle")];
        let panes = vec![pane("w1:p1", "w1", Some("/api"), None)];
        let mut working = tab("w1:t1", "w1", Some("api server"));
        working.agent_status = Some("working".into());

        let rows = assemble(
            &HashMap::new(),
            &workspaces,
            &panes,
            &[working],
            &[],
            &[],
            None,
        );

        match &rows[1].kind {
            Kind::Tab { state, .. } => assert_eq!(*state, AgentState::Working),
            _ => panic!("expected tab row"),
        }
    }

    #[test]
    fn default_numbered_tabs_are_hidden_from_rows_and_context() {
        let workspaces = vec![workspace("w1", "api", "idle")];
        let panes = vec![pane("w1:p1", "w1", Some("/api"), None)];
        let tabs = vec![
            numbered_tab("w1:t1", "w1", Some("1"), 1),
            numbered_tab("w1:t2", "w1", Some("build logs"), 2),
        ];

        let rows = assemble(&HashMap::new(), &workspaces, &panes, &tabs, &[], &[], None);

        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["api", "build logs"]);
        // The workspace row's inline tab context hides defaults too.
        assert_eq!(rows[0].tab_names, vec!["build logs"]);
    }

    #[test]
    fn tabs_without_a_number_are_kept() {
        let workspaces = vec![workspace("w1", "api", "idle")];
        let panes = vec![pane("w1:p1", "w1", Some("/api"), None)];

        // Older payloads omit `number`; the label is then the only signal, so
        // it is shown rather than guessed away.
        let rows = assemble(
            &HashMap::new(),
            &workspaces,
            &panes,
            &[tab("w1:t1", "w1", Some("1"))],
            &[],
            &[],
            None,
        );

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].name, "1");
    }
}

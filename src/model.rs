use crate::herdr::{Pane, Workspace};
use crate::sources::{basename, collapse_home, Candidate};
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
    Dormant,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowId {
    Open(String),
    Project(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub path: PathBuf,
    pub display: String,
    /// Names of panes belonging to an open workspace, in Herdr response order.
    pub pane_names: Vec<String>,
    pub kind: Kind,
}

impl Row {
    pub fn id(&self) -> RowId {
        match &self.kind {
            Kind::Open { workspace_id, .. } => RowId::Open(workspace_id.clone()),
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

        if let Some(name) = pane.display_name() {
            pane_names
                .entry(pane.workspace_id.as_str())
                .or_default()
                .push(name.to_string());
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
        rows.push(Row {
            name,
            display,
            path,
            pane_names: pane_names.get(id).cloned().unwrap_or_default(),
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
            kind: Kind::Dormant,
        });
    }

    rows.sort_by_key(|row| sort_key(row, mru, origin_workspace));
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
        Kind::Dormant => (1, 0, 0, row.name.to_lowercase()),
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
            cwd: cwd.map(Into::into),
            agent: agent.map(Into::into),
            label: name.map(Into::into),
            terminal_title: None,
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

        let rows = assemble(&bound, &workspaces, &panes, &dormant, &[], None);

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
        assert_eq!(rows[1].pane_names, vec!["editor", "tests"]);
        match &rows[1].kind {
            Kind::Open { agent, .. } => assert_eq!(agent.as_deref(), Some("codex")),
            Kind::Dormant => panic!("expected open row"),
        }
        assert_eq!(rows[2].name, "alpha");
        assert!(matches!(rows[2].kind, Kind::Dormant));
        assert_eq!(rows[3].name, "zeta");
        assert_eq!(rows.len(), 4);
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
        let rows = assemble(&bound, &workspaces, &panes, &dormant, &mru, Some("w2"));
        assert_eq!(rows[0].name, "other");
        assert_eq!(rows[1].name, "web");
        assert_eq!(rows[2].name, "never");
        assert_eq!(rows[3].name, "api");
        assert_eq!(rows[4].name, "zeta");
        assert_eq!(rows.len(), 5);

        // Without an origin, persisted MRU order applies; never-focused
        // workspaces sort after it by agent-state priority, then name.
        let rows = assemble(&bound, &workspaces, &panes, &dormant, &mru, None);
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

        let rows = assemble(&HashMap::new(), &workspaces, &panes, &[], &[], None);

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

        let rows = assemble(&HashMap::new(), &workspaces, &panes, &[], &[], None);

        assert_eq!(rows[0].name, "fallback");
        assert_eq!(rows[0].path, PathBuf::from("fallback"));
    }

    #[test]
    fn missing_cwd_uses_a_sanitized_workspace_label() {
        let workspaces = crate::herdr::parse_workspaces(
            r#"{"result":{"workspaces":[{"workspace_id":"w1","label":"\u001b[2Jfallback","agent_status":"idle"}]}}"#,
        )
        .unwrap();

        let rows = assemble(&HashMap::new(), &workspaces, &[], &[], &[], None);

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
            kind: Kind::Open {
                workspace_id: "w1".into(),
                state: AgentState::Idle,
                agent: None,
            },
        };
        assert_eq!(open.id(), RowId::Open("w1".into()));
    }
}

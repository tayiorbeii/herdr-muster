use crate::config::Config;
use crate::herdr::{command_output, CliHerdr, Herdr, Pane, TabInfo, Workspace};
use crate::model::{self, Row};
use crate::sources;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const WORKER_COMPLETION_TIMEOUT: Duration = Duration::from_millis(200);
const ZOXIDE_FALLBACK_DIRS: &[&str] = &[
    "/opt/homebrew/bin",              // Homebrew on Apple Silicon
    "/usr/local/bin",                 // Homebrew on Intel macOS
    "/home/linuxbrew/.linuxbrew/bin", // Linuxbrew
];

pub struct Updates {
    receiver: Receiver<Message>,
    cancelled: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Updates {
    pub fn try_recv(&self) -> Result<Message, TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Drop for Updates {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            // Discovery can block in a filesystem syscall (for example, an
            // unavailable automount). Never let picker teardown inherit that
            // unbounded wait: cooperative workers get a short chance to exit;
            // a stuck worker is deliberately detached after cancellation.
            let deadline = Instant::now() + WORKER_COMPLETION_TIMEOUT;
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSourceStatus {
    Searching,
    Available,
    Unavailable,
    Disabled,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub rows: Vec<Row>,
    /// Availability of the optional zoxide project source.
    pub project_source_status: ProjectSourceStatus,
    /// `None` means the workspace query failed, so callers must not reconcile
    /// persisted bindings against this incomplete snapshot.
    pub live_workspace_ids: Option<HashSet<String>>,
    /// The workspace the picker pane was opened from, when identifiable. The
    /// picker ranks it last so Enter fast-tracks to where you were before it;
    /// Escape returns to it.
    pub origin_workspace: Option<String>,
}

#[derive(Debug)]
pub enum Message {
    /// Herdr data is available; project discovery is still running.
    Partial(Snapshot),
    Ready(Snapshot),
    Failed(String),
}

#[derive(Debug)]
struct HerdrData {
    workspaces: Vec<Workspace>,
    panes: Vec<Pane>,
    tabs: Vec<TabInfo>,
    live_workspace_ids: Option<HashSet<String>>,
}

fn load_herdr<H: Herdr>(client: &H) -> crate::herdr::Result<HerdrData> {
    // A failed workspace query is not evidence that persisted bindings are
    // stale. Continue with no live rows so configured projects remain usable
    // as dormant entries, and mark the live set unavailable for reconciliation.
    let workspaces = match client.list_workspaces() {
        Ok(workspaces) => workspaces,
        Err(_) => {
            return Ok(HerdrData {
                workspaces: Vec::new(),
                panes: Vec::new(),
                tabs: Vec::new(),
                live_workspace_ids: None,
            });
        }
    };
    let panes = client.list_panes()?;
    // Tab labels are optional enrichment: a missing or failing `tab list`
    // degrades to rows without tab context and never fails the snapshot. The
    // extra call is skipped entirely when there are no panes.
    let tabs = if panes.is_empty() {
        Vec::new()
    } else {
        client.list_tabs().unwrap_or_default()
    };
    let live_workspace_ids = workspaces
        .iter()
        .map(|workspace| workspace.workspace_id.clone())
        .collect();
    Ok(HerdrData {
        workspaces,
        panes,
        tabs,
        live_workspace_ids: Some(live_workspace_ids),
    })
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn resolve_executable(
    name: &str,
    path: Option<&OsStr>,
    fallback_dirs: &[PathBuf],
) -> Option<PathBuf> {
    path.into_iter()
        .flat_map(std::env::split_paths)
        .chain(fallback_dirs.iter().cloned())
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

fn zoxide_program() -> OsString {
    let fallback_dirs: Vec<_> = ZOXIDE_FALLBACK_DIRS.iter().map(PathBuf::from).collect();
    resolve_executable(
        "zoxide",
        std::env::var_os("PATH").as_deref(),
        &fallback_dirs,
    )
    .map(PathBuf::into_os_string)
    .unwrap_or_else(|| OsString::from("zoxide"))
}

fn duplicate_space_branches(rows: &[Row], cancellation: &AtomicBool) -> HashMap<String, String> {
    let mut label_counts = HashMap::new();
    for row in rows {
        if matches!(&row.kind, model::Kind::Open { .. }) {
            if let Some(space) = &row.space {
                *label_counts.entry(space.label.clone()).or_insert(0usize) += 1;
            }
        }
    }

    let mut branches = HashMap::new();
    for row in rows {
        if cancellation.load(Ordering::Relaxed) {
            break;
        }
        let (model::Kind::Open { workspace_id, .. }, Some(space)) = (&row.kind, &row.space) else {
            continue;
        };
        if label_counts.get(&space.label).copied().unwrap_or_default() < 2 {
            continue;
        }

        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&row.path)
            .args(["branch", "--show-current"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0");
        let Ok(output) = command_output(&mut command, "git branch lookup", Some(cancellation))
        else {
            continue;
        };
        if output.status.success() {
            let branch =
                crate::herdr::sanitize_text(String::from_utf8_lossy(&output.stdout).trim());
            if !branch.trim().is_empty() {
                branches.insert(workspace_id.clone(), branch);
            }
        }
    }
    branches
}

fn zoxide_lines(enabled: bool, cancellation: &AtomicBool) -> (Vec<String>, ProjectSourceStatus) {
    if !enabled {
        return (Vec::new(), ProjectSourceStatus::Disabled);
    }
    if cancellation.load(Ordering::Relaxed) {
        return (Vec::new(), ProjectSourceStatus::Searching);
    }
    let mut command = Command::new(zoxide_program());
    command.args(["query", "-l"]);
    let Ok(output) = command_output(&mut command, "zoxide query -l", Some(cancellation)) else {
        return (Vec::new(), ProjectSourceStatus::Unavailable);
    };
    if !output.status.success() {
        return (Vec::new(), ProjectSourceStatus::Unavailable);
    }
    (
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        ProjectSourceStatus::Available,
    )
}

pub fn spawn(
    client: CliHerdr,
    config_path: PathBuf,
    bound: HashMap<PathBuf, String>,
    recent_projects: Vec<PathBuf>,
    mru: Vec<String>,
    origin_pane_id: Option<String>,
) -> Updates {
    let (sender, receiver) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancellation = cancelled.clone();
    let client = client.with_cancellation(worker_cancellation.clone());
    let worker = std::thread::spawn(move || {
        if worker_cancellation.load(Ordering::Relaxed) {
            return;
        }
        let config = match Config::load(&config_path) {
            Ok(config) => config,
            Err(error) => {
                let _ = sender.send(Message::Failed(error));
                return;
            }
        };
        let data = match load_herdr(&client) {
            Ok(data) => data,
            Err(error) => {
                let _ = sender.send(Message::Failed(error.to_string()));
                return;
            }
        };
        if worker_cancellation.load(Ordering::Relaxed) {
            return;
        }

        let origin_workspace = origin_pane_id.as_deref().and_then(|pane_id| {
            data.panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .map(|pane| pane.workspace_id.clone())
        });

        let mut open_rows = model::assemble(
            &bound,
            &data.workspaces,
            &data.panes,
            &data.tabs,
            &[],
            &mru,
            origin_workspace.as_deref(),
        );
        let partial = Snapshot {
            rows: open_rows.clone(),
            project_source_status: ProjectSourceStatus::Searching,
            live_workspace_ids: data.live_workspace_ids.clone(),
            origin_workspace: origin_workspace.clone(),
        };
        if sender.send(Message::Partial(partial)).is_err() {
            return;
        }

        let branches = duplicate_space_branches(&open_rows, &worker_cancellation);
        if worker_cancellation.load(Ordering::Relaxed) {
            return;
        }
        if !branches.is_empty() {
            model::apply_space_disambiguators(&mut open_rows, &branches);
            if sender
                .send(Message::Partial(Snapshot {
                    rows: open_rows.clone(),
                    project_source_status: ProjectSourceStatus::Searching,
                    live_workspace_ids: data.live_workspace_ids.clone(),
                    origin_workspace: origin_workspace.clone(),
                }))
                .is_err()
            {
                return;
            }
        }

        let (zoxide_candidates, project_source_status) =
            zoxide_lines(config.use_zoxide, &worker_cancellation);
        let Some(projects) = sources::gather_with_recent(
            &config,
            &zoxide_candidates,
            &recent_projects,
            &worker_cancellation,
        ) else {
            return;
        };
        if worker_cancellation.load(Ordering::Relaxed) {
            return;
        }
        let mut rows = model::assemble(
            &bound,
            &data.workspaces,
            &data.panes,
            &data.tabs,
            &projects,
            &mru,
            origin_workspace.as_deref(),
        );
        model::apply_space_disambiguators(&mut rows, &branches);
        let _ = sender.send(Message::Ready(Snapshot {
            rows,
            project_source_status,
            live_workspace_ids: data.live_workspace_ids,
            origin_workspace,
        }));
    });
    Updates {
        receiver,
        cancelled,
        worker: Some(worker),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::{HerdrError, Result};

    struct FakeHerdr {
        fail_workspaces: bool,
        fail_panes: bool,
        fail_tabs: bool,
    }

    impl Herdr for FakeHerdr {
        fn list_workspaces(&self) -> Result<Vec<Workspace>> {
            if self.fail_workspaces {
                Err(HerdrError::Command("workspace failure".into()))
            } else {
                Ok(vec![Workspace {
                    workspace_id: "w1".into(),
                    label: "api".into(),
                    number: None,
                    agent_status: "working".into(),
                }])
            }
        }

        fn list_panes(&self) -> Result<Vec<Pane>> {
            if self.fail_panes {
                Err(HerdrError::Command("pane failure".into()))
            } else {
                Ok(vec![Pane {
                    pane_id: "w1:p1".into(),
                    workspace_id: "w1".into(),
                    tab_id: None,
                    cwd: Some("/api".into()),
                    foreground_cwd: None,
                    agent: None,
                    agent_status: None,
                    label: Some("editor".into()),
                    title: None,
                    terminal_title: None,
                    focused: false,
                    hidden: false,
                    plugin: false,
                    floating: false,
                    suppressed: false,
                }])
            }
        }

        fn list_tabs(&self) -> Result<Vec<TabInfo>> {
            if self.fail_tabs {
                Err(HerdrError::Command("tab failure".into()))
            } else {
                Ok(vec![TabInfo {
                    tab_id: "w1:t1".into(),
                    workspace_id: "w1".into(),
                    label: Some("api tab".into()),
                    number: None,
                    agent_status: None,
                }])
            }
        }

        fn create_workspace(&self, _cwd: &str, _label: &str) -> Result<String> {
            unreachable!()
        }

        fn focus_workspace(&self, _id: &str) -> Result<()> {
            unreachable!()
        }

        fn close_workspace(&self, _id: &str) -> Result<()> {
            unreachable!()
        }

        fn close_pane(&self, _id: &str) -> Result<()> {
            unreachable!()
        }
    }

    #[test]
    fn resolves_zoxide_outside_a_sparse_plugin_path() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let inherited = temp.path().join("inherited");
        let homebrew = temp.path().join("homebrew");
        std::fs::create_dir_all(&inherited).unwrap();
        std::fs::create_dir_all(&homebrew).unwrap();
        let executable = homebrew.join("zoxide");
        std::fs::write(&executable, "#!/bin/sh\\nexit 0\\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let path = std::env::join_paths([&inherited]).unwrap();
        assert_eq!(
            resolve_executable("zoxide", Some(&path), std::slice::from_ref(&homebrew)),
            Some(executable)
        );
    }

    #[test]
    fn executable_from_path_precedes_fallback_directory() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let inherited = temp.path().join("inherited");
        let fallback = temp.path().join("fallback");
        std::fs::create_dir_all(&inherited).unwrap();
        std::fs::create_dir_all(&fallback).unwrap();
        let path_executable = inherited.join("zoxide");
        let fallback_executable = fallback.join("zoxide");
        for executable in [&path_executable, &fallback_executable] {
            std::fs::write(executable, "#!/bin/sh\\nexit 0\\n").unwrap();
            let mut permissions = std::fs::metadata(executable).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(executable, permissions).unwrap();
        }

        let path = std::env::join_paths([&inherited]).unwrap();
        assert_eq!(
            resolve_executable("zoxide", Some(&path), std::slice::from_ref(&fallback)),
            Some(path_executable)
        );
    }

    #[test]
    fn successful_refresh_distinguishes_valid_live_data() {
        let data = load_herdr(&FakeHerdr {
            fail_workspaces: false,
            fail_panes: false,
            fail_tabs: false,
        })
        .unwrap();
        assert_eq!(data.live_workspace_ids, Some(HashSet::from(["w1".into()])));
        assert_eq!(data.panes[0].display_name(), Some("editor"));
    }

    #[test]
    fn workspace_failure_leaves_live_set_unavailable_and_projects_dormant() {
        let data = load_herdr(&FakeHerdr {
            fail_workspaces: true,
            fail_panes: false,
            fail_tabs: false,
        })
        .unwrap();

        assert!(data.workspaces.is_empty());
        assert!(data.panes.is_empty());
        assert_eq!(data.live_workspace_ids, None);
        let projects = vec![crate::sources::Candidate {
            path: PathBuf::from("/configured"),
            display: "/configured".into(),
        }];
        let rows = model::assemble(
            &HashMap::new(),
            &data.workspaces,
            &data.panes,
            &data.tabs,
            &projects,
            &[],
            None,
        );
        assert!(matches!(rows.as_slice(), [row] if matches!(row.kind, model::Kind::Dormant)));
    }

    #[test]
    fn pane_failure_rejects_the_whole_snapshot() {
        let error = load_herdr(&FakeHerdr {
            fail_workspaces: false,
            fail_panes: true,
            fail_tabs: false,
        })
        .unwrap_err();
        assert!(error.to_string().contains("pane failure"));
    }

    #[test]
    fn drop_does_not_wait_for_a_stuck_worker() {
        let (_sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn(|| thread::sleep(Duration::from_secs(1)));
        let updates = Updates {
            receiver,
            cancelled: cancelled.clone(),
            worker: Some(worker),
        };

        let started = Instant::now();
        drop(updates);

        assert!(cancelled.load(Ordering::Relaxed));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn spawn_orders_open_rows_by_recency_with_origin_last() {
        use std::fs;

        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "use_zoxide = false\n").unwrap();
        let fake = directory.path().join("fake-herdr");
        fs::write(
            &fake,
            r#"#!/bin/sh
case "$1 $2" in
  'workspace list') echo '{"result":{"workspaces":[{"workspace_id":"w1","label":"api","agent_status":"blocked"},{"workspace_id":"w2","label":"web","agent_status":"working"}]}}' ;;
  'pane list') echo '{"result":{"panes":[{"pane_id":"w1:p1","workspace_id":"w1","cwd":"/dev/api"},{"pane_id":"w2:p1","workspace_id":"w2","cwd":"/dev/web"},{"pane_id":"w2:p9","workspace_id":"w2","cwd":"/dev/web"}]}}' ;;
esac
"#,
        )
        .unwrap();
        #[allow(clippy::permissions_set_readonly_false)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&fake).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&fake, permissions).unwrap();
        }

        let client = CliHerdr::new(fake.to_string_lossy().into_owned());
        // w1 was the most recent before this picker opened, so it leads and
        // Enter fast-tracks back to it; the picker pane (HERDR_PANE_ID)
        // lives in w2, so w2 sorts last.
        let mru = vec!["w1".to_string(), "w2".to_string()];
        let updates = spawn(
            client,
            config_path,
            HashMap::new(),
            Vec::new(),
            mru,
            Some("w2:p9".into()),
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match updates.try_recv() {
                Ok(Message::Ready(ready)) => {
                    assert_eq!(ready.origin_workspace.as_deref(), Some("w2"));
                    let names: Vec<_> = ready.rows.iter().map(|row| row.name.as_str()).collect();
                    assert_eq!(names, vec!["api", "web"]);
                    break;
                }
                Ok(Message::Failed(error)) => panic!("refresh failed: {error}"),
                Ok(Message::Partial(_)) => {}
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(TryRecvError::Empty) => panic!("refresh worker did not finish in time"),
                Err(TryRecvError::Disconnected) => panic!("refresh worker disconnected"),
            }
        }
    }

    #[test]
    fn tab_list_is_loaded_into_snapshot_data() {
        let data = load_herdr(&FakeHerdr {
            fail_workspaces: false,
            fail_panes: false,
            fail_tabs: false,
        })
        .unwrap();

        assert_eq!(data.tabs.len(), 1);
        assert_eq!(data.tabs[0].label.as_deref(), Some("api tab"));
    }

    #[test]
    fn tab_list_failure_degrades_to_rows_without_tab_names() {
        let data = load_herdr(&FakeHerdr {
            fail_workspaces: false,
            fail_panes: false,
            fail_tabs: true,
        })
        .unwrap();

        assert!(data.tabs.is_empty());
        let rows = model::assemble(
            &HashMap::new(),
            &data.workspaces,
            &data.panes,
            &data.tabs,
            &[],
            &[],
            None,
        );
        // The workspace row and the renamed-pane row both survive without tabs.
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.tab_names.is_empty()));
        assert!(rows[0].pane_names.is_empty());
        assert!(matches!(rows[1].kind, model::Kind::Pane { .. }));
    }

    /// A runtime with no panes must not pay for the extra tab call at all.
    struct NoPaneHerdr;

    impl Herdr for NoPaneHerdr {
        fn list_workspaces(&self) -> Result<Vec<Workspace>> {
            Ok(vec![Workspace {
                workspace_id: "w1".into(),
                label: "api".into(),
                number: None,
                agent_status: "idle".into(),
            }])
        }

        fn list_panes(&self) -> Result<Vec<Pane>> {
            Ok(Vec::new())
        }

        fn list_tabs(&self) -> Result<Vec<TabInfo>> {
            unreachable!("tab list must be skipped when there are no panes")
        }

        fn create_workspace(&self, _cwd: &str, _label: &str) -> Result<String> {
            unreachable!()
        }

        fn focus_workspace(&self, _id: &str) -> Result<()> {
            unreachable!()
        }

        fn close_workspace(&self, _id: &str) -> Result<()> {
            unreachable!()
        }

        fn close_pane(&self, _id: &str) -> Result<()> {
            unreachable!()
        }
    }

    #[test]
    fn empty_pane_list_skips_the_tab_call() {
        let data = load_herdr(&NoPaneHerdr).unwrap();

        assert!(data.panes.is_empty());
        assert!(data.tabs.is_empty());
    }
}

use crate::config::Config;
use crate::herdr::{command_output, CliHerdr, Herdr, Pane, Workspace};
use crate::model::{self, Row};
use crate::sources;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const WORKER_COMPLETION_TIMEOUT: Duration = Duration::from_millis(200);

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

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub rows: Vec<Row>,
    /// `None` means the workspace query failed, so callers must not reconcile
    /// persisted bindings against this incomplete snapshot.
    pub live_workspace_ids: Option<HashSet<String>>,
    /// The workspace the picker pane was opened from, when identifiable. The
    /// picker ranks it first so Enter fast-tracks back to where you were.
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
                live_workspace_ids: None,
            });
        }
    };
    let panes = client.list_panes()?;
    let live_workspace_ids = workspaces
        .iter()
        .map(|workspace| workspace.workspace_id.clone())
        .collect();
    Ok(HerdrData {
        workspaces,
        panes,
        live_workspace_ids: Some(live_workspace_ids),
    })
}

fn zoxide_lines(enabled: bool, cancellation: &AtomicBool) -> Vec<String> {
    if !enabled || cancellation.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let mut command = Command::new("zoxide");
    command.args(["query", "-l"]);
    let Ok(output) = command_output(&mut command, "zoxide query -l", Some(cancellation)) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

pub fn spawn(
    client: CliHerdr,
    config_path: PathBuf,
    bound: HashMap<PathBuf, String>,
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

        let open_rows = model::assemble(
            &bound,
            &data.workspaces,
            &data.panes,
            &[],
            &mru,
            origin_workspace.as_deref(),
        );
        let partial = Snapshot {
            rows: open_rows,
            live_workspace_ids: data.live_workspace_ids.clone(),
            origin_workspace: origin_workspace.clone(),
        };
        if sender.send(Message::Partial(partial)).is_err() {
            return;
        }

        let Some(projects) = sources::gather(
            &config,
            &zoxide_lines(config.use_zoxide, &worker_cancellation),
            &worker_cancellation,
        ) else {
            return;
        };
        if worker_cancellation.load(Ordering::Relaxed) {
            return;
        }
        let rows = model::assemble(
            &bound,
            &data.workspaces,
            &data.panes,
            &projects,
            &mru,
            origin_workspace.as_deref(),
        );
        let _ = sender.send(Message::Ready(Snapshot {
            rows,
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
    }

    impl Herdr for FakeHerdr {
        fn list_workspaces(&self) -> Result<Vec<Workspace>> {
            if self.fail_workspaces {
                Err(HerdrError::Command("workspace failure".into()))
            } else {
                Ok(vec![Workspace {
                    workspace_id: "w1".into(),
                    label: "api".into(),
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
                    cwd: Some("/api".into()),
                    agent: None,
                    label: Some("editor".into()),
                    terminal_title: None,
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
    fn successful_refresh_distinguishes_valid_live_data() {
        let data = load_herdr(&FakeHerdr {
            fail_workspaces: false,
            fail_panes: false,
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
    fn spawn_orders_open_rows_by_recency_with_origin_first() {
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
        // w1 was the most recent before this picker opened, but the picker
        // pane (HERDR_PANE_ID) lives in w2, so w2 must rank first.
        let mru = vec!["w1".to_string(), "w2".to_string()];
        let updates = spawn(
            client,
            config_path,
            HashMap::new(),
            mru,
            Some("w2:p9".into()),
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match updates.try_recv() {
                Ok(Message::Ready(ready)) => {
                    assert_eq!(ready.origin_workspace.as_deref(), Some("w2"));
                    let names: Vec<_> =
                        ready.rows.iter().map(|row| row.name.as_str()).collect();
                    assert_eq!(names, vec!["web", "api"]);
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
}

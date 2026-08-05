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
            // command_output observes cancellation, terminates its process
            // group, and joins its pipe readers before this worker returns.
            let _ = worker.join();
        }
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub rows: Vec<Row>,
    pub live_workspace_ids: HashSet<String>,
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
    live_workspace_ids: HashSet<String>,
}

fn load_herdr<H: Herdr>(client: &H) -> crate::herdr::Result<HerdrData> {
    let workspaces = client.list_workspaces()?;
    let panes = client.list_panes()?;
    let live_workspace_ids = workspaces
        .iter()
        .map(|workspace| workspace.workspace_id.clone())
        .collect();
    Ok(HerdrData {
        workspaces,
        panes,
        live_workspace_ids,
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

pub fn spawn(client: CliHerdr, config_path: PathBuf, bound: HashMap<PathBuf, String>) -> Updates {
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

        let open_rows = model::assemble(&bound, &data.workspaces, &data.panes, &[]);
        let partial = Snapshot {
            rows: open_rows,
            live_workspace_ids: data.live_workspace_ids.clone(),
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
        let rows = model::assemble(&bound, &data.workspaces, &data.panes, &projects);
        let _ = sender.send(Message::Ready(Snapshot {
            rows,
            live_workspace_ids: data.live_workspace_ids,
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
        assert_eq!(data.live_workspace_ids, HashSet::from(["w1".into()]));
        assert_eq!(data.panes[0].display_name(), Some("editor"));
    }

    #[test]
    fn workspace_failure_never_produces_a_live_set() {
        let error = load_herdr(&FakeHerdr {
            fail_workspaces: true,
            fail_panes: false,
        })
        .unwrap_err();
        assert!(error.to_string().contains("workspace failure"));
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
}

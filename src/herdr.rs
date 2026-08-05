use serde::Deserialize;
use std::fmt;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const PUBLIC_ID_ALPHABET: &[u8; 32] = b"123456789ABCDEFGHJKMNPQRSTVWXYZ0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HerdrError {
    Spawn(String),
    Command(String),
    InvalidJson(String),
    Cancelled(String),
    Timeout(String),
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HerdrError::Spawn(message)
            | HerdrError::Command(message)
            | HerdrError::InvalidJson(message)
            | HerdrError::Cancelled(message)
            | HerdrError::Timeout(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for HerdrError {}

pub type Result<T> = std::result::Result<T, HerdrError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub workspace_id: String,
    pub label: String,
    pub agent_status: String,
}

/// A live pane. Carries the directory identity for its workspace when muster
/// did not create it (root-pane cwd), plus any detected agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub pane_id: String,
    pub workspace_id: String,
    pub cwd: Option<String>,
    pub agent: Option<String>,
    /// User-assigned pane name (`herdr pane rename`), when present.
    pub label: Option<String>,
    /// Terminal title is a useful fallback when a pane has no explicit label.
    pub terminal_title: Option<String>,
}

impl Pane {
    /// The name shown for this pane, preferring its explicit label.
    pub fn display_name(&self) -> Option<&str> {
        self.label.as_deref().or(self.terminal_title.as_deref())
    }

    /// Public pane number from `wX:pN`, using Herdr's public-ID alphabet.
    pub fn number(&self) -> Option<usize> {
        self.pane_id
            .rsplit(':')
            .next()
            .and_then(|suffix| suffix.strip_prefix('p'))
            .and_then(decode_public_number)
    }
}

pub trait Herdr {
    fn list_workspaces(&self) -> Result<Vec<Workspace>>;
    fn list_panes(&self) -> Result<Vec<Pane>>;
    fn create_workspace(&self, cwd: &str, label: &str) -> Result<String>;
    fn focus_workspace(&self, id: &str) -> Result<()>;
    fn close_workspace(&self, id: &str) -> Result<()>;
    fn close_pane(&self, id: &str) -> Result<()>;
}

// ---- JSON shapes (Herdr 0.7.x); unknown fields ignored ----

#[derive(Deserialize)]
struct WsResp {
    result: WsResult,
}
#[derive(Deserialize)]
struct WsResult {
    workspaces: Vec<WsItem>,
}
#[derive(Deserialize)]
struct WsItem {
    workspace_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    agent_status: String,
}

#[derive(Deserialize)]
struct PnResp {
    result: PnResult,
}
#[derive(Deserialize)]
struct PnResult {
    panes: Vec<PnItem>,
}
#[derive(Deserialize)]
struct PnItem {
    pane_id: String,
    workspace_id: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    terminal_title_stripped: Option<String>,
}

#[derive(Deserialize)]
struct CrResp {
    result: CrResult,
}
#[derive(Deserialize)]
struct CrResult {
    workspace: CrWs,
}
#[derive(Deserialize)]
struct CrWs {
    workspace_id: String,
}

fn clean(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        // Herdr titles originate in terminal-controlled state. Remove every
        // terminal control range before trimming so pane text cannot inject
        // escape sequences into the picker.
        let sanitized: String = value
            .chars()
            .filter(|character| {
                !matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}' | '\u{0080}'..='\u{009f}')
            })
            .collect();
        let trimmed = sanitized.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn decode_public_number(value: &str) -> Option<usize> {
    if value.is_empty() {
        return None;
    }

    let mut decoded = 0usize;
    for ch in value.chars() {
        let digit = PUBLIC_ID_ALPHABET
            .iter()
            .position(|candidate| *candidate as char == ch)?;
        decoded = decoded
            .checked_mul(PUBLIC_ID_ALPHABET.len())?
            .checked_add(digit + 1)?;
    }
    Some(decoded)
}

pub fn parse_workspaces(json: &str) -> Result<Vec<Workspace>> {
    let response: WsResp = serde_json::from_str(json).map_err(|error| {
        HerdrError::InvalidJson(format!("invalid workspace list JSON: {error}"))
    })?;
    Ok(response
        .result
        .workspaces
        .into_iter()
        .map(|workspace| Workspace {
            workspace_id: workspace.workspace_id,
            label: workspace.label,
            agent_status: if workspace.agent_status.is_empty() {
                "unknown".into()
            } else {
                workspace.agent_status
            },
        })
        .collect())
}

pub fn parse_created_id(json: &str) -> Result<String> {
    let response: CrResp = serde_json::from_str(json).map_err(|error| {
        HerdrError::InvalidJson(format!("invalid workspace-create JSON: {error}"))
    })?;
    Ok(response.result.workspace.workspace_id)
}

pub fn parse_panes(json: &str) -> Result<Vec<Pane>> {
    let response: PnResp = serde_json::from_str(json)
        .map_err(|error| HerdrError::InvalidJson(format!("invalid pane list JSON: {error}")))?;
    Ok(response
        .result
        .panes
        .into_iter()
        .map(|pane| Pane {
            pane_id: pane.pane_id,
            workspace_id: pane.workspace_id,
            cwd: clean(pane.cwd),
            agent: clean(pane.agent),
            label: clean(pane.label),
            terminal_title: clean(pane.terminal_title_stripped),
        })
        .collect())
}

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_POLL: Duration = Duration::from_millis(20);
const PROCESS_GROUP_GRACE: Duration = Duration::from_millis(50);
const READER_COMPLETION_TIMEOUT: Duration = Duration::from_secs(2);

/// Stop a command and every descendant that inherited its process group. This
/// also closes pipes held by backgrounded descendants so output readers finish.
fn stop_command(child: &mut Child) {
    #[cfg(unix)]
    {
        let process_group = -(child.id() as i32);
        // On the successful path the direct child has already exited. Avoid a
        // needless grace-period delay when it had no pipe-holding descendants.
        // EPERM still means the group exists, so it must be terminated.
        let group_exists = unsafe { libc::kill(process_group, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if group_exists {
            // The direct child can already have exited; its process group may
            // still contain a background descendant holding stdout or stderr.
            unsafe { libc::kill(process_group, libc::SIGTERM) };
            thread::sleep(PROCESS_GROUP_GRACE);
            unsafe { libc::kill(process_group, libc::SIGKILL) };
        }
    }
    // Also kill the direct child explicitly. A shell can ignore SIGTERM while
    // waiting even after its descendants received the group signal.
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn command_output(
    command: &mut Command,
    label: &str,
    cancellation: Option<&AtomicBool>,
) -> Result<Output> {
    if cancellation.is_some_and(|token| token.load(Ordering::Relaxed)) {
        return Err(HerdrError::Cancelled(format!("{label} cancelled")));
    }

    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| HerdrError::Spawn(format!("spawn {label}: {error}")))?;
    let stdout = child.stdout.take().expect("stdout configured as piped");
    let stderr = child.stderr.take().expect("stderr configured as piped");
    let (stdout_sender, stdout_receiver) = std::sync::mpsc::sync_channel(1);
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stdout = stdout;
        let _ = stdout_sender.send(stdout.read_to_end(&mut bytes).map(|_| bytes));
    });
    let (stderr_sender, stderr_receiver) = std::sync::mpsc::sync_channel(1);
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stderr = stderr;
        let _ = stderr_sender.send(stderr.read_to_end(&mut bytes).map(|_| bytes));
    });
    let started = Instant::now();

    let status = loop {
        if cancellation.is_some_and(|token| token.load(Ordering::Relaxed)) {
            stop_command(&mut child);
            break Err(HerdrError::Cancelled(format!("{label} cancelled")));
        }
        if started.elapsed() >= COMMAND_TIMEOUT {
            stop_command(&mut child);
            break Err(HerdrError::Timeout(format!(
                "{label} timed out after {} seconds",
                COMMAND_TIMEOUT.as_secs()
            )));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // A shell may exit successfully after placing work in the
                // background. Stop descendants before reader joins, since they
                // may otherwise retain stdout/stderr indefinitely.
                stop_command(&mut child);
                break Ok(status);
            }
            Ok(None) => thread::sleep(COMMAND_POLL),
            Err(error) => {
                stop_command(&mut child);
                break Err(HerdrError::Spawn(format!("wait for {label}: {error}")));
            }
        }
    };

    // A descendant can escape the process group with setsid() while retaining
    // these pipe ends. Do not let that make cancellation or timeout teardown
    // wait forever: readers get one shared bounded grace period, after which
    // dropping their JoinHandles deliberately detaches them.
    let reader_deadline = Instant::now() + READER_COMPLETION_TIMEOUT;
    let receive_reader = |receiver: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>, stream| {
        let remaining = reader_deadline.saturating_duration_since(Instant::now());
        receiver
            .recv_timeout(remaining)
            .map_err(|error| match error {
                std::sync::mpsc::RecvTimeoutError::Timeout => HerdrError::Spawn(format!(
                    "read {label} {stream}: reader did not finish after process teardown"
                )),
                std::sync::mpsc::RecvTimeoutError::Disconnected => {
                    HerdrError::Spawn(format!("read {label} {stream}: reader disconnected"))
                }
            })?
            .map_err(|error| HerdrError::Spawn(format!("read {label} {stream}: {error}")))
    };
    let stdout = receive_reader(stdout_receiver, "stdout");
    let stderr = receive_reader(stderr_receiver, "stderr");

    // Join only readers that have reported completion. Dropping a JoinHandle
    // detaches a blocked reader, whose pipe will close when an escaped holder
    // eventually exits.
    if stdout.is_ok() {
        let _ = stdout_reader.join();
    }
    if stderr.is_ok() {
        let _ = stderr_reader.join();
    }
    let stdout = stdout?;
    let stderr = stderr?;
    let status = status?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[derive(Clone)]
pub struct CliHerdr {
    pub bin: String,
    cancellation: Option<Arc<AtomicBool>>,
}

impl CliHerdr {
    pub fn new(bin: String) -> Self {
        CliHerdr {
            bin,
            cancellation: None,
        }
    }

    pub fn with_cancellation(&self, cancellation: Arc<AtomicBool>) -> Self {
        CliHerdr {
            bin: self.bin.clone(),
            cancellation: Some(cancellation),
        }
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let label = format!("herdr {args:?}");
        let mut command = Command::new(&self.bin);
        command.args(args);
        let output = command_output(&mut command, &label, self.cancellation.as_deref())?;
        if !output.status.success() {
            return Err(HerdrError::Command(format!(
                "{label}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl Herdr for CliHerdr {
    fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        parse_workspaces(&self.run(&["workspace", "list"])?)
    }

    fn list_panes(&self) -> Result<Vec<Pane>> {
        parse_panes(&self.run(&["pane", "list"])?)
    }

    fn create_workspace(&self, cwd: &str, label: &str) -> Result<String> {
        parse_created_id(&self.run(&[
            "workspace",
            "create",
            "--cwd",
            cwd,
            "--label",
            label,
            "--focus",
        ])?)
    }

    fn focus_workspace(&self, id: &str) -> Result<()> {
        self.run(&["workspace", "focus", id]).map(|_| ())
    }

    fn close_workspace(&self, id: &str) -> Result<()> {
        self.run(&["workspace", "close", id]).map(|_| ())
    }

    fn close_pane(&self, id: &str) -> Result<()> {
        self.run(&["pane", "close", id]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = r#"{"result":{"type":"workspace_list","workspaces":[{"workspace_id":"w5","label":"~","agent_status":"working"},{"workspace_id":"w6","label":"/tmp","agent_status":""}]}}"#;
    const CR: &str = r#"{"result":{"workspace":{"workspace_id":"w9"},"root_pane":{"cwd":"/p"},"type":"workspace_created"}}"#;
    const PN: &str = r#"{"result":{"type":"pane_list","panes":[{"pane_id":"wE:p1","workspace_id":"wE","cwd":"/home/x/dev/api","agent":"claude","agent_status":"working","label":"api-shell","terminal_title_stripped":"π - api"},{"pane_id":"wE:p2","workspace_id":"wE","cwd":"/tmp","terminal_title_stripped":"π - tmp"},{"pane_id":"wB:pA","workspace_id":"wB"}]}}"#;

    #[test]
    fn parses_workspaces_with_status_default() {
        let workspaces = parse_workspaces(WS).unwrap();
        assert_eq!(workspaces[0].workspace_id, "w5");
        assert_eq!(workspaces[0].agent_status, "working");
        assert_eq!(workspaces[1].agent_status, "unknown");
    }

    #[test]
    fn parses_created_id() {
        assert_eq!(parse_created_id(CR).unwrap(), "w9");
    }

    #[test]
    fn parses_optional_cwd_agent_and_names() {
        let panes = parse_panes(PN).unwrap();
        assert_eq!(panes[0].workspace_id, "wE");
        assert_eq!(panes[0].cwd.as_deref(), Some("/home/x/dev/api"));
        assert_eq!(panes[0].agent.as_deref(), Some("claude"));
        assert_eq!(panes[0].display_name(), Some("api-shell"));
        assert_eq!(panes[1].agent, None);
        assert_eq!(panes[1].display_name(), Some("π - tmp"));
        assert_eq!(panes[2].cwd, None);
        assert_eq!(panes[2].display_name(), None);
    }

    #[test]
    fn accepts_null_cwd_and_ignores_blank_names() {
        let json = r#"{"result":{"panes":[{"pane_id":"w1:p1","workspace_id":"w1","cwd":null,"label":"  ","terminal_title_stripped":" title "}]}}"#;
        let panes = parse_panes(json).unwrap();
        assert_eq!(panes[0].cwd, None);
        assert_eq!(panes[0].display_name(), Some("title"));
    }

    #[test]
    fn removes_terminal_control_characters_from_pane_names() {
        let json = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","workspace_id":"w1","label":" \u001b[2J dashboard\u0007 "},
            {"pane_id":"w1:p2","workspace_id":"w1","terminal_title_stripped":" \u001b]0;build\u0007 "},
            {"pane_id":"w1:p3","workspace_id":"w1","label":"\u009b2J\u007fclean"}
        ]}}"#;
        let panes = parse_panes(json).unwrap();

        // ESC/BEL and C1 are removed, leaving their formerly-controlled text
        // printable and harmless; the prior whitespace trimming is retained.
        assert_eq!(panes[0].label.as_deref(), Some("[2J dashboard"));
        assert_eq!(panes[1].terminal_title.as_deref(), Some("]0;build"));
        assert_eq!(panes[2].label.as_deref(), Some("2Jclean"));
    }

    #[test]
    fn decodes_herdr_public_pane_numbers() {
        let pane = |suffix: &str| Pane {
            pane_id: format!("w1:p{suffix}"),
            workspace_id: "w1".into(),
            cwd: None,
            agent: None,
            label: None,
            terminal_title: None,
        };
        assert_eq!(pane("1").number(), Some(1));
        assert_eq!(pane("9").number(), Some(9));
        assert_eq!(pane("A").number(), Some(10));
        assert_eq!(pane("0").number(), Some(32));
        assert_eq!(pane("11").number(), Some(33));
        assert_eq!(pane("I").number(), None);
        assert_eq!(pane("").number(), None);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_stops_a_running_command() {
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || {
            let mut command = Command::new("sleep");
            command.arg("5");
            command_output(&mut command, "sleep", Some(&worker_cancellation))
        });

        thread::sleep(Duration::from_millis(50));
        cancellation.store(true, Ordering::Relaxed);
        let started = Instant::now();
        let error = worker.join().unwrap().unwrap_err();

        assert!(matches!(error, HerdrError::Cancelled(_)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_returns_when_setsid_descendant_holds_output_pipes() {
        struct EscapedProcess(Option<i32>);

        impl Drop for EscapedProcess {
            fn drop(&mut self) {
                let Some(pid) = self.0 else { return };
                unsafe { libc::kill(pid, libc::SIGTERM) };
                // The process is no longer our child after its parent is
                // killed, so poll briefly for init to reap it.
                for _ in 0..20 {
                    if unsafe { libc::kill(pid, 0) } == -1
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                    {
                        return;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("escaped.pid");
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn({
            let pid_file = pid_file.clone();
            move || {
                let mut command = Command::new("python3");
                command.args([
                    "-c",
                    "import os, sys, time\npid = os.fork()\nif pid:\n    os.waitpid(pid, 0)\nelse:\n    os.setsid()\n    tmp = sys.argv[1] + '.tmp'\n    open(tmp, 'w').write(str(os.getpid()))\n    os.rename(tmp, sys.argv[1])\n    time.sleep(5)",
                    pid_file.to_str().unwrap(),
                ]);
                command_output(
                    &mut command,
                    "escaped pipe holder",
                    Some(&worker_cancellation),
                )
            }
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        while !pid_file.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let escaped = EscapedProcess(Some(
            std::fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        ));
        cancellation.store(true, Ordering::Relaxed);
        let started = Instant::now();
        let error = worker.join().unwrap().unwrap_err();

        assert!(
            matches!(error, HerdrError::Spawn(message) if message.contains("reader did not finish"))
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(escaped);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_reaps_background_descendants_and_pipe_readers() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("descendant.pid");
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || {
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                &format!("sleep 5 & echo $! > {}; wait", pid_file.display()),
            ]);
            command_output(&mut command, "background sleep", Some(&worker_cancellation))
        });

        thread::sleep(Duration::from_millis(50));
        cancellation.store(true, Ordering::Relaxed);
        let started = Instant::now();
        let error = worker.join().unwrap().unwrap_err();
        assert!(matches!(error, HerdrError::Cancelled(_)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bad_json_errors_include_context() {
        let error = parse_workspaces("nope").unwrap_err().to_string();
        assert!(error.contains("workspace list"));
    }
}

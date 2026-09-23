use serde::Deserialize;
use std::fmt;
use std::io::{self, BufRead, Read, Write};
#[cfg(unix)]
use std::os::unix::{io::AsRawFd, net::UnixStream, process::CommandExt};
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

/// A tab inside a workspace. Used only to enrich pane rows with a human-readable
/// tab name for search and display; it is never part of pane identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
    pub label: Option<String>,
    /// Herdr's positional tab number; a label equal to it means "never renamed".
    pub number: Option<usize>,
    pub agent_status: Option<String>,
}

/// A live pane. Carries the directory identity for its workspace when muster
/// did not create it (root-pane cwd), plus any detected agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub agent: Option<String>,
    pub agent_status: Option<String>,
    /// User-assigned pane name (`herdr pane rename`), when present.
    pub label: Option<String>,
    pub title: Option<String>,
    /// Terminal title is a useful fallback when a pane has no explicit label.
    pub terminal_title: Option<String>,
    pub focused: bool,
    pub hidden: bool,
    pub plugin: bool,
    pub floating: bool,
    pub suppressed: bool,
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
    /// List tabs for this runtime. Optional capability: runtimes that do not
    /// support it return an empty list so pane navigation is unaffected.
    fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        Ok(Vec::new())
    }
    /// Focus an existing tab by absolute identity. Optional capability, like
    /// `focus_pane`: runtimes without a socket refuse instead of misrouting.
    fn focus_tab(&self, _id: &str) -> Result<()> {
        Err(HerdrError::Command(
            "absolute tab focus is unavailable".into(),
        ))
    }
    /// Focus an existing pane by absolute identity. This is deliberately not
    /// implemented in terms of directional or workspace focus.
    fn focus_pane(&self, _id: &str) -> Result<()> {
        Err(HerdrError::Command(
            "absolute pane focus is unavailable".into(),
        ))
    }
    /// Return only a safe foreground process name, when the runtime supports it.
    #[allow(dead_code)]
    fn process_name(&self, _id: &str) -> Result<Option<String>> {
        Err(HerdrError::Command(
            "pane process info is unavailable".into(),
        ))
    }
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
    tab_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    foreground_cwd: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    agent_status: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    terminal_title_stripped: Option<String>,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    plugin: bool,
    #[serde(default)]
    floating: bool,
    #[serde(default)]
    suppressed: bool,
}

#[derive(Deserialize)]
struct TbResp {
    result: TbResult,
}
#[derive(Deserialize)]
struct TbResult {
    tabs: Vec<TbItem>,
}
#[derive(Deserialize)]
struct TbItem {
    tab_id: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    number: Option<usize>,
    #[serde(default)]
    agent_status: Option<String>,
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

/// Remove terminal control characters (C0, DEL, C1) from text that the picker
/// will render, so pane titles, labels, and paths cannot inject escape
/// sequences into the terminal. Used for display-only strings; identity and
/// command arguments always keep the original value.
pub(crate) fn sanitize_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            !matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}' | '\u{0080}'..='\u{009f}')
        })
        .collect()
}

fn clean(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        // Herdr titles originate in terminal-controlled state. Remove every
        // terminal control range before trimming so pane text cannot inject
        // escape sequences into the picker.
        let sanitized = sanitize_text(&value);
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
            // Workspace labels are rendered as a fallback when no directory is
            // available, so apply the same terminal-control filtering as pane
            // names before they reach Ratatui.
            label: clean(Some(workspace.label)).unwrap_or_default(),
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

/// Extract only the safe process `name` field. Arguments, environment and
/// command lines are intentionally ignored even when returned by Herdr.
#[allow(dead_code)]
pub fn parse_process_name(json: &str) -> Result<Option<String>> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| HerdrError::InvalidJson(format!("invalid pane process-info JSON: {e}")))?;
    let processes = value
        .pointer("/result/processes")
        .and_then(serde_json::Value::as_array)
        .or_else(|| {
            value
                .pointer("/processes")
                .and_then(serde_json::Value::as_array)
        });
    Ok(processes.and_then(|items| items.first()).and_then(|item| {
        item.get("name")
            .and_then(serde_json::Value::as_str)
            .map(sanitize_text)
            .filter(|s| !s.trim().is_empty())
    }))
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
            tab_id: clean(pane.tab_id),
            cwd: clean(pane.cwd),
            foreground_cwd: clean(pane.foreground_cwd),
            agent: clean(pane.agent),
            agent_status: clean(pane.agent_status),
            label: clean(pane.label),
            title: clean(pane.title),
            terminal_title: clean(pane.terminal_title_stripped),
            focused: pane.focused,
            hidden: pane.hidden,
            plugin: pane.plugin,
            floating: pane.floating,
            suppressed: pane.suppressed,
        })
        .collect())
}

pub fn parse_tabs(json: &str) -> Result<Vec<TabInfo>> {
    let response: TbResp = serde_json::from_str(json)
        .map_err(|error| HerdrError::InvalidJson(format!("invalid tab list JSON: {error}")))?;
    Ok(response
        .result
        .tabs
        .into_iter()
        .map(|tab| TabInfo {
            tab_id: tab.tab_id,
            workspace_id: tab.workspace_id,
            label: clean(tab.label),
            number: tab.number,
            agent_status: clean(tab.agent_status),
        })
        .collect())
}

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_POLL: Duration = Duration::from_millis(20);
const PROCESS_GROUP_GRACE: Duration = Duration::from_millis(50);
const MAX_COMMAND_OUTPUT: usize = 8 * 1024 * 1024;

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

#[cfg(unix)]
fn set_nonblocking(stream: &impl AsRawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn drain_available(stream: &mut impl Read, output: &mut Vec<u8>) -> io::Result<()> {
    let mut buffer = [0; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                if output.len().saturating_add(count) > MAX_COMMAND_OUTPUT {
                    return Err(io::Error::other("command output exceeds 8 MiB limit"));
                }
                output.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
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
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| HerdrError::Spawn(format!("spawn {label}: {error}")))?;
    let mut stdout = child.stdout.take().expect("stdout configured as piped");
    let mut stderr = child.stderr.take().expect("stderr configured as piped");

    #[cfg(unix)]
    {
        set_nonblocking(&stdout)
            .and_then(|()| set_nonblocking(&stderr))
            .map_err(|error| {
                stop_command(&mut child);
                HerdrError::Spawn(format!("configure {label} output: {error}"))
            })?;
    }

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let started = Instant::now();
    let status = loop {
        if cancellation.is_some_and(|token| token.load(Ordering::Relaxed)) {
            stop_command(&mut child);
            return Err(HerdrError::Cancelled(format!("{label} cancelled")));
        }
        if started.elapsed() >= COMMAND_TIMEOUT {
            stop_command(&mut child);
            return Err(HerdrError::Timeout(format!(
                "{label} timed out after {} seconds",
                COMMAND_TIMEOUT.as_secs()
            )));
        }

        #[cfg(unix)]
        if let Err(error) = drain_available(&mut stdout, &mut stdout_bytes)
            .and_then(|()| drain_available(&mut stderr, &mut stderr_bytes))
        {
            stop_command(&mut child);
            return Err(HerdrError::Spawn(format!("read {label} output: {error}")));
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                // A shell may exit successfully after placing work in the
                // background. Stop descendants before returning, then retain
                // only output already available from our pipe ends. An escaped
                // descendant can keep its inherited ends open forever, but it
                // can no longer block this command or retain a reader thread.
                stop_command(&mut child);
                #[cfg(unix)]
                if let Err(error) = drain_available(&mut stdout, &mut stdout_bytes)
                    .and_then(|()| drain_available(&mut stderr, &mut stderr_bytes))
                {
                    return Err(HerdrError::Spawn(format!("read {label} output: {error}")));
                }
                break status;
            }
            Ok(None) => thread::sleep(COMMAND_POLL),
            Err(error) => {
                stop_command(&mut child);
                return Err(HerdrError::Spawn(format!("wait for {label}: {error}")));
            }
        }
    };

    #[cfg(not(unix))]
    {
        // Muster is supported only on Unix platforms, but retain a portable
        // fallback for compilation on other hosts.
        stdout
            .read_to_end(&mut stdout_bytes)
            .map_err(|error| HerdrError::Spawn(format!("read {label} stdout: {error}")))?;
        stderr
            .read_to_end(&mut stderr_bytes)
            .map_err(|error| HerdrError::Spawn(format!("read {label} stderr: {error}")))?;
    }

    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

#[derive(Clone)]
pub struct CliHerdr {
    pub bin: String,
    /// Arguments inserted before every operation (for named/remote targets).
    /// Keeping these separate from `bin` avoids shell parsing and injection.
    pub arg_prefix: Vec<String>,
    /// Optional daemon socket used for absolute pane.focus. Keeping this
    /// separate from the CLI is intentional: directional CLI focus is not an
    /// acceptable substitute for Jump Pane routing.
    pub socket: Option<String>,
    /// Only the default local target may infer the default socket. Configured
    /// targets must provide an explicit socket or are safely unsupported.
    pub default_socket_allowed: bool,
    cancellation: Option<Arc<AtomicBool>>,
}

impl CliHerdr {
    pub fn new(bin: String) -> Self {
        CliHerdr {
            bin,
            arg_prefix: Vec::new(),
            socket: None,
            default_socket_allowed: true,
            cancellation: None,
        }
    }

    pub fn with_cancellation(&self, cancellation: Arc<AtomicBool>) -> Self {
        CliHerdr {
            bin: self.bin.clone(),
            arg_prefix: self.arg_prefix.clone(),
            socket: self.socket.clone(),
            default_socket_allowed: self.default_socket_allowed,
            cancellation: Some(cancellation),
        }
    }

    pub fn with_socket(&self, socket: impl Into<String>) -> Self {
        CliHerdr {
            bin: self.bin.clone(),
            arg_prefix: self.arg_prefix.clone(),
            socket: Some(socket.into()),
            default_socket_allowed: self.default_socket_allowed,
            cancellation: self.cancellation.clone(),
        }
    }

    pub fn with_arg_prefix(&self, args: Vec<String>) -> Self {
        CliHerdr {
            bin: self.bin.clone(),
            arg_prefix: args,
            socket: self.socket.clone(),
            default_socket_allowed: self.default_socket_allowed,
            cancellation: self.cancellation.clone(),
        }
    }

    pub fn without_default_socket(&self) -> Self {
        CliHerdr {
            bin: self.bin.clone(),
            arg_prefix: self.arg_prefix.clone(),
            socket: self.socket.clone(),
            default_socket_allowed: false,
            cancellation: self.cancellation.clone(),
        }
    }

    fn socket_path(&self) -> Result<String> {
        if let Some(socket) = &self.socket {
            return Ok(socket.clone());
        }
        if !self.default_socket_allowed {
            return Err(HerdrError::Command(
                "absolute pane focus is unsupported: target has no explicit Unix socket".into(),
            ));
        }
        Ok(std::env::var("HERDR_SOCKET_PATH")
            .ok()
            .or_else(|| std::env::var("HERDR_SOCKET").ok())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".config/herdr/herdr.sock")
                    .to_string_lossy()
                    .into_owned()
            }))
    }

    #[cfg(unix)]
    fn focus_socket(&self, pane_id: &str) -> Result<()> {
        self.socket_call("pane.focus", serde_json::json!({"pane_id": pane_id}))
    }

    #[cfg(unix)]
    fn focus_tab_socket(&self, tab_id: &str) -> Result<()> {
        self.socket_call("tab.focus", serde_json::json!({"tab_id": tab_id}))
    }

    /// One absolute-focus socket round trip. `pane.focus` and `tab.focus` share
    /// the same envelope, so only the method and params differ.
    #[cfg(unix)]
    fn socket_call(&self, method: &str, params: serde_json::Value) -> Result<()> {
        let path = self.socket_path()?;
        let mut stream = UnixStream::connect(&path)
            .map_err(|e| HerdrError::Command(format!("connect Herdr socket {path}: {e}")))?;
        let request = serde_json::json!({
            "id": format!("muster-{}", std::process::id()),
            "method": method,
            "params": params
        });
        stream
            .write_all(format!("{request}\n").as_bytes())
            .map_err(|e| HerdrError::Command(format!("write {method}: {e}")))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| HerdrError::Command(format!("read {method}: {e}")))?;
        let mut response = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut response)
            .map_err(|e| HerdrError::Command(format!("read {method}: {e}")))?;
        let value: serde_json::Value = serde_json::from_str(response.trim())
            .map_err(|e| HerdrError::InvalidJson(format!("invalid {method} response: {e}")))?;
        if value.get("error").is_some() {
            return Err(HerdrError::Command(format!(
                "{method} failed: {}",
                sanitize_text(&value["error"].to_string())
            )));
        }
        Ok(())
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let label = format!("herdr {args:?}");
        let mut command = Command::new(&self.bin);
        command.args(&self.arg_prefix);
        command.args(args);
        let output = command_output(&mut command, &label, self.cancellation.as_deref())?;
        if !output.status.success() {
            return Err(HerdrError::Command(format!(
                "{label}: {}",
                sanitize_text(String::from_utf8_lossy(&output.stderr).trim()).trim()
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

    fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        parse_tabs(&self.run(&["tab", "list"])?)
    }

    fn focus_tab(&self, id: &str) -> Result<()> {
        #[cfg(unix)]
        {
            self.focus_tab_socket(id)
        }
        #[cfg(not(unix))]
        {
            let _ = id;
            Err(HerdrError::Command(
                "absolute tab focus requires a Unix Herdr socket".into(),
            ))
        }
    }

    fn focus_pane(&self, id: &str) -> Result<()> {
        #[cfg(unix)]
        {
            self.focus_socket(id)
        }
        #[cfg(not(unix))]
        {
            let _ = id;
            Err(HerdrError::Command(
                "absolute pane focus requires a Unix Herdr socket".into(),
            ))
        }
    }

    fn process_name(&self, id: &str) -> Result<Option<String>> {
        parse_process_name(&self.run(&["pane", "process-info", "--pane", id])?)
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
    fn removes_terminal_control_characters_from_workspace_labels() {
        let json = r#"{"result":{"workspaces":[{"workspace_id":"w1","label":" \u001b[2J dashboard\u009bK ","agent_status":"idle"}]}}"#;
        let workspaces = parse_workspaces(json).unwrap();

        assert_eq!(workspaces[0].label, "[2J dashboardK");
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
    fn parse_tabs_reads_tab_id_workspace_and_label() {
        let json = r#"{"id":"1","type":"tab_list","result":{"type":"tab_list","tabs":[{"tab_id":"wE:t1","workspace_id":"wE","label":"api","number":2,"agent_status":"working"},{"tab_id":"wB:tA","workspace_id":"wB"}]}}"#;
        let tabs = parse_tabs(json).unwrap();

        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].tab_id, "wE:t1");
        assert_eq!(tabs[0].workspace_id, "wE");
        assert_eq!(tabs[0].label.as_deref(), Some("api"));
        assert_eq!(tabs[0].number, Some(2));
        assert_eq!(tabs[0].agent_status.as_deref(), Some("working"));
        assert_eq!(tabs[1].tab_id, "wB:tA");
        assert_eq!(tabs[1].workspace_id, "wB");
        assert_eq!(tabs[1].label, None);
        assert_eq!(tabs[1].number, None);
        assert_eq!(tabs[1].agent_status, None);
    }

    #[test]
    fn parse_tabs_rejects_malformed_json_and_missing_tabs() {
        let cases = [
            "nope",
            "{}",
            r#"{"result":{}}"#,
            r#"{"result":{"tabs":{}}}"#,
            r#"{"result":{"tabs":[{"tab_id":7}]}}"#,
        ];

        for json in cases {
            let error = parse_tabs(json).unwrap_err();
            assert!(
                matches!(&error, HerdrError::InvalidJson(message) if message.contains("invalid tab list JSON")),
                "{json} => {error}"
            );
        }
    }

    #[test]
    fn parse_tabs_discards_blank_labels_and_sanitizes_control_characters() {
        let json = r#"{"result":{"tabs":[{"tab_id":"w1:t1","workspace_id":"w1","label":"   "},{"tab_id":"w1:t2","workspace_id":"w1","label":"\u001b[2J api \u009bK"},{"tab_id":"w1:t3","workspace_id":"w1","label":null}]}}"#;
        let tabs = parse_tabs(json).unwrap();

        assert_eq!(tabs[0].label, None);
        assert_eq!(tabs[1].label.as_deref(), Some("[2J api K"));
        assert_eq!(tabs[2].label, None);
    }

    #[test]
    fn parse_tabs_ignores_unknown_tabinfo_fields() {
        let json = r#"{"result":{"tabs":[{"tab_id":"wE:t1","workspace_id":"wE","label":"api","number":3,"focused":true,"pane_count":2,"agent_status":"working","extra":{"nested":1}}]}}"#;
        let tabs = parse_tabs(json).unwrap();

        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].tab_id, "wE:t1");
        assert_eq!(tabs[0].label.as_deref(), Some("api"));
    }

    /// A runtime that never overrides `list_tabs`; the default must stay empty
    /// so tab metadata can never affect pane navigation.
    struct TablessHerdr;

    impl Herdr for TablessHerdr {
        fn list_workspaces(&self) -> Result<Vec<Workspace>> {
            Ok(Vec::new())
        }

        fn list_panes(&self) -> Result<Vec<Pane>> {
            Ok(Vec::new())
        }

        fn create_workspace(&self, _cwd: &str, _label: &str) -> Result<String> {
            Ok(String::new())
        }

        fn focus_workspace(&self, _id: &str) -> Result<()> {
            Ok(())
        }

        fn close_workspace(&self, _id: &str) -> Result<()> {
            Ok(())
        }

        fn close_pane(&self, _id: &str) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn list_tabs_default_implementation_returns_empty() {
        assert!(TablessHerdr.list_tabs().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn focus_pane_uses_absolute_socket_contract() {
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(&mut stream), &mut request)
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(request.trim()).unwrap();
            assert_eq!(value["method"], "pane.focus");
            assert_eq!(value["params"]["pane_id"], "runtime-pane");
            stream
                .write_all(b"{\"result\":{\"pane_info\":{\"pane_id\":\"runtime-pane\"}}}\n")
                .unwrap();
        });
        let client = CliHerdr::new("unused".into()).with_socket(socket.to_string_lossy());
        client.focus_pane("runtime-pane").unwrap();
        worker.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn focus_tab_uses_absolute_socket_contract() {
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            std::io::BufRead::read_line(&mut std::io::BufReader::new(&mut stream), &mut request)
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(request.trim()).unwrap();
            assert_eq!(value["method"], "tab.focus");
            assert_eq!(value["params"]["tab_id"], "w1:t2");
            stream
                .write_all(b"{\"result\":{\"tab_info\":{\"tab_id\":\"w1:t2\"}}}\n")
                .unwrap();
        });
        let client = CliHerdr::new("unused".into()).with_socket(socket.to_string_lossy());
        client.focus_tab("w1:t2").unwrap();
        worker.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn configured_target_without_socket_is_explicitly_unsupported() {
        let client = CliHerdr::new("unused".into()).without_default_socket();
        let error = client.focus_pane("pane").unwrap_err().to_string();
        assert!(error.contains("unsupported") && error.contains("explicit"));
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
            tab_id: None,
            cwd: None,
            foreground_cwd: None,
            agent: None,
            agent_status: None,
            label: None,
            title: None,
            terminal_title: None,
            focused: false,
            hidden: false,
            plugin: false,
            floating: false,
            suppressed: false,
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
    fn cancellation_returns_promptly_when_setsid_descendant_streams_output() {
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
                    "import os, sys, time\npid = os.fork()\nif pid:\n    os.waitpid(pid, 0)\nelse:\n    os.setsid()\n    tmp = sys.argv[1] + '.tmp'\n    open(tmp, 'w').write(str(os.getpid()))\n    os.rename(tmp, sys.argv[1])\n    while True:
        sys.stdout.write('x' * 1024)
        sys.stdout.flush()
        time.sleep(0.01)",
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

        assert!(matches!(error, HerdrError::Cancelled(_)));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(escaped);
    }

    #[cfg(unix)]
    #[test]
    fn excessive_command_output_is_bounded() {
        let started = Instant::now();
        let mut command = Command::new("python3");
        command.args([
            "-c",
            "import sys
while True: sys.stdout.write('x' * 8192)",
        ]);
        let error = command_output(&mut command, "unbounded output", None).unwrap_err();

        assert!(matches!(error, HerdrError::Spawn(message) if message.contains("8 MiB limit")));
        assert!(started.elapsed() < Duration::from_secs(5));
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

//! End-to-end coverage for the real `--jump-pane` entry point.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

fn fake_cli(dir: &Path) -> PathBuf {
    let path = dir.join("fake-herdr.sh");
    fs::write(&path, r##"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_HERDR_LOG"
case "$*" in
  *"tab list"*)
    if [ "${FAKE_FAIL_TABS:-0}" = 1 ]; then echo tab-list-unavailable >&2; exit 9; fi
    printf '%s\n' '{"result":{"type":"tab_list","tabs":[{"tab_id":"tab-one","workspace_id":"workspace-one","label":"one tab label"},{"tab_id":"tab-two","workspace_id":"workspace-two","label":"two tab label"},{"tab_id":"tab-three","workspace_id":"workspace-two","label":"three tab label"}]}}'
    ;;
  *"--session one"*)
    if [ "${FAKE_FAIL_ONE:-0}" = 1 ]; then echo unavailable >&2; exit 7; fi
    printf '%s\n' '{"result":{"panes":[{"pane_id":"collision","workspace_id":"workspace-one","tab_id":"tab-one","cwd":"/stale/Downloads","foreground_cwd":"/one/project","label":"one-agent","agent":"claude","agent_status":"working"}]}}'
    ;;
  *"--session two"*)
    printf '%s\n' '{"result":{"panes":[{"pane_id":"collision","workspace_id":"workspace-two","tab_id":"tab-two","cwd":"/two/project","foreground_cwd":"/Users/test/Downloads","label":"two-shell"},{"pane_id":"ordinary","workspace_id":"workspace-two","tab_id":"tab-three","cwd":"/two/other","label":"ordinary"}]}}'
    ;;
  *)
    printf '%s\n' '{"result":{"panes":[]}}'
    ;;
esac
"##).unwrap();
    Command::new("chmod").arg("+x").arg(&path).status().unwrap();
    path
}

fn socket(path: &Path) -> (PathBuf, thread::JoinHandle<String>) {
    let listener = UnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let path = path.to_path_buf();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut request = String::new();
                    let _ = std::io::BufRead::read_line(
                        &mut std::io::BufReader::new(&mut stream),
                        &mut request,
                    );
                    let _ = stream.write_all(b"{\"result\":{}}\n");
                    return request;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return String::new();
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return String::new(),
            }
        }
    });
    (path, handle)
}

fn launch(
    root: &Path,
    cli: &Path,
    default_socket: &Path,
    mode: &str,
    fail_one: bool,
) -> (String, String) {
    launch_with(root, cli, default_socket, mode, fail_one, false)
}

fn launch_with(
    root: &Path,
    cli: &Path,
    default_socket: &Path,
    mode: &str,
    fail_one: bool,
    fail_tabs: bool,
) -> (String, String) {
    let log = root.join("herdr.log");
    let script = r#"
import os, pty, select, sys, time
binary, mode = sys.argv[1:]
key = (b'Downloads\n' if mode in ('search', 'tabfail') else (b'ordinary\n' if mode == 'ordinary' else (b'two tab label\n' if mode == 'tab' else b'\x1b')))
env = os.environ.copy()
pid, fd = pty.fork()
if pid == 0: os.execve(binary, [binary, '--jump-pane'], env)
time.sleep(.25)
os.write(fd, key)
out = b''
deadline = time.monotonic() + 3
while time.monotonic() < deadline:
    ready, _, _ = select.select([fd], [], [], .05)
    if ready:
        try: out += os.read(fd, 8192)
        except OSError: pass
    waited, status = os.waitpid(pid, os.WNOHANG)
    if waited: break
else:
    os.kill(pid, 9); os.waitpid(pid, 0)
    print(out.decode('utf-8', 'replace'), file=sys.stderr)
    raise SystemExit('jump pane exceeded bounded timeout')
print(out.decode('utf-8', 'replace'))
"#;
    let output = Command::new("python3")
        .args(["-c", script, env!("CARGO_BIN_EXE_herdr-muster"), mode])
        .env("HERDR_BIN_PATH", cli)
        .env("HERDR_SOCKET_PATH", default_socket)
        .env("HERDR_PLUGIN_CONFIG_DIR", root)
        .env("HERDR_PLUGIN_STATE_DIR", root)
        .env("FAKE_HERDR_LOG", &log)
        .env("FAKE_FAIL_ONE", if fail_one { "1" } else { "0" })
        .env("FAKE_FAIL_TABS", if fail_tabs { "1" } else { "0" })
        .output()
        .expect("run controlling-PTY fixture");
    assert!(
        output.status.success(),
        "PTY fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        fs::read_to_string(log).unwrap_or_default(),
    )
}

fn config(root: &Path, one: &Path, two: &Path) -> PathBuf {
    let config = root.join("config.toml");
    fs::write(&config, format!(
        "[jump_pane]\nsessions = [{{name=\"one\", socket=\"{}\", command=\"{}\", remote=\"one.example\"}}, {{name=\"two\", socket=\"{}\", command=\"{}\", remote=\"two.example\"}}]\n",
        one.display(), root.join("fake-herdr.sh").display(), two.display(), root.join("fake-herdr.sh").display())).unwrap();
    config
}

#[test]
fn jump_pane_e2e_routes_colliding_ids_and_never_runs_workspace_actions() {
    let root = tempfile::tempdir().unwrap();
    let cli = fake_cli(root.path());
    let one_path = root.path().join("one.sock");
    let two_path = root.path().join("two.sock");
    let (_one, one_requests) = socket(&one_path);
    let (_two, two_requests) = socket(&two_path);
    let _config = config(root.path(), &one_path, &two_path);
    let (_output, calls) = launch(root.path(), &cli, &one_path, "search", false);
    let one = one_requests.join().unwrap();
    let two = two_requests.join().unwrap();
    assert!(two.contains("pane.focus") && two.contains("collision"));
    assert!(!one.contains("pane.focus"));
    assert!(!calls.contains("workspace create"));
    assert!(!calls.contains("workspace focus"));
    assert!(!calls.contains("pane create"));
    assert!(!calls.contains("pane close"));
}

#[test]
fn jump_pane_e2e_escape_and_scoped_runtime_failure_keep_other_rows() {
    let root = tempfile::tempdir().unwrap();
    let cli = fake_cli(root.path());
    let one_path = root.path().join("one.sock");
    let two_path = root.path().join("two.sock");
    let (_one, one_requests) = socket(&one_path);
    let (_two, two_requests) = socket(&two_path);
    let _config = config(root.path(), &one_path, &two_path);
    let (output, calls) = launch(root.path(), &cli, &one_path, "escape", true);
    assert!(output.contains("two-shell") || output.contains("ordinary"));
    assert!(calls.contains("--session one") && calls.contains("--session two"));
    assert!(!one_requests.join().unwrap().contains("pane.focus"));
    assert!(!two_requests.join().unwrap().contains("pane.focus"));
}

#[test]
fn jump_pane_e2e_selects_ordinary_no_agent_pane() {
    let root = tempfile::tempdir().unwrap();
    let cli = fake_cli(root.path());
    let one_path = root.path().join("one.sock");
    let two_path = root.path().join("two.sock");
    let (_one, one_requests) = socket(&one_path);
    let (_two, two_requests) = socket(&two_path);
    let _config = config(root.path(), &one_path, &two_path);
    let (_output, _calls) = launch(root.path(), &cli, &one_path, "ordinary", false);
    assert!(!one_requests.join().unwrap().contains("pane.focus"));
    assert!(two_requests.join().unwrap().contains("ordinary"));
}

#[test]
fn tab_label_search_focuses_exact_pane() {
    let root = tempfile::tempdir().unwrap();
    let cli = fake_cli(root.path());
    let one_path = root.path().join("one.sock");
    let two_path = root.path().join("two.sock");
    let (_one, one_requests) = socket(&one_path);
    let (_two, two_requests) = socket(&two_path);
    let _config = config(root.path(), &one_path, &two_path);
    let (_output, calls) = launch(root.path(), &cli, &one_path, "tab", false);

    assert!(
        calls.contains("tab list"),
        "tab list was not invoked: {calls}"
    );
    // "two tab label" belongs to the tab of session two's colliding pane.
    assert!(two_requests.join().unwrap().contains("collision"));
    assert!(!one_requests.join().unwrap().contains("pane.focus"));
    assert!(!calls.contains("workspace create"));
    assert!(!calls.contains("workspace focus"));
}

#[test]
fn tab_list_failure_still_lists_and_focuses_panes() {
    let root = tempfile::tempdir().unwrap();
    let cli = fake_cli(root.path());
    let one_path = root.path().join("one.sock");
    let two_path = root.path().join("two.sock");
    let (_one, one_requests) = socket(&one_path);
    let (_two, two_requests) = socket(&two_path);
    let _config = config(root.path(), &one_path, &two_path);
    let (output, calls) = launch_with(root.path(), &cli, &one_path, "tabfail", false, true);

    assert!(calls.contains("tab list"));
    assert!(output.contains("two-shell") || output.contains("collision"));
    assert!(two_requests.join().unwrap().contains("collision"));
    assert!(!one_requests.join().unwrap().contains("pane.focus"));
}

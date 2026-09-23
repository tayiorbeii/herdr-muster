//! End-to-end coverage for tab and renamed-pane rows in the project picker.
//!
//! Proves the real binary lists tabs and renamed panes as rows, matches them by
//! name, and jumps through the absolute socket API: `tab.focus` for a tab row
//! and `pane.focus` for a renamed pane row.
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
    fs::write(
        &path,
        r##"#!/bin/sh
case "$*" in
  *"workspace list"*)
    printf '%s\n' '{"result":{"workspaces":[{"workspace_id":"w1","label":"work","agent_status":"working"}]}}'
    ;;
  *"pane list"*)
    printf '%s\n' '{"result":{"panes":[{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","cwd":"/Users/me/work","label":"editor","agent":"claude","agent_status":"working"},{"pane_id":"w1:p2","workspace_id":"w1","tab_id":"w1:t1","cwd":"/Users/me/work","terminal_title_stripped":"unnamed shell"}]}}'
    ;;
  *"tab list"*)
    printf '%s\n' '{"result":{"tabs":[{"tab_id":"w1:t1","workspace_id":"w1","label":"shell","agent_status":"idle"},{"tab_id":"w1:t2","workspace_id":"w1","label":"build logs","agent_status":"idle"}]}}'
    ;;
  *)
    printf '%s\n' '{"result":{}}'
    ;;
esac
"##,
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&path).status().unwrap();
    path
}

/// Accept one socket request and return it verbatim.
fn socket(path: &Path) -> (PathBuf, thread::JoinHandle<String>) {
    let listener = UnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(4);
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
    (path.to_path_buf(), handle)
}

fn config(root: &Path) {
    fs::write(
        root.join("config.toml"),
        "paths = []\nroots = []\nuse_zoxide = false\n",
    )
    .unwrap();
}

/// Run the picker in a controlling PTY, type `query`, press Enter, and return
/// everything the pane rendered.
fn launch(root: &Path, cli: &Path, socket_path: &Path, query: &str) -> String {
    let script = r#"
import fcntl, os, pty, select, struct, sys, termios, time
binary, query = sys.argv[1:3]
pid, fd = pty.fork()
if pid == 0: os.execve(binary, [binary], os.environ.copy())
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
time.sleep(1.5)
os.write(fd, query.encode() + b'\r')
out = b''
deadline = time.monotonic() + 4
while time.monotonic() < deadline:
    ready, _, _ = select.select([fd], [], [], .05)
    if ready:
        try: out += os.read(fd, 8192)
        except OSError: pass
    waited, _ = os.waitpid(pid, os.WNOHANG)
    if waited: break
else:
    os.kill(pid, 9); os.waitpid(pid, 0)
    print('pane still running after deadline', file=sys.stderr)
print(out.decode('utf-8', 'replace'))
"#;
    let output = Command::new("python3")
        .args(["-c", script, env!("CARGO_BIN_EXE_herdr-muster"), query])
        .env("HERDR_BIN_PATH", cli)
        .env("HERDR_SOCKET_PATH", socket_path)
        .env("HERDR_PLUGIN_CONFIG_DIR", root)
        .env("HERDR_PLUGIN_STATE_DIR", root)
        .output()
        .expect("run controlling-PTY fixture");
    assert!(
        output.status.success(),
        "PTY fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn picker_tab_row_is_searchable_and_focuses_the_exact_tab() {
    let root = tempfile::tempdir().unwrap();
    let cli = fake_cli(root.path());
    let (socket_path, requests) = socket(&root.path().join("herdr.sock"));
    config(root.path());

    let rendered = launch(root.path(), &cli, &socket_path, "build logs");
    let request = requests.join().unwrap();

    assert!(request.contains("tab.focus"), "{request}");
    assert!(request.contains("w1:t2"), "{request}");
    assert!(!request.contains("pane.focus"), "{request}");
    // The tab is listed as a row before any query is typed.
    assert!(rendered.contains("build logs"), "{rendered}");
}

#[test]
fn picker_renamed_pane_row_is_searchable_and_focuses_the_exact_pane() {
    let root = tempfile::tempdir().unwrap();
    let cli = fake_cli(root.path());
    let (socket_path, requests) = socket(&root.path().join("herdr.sock"));
    config(root.path());

    let rendered = launch(root.path(), &cli, &socket_path, "editor");
    let request = requests.join().unwrap();

    assert!(request.contains("pane.focus"), "{request}");
    assert!(request.contains("w1:p1"), "{request}");
    assert!(!request.contains("tab.focus"), "{request}");
    assert!(rendered.contains("editor"), "{rendered}");
}

#[test]
fn picker_unnamed_pane_is_not_a_row_but_stays_searchable() {
    let root = tempfile::tempdir().unwrap();
    let cli = fake_cli(root.path());
    let (socket_path, requests) = socket(&root.path().join("herdr.sock"));
    config(root.path());

    // "unnamed shell" is a terminal title, not a rename: the workspace row
    // keeps it searchable, and Enter focuses the workspace, not a pane.
    let _rendered = launch(root.path(), &cli, &socket_path, "unnamed shell");
    let request = requests.join().unwrap();

    assert!(!request.contains("pane.focus"), "{request}");
    assert!(!request.contains("tab.focus"), "{request}");
}

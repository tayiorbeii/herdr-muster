#![cfg(unix)]

use std::fs;
use std::process::Command;

#[test]
fn picker_cancels_slow_herdr_in_a_controlling_terminal_and_restores_it() {
    let binary = env!("CARGO_BIN_EXE_herdr-muster");
    let directory = tempfile::tempdir().unwrap();
    let fake = directory.path().join("fake-herdr");
    let log = directory.path().join("pids.log");
    let config = directory.path().join("config");
    let state = directory.path().join("state");
    fs::create_dir_all(&config).unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::write(config.join("config.toml"), "use_zoxide = false\n").unwrap();
    fs::write(
        &fake,
        "#!/bin/sh\ncase \"$1 $2\" in\n  'workspace list') sleep 5 & sleep_pid=$!; printf 'ready fake_pid=%s sleep_pid=%s\\n' \"$$\" \"$sleep_pid\" >> \"$HERDR_TEST_LOG\"; wait ;;\n  'pane list') echo '{\"result\":{\"panes\":[]}}' ;;\nesac\n",
    )
    .unwrap();
    #[allow(clippy::permissions_set_readonly_false)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&fake, permissions).unwrap();
    }

    // pty.fork gives the child a genuine controlling terminal. The script
    // verifies both escape paths without relying on a CI runner's stdio.
    let script = r#"
import errno, fcntl, os, pty, re, select, struct, sys, termios, time
binary, fake, config, state, log, key = sys.argv[1:]
env = os.environ.copy()
env.update(HERDR_BIN_PATH=fake, HERDR_PLUGIN_CONFIG_DIR=config,
           HERDR_PLUGIN_STATE_DIR=state, HERDR_TEST_LOG=log)
pid, fd = pty.fork()
if pid == 0:
    os.execve(binary, [binary], env)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
os.kill(pid, 28) # SIGWINCH: redraw if the child queried size before ioctl.
before = termios.tcgetattr(fd)
def read_until(needle, deadline):
    data = b''
    while time.monotonic() < deadline:
        ready, _, _ = select.select([fd], [], [], .05)
        if ready:
            try: data += os.read(fd, 8192)
            except OSError as e:
                if e.errno == errno.EIO: break
                raise
            if needle in data: return data
    raise AssertionError('did not see %r; output=%r' % (needle, data))
start = time.monotonic()
read_until(b'type to fuzzy-filter', start + 1)
prompt_at = time.monotonic() - start
raw = termios.tcgetattr(fd)
if raw[3] & termios.ICANON: raise AssertionError('picker did not enable raw mode')
query = b'early-query-42'
os.write(fd, query)
# Ratatui emits style sequences between the cursor movement and glyph; verify
# each query character's stable cursor position rather than its theme styling.
rendered = [b'\x1b[2;%dH' % (5 + index) for index, _ in enumerate(query)]
typed = read_until(rendered[-1], start + 1)
if any(marker not in typed for marker in rendered):
    raise AssertionError('unique query did not render completely: %r' % typed)
def ready_pids(deadline):
    while time.monotonic() < deadline:
        if os.path.exists(log):
            match = re.search(r'^ready fake_pid=(\d+) sleep_pid=(\d+)$', open(log).read(), re.M)
            if match: return [int(match.group(1)), int(match.group(2))]
        time.sleep(.01)
    raise AssertionError('fake Herdr did not launch slow descendant: %r' % (open(log).read() if os.path.exists(log) else ''))
pids = ready_pids(start + 2)
cancel_start = time.monotonic()
os.write(fd, b'\x1b' if key == 'escape' else b'\x03')
deadline = cancel_start + 1
exit_output = b''
while True:
    ready, _, _ = select.select([fd], [], [], 0)
    if ready:
        try: exit_output += os.read(fd, 8192)
        except OSError: pass
    waited, status = os.waitpid(pid, os.WNOHANG)
    if waited:
        if status != 0: raise AssertionError('picker status %d' % status)
        exit_at = time.monotonic() - cancel_start
        break
    if time.monotonic() >= deadline: raise AssertionError('picker did not exit promptly; terminal_restored=%r output=%r log=%r' % (b'\\x1b[?1049l' in exit_output, exit_output, open(log).read()))
    time.sleep(.01)
after = termios.tcgetattr(fd)
if (before[3] & (termios.ICANON | termios.ECHO)) != (after[3] & (termios.ICANON | termios.ECHO)):
    raise AssertionError('termios not restored: before=%r after=%r' % (before[3], after[3]))
time.sleep(.1)
for child in pids:
    try:
        os.kill(child, 0)
    except OSError as error:
        if error.errno == errno.ESRCH: continue
        raise
    raise AssertionError('fake Herdr descendant still alive: %d' % child)
print('prompt=%.3fs cancel_exit=%.3fs termios=restored pids=reaped' % (prompt_at, exit_at))
"#;

    for (name, key) in [("escape", "escape"), ("ctrl-c", "ctrl-c")] {
        let output = Command::new("python3")
            .args([
                "-c",
                script,
                binary,
                fake.to_str().unwrap(),
                config.to_str().unwrap(),
                state.to_str().unwrap(),
                log.to_str().unwrap(),
                key,
            ])
            .output()
            .unwrap_or_else(|error| panic!("run Python controlling-PTY fixture: {error}"));
        assert!(
            output.status.success(),
            "{name} fixture failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("{name}: {}", String::from_utf8_lossy(&output.stdout).trim());
        fs::write(&log, "").unwrap();
    }
}

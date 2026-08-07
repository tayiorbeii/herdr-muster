use crate::config::{expand_tilde, Config};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub display: String,
}

pub fn basename(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

pub fn collapse_home(p: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = p.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

fn cancelled(cancellation: &AtomicBool) -> bool {
    cancellation.load(Ordering::Relaxed)
}

fn git_repos_under_with_checkpoint<F>(
    root: &Path,
    cancellation: &AtomicBool,
    checkpoint: &mut F,
) -> Option<Vec<PathBuf>>
where
    F: FnMut(),
{
    if cancelled(cancellation) {
        return None;
    }
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return Some(out);
    };
    for entry in entries {
        checkpoint();
        if cancelled(cancellation) {
            return None;
        }
        let Ok(entry) = entry else { continue };
        let p = entry.path();
        if is_project_root_with_cancellation(&p, cancellation)? {
            out.push(p);
        }
    }
    Some(out)
}

/// A real git repo root worth suggesting: an existing dir whose basename is not
/// hidden and whose `.git` is a directory. Excludes hidden dirs (`.claude`,
/// `.git`), and linked worktrees / submodules (their `.git` is a *file*).
fn is_project_root_with_cancellation(p: &Path, cancellation: &AtomicBool) -> Option<bool> {
    if cancelled(cancellation) {
        return None;
    }
    if !p.is_dir() || basename(p).starts_with('.') {
        return Some(false);
    }
    if cancelled(cancellation) {
        return None;
    }
    Some(p.join(".git").is_dir())
}

#[allow(dead_code)]
pub fn is_project_root(p: &Path) -> bool {
    let cancellation = AtomicBool::new(false);
    is_project_root_with_cancellation(p, &cancellation).unwrap_or(false)
}

fn finalize(raw: Vec<PathBuf>, cancellation: &AtomicBool) -> Option<Vec<Candidate>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for p in raw {
        if cancelled(cancellation) {
            return None;
        }
        let Ok(canon) = std::fs::canonicalize(&p) else {
            continue;
        };
        if cancelled(cancellation) {
            return None;
        }
        if !canon.is_dir() {
            continue;
        }
        if seen.insert(canon.clone()) {
            out.push(Candidate {
                // Display strings reach Ratatui raw, so strip terminal control
                // characters from directory names (a hostile checkout or
                // zoxide entry must not be able to inject escape sequences).
                // The path itself is preserved untouched for identity.
                display: crate::herdr::sanitize_text(&collapse_home(&canon)),
                path: canon,
            });
        }
    }
    Some(out)
}

fn gather_with_checkpoint<F>(
    cfg: &Config,
    zoxide_lines: &[String],
    cancellation: &AtomicBool,
    checkpoint: &mut F,
) -> Option<Vec<Candidate>>
where
    F: FnMut(),
{
    let mut raw: Vec<PathBuf> = Vec::new();
    // Explicit paths bypass the repo-root filter — user opted in by naming them.
    for p in &cfg.paths {
        checkpoint();
        if cancelled(cancellation) {
            return None;
        }
        raw.push(expand_tilde(p));
    }
    // roots + zoxide are noisy: keep only git repo roots.
    for r in &cfg.roots {
        checkpoint();
        if cancelled(cancellation) {
            return None;
        }
        raw.extend(git_repos_under_with_checkpoint(
            &expand_tilde(r),
            cancellation,
            checkpoint,
        )?);
    }
    if cfg.use_zoxide {
        for l in zoxide_lines {
            checkpoint();
            if cancelled(cancellation) {
                return None;
            }
            let p = PathBuf::from(l);
            if is_project_root_with_cancellation(&p, cancellation)? {
                raw.push(p);
            }
        }
    }
    finalize(raw, cancellation)
}

/// Returns `None` when cancellation interrupts discovery, so callers do not
/// mistake a partial traversal for a complete project snapshot.
pub fn gather(
    cfg: &Config,
    zoxide_lines: &[String],
    cancellation: &AtomicBool,
) -> Option<Vec<Candidate>> {
    gather_with_checkpoint(cfg, zoxide_lines, cancellation, &mut || {})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn git_repos_under_finds_only_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("proj");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(tmp.path().join("plain")).unwrap();
        let cancellation = AtomicBool::new(false);
        assert_eq!(
            git_repos_under_with_checkpoint(tmp.path(), &cancellation, &mut || {}),
            Some(vec![repo])
        );
    }

    #[test]
    fn gather_dedups_and_drops_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        fs::create_dir_all(&a).unwrap();
        let cfg = Config {
            paths: vec![
                a.to_string_lossy().to_string(),
                a.to_string_lossy().to_string(),
            ],
            roots: vec![],
            use_zoxide: true,
        };
        let z = vec![
            a.to_string_lossy().to_string(),
            tmp.path().join("ghost").to_string_lossy().to_string(),
        ];
        let cancellation = AtomicBool::new(false);
        let got = gather(&cfg, &z, &cancellation).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, fs::canonicalize(&a).unwrap());
    }

    #[test]
    fn gather_stops_promptly_when_cancelled_mid_root_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        fs::create_dir_all(root.join("repo/.git")).unwrap();
        fs::create_dir_all(root.join("plain")).unwrap();
        let cfg = Config {
            paths: vec![],
            roots: vec![root.to_string_lossy().to_string()],
            use_zoxide: false,
        };
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = cancellation.clone();
        let (at_entry, entry_reached) = mpsc::sync_channel(1);
        let (resume, resume_worker) = mpsc::sync_channel(1);
        let (result_sender, result_receiver) = mpsc::sync_channel(1);

        let worker = thread::spawn(move || {
            let mut checkpoints = 0;
            let result = gather_with_checkpoint(&cfg, &[], &worker_cancellation, &mut || {
                checkpoints += 1;
                // The first checkpoint is before the root traversal; the
                // second is after read_dir yielded its first filesystem entry.
                if checkpoints == 2 {
                    at_entry.send(()).unwrap();
                    resume_worker.recv().unwrap();
                }
            });
            result_sender.send(result).unwrap();
        });

        entry_reached.recv_timeout(Duration::from_secs(1)).unwrap();
        cancellation.store(true, Ordering::Relaxed);
        resume.send(()).unwrap();
        assert_eq!(
            result_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            None
        );
        worker.join().unwrap();
    }

    #[test]
    fn is_project_root_rules() {
        let tmp = tempfile::tempdir().unwrap();
        // real repo root: .git is a dir
        let repo = tmp.path().join("proj");
        fs::create_dir_all(repo.join(".git")).unwrap();
        assert!(is_project_root(&repo));
        // hidden basename dropped
        let hidden = tmp.path().join(".claude");
        fs::create_dir_all(hidden.join(".git")).unwrap();
        assert!(!is_project_root(&hidden));
        // linked worktree: .git is a file, not a dir
        let wt = tmp.path().join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: /somewhere\n").unwrap();
        assert!(!is_project_root(&wt));
        // plain dir with no .git
        let plain = tmp.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert!(!is_project_root(&plain));
    }

    #[test]
    fn basename_and_collapse() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(basename(Path::new("/x/y/proj")), "proj");
        assert_eq!(collapse_home(&home.join("dev")), "~/dev");
    }

    #[test]
    fn gather_sanitizes_display_but_keeps_the_real_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("api\u{1b}[2J");
        fs::create_dir_all(&dir).unwrap();
        let cfg = Config {
            paths: vec![dir.to_string_lossy().to_string()],
            roots: vec![],
            use_zoxide: false,
        };
        let cancellation = AtomicBool::new(false);
        let got = gather(&cfg, &[], &cancellation).unwrap();

        assert_eq!(got.len(), 1);
        assert!(
            !got[0].display.contains('\u{1b}'),
            "display escaped: {:?}",
            got[0].display
        );
        assert_eq!(got[0].path, fs::canonicalize(&dir).unwrap());
    }
}

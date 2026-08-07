use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    map: HashMap<String, String>,
    /// Workspace ids ordered most-recently-focused first. The picker ranks its
    /// open group by this list, so Enter fast-tracks back to the workspace the
    /// picker was opened from and Down+Enter reaches the one before it.
    #[serde(default)]
    mru: Vec<String>,
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn key(directory: &Path) -> String {
    normalize_path(directory).to_string_lossy().into_owned()
}

impl Registry {
    pub fn load(path: &Path) -> Registry {
        let registry: Registry = match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Registry::default(),
        };

        // Older registries may contain both a symlink spelling and its real
        // path. Collapse aliases deterministically: prefer a non-symlink
        // spelling, then lexical path/workspace order. This avoids HashMap
        // iteration deciding a project's identity during migration.
        let mut entries: Vec<_> = registry.map.into_iter().collect();
        entries.sort_by(
            |(left_path, left_workspace), (right_path, right_workspace)| {
                let left_is_symlink = fs::symlink_metadata(left_path)
                    .map(|metadata| metadata.file_type().is_symlink())
                    .unwrap_or(false);
                let right_is_symlink = fs::symlink_metadata(right_path)
                    .map(|metadata| metadata.file_type().is_symlink())
                    .unwrap_or(false);
                (left_is_symlink, left_path, left_workspace).cmp(&(
                    right_is_symlink,
                    right_path,
                    right_workspace,
                ))
            },
        );
        let mut map = HashMap::new();
        for (directory, workspace) in entries {
            map.entry(key(Path::new(&directory))).or_insert(workspace);
        }
        Registry {
            map,
            mru: registry.mru,
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;

        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(text.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        // The replacement updates the parent directory entry. Sync it too so
        // a successful save is durable across a power loss, not merely atomic.
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    #[cfg(test)]
    pub fn workspace_for(&self, directory: &Path) -> Option<&String> {
        self.map.get(&key(directory))
    }

    pub fn bind(&mut self, directory: &Path, workspace: &str) {
        self.map.insert(key(directory), workspace.to_string());
    }

    /// Remove a binding only if it still identifies the workspace being closed.
    pub fn unbind_if_bound(&mut self, directory: &Path, workspace: &str) -> bool {
        let directory = key(directory);
        if self
            .map
            .get(&directory)
            .is_some_and(|bound| bound == workspace)
        {
            self.map.remove(&directory);
            true
        } else {
            false
        }
    }

    /// Mark a workspace as most-recently used, moving it to the front of the
    /// picker order. Returns whether the ordering changed.
    pub fn touch(&mut self, workspace_id: &str) -> bool {
        if self.mru.first().map(String::as_str) == Some(workspace_id) {
            return false;
        }
        self.mru.retain(|id| id != workspace_id);
        self.mru.insert(0, workspace_id.to_string());
        true
    }

    /// Forget a workspace's recency, typically when its workspace is closed.
    pub fn forget(&mut self, workspace_id: &str) -> bool {
        let before = self.mru.len();
        self.mru.retain(|id| id != workspace_id);
        before != self.mru.len()
    }

    /// Most-recently-used workspace ids, newest first.
    pub fn mru(&self) -> &[String] {
        &self.mru
    }

    pub fn reconcile(&mut self, live: &HashSet<String>) -> bool {
        let before = self.map.len();
        self.map.retain(|_, workspace| live.contains(workspace));
        let mru_before = self.mru.len();
        self.mru.retain(|workspace| live.contains(workspace));
        before != self.map.len() || mru_before != self.mru.len()
    }

    pub fn live_map(&self) -> HashMap<PathBuf, String> {
        self.map
            .iter()
            .map(|(directory, workspace)| (PathBuf::from(directory), workspace.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_unbind_lookup() {
        let mut registry = Registry::default();
        registry.bind(Path::new("/a"), "w1");
        assert_eq!(
            registry.workspace_for(Path::new("/a")).map(String::as_str),
            Some("w1")
        );
        assert!(registry.unbind_if_bound(Path::new("/a"), "w1"));
        assert!(registry.workspace_for(Path::new("/a")).is_none());
    }

    #[test]
    fn reconcile_drops_dead() {
        let mut registry = Registry::default();
        registry.bind(Path::new("/a"), "w1");
        registry.bind(Path::new("/b"), "w2");
        let live: HashSet<String> = ["w2".to_string()].into_iter().collect();
        assert!(registry.reconcile(&live));
        assert!(registry.workspace_for(Path::new("/a")).is_none());
        assert_eq!(
            registry.workspace_for(Path::new("/b")).map(String::as_str),
            Some("w2")
        );
    }

    #[test]
    fn mru_touch_forget_and_reconcile() {
        let mut registry = Registry::default();
        assert!(registry.touch("w1"));
        assert!(!registry.touch("w1"));
        assert!(registry.touch("w2"));
        assert_eq!(registry.mru(), &["w2".to_string(), "w1".to_string()]);
        assert!(registry.touch("w1"));
        assert_eq!(registry.mru(), &["w1".to_string(), "w2".to_string()]);

        let live: HashSet<String> = ["w2".to_string()].into_iter().collect();
        assert!(registry.reconcile(&live));
        assert_eq!(registry.mru(), &["w2".to_string()]);
        assert!(!registry.forget("w3"));
        assert!(registry.forget("w2"));
        assert!(registry.mru().is_empty());
    }

    #[test]
    fn save_load_roundtrip_is_atomic() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sub").join("state.json");
        fs::create_dir(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"map":{"/old":"w0"}}"#).unwrap();

        let mut registry = Registry::default();
        registry.bind(Path::new("/a"), "w1");
        registry.touch("w1");
        registry.save(&path).unwrap();

        let loaded = Registry::load(&path);
        assert_eq!(
            loaded.workspace_for(Path::new("/a")).map(String::as_str),
            Some("w1")
        );
        assert_eq!(loaded.mru(), &["w1".to_string()]);
        assert!(loaded.workspace_for(Path::new("/old")).is_none());
        assert!(fs::read_dir(path.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")));
    }

    #[cfg(unix)]
    #[test]
    fn save_does_not_follow_former_predictable_temp_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let target = directory.path().join("target");
        fs::write(&target, "do not overwrite").unwrap();
        let former_temporary = directory
            .path()
            .join(format!(".state.json.{}.tmp", std::process::id()));
        symlink(&target, &former_temporary).unwrap();

        let mut registry = Registry::default();
        registry.bind(Path::new("/a"), "w1");
        registry.save(&path).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "do not overwrite");
        assert_eq!(fs::read_link(&former_temporary).unwrap(), target);
    }

    #[test]
    fn unbind_if_bound_preserves_a_newer_rebinding() {
        let mut registry = Registry::default();
        registry.bind(Path::new("/a"), "w1");
        registry.bind(Path::new("/a"), "w2");

        assert!(!registry.unbind_if_bound(Path::new("/a"), "w1"));
        assert_eq!(
            registry.workspace_for(Path::new("/a")).map(String::as_str),
            Some("w2")
        );
        assert!(registry.unbind_if_bound(Path::new("/a"), "w2"));
        assert!(registry.workspace_for(Path::new("/a")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn loading_symlink_aliases_prefers_the_canonical_legacy_key() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let alias = directory.path().join("alias");
        fs::create_dir(&real).unwrap();
        symlink(&real, &alias).unwrap();
        let state = directory.path().join("state.json");
        fs::write(
            &state,
            format!(
                r#"{{"map":{{"{}":"alias-workspace","{}":"real-workspace"}}}}"#,
                alias.display(),
                real.display()
            ),
        )
        .unwrap();

        let registry = Registry::load(&state);
        assert_eq!(
            registry.workspace_for(&real).map(String::as_str),
            Some("real-workspace")
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonicalizes_symlink_keys() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let alias = directory.path().join("alias");
        fs::create_dir(&real).unwrap();
        symlink(&real, &alias).unwrap();

        let mut registry = Registry::default();
        registry.bind(&alias, "w1");
        assert_eq!(
            registry.workspace_for(&real).map(String::as_str),
            Some("w1")
        );
    }

    #[test]
    fn load_missing_or_corrupt_is_default() {
        assert!(Registry::load(Path::new("/no/such")).live_map().is_empty());
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), "{not json").unwrap();
        assert!(Registry::load(file.path()).live_map().is_empty());
    }
}

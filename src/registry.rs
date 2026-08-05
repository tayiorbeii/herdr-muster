use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    map: HashMap<String, String>,
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

        Registry {
            map: registry
                .map
                .into_iter()
                .map(|(directory, workspace)| (key(Path::new(&directory)), workspace))
                .collect(),
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
        Ok(())
    }

    #[cfg(test)]
    pub fn workspace_for(&self, directory: &Path) -> Option<&String> {
        self.map.get(&key(directory))
    }

    pub fn bind(&mut self, directory: &Path, workspace: &str) {
        self.map.insert(key(directory), workspace.to_string());
    }

    pub fn unbind(&mut self, directory: &Path) {
        self.map.remove(&key(directory));
    }

    pub fn reconcile(&mut self, live: &HashSet<String>) -> bool {
        let before = self.map.len();
        self.map.retain(|_, workspace| live.contains(workspace));
        before != self.map.len()
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
        registry.unbind(Path::new("/a"));
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
    fn save_load_roundtrip_is_atomic() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sub").join("state.json");
        fs::create_dir(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"map":{"/old":"w0"}}"#).unwrap();

        let mut registry = Registry::default();
        registry.bind(Path::new("/a"), "w1");
        registry.save(&path).unwrap();

        let loaded = Registry::load(&path);
        assert_eq!(
            loaded.workspace_for(Path::new("/a")).map(String::as_str),
            Some("w1")
        );
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

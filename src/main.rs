mod config;
mod herdr;
mod model;
mod picker;
mod refresh;
mod registry;
mod sources;

use herdr::Herdr;
use model::Kind;
use registry::Registry;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn config_path() -> PathBuf {
    match std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
        Ok(directory) => PathBuf::from(directory).join("config.toml"),
        Err(_) => PathBuf::from("config.toml"),
    }
}

fn state_path() -> PathBuf {
    match std::env::var("HERDR_PLUGIN_STATE_DIR") {
        Ok(directory) => PathBuf::from(directory).join("state.json"),
        Err(_) => PathBuf::from("state.json"),
    }
}

fn create_and_bind<H: Herdr>(
    herdr: &H,
    registry: &mut Registry,
    directory: &Path,
) -> Result<String, String> {
    let cwd = directory.to_string_lossy().to_string();
    let id = herdr
        .create_workspace(&cwd, &sources::basename(directory))
        .map_err(|error| error.to_string())?;
    registry.bind(directory, &id);
    Ok(id)
}

fn reconcile_after_refresh(
    registry: &mut Registry,
    live_workspace_ids: Option<&HashSet<String>>,
) -> bool {
    live_workspace_ids.is_some_and(|live| registry.reconcile(live))
}

fn run() -> Result<(), String> {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let client = herdr::CliHerdr::new(bin);
    let config_path = config_path();
    let registry_path = state_path();
    let origin_pane = std::env::var("HERDR_PANE_ID").ok();
    let mut registry = Registry::load(&registry_path);
    let mut dirty = false;
    let mut previous_state = picker::PickerState::default();

    let mut result = (|| -> Result<(), String> {
        loop {
            let updates = refresh::spawn(
                client.clone(),
                config_path.clone(),
                registry.live_map(),
                registry.mru().to_vec(),
                origin_pane.clone(),
            );
            let session = picker::run(std::mem::take(&mut previous_state), updates)
                .map_err(|error| error.to_string())?;
            let picker::Session {
                outcome,
                live_workspace_ids,
                mut state,
                origin_workspace,
            } = session;

            if reconcile_after_refresh(&mut registry, live_workspace_ids.as_ref()) {
                dirty = true;
            }
            // The workspace the picker was opened from is the most recently
            // used one by definition, so Enter fast-tracks back to it.
            if let Some(origin) = origin_workspace {
                dirty |= registry.touch(&origin);
            }

            match outcome {
                picker::Outcome::Cancel => return Ok(()),
                picker::Outcome::Jump(row) => {
                    match &row.kind {
                        Kind::Open { workspace_id, .. } => {
                            client
                                .focus_workspace(workspace_id)
                                .map_err(|error| error.to_string())?;
                            dirty |= registry.touch(workspace_id);
                        }
                        Kind::Dormant => {
                            let id = create_and_bind(&client, &mut registry, &row.path)?;
                            dirty = true;
                            registry.touch(&id);
                        }
                    }
                    return Ok(());
                }
                picker::Outcome::ForceNew(row) => {
                    let id = create_and_bind(&client, &mut registry, &row.path)?;
                    dirty = true;
                    registry.touch(&id);
                    return Ok(());
                }
                picker::Outcome::Close(row) => {
                    let closed_id = row.id();
                    if let Kind::Open { workspace_id, .. } = &row.kind {
                        client
                            .close_workspace(workspace_id)
                            .map_err(|error| error.to_string())?;
                        dirty |= registry.unbind_if_bound(&row.path, workspace_id);
                        dirty |= registry.forget(workspace_id);
                    }
                    state.remove(&closed_id);
                    previous_state = state;
                }
            }
        }
    })();

    if dirty {
        if let Err(error) = registry.save(&registry_path) {
            let save_error = format!("save {}: {error}", registry_path.display());
            if result.is_ok() {
                result = Err(save_error);
            } else {
                eprintln!("herdr-muster: {save_error}");
            }
        }
    }
    if let Ok(pane) = std::env::var("HERDR_PANE_ID") {
        if let Err(error) = client.close_pane(&pane) {
            let close_error = format!("close picker pane {pane}: {error}");
            if result.is_ok() {
                result = Err(close_error);
            } else {
                eprintln!("herdr-muster: {close_error}");
            }
        }
    }
    result
}

fn main() {
    if let Err(error) = run() {
        eprintln!("herdr-muster: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_refresh_never_clears_registry_bindings() {
        let mut registry = Registry::default();
        registry.bind(Path::new("/api"), "w1");

        assert!(!reconcile_after_refresh(&mut registry, None));
        assert_eq!(
            registry
                .workspace_for(Path::new("/api"))
                .map(String::as_str),
            Some("w1")
        );
    }

    #[test]
    fn successful_empty_refresh_reconciles_registry() {
        let mut registry = Registry::default();
        registry.bind(Path::new("/api"), "w1");

        assert!(reconcile_after_refresh(
            &mut registry,
            Some(&HashSet::new())
        ));
        assert!(registry.workspace_for(Path::new("/api")).is_none());
    }
}

use crate::config::{ClientOptions, ProjectConfig};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub(crate) struct RuntimeConfig {
    state: Arc<Mutex<RuntimeConfigState>>,
}

#[derive(Debug, Clone)]
struct RuntimeConfigState {
    client_options: ClientOptions,
    project_config: ProjectConfig,
    options: ClientOptions,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let client_options = ClientOptions::default();
        let project_config = ProjectConfig::default();
        let options = project_config.merged_options(&client_options);
        Self {
            state: Arc::new(Mutex::new(RuntimeConfigState {
                client_options,
                project_config,
                options,
            })),
        }
    }
}

impl RuntimeConfig {
    pub(crate) fn options(&self) -> ClientOptions {
        self.state
            .lock()
            .expect("runtime config poisoned")
            .options
            .clone()
    }

    pub(crate) fn project_config_path(&self, root: &Path) -> PathBuf {
        self.client_options().project_config_path(root)
    }

    pub(crate) fn project_config_display_path(&self) -> String {
        self.client_options().project_config_display_path()
    }

    pub(crate) fn set_client_options(&self, client_options: ClientOptions, root: &Path) {
        let project_config = ProjectConfig::load(&client_options.project_config_path(root));
        self.replace(client_options, project_config);
    }

    pub(crate) fn update_client_options(
        &self,
        settings: Value,
        root: &Path,
    ) -> serde_json::Result<()> {
        let client_options = self.client_options().merged_with_value(settings)?;
        let project_config = ProjectConfig::load(&client_options.project_config_path(root));
        self.replace(client_options, project_config);
        Ok(())
    }

    pub(crate) fn update_project_config(
        &self,
        path: &Path,
        update: impl FnOnce(&mut ProjectConfig) -> bool,
    ) -> Result<bool, String> {
        let next_config = {
            let mut state = self.state.lock().expect("runtime config poisoned");
            let mut next_config = state.project_config.clone();
            let updated = update(&mut next_config);
            if !updated {
                return Ok(false);
            }

            state.options = next_config.merged_options(&state.client_options);
            state.project_config = next_config.clone();
            next_config
        };

        next_config
            .save(path)
            .map_err(|err| format!("Failed to save project config: {err}"))?;
        Ok(true)
    }

    fn replace(&self, client_options: ClientOptions, project_config: ProjectConfig) {
        let options = project_config.merged_options(&client_options);
        *self.state.lock().expect("runtime config poisoned") = RuntimeConfigState {
            client_options,
            project_config,
            options,
        };
    }

    fn client_options(&self) -> ClientOptions {
        self.state
            .lock()
            .expect("runtime config poisoned")
            .client_options
            .clone()
    }
}

use crate::config::{ClientOptions, ProjectConfig};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone)]
pub(crate) struct RuntimeConfig {
    client_options: Arc<RwLock<ClientOptions>>,
    project_config: Arc<RwLock<ProjectConfig>>,
    options: Arc<RwLock<ClientOptions>>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let client_options = ClientOptions::default();
        let project_config = ProjectConfig::default();
        let options = project_config.merged_options(&client_options);
        Self {
            client_options: Arc::new(RwLock::new(client_options)),
            project_config: Arc::new(RwLock::new(project_config)),
            options: Arc::new(RwLock::new(options)),
        }
    }
}

impl RuntimeConfig {
    pub(crate) fn options(&self) -> ClientOptions {
        self.options.read().expect("options poisoned").clone()
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
        let (updated, next_config) = {
            let project_config = self.project_config.read().expect("project config poisoned");
            let mut next_config = project_config.clone();
            let updated = update(&mut next_config);
            (updated, next_config)
        };

        if !updated {
            return Ok(false);
        }

        next_config
            .save(path)
            .map_err(|err| format!("Failed to save project config: {err}"))?;
        let client_options = self.client_options();
        self.replace(client_options, next_config);
        Ok(true)
    }

    fn replace(&self, client_options: ClientOptions, project_config: ProjectConfig) {
        let options = project_config.merged_options(&client_options);
        *self
            .client_options
            .write()
            .expect("client options poisoned") = client_options;
        *self
            .project_config
            .write()
            .expect("project config poisoned") = project_config;
        *self.options.write().expect("options poisoned") = options;
    }

    fn client_options(&self) -> ClientOptions {
        self.client_options
            .read()
            .expect("client options poisoned")
            .clone()
    }
}

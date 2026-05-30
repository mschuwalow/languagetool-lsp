use crate::config::{ClientOptions, ProjectConfig};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub(crate) struct RuntimeConfig {
    state: Arc<RwLock<RuntimeConfigState>>,
}

#[derive(Debug, Clone)]
struct RuntimeConfigState {
    client_options: ClientOptions,
    project_config: ProjectConfig,
    options: Arc<ClientOptions>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let client_options = ClientOptions::default();
        let project_config = ProjectConfig::default();
        let options = Arc::new(project_config.merged_options(&client_options));
        Self {
            state: Arc::new(RwLock::new(RuntimeConfigState {
                client_options,
                project_config,
                options,
            })),
        }
    }
}

impl RuntimeConfig {
    pub(crate) async fn options(&self) -> Arc<ClientOptions> {
        self.state.read().await.options.clone()
    }

    pub(crate) async fn project_config_path(&self, root: &Path) -> PathBuf {
        self.state
            .read()
            .await
            .client_options
            .project_config_path(root)
    }

    pub(crate) async fn project_config_display_path(&self) -> String {
        self.state
            .read()
            .await
            .client_options
            .project_config_display_path()
    }

    pub(crate) async fn set_client_options(&self, client_options: ClientOptions, root: &Path) {
        let project_config = ProjectConfig::load(&client_options.project_config_path(root)).await;
        self.replace(client_options, project_config).await;
    }

    pub(crate) async fn update_client_options(
        &self,
        settings: Value,
        root: &Path,
    ) -> serde_json::Result<()> {
        let mut state = self.state.write().await;
        let old_project_config_path = state.client_options.project_config_path(root);
        let client_options = state.client_options.merged_with_value(settings)?;
        let new_project_config_path = client_options.project_config_path(root);
        let project_config = if old_project_config_path == new_project_config_path {
            state.project_config.clone()
        } else {
            ProjectConfig::load(&new_project_config_path).await
        };

        state.options = Arc::new(project_config.merged_options(&client_options));
        state.client_options = client_options;
        state.project_config = project_config;
        Ok(())
    }

    pub(crate) async fn update_project_config(
        &self,
        path: &Path,
        update: impl FnOnce(&mut ProjectConfig) -> bool,
    ) -> Result<bool, String> {
        let mut state = self.state.write().await;
        let mut next_config = state.project_config.clone();
        let updated = update(&mut next_config);
        if !updated {
            return Ok(false);
        }

        next_config
            .save(path)
            .await
            .map_err(|err| format!("Failed to save project config: {err}"))?;

        state.options = Arc::new(next_config.merged_options(&state.client_options));
        state.project_config = next_config;
        Ok(true)
    }

    async fn replace(&self, client_options: ClientOptions, project_config: ProjectConfig) {
        let options = Arc::new(project_config.merged_options(&client_options));
        *self.state.write().await = RuntimeConfigState {
            client_options,
            project_config,
            options,
        };
    }
}

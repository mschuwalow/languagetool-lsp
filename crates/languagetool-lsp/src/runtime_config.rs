use crate::config::{ClientOptions, ProjectConfig};
use crate::languagetool::LanguageToolClient;
use std::sync::Arc;

pub struct RuntimeConfig {
    pub client_options: ClientOptions,
    pub project_config: ProjectConfig,
    /// `project_config` merged into `client_options`; what checks
    /// actually use.
    pub options: Arc<ClientOptions>,
    /// Rebuilt alongside `options` from `client_options.backend`.
    pub language_tool: Arc<LanguageToolClient>,
    /// Bumped on every replacement, so a document's diagnostics cache
    /// can tell a check that ran under old options apart from the
    /// current ones.
    pub options_version: u64,
    /// Bumped only when `client_options` (and therefore the resolved
    /// project config path) changes, *not* on a project-config-only
    /// reload. Sent to [`crate::project_config_watcher::ProjectConfigWatcher`]
    /// alongside the new path and echoed back in its change
    /// notifications, so a notification for a path this backend has
    /// since moved on from can be recognized and dropped.
    pub revision: u64,
}

impl RuntimeConfig {
    pub fn new(client_options: ClientOptions, project_config: ProjectConfig) -> Self {
        let options = Arc::new(project_config.merged_options(&client_options));
        let language_tool = Arc::new(LanguageToolClient::new(&client_options.backend));
        Self {
            client_options,
            project_config,
            options,
            language_tool,
            options_version: 0,
            revision: 0,
        }
    }

    pub fn with_new_client_options(
        &self,
        client_options: ClientOptions,
        project_config: ProjectConfig,
    ) -> Self {
        let options = Arc::new(project_config.merged_options(&client_options));
        let language_tool = Arc::new(LanguageToolClient::new(&client_options.backend));
        Self {
            client_options,
            project_config,
            options,
            language_tool,
            options_version: self.options_version + 1,
            revision: self.revision + 1,
        }
    }

    pub fn with_new_project_config(&self, project_config: ProjectConfig) -> Self {
        let options = Arc::new(project_config.merged_options(&self.client_options));
        Self {
            client_options: self.client_options.clone(),
            project_config,
            options,
            language_tool: Arc::clone(&self.language_tool),
            options_version: self.options_version + 1,
            revision: self.revision,
        }
    }
}

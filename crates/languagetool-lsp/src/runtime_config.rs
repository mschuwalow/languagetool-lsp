use crate::config::{ClientOptions, ProjectConfig};
use crate::languagetool::LanguageToolClient;
use std::sync::Arc;

/// Everything about a backend's configuration that changes together as a
/// single atomic unit: the raw `client_options` from
/// `initialize`/`workspace/didChangeConfiguration`, the `project_config`
/// loaded from disk, their merge (`options`), the `language_tool` HTTP
/// client built from `client_options.backend`, and two independent
/// counters -- see [`Self::options_version`] and [`Self::revision`]'s
/// docs for why they're not the same thing.
///
/// Always replaced as a whole behind a single `ArcSwap<RuntimeConfig>`,
/// never mutated field-by-field or swapped alongside a second `ArcSwap`,
/// so that one `.load()` always sees a mutually consistent snapshot of
/// all of them -- there's no window where a reader could observe, say,
/// the new `options` paired with the old `language_tool` client.
pub struct RuntimeConfig {
    pub client_options: ClientOptions,
    pub project_config: ProjectConfig,
    /// `project_config` merged into `client_options`; what checks
    /// actually use.
    pub options: Arc<ClientOptions>,
    /// Built from `client_options.backend`; rebuilt alongside `options`
    /// so a check always sees the client matching the options it was
    /// scheduled under.
    pub language_tool: Arc<LanguageToolClient>,
    /// Bumped on every replacement of this struct, whether from a full
    /// `client_options` change or a project-config-only reload, so a
    /// document's per-block diagnostics cache can tell a check that ran
    /// under old options apart from one that ran under the current ones.
    pub options_version: u64,
    /// Bumped only when `client_options` (and therefore the resolved
    /// project config path) changes -- *not* on a project-config-only
    /// reload, which never moves the watched path. Sent to the project
    /// config file watcher alongside the new path (see
    /// `ProjectConfigWatcherHandle::set_watch_target`), and echoed back
    /// in its change notifications so a stale one (sent for a path this
    /// backend has since moved on from) can be recognized and dropped
    /// rather than acted on.
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

    /// Replaces `client_options` wholesale (the caller resolves and loads
    /// the corresponding `project_config` itself, since that's an async
    /// disk read this synchronous method can't do); bumps both
    /// `options_version` and `revision`.
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

    /// Replaces `project_config` only -- `client_options`, and therefore
    /// `language_tool` and the resolved project config path, are
    /// untouched; bumps `options_version` but deliberately *not*
    /// `revision`.
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

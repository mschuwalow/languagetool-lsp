use crate::config::{ClientOptions, ProjectConfig};
use crate::project_config_watcher::{
    ProjectConfigChanged, ProjectConfigWatcher, ProjectConfigWatcherHandle,
};
use crate::runtime_config::RuntimeConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};

type ProjectConfigUpdate = Box<dyn FnOnce(&mut ProjectConfig) -> bool + Send>;

enum ConfigEvent {
    ApplyClientOptions(Box<ClientOptions>),
    UpdateProjectConfig(ProjectConfigUpdate, oneshot::Sender<Result<(), String>>),
}

#[derive(Clone)]
pub struct ConfigActorHandle(mpsc::UnboundedSender<ConfigEvent>);

impl ConfigActorHandle {
    pub fn apply_client_options(&self, client_options: ClientOptions) {
        let _ = self
            .0
            .send(ConfigEvent::ApplyClientOptions(Box::new(client_options)));
    }

    pub async fn update_project_config(
        &self,
        update: impl FnOnce(&mut ProjectConfig) -> bool + Send + 'static,
    ) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self
            .0
            .send(ConfigEvent::UpdateProjectConfig(Box::new(update), reply_tx));
        reply_rx
            .await
            .unwrap_or_else(|_| Err("the config actor is gone".to_string()))
    }
}

pub async fn spawn(
    root: Option<PathBuf>,
    client_options: ClientOptions,
) -> (ConfigActorHandle, watch::Receiver<Arc<RuntimeConfig>>) {
    let project_config_path = client_options.resolved_project_config_path(root.as_deref());
    let project_config = load_project_config(project_config_path.as_deref()).await;
    let runtime_config = Arc::new(RuntimeConfig::new(client_options, project_config));

    let (watcher_events_tx, watcher_events_rx) = mpsc::unbounded_channel();
    let (watcher_task, watcher_handle) = ProjectConfigWatcher::spawn(watcher_events_tx);
    watcher_handle.set_watch_target(project_config_path, runtime_config.revision);

    let (config_tx, config_rx) = watch::channel(runtime_config);
    let (events_tx, events_rx) = mpsc::unbounded_channel();

    let actor = ConfigActor {
        root,
        watcher_handle,
        _watcher_task: watcher_task,
        config_tx,
    };
    tokio::spawn(run(actor, events_rx, watcher_events_rx));

    (ConfigActorHandle(events_tx), config_rx)
}

struct ConfigActor {
    root: Option<PathBuf>,
    watcher_handle: ProjectConfigWatcherHandle,
    _watcher_task: ProjectConfigWatcher,
    config_tx: watch::Sender<Arc<RuntimeConfig>>,
}

impl ConfigActor {
    fn current(&self) -> Arc<RuntimeConfig> {
        Arc::clone(&self.config_tx.borrow())
    }

    async fn apply_client_options(&self, client_options: ClientOptions) {
        let project_config_path = client_options.resolved_project_config_path(self.root.as_deref());
        let project_config = load_project_config(project_config_path.as_deref()).await;
        let new_config = self
            .current()
            .with_new_client_options(client_options, project_config);
        self.watcher_handle
            .set_watch_target(project_config_path, new_config.revision);
        let _ = self.config_tx.send(Arc::new(new_config));
    }

    // The watcher tags its notifications with the revision that was current
    // when it was told to watch; if that no longer matches, a
    // `didChangeConfiguration` has since moved on to a different path (or
    // reloaded the same path itself), making this notification stale.
    async fn project_config_changed(&self, revision: u64) {
        let current = self.current();
        if current.revision != revision {
            log::debug!(
                "Dropping stale project config change notification for revision {revision} \
                 (current revision is {})",
                current.revision
            );
            return;
        }

        let project_config_path = current
            .client_options
            .resolved_project_config_path(self.root.as_deref());
        let project_config = load_project_config(project_config_path.as_deref()).await;
        if project_config == current.project_config {
            return;
        }

        log::info!("Project config file changed on disk; reloading");
        let new_config = current.with_new_project_config(project_config);
        let _ = self.config_tx.send(Arc::new(new_config));
    }

    // Only writes to disk; the project config watcher notices the change
    // and feeds it back through `project_config_changed`, so there's one
    // single path for installing a new project config regardless of
    // whether it came from a hand edit or from here.
    async fn update_project_config(&self, update: ProjectConfigUpdate) -> Result<(), String> {
        let current = self.current();
        let Some(path) = current
            .client_options
            .resolved_project_config_path(self.root.as_deref())
        else {
            return Err(
                "No workspace folder is open and `projectConfigPath` is not an absolute path; \
                 can't persist project config"
                    .to_string(),
            );
        };

        let mut project_config = current.project_config.clone();
        if !update(&mut project_config) {
            return Ok(());
        }

        project_config
            .save(&path)
            .await
            .map_err(|err| format!("Failed to save project config: {err}"))?;
        log::info!(
            "Saved LanguageTool project config to {}; the project config watcher will pick up \
             the change",
            path.display()
        );
        Ok(())
    }
}

async fn run(
    actor: ConfigActor,
    mut events: mpsc::UnboundedReceiver<ConfigEvent>,
    mut watcher_events: mpsc::UnboundedReceiver<ProjectConfigChanged>,
) {
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                match event {
                    ConfigEvent::ApplyClientOptions(client_options) => {
                        actor.apply_client_options(*client_options).await;
                    }
                    ConfigEvent::UpdateProjectConfig(update, reply) => {
                        let _ = reply.send(actor.update_project_config(update).await);
                    }
                }
            }
            changed = watcher_events.recv() => {
                let Some(ProjectConfigChanged { revision }) = changed else { break };
                actor.project_config_changed(revision).await;
            }
        }
    }
}

async fn load_project_config(path: Option<&Path>) -> ProjectConfig {
    match path {
        Some(path) => ProjectConfig::load(path).await,
        None => ProjectConfig::default(),
    }
}

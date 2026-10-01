use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Settles bursts of filesystem events (e.g. an editor's
/// write-temp-then-rename save pattern) into a single notification.
const DEBOUNCE: Duration = Duration::from_millis(200);

struct WatchTarget {
    path: Option<PathBuf>,
    revision: u64,
}

/// Sent when the file at the most recently requested watch target changes
/// on disk, tagged with the revision that was active when the target was
/// set. The receiver drops this if its own current revision has since
/// moved on (e.g. `didChangeConfiguration` repointed the watcher before
/// this notification arrived).
pub struct ProjectConfigChanged {
    pub revision: u64,
}

/// A long-lived background task that watches a project config file path
/// for out-of-band changes (hand edits, another editor window, this
/// server's own `executeCommand` handlers writing to disk) and reports
/// them as [`ProjectConfigChanged`]; reloading and installing a new
/// config is the receiving end's job. One task serves the whole server
/// lifetime, re-pointed via [`Self::set_watch_target`] as needed.
/// Dropping it stops the background task and the underlying OS watch.
pub struct ProjectConfigWatcher {
    targets: UnboundedSender<WatchTarget>,
    _task: tokio::task::JoinHandle<()>,
}

impl ProjectConfigWatcher {
    pub fn spawn() -> (Self, UnboundedReceiver<ProjectConfigChanged>) {
        let (changed_tx, changed_rx) = unbounded_channel();
        let (targets_tx, targets_rx) = unbounded_channel();
        let task = tokio::spawn(run(targets_rx, changed_tx));
        let watcher = Self {
            targets: targets_tx,
            _task: task,
        };
        (watcher, changed_rx)
    }

    /// Watches `path` (or nothing, if `None`) from now on, superseding
    /// any previous target. Subsequent [`ProjectConfigChanged`]
    /// notifications for it carry `revision`.
    pub fn set_watch_target(&self, path: Option<PathBuf>, revision: u64) {
        let _ = self.targets.send(WatchTarget { path, revision });
    }
}

async fn run(
    mut targets: UnboundedReceiver<WatchTarget>,
    changed: UnboundedSender<ProjectConfigChanged>,
) {
    let (fs_tx, mut fs_events) = unbounded_channel::<DebounceEventResult>();
    let mut debouncer = match new_debouncer(DEBOUNCE, move |result| {
        // Runs on a thread owned by `notify`, not the tokio runtime; just
        // forward the result and let this task do the async work below.
        let _ = fs_tx.send(result);
    }) {
        Ok(debouncer) => debouncer,
        Err(err) => {
            log::warn!(
                "Failed to start the project config file watcher; changes made to it outside \
                 of this server's own commands won't be picked up automatically: {err}"
            );
            // Keep draining so `set_watch_target` callers don't see a
            // closed channel; there's just nothing to act on.
            while targets.recv().await.is_some() {}
            return;
        }
    };

    let mut current: Option<WatchTarget> = None;
    let mut watched_dir: Option<PathBuf> = None;

    loop {
        tokio::select! {
            target = targets.recv() => {
                let Some(target) = target else { break };
                current = Some(target);
                let dir = resolved_target_dir(&current).await;
                rearm(&mut debouncer, &mut watched_dir, dir).await;
            }
            event = fs_events.recv() => {
                match event {
                    Some(Ok(_events)) => {
                        if let Some(target) = &current {
                            let _ = changed.send(ProjectConfigChanged {
                                revision: target.revision,
                            });
                        }
                        // A closer ancestor of the target path may have
                        // just appeared; re-check so later events are
                        // watched precisely (see `nearest_existing_ancestor`).
                        let dir = resolved_target_dir(&current).await;
                        rearm(&mut debouncer, &mut watched_dir, dir).await;
                    }
                    Some(Err(err)) => {
                        log::warn!("Project config file watcher error: {err}");
                    }
                    // Only reachable if `debouncer` (which owns the
                    // sending half) were dropped, which this loop never
                    // does.
                    None => break,
                }
            }
        }
    }
}

async fn resolved_target_dir(current: &Option<WatchTarget>) -> Option<PathBuf> {
    let path = current.as_ref()?.path.as_deref()?;
    Some(nearest_existing_ancestor(path).await)
}

/// Adjusts `debouncer`'s watch set to `target_dir`, if it changed; a
/// true no-op (no syscalls) otherwise.
async fn rearm(
    debouncer: &mut Debouncer<RecommendedWatcher>,
    watched_dir: &mut Option<PathBuf>,
    target_dir: Option<PathBuf>,
) {
    if watched_dir.as_ref() == target_dir.as_ref() {
        return;
    }

    if let Some(previous) = watched_dir.take()
        && let Err(err) = debouncer.watcher().unwatch(&previous)
    {
        log::debug!(
            "Failed to unwatch {} for project config changes: {err}",
            previous.display()
        );
    }

    let Some(target_dir) = target_dir else {
        return;
    };
    match debouncer
        .watcher()
        .watch(&target_dir, RecursiveMode::NonRecursive)
    {
        Ok(()) => {
            log::debug!(
                "Watching {} for project config changes",
                target_dir.display()
            );
            *watched_dir = Some(target_dir);
        }
        Err(err) => {
            log::warn!(
                "Failed to watch {} for project config changes: {err}",
                target_dir.display()
            );
        }
    }
}

/// `notify` can't watch a path that doesn't exist yet, so this walks up
/// to the nearest existing ancestor and watches that non-recursively
/// instead -- also `notify`'s recommended way to survive the file being
/// replaced (e.g. write-temp-then-rename) or removed. Once a closer
/// ancestor appears, the watcher observes *that* creation event, which
/// re-triggers this via `run`'s main loop and re-arms against the now
/// more specific directory.
async fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut candidate = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf());
    loop {
        if matches!(tokio::fs::metadata(&candidate).await, Ok(metadata) if metadata.is_dir()) {
            return candidate;
        }
        match candidate.parent() {
            Some(parent) if parent != candidate => candidate = parent.to_path_buf(),
            _ => return candidate,
        }
    }
}

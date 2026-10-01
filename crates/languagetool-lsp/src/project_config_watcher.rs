use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// How long to wait for a burst of filesystem events (e.g. an editor's
/// write-temp-file-then-rename save pattern, which can produce several
/// raw events for a single logical change) to settle before notifying.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// Tells the watcher what to watch from now on (sent by whoever installs
/// a new config generation; see [`ProjectConfigWatcherHandle`]).
struct WatchTarget {
    path: Option<PathBuf>,
    generation: u64,
}

/// Sent by the watcher when the file at the most recently requested watch
/// target changes on disk, tagged with the `generation` from the
/// [`WatchTarget`] that was active at the time. The receiver is
/// responsible for comparing this against whatever it considers the
/// current generation and dropping the message if it's fallen behind
/// (e.g. a `workspace/didChangeConfiguration` moved `projectConfigPath`
/// to a new generation after this event was already in flight for the
/// old one).
pub struct ProjectConfigChanged {
    pub generation: u64,
}

/// A cheap, cloneable handle for telling the long-lived
/// [`ProjectConfigWatcher`] task what to watch. Sending a new target
/// doesn't wait for the watcher to act on it -- the whole point of this
/// being message-passing is that neither side blocks on the other; the
/// watcher will get to it and emit [`ProjectConfigChanged`] (tagged with
/// `generation`) once it notices a change, whenever that happens to be.
#[derive(Clone)]
pub struct ProjectConfigWatcherHandle(UnboundedSender<WatchTarget>);

impl ProjectConfigWatcherHandle {
    /// Tells the watcher to watch `path` (or nothing at all, if `None`)
    /// from now on, superseding whatever it was watching before.
    /// Subsequent [`ProjectConfigChanged`] notifications for this target
    /// will carry `generation`.
    pub fn set_watch_target(&self, path: Option<PathBuf>, generation: u64) {
        // Only fails if the watcher task is gone (server shutting down);
        // nothing useful to do about that here.
        let _ = self.0.send(WatchTarget { path, generation });
    }
}

/// Owns the long-lived background task that watches a project config file
/// path for out-of-band changes (hand edits, another editor window's
/// commands, version control checkouts, this server's own
/// project-config-mutating `executeCommand` handlers which only write to
/// disk and rely on this watcher noticing the write, ...) and reports
/// them via [`ProjectConfigChanged`] messages. It does *not* reload or
/// install anything itself -- that's the receiving end's job, once it has
/// validated the message's generation is still current.
///
/// What path is being watched is driven entirely by [`WatchTarget`]
/// messages sent through a [`ProjectConfigWatcherHandle`]; there's a
/// single watcher task for the server's whole lifetime, re-pointed as
/// needed rather than respawned per config generation.
///
/// Must be kept alive for as long as watching should continue: dropping
/// this drops the background task (and, with it, the underlying OS-level
/// watch), stopping notification of file changes.
pub struct ProjectConfigWatcher {
    _task: tokio::task::JoinHandle<()>,
}

impl ProjectConfigWatcher {
    /// Spawns the background task. `changed` is where it sends
    /// [`ProjectConfigChanged`] notifications. Returns the watcher (keep
    /// it alive) and a handle for directing what it watches.
    pub fn spawn(
        changed: UnboundedSender<ProjectConfigChanged>,
    ) -> (Self, ProjectConfigWatcherHandle) {
        let (targets_tx, targets_rx) = unbounded_channel();
        let task = tokio::spawn(run(targets_rx, changed));
        (Self { _task: task }, ProjectConfigWatcherHandle(targets_tx))
    }
}

async fn run(
    mut targets: UnboundedReceiver<WatchTarget>,
    changed: UnboundedSender<ProjectConfigChanged>,
) {
    let (fs_tx, mut fs_events) = unbounded_channel::<DebounceEventResult>();
    let mut debouncer = match new_debouncer(DEBOUNCE, move |result| {
        // Runs on a native OS thread managed by `notify`/`notify-debouncer-mini`,
        // not on the tokio runtime; just forward the (already debounced)
        // result and let this task do the actual async work below.
        let _ = fs_tx.send(result);
    }) {
        Ok(debouncer) => debouncer,
        Err(err) => {
            log::warn!(
                "Failed to start the project config file watcher; changes made to it outside \
                 of this server's own commands won't be picked up automatically: {err}"
            );
            // Keep draining `targets` so `set_watch_target` callers don't
            // see a closed channel; there's just nothing to act on them
            // with.
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
                                generation: target.generation,
                            });
                        }
                        // A closer ancestor of the target path may have
                        // just been created (see
                        // `nearest_existing_ancestor`'s docs); check
                        // again so later events are observed precisely.
                        let dir = resolved_target_dir(&current).await;
                        rearm(&mut debouncer, &mut watched_dir, dir).await;
                    }
                    Some(Err(err)) => {
                        log::warn!("Project config file watcher error: {err}");
                    }
                    // The sender (owned by the `notify` callback closure,
                    // which is owned by `debouncer`, which this task
                    // owns) can only have been dropped by dropping
                    // `debouncer` -- which this loop never does, so this
                    // is unreachable in practice. Stop rather than spin
                    // if it somehow does happen.
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

/// (Re-)adjusts `debouncer`'s watch set to match `target_dir`, if it
/// changed. A no-op (not just a no-op watch call, but no filesystem
/// syscalls at all) when the target is unchanged from last time.
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

/// `notify` needs an existing path to watch (a file can be watched
/// directly, but not one that doesn't exist yet; watching a nonexistent
/// directory fails outright on most platforms). Rather than requiring the
/// project config file (or even its parent directory, e.g. `.zed/`) to
/// already exist, this walks up to the nearest ancestor that does, and
/// watches that non-recursively instead. Once a closer ancestor is
/// created, the watcher will observe *that* creation event, which
/// triggers a re-check of this function's result (see `run`'s main
/// loop), re-arming the watch against the now-existing (and more
/// specific) directory.
///
/// Watching the parent directory rather than the file itself is also
/// `notify`'s own recommended pattern for surviving the file being
/// replaced (e.g. an editor's write-temp-then-rename save) or removed,
/// which behave surprisingly if the file itself is the watch target.
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

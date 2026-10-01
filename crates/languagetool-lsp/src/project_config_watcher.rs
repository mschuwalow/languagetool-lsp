use crate::backend::SharedBackend;
use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// How long to wait for a burst of filesystem events (e.g. an editor's
/// write-temp-file-then-rename save pattern, which can produce several
/// raw events for a single logical change) to settle before reloading.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// Watches the project config file's resolved path for out-of-band
/// changes (hand edits, another editor window's commands, version control
/// checkouts, ...) and reloads+installs it into [`SharedBackend`] when it
/// changes. This is also how *this* server's own project-config-mutating
/// `workspace/executeCommand` handlers take effect: they only write to
/// disk (see `LanguageServerBackend::handle_execute_command`) and rely on
/// this watcher noticing the write, rather than installing the change
/// directly.
///
/// Must be kept alive for as long as watching should continue: dropping
/// this drops the background task (and, with it, the underlying OS-level
/// watch), stopping notification of file changes.
pub struct ProjectConfigWatcher {
    /// Wakes the background task immediately to re-resolve and re-arm the
    /// watched directory, without waiting for a filesystem event. Used
    /// after installing a backend whose resolved project config path may
    /// have changed (`initialize`, `workspace/didChangeConfiguration`).
    rearm_signal: Arc<Notify>,
    _task: tokio::task::JoinHandle<()>,
}

impl ProjectConfigWatcher {
    pub fn spawn(shared_backend: SharedBackend) -> Self {
        let rearm_signal = Arc::new(Notify::new());
        let (tx, rx) = unbounded_channel::<DebounceEventResult>();

        match new_debouncer(DEBOUNCE, move |result| {
            // Runs on a native OS thread managed by `notify`/`notify-debouncer-mini`,
            // not on the tokio runtime; just forward the (already debounced)
            // result and let the background task below do the actual async
            // reload work. `send` only fails if the receiver end (owned by
            // that same background task) was dropped, which can't race with
            // this callback since dropping the task also drops this `Debouncer`
            // (see below), stopping this callback from firing again.
            let _ = tx.send(result);
        }) {
            Ok(debouncer) => {
                let task = tokio::spawn(run(
                    debouncer,
                    rx,
                    Arc::clone(&rearm_signal),
                    shared_backend,
                ));
                Self {
                    rearm_signal,
                    _task: task,
                }
            }
            Err(err) => {
                log::warn!(
                    "Failed to start the project config file watcher; changes to the project \
                     config file made outside of this server's own commands won't be picked up \
                     automatically: {err}"
                );
                Self {
                    rearm_signal,
                    _task: tokio::spawn(async {}),
                }
            }
        }
    }

    /// Signals the watcher to immediately re-resolve and re-arm its
    /// watched directory against the currently installed backend, rather
    /// than waiting for the next filesystem event. Call this after
    /// installing a backend whose resolved project config path may have
    /// changed.
    pub fn backend_changed(&self) {
        self.rearm_signal.notify_one();
    }
}

async fn run(
    mut debouncer: Debouncer<RecommendedWatcher>,
    mut events: UnboundedReceiver<DebounceEventResult>,
    rearm_signal: Arc<Notify>,
    shared_backend: SharedBackend,
) {
    let mut watched_dir: Option<PathBuf> = None;
    loop {
        rearm(&mut debouncer, &mut watched_dir, &shared_backend).await;

        tokio::select! {
            event = events.recv() => {
                match event {
                    Some(Ok(_events)) => reload_if_changed(&shared_backend).await,
                    Some(Err(err)) => {
                        log::warn!("Project config file watcher error: {err}");
                    }
                    // The sender (owned by the `notify` callback closure, which
                    // is itself owned by `debouncer`, which this task owns) can
                    // only have been dropped by dropping `debouncer` -- which
                    // this loop never does, so this is unreachable in practice.
                    // Stop rather than spin if it somehow does happen.
                    None => break,
                }
            }
            () = rearm_signal.notified() => {}
        }
    }
}

/// (Re-)resolves the directory that should be watched for the currently
/// installed backend's project config path and adjusts `debouncer`'s
/// watch set to match, if it changed. A no-op (not just a no-op watch
/// call, but no filesystem syscalls at all) when the target is unchanged
/// from last time.
async fn rearm(
    debouncer: &mut Debouncer<RecommendedWatcher>,
    watched_dir: &mut Option<PathBuf>,
    shared_backend: &SharedBackend,
) {
    let target_dir = match shared_backend
        .get()
        .and_then(|backend| backend.project_config_path())
    {
        Some(path) => Some(nearest_existing_ancestor(&path).await),
        None => None,
    };

    if watched_dir.as_deref() == target_dir.as_deref() {
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

async fn reload_if_changed(shared_backend: &SharedBackend) {
    let Some(backend) = shared_backend.get() else {
        return;
    };
    let Some(new_backend) = backend.reload_project_config_if_changed().await else {
        return;
    };
    log::info!("Project config file changed on disk; reloading and rechecking open documents");
    shared_backend.install(new_backend.clone());
    new_backend.recheck_all().await;
}

/// `notify` needs an existing path to watch (a file can be watched
/// directly, but not one that doesn't exist yet; watching a nonexistent
/// directory fails outright on most platforms). Rather than requiring the
/// project config file (or even its parent directory, e.g. `.zed/`) to
/// already exist, this walks up to the nearest ancestor that does, and
/// watches that non-recursively instead. Once a closer ancestor is
/// created, the watcher will observe *that* creation event, which
/// triggers `reload_if_changed` and then, on the loop's next iteration,
/// `rearm` -- re-resolving this function's result and re-arming the watch
/// against the now-existing (and more specific) directory.
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

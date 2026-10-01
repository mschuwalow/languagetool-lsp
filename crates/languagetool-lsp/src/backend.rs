use crate::config::{ClientOptions, LanguageToolBackend, ProjectConfig};
use crate::diagnostics::{
    RawDiagnostic, SOURCE, diagnostic_data_for_text, make_lsp_diagnostic_for_range,
    match_utf16_range, parse_diagnostic_data,
};
use crate::document_cache::{CheckedBlock, DocumentCache, DocumentToken, PreparedCheck};
use crate::languagetool::{Annotation, LanguageToolError, LanguageToolMatch, LanguageToolResponse};
use crate::masking::CheckBlock;
use crate::project_config_watcher::{
    ProjectConfigChanged, ProjectConfigWatcher, ProjectConfigWatcherHandle,
};
use crate::runtime_config::RuntimeConfig;
use crate::text_index::{ByteRange, TextIndex, Utf16Range};
use arc_swap::ArcSwap;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tower_lsp_server::Client;
use tower_lsp_server::jsonrpc::{Error as RpcError, Result as RpcResult};
use tower_lsp_server::ls_types::*;

const COMMAND_IGNORE_WORD: &str = "languagetool.ignoreWordInWorkspace";
const COMMAND_DISABLE_RULE: &str = "languagetool.disableRuleInWorkspace";
const COMMAND_DISABLE_CATEGORY: &str = "languagetool.disableCategoryInWorkspace";

/// Everything the server knows for the lifetime of one `initialize`d
/// connection. Constructed once (see [`Self::new`]) and returned as an
/// `Arc`, since both request handlers and the background tasks it spawns
/// (the project config file watcher and the loop reacting to it) need
/// their own cheaply-clonable handle to the *same* instance.
///
/// Only `runtime_config` is actually mutable (behind an `ArcSwap`, see
/// [`Self::apply_new_client_options`]); everything else (`client`,
/// `root`, `documents`, the watcher handle) is fixed for the connection's
/// whole lifetime. This is a deliberate departure from treating the
/// *whole* backend as an immutable snapshot that gets replaced wholesale
/// on every configuration change: `documents` in particular is a shared
/// cache that every open document, debounced check, and project config
/// reload needs to see consistently, which is much simpler to reason
/// about as one long-lived object than as a chain of snapshots each
/// capturing their own clone of it.
pub struct LanguageServerBackend {
    client: Client,
    /// Workspace root, resolved once from the LSP workspace folders / root
    /// URI during `initialize`. `None` when the client opened a single
    /// file with no folder (legal per the LSP spec: `rootUri` and
    /// `workspaceFolders` may both be `null`).
    root: Option<PathBuf>,
    documents: DocumentCache,
    runtime_config: ArcSwap<RuntimeConfig>,
    /// Handle for telling the project config file watcher what to watch;
    /// see [`Self::apply_new_client_options`] for when that happens.
    project_config_watcher: ProjectConfigWatcherHandle,
    /// Keeps the watcher's background task alive for as long as this
    /// backend is; see [`ProjectConfigWatcher`]'s docs.
    _project_config_watcher_task: ProjectConfigWatcher,
}

impl LanguageServerBackend {
    pub async fn new(
        client: Client,
        root: Option<PathBuf>,
        client_options: ClientOptions,
    ) -> Arc<Self> {
        let project_config_path = client_options.resolved_project_config_path(root.as_deref());
        let project_config = load_project_config(project_config_path.as_deref()).await;
        let runtime_config = RuntimeConfig::new(client_options, project_config);
        let revision = runtime_config.revision;

        let (changed_tx, changed_rx) = unbounded_channel();
        let (project_config_watcher_task, project_config_watcher) =
            ProjectConfigWatcher::spawn(changed_tx);
        project_config_watcher.set_watch_target(project_config_path, revision);

        let backend = Arc::new(Self {
            client,
            root,
            documents: DocumentCache::default(),
            runtime_config: ArcSwap::new(Arc::new(runtime_config)),
            project_config_watcher,
            _project_config_watcher_task: project_config_watcher_task,
        });

        // Give the watcher's message loop a handle to this exact backend
        // (not a lookup into some separate shared slot) so it can react
        // directly -- see `run_project_config_reload_loop`'s docs. This
        // is also why `new` returns an `Arc<Self>` rather than `Self`:
        // the backend needs to exist, fully constructed, before anything
        // can hold a clone of it.
        tokio::spawn(run_project_config_reload_loop(
            Arc::clone(&backend),
            changed_rx,
        ));

        backend
    }

    pub fn options(&self) -> Arc<ClientOptions> {
        Arc::clone(&self.runtime_config.load().options)
    }

    /// Applies a full replacement [`ClientOptions`], synchronously
    /// reloading the project config from its (possibly new) resolved
    /// path and installing both together with a freshly built
    /// LanguageTool HTTP client (see
    /// [`RuntimeConfig::with_new_client_options`]) under a freshly
    /// bumped revision, then pointing the project config file watcher at
    /// the new path.
    ///
    /// Ordering matters here: `runtime_config` (and so the new revision)
    /// is installed *before* the watcher is told anything, so by the
    /// time it could possibly fire for the new target, any
    /// [`ProjectConfigChanged`] message still in flight for the *old*
    /// revision is unambiguously stale -- see
    /// [`run_project_config_reload_loop`]'s docs.
    ///
    /// Does *not* recheck open documents itself; the caller is expected
    /// to call [`Self::recheck_all`] afterwards.
    pub async fn apply_new_client_options(&self, client_options: ClientOptions) {
        let project_config_path = client_options.resolved_project_config_path(self.root.as_deref());
        let project_config = load_project_config(project_config_path.as_deref()).await;
        let new_runtime_config = self
            .runtime_config
            .load()
            .with_new_client_options(client_options, project_config);
        let revision = new_runtime_config.revision;

        self.runtime_config.store(Arc::new(new_runtime_config));
        self.project_config_watcher
            .set_watch_target(project_config_path, revision);
    }

    /// Parses `settings` as a full replacement [`ClientOptions`] value and
    /// applies it via [`Self::apply_new_client_options`]. This does *not*
    /// merge with the previous options: any field the payload doesn't
    /// mention falls back to its `#[serde(default)]`, not whatever was
    /// set at `initialize` or by an earlier config change -- if a user
    /// drops a setting from their config, they expect the default to
    /// take effect, not for a stale value to linger.
    ///
    /// Returns `false` for a `null` notification (nothing changed) or a
    /// value that fails to parse (reported to the client, previous
    /// options kept as-is), in which case there's nothing for the caller
    /// to recheck. Returns `true` otherwise, in which case the caller is
    /// expected to call [`Self::recheck_all`] afterwards.
    pub async fn with_new_config_from_settings(&self, settings: Value) -> bool {
        if settings == Value::Null {
            log::debug!("Ignoring null configuration change notification");
            return false;
        }

        let client_options = match ClientOptions::parse_value(settings) {
            Ok(client_options) => client_options,
            Err(err) => {
                let message = format!(
                    "Ignoring invalid LanguageTool configuration change; keeping previous options: {err}"
                );
                log::error!("{message}");
                self.client.log_message(MessageType::ERROR, message).await;
                return false;
            }
        };

        log::info!("LanguageTool configuration changed; reloading options and project config");
        self.apply_new_client_options(client_options).await;
        true
    }

    /// Reacts to a [`ProjectConfigChanged`] notification tagged with
    /// `revision`. If `revision` doesn't match the currently installed
    /// `RuntimeConfig::revision`, the notification is stale -- a
    /// `workspace/didChangeConfiguration` moved the watched path on from
    /// underneath it, whether because the message was already in flight
    /// or because the watcher hadn't gotten around to re-arming yet --
    /// and is dropped rather than acted on. Otherwise, reloads
    /// `project_config` from disk and, if it actually changed, installs
    /// it and rechecks every open document.
    ///
    /// Installs via `compare_and_swap` rather than a plain `store`: the
    /// disk read above is an `.await` point, during which a concurrent
    /// `apply_new_client_options` could install a new revision. Without
    /// the compare, storing unconditionally afterwards would silently
    /// revert that concurrent change back to the stale revision this
    /// function started with. If the swap doesn't go through, whatever
    /// replaced `runtime_config` in the meantime already read this same
    /// file itself (see [`Self::apply_new_client_options`]), so dropping
    /// here loses nothing.
    async fn handle_project_config_changed(self: &Arc<Self>, revision: u64) {
        let current = self.runtime_config.load_full();
        if current.revision != revision {
            log::debug!(
                "Dropping project config change notification for stale revision {revision} \
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

        let new_runtime_config = Arc::new(current.with_new_project_config(project_config));
        let previous = self
            .runtime_config
            .compare_and_swap(&current, new_runtime_config);
        if !Arc::ptr_eq(&previous, &current) {
            log::debug!(
                "Dropping project config reload: configuration changed concurrently while \
                 reading the file from disk"
            );
            return;
        }

        log::info!("Project config file changed on disk; reloading and rechecking open documents");
        self.recheck_all().await;
    }

    async fn schedule_check(self: &Arc<Self>, uri: Uri) {
        let Some(token) = self.documents.token(&uri).await else {
            log::debug!(
                "Skipping check schedule for {uri}: document not cached",
                uri = uri.as_str()
            );
            return;
        };
        // Snapshotted once up front (not re-read when the debounce timer
        // fires below) so this check runs under the options and
        // LanguageTool client active when it was *scheduled*, matching
        // what a non-debounced check would have used had it run
        // immediately.
        let runtime_config = self.runtime_config.load_full();
        let debounce = runtime_config.options.debounce_ms;
        log::debug!(
            "Scheduling check for {uri} token={token:?} debounce_ms={debounce}",
            uri = uri.as_str()
        );
        let backend = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce)).await;
            let prepared = backend
                .documents
                .prepare_check_if_current(&uri, token, runtime_config.options_version)
                .await;
            if let Some((prepared, token)) = prepared {
                log::debug!(
                    "Running debounced check for {uri} token={token:?}",
                    uri = uri.as_str()
                );
                backend
                    .run_prepared_check(prepared, token, runtime_config)
                    .await;
            } else {
                log::debug!(
                    "Skipping stale debounced check for {uri} token={token:?}",
                    uri = uri.as_str()
                );
            }
        });
    }

    async fn check_uri_now(&self, uri: &Uri) {
        let runtime_config = self.runtime_config.load_full();
        let Some(prepared) = self
            .documents
            .prepare_check(uri, runtime_config.options_version)
            .await
        else {
            log::debug!(
                "Skipping immediate check for {uri}: document not cached",
                uri = uri.as_str()
            );
            return;
        };
        log::debug!("Running immediate check for {uri}", uri = uri.as_str());
        let (prepared, token) = prepared;
        self.run_prepared_check(prepared, token, runtime_config)
            .await;
    }

    async fn clear_stale_diagnostics(&self, uri: &Uri, version: Option<i32>) {
        log::debug!(
            "Clearing stale diagnostics for {uri} version={version:?}",
            uri = uri.as_str()
        );
        self.client
            .publish_diagnostics(uri.clone(), Vec::new(), version)
            .await;
    }

    async fn run_prepared_check(
        &self,
        prepared: PreparedCheck,
        token: DocumentToken,
        runtime_config: Arc<RuntimeConfig>,
    ) {
        let options = &runtime_config.options;
        let language_tool = &runtime_config.language_tool;
        match prepared {
            PreparedCheck::Check(data) => {
                let uri = data.uri;
                let version = data.version;
                let text = data.text;
                let index = data.index;

                log::debug!(
                    "Starting check for {uri} token={token:?} version={version:?} check_blocks={}",
                    data.blocks.len(),
                    uri = uri.as_str()
                );

                let mut checks = tokio::task::JoinSet::new();
                for block in data.blocks {
                    let language_tool = Arc::clone(language_tool);
                    let options = Arc::clone(options);
                    checks.spawn(async move {
                        let result = language_tool
                            .check_annotated(&block.annotated, &options)
                            .await;
                        (block, result)
                    });
                }

                let mut responses = Vec::new();
                while let Some(result) = checks.join_next().await {
                    match result {
                        Ok((block, Ok(response))) => {
                            log::debug!(
                                "LanguageTool returned {} match(es) for {} token={token:?} block={:?}",
                                response.matches.len(),
                                uri.as_str(),
                                block.byte_range
                            );
                            responses.push((block, response));
                        }
                        Ok((_, Err(err))) => {
                            self.log_check_error(options.as_ref(), err).await;
                        }
                        Err(err) => {
                            let message = format!("LanguageTool check task failed: {err}");
                            log::warn!("{message}");
                            self.client.log_message(MessageType::WARNING, message).await;
                        }
                    }
                }

                responses.sort_by_key(|(block, _)| block.byte_range.start.0);
                let checked_blocks = completed_blocks_from_responses(
                    responses,
                    &text,
                    &index,
                    version,
                    options.as_ref(),
                );
                self.complete_and_publish_check(uri, version, token, checked_blocks)
                    .await;
            }
            PreparedCheck::ReuseCached { uri, version } => {
                self.complete_and_publish_check(uri, version, token, Vec::new())
                    .await;
            }
            PreparedCheck::Clear { uri, version } => {
                log::debug!(
                    "Document {uri} is not checkable; clearing diagnostics",
                    uri = uri.as_str()
                );
                self.clear_stale_diagnostics(&uri, Some(version)).await;
            }
        }
    }

    async fn complete_and_publish_check(
        &self,
        uri: Uri,
        version: i32,
        token: DocumentToken,
        checked_blocks: Vec<CheckedBlock>,
    ) {
        let Some(diagnostics) = self
            .documents
            .complete_check_if_current(&uri, token, checked_blocks)
            .await
        else {
            log::debug!(
                "Discarding stale check result for {} token={:?}",
                uri.as_str(),
                token
            );
            return;
        };

        log::debug!(
            "Publishing {} diagnostic(s) for {uri} token={token:?} version={version:?}",
            diagnostics.len(),
            uri = uri.as_str()
        );

        self.client
            .publish_diagnostics(uri, diagnostics, Some(version))
            .await;
    }

    async fn log_check_error(&self, options: &ClientOptions, err: LanguageToolError) {
        let message = match &err {
            LanguageToolError::Api { .. }
                if matches!(options.backend, LanguageToolBackend::Custom { .. }) =>
            {
                format!(
                    "LanguageTool is not reachable at {}. Is the custom server running? {err}",
                    options.endpoint()
                )
            }
            _ => format!("LanguageTool check failed: {err}"),
        };

        log::warn!("{message}");
        self.client.log_message(MessageType::WARNING, message).await;
    }

    /// Rechecks every open document against this backend's current
    /// options. Called after a configuration change has been installed,
    /// to bring all documents back in sync.
    pub async fn recheck_all(self: &Arc<Self>) {
        let urls = self.documents.urls().await;
        log::info!("Rechecking {} open document(s)", urls.len());
        let mut tasks = tokio::task::JoinSet::new();
        for uri in urls {
            let backend = Arc::clone(self);
            tasks.spawn(async move { backend.check_uri_now(&uri).await });
        }
        while tasks.join_next().await.is_some() {}
    }

    fn project_config_path(&self) -> Option<PathBuf> {
        self.runtime_config
            .load()
            .client_options
            .resolved_project_config_path(self.root.as_deref())
    }

    fn project_config_display_path(&self) -> String {
        self.runtime_config
            .load()
            .client_options
            .project_config_display_path()
    }

    /// Applies `update` to a copy of the current project config and, if it
    /// actually changed anything, saves the result to disk. Deliberately
    /// does *not* install the result into `runtime_config`: per
    /// [`Self::handle_project_config_changed`]'s docs, that's the project
    /// config file watcher's job once it notices the write below.
    async fn updated_project_config(
        &self,
        update: impl FnOnce(&mut ProjectConfig) -> bool,
    ) -> Result<(), String> {
        let Some(project_config_path) = self.project_config_path() else {
            return Err(
                "No workspace folder is open and `projectConfigPath` is not an absolute path; \
                 can't persist project config"
                    .to_string(),
            );
        };

        let mut next_config = self.runtime_config.load().project_config.clone();
        if !update(&mut next_config) {
            log::debug!("Project config update made no changes");
            return Ok(());
        }

        next_config
            .save(&project_config_path)
            .await
            .map_err(|err| format!("Failed to save project config: {err}"))?;

        log::info!(
            "Saved LanguageTool project config to {}; the project config watcher will pick up \
             the change",
            project_config_path.display()
        );
        Ok(())
    }

    async fn add_ignored_word(&self, word: &str) -> Result<(), String> {
        self.updated_project_config(|project_config| project_config.add_ignored_word(word))
            .await
    }

    async fn add_disabled_rule(&self, rule_id: &str) -> Result<(), String> {
        self.updated_project_config(|project_config| project_config.add_disabled_rule(rule_id))
            .await
    }

    async fn add_disabled_category(&self, category_id: &str) -> Result<(), String> {
        self.updated_project_config(|project_config| {
            project_config.add_disabled_category(category_id)
        })
        .await
    }

    pub async fn log_ready(&self) {
        let options = self.options();
        log::info!("LanguageTool LSP ready: {}", options.endpoint());
        self.client
            .log_message(
                MessageType::INFO,
                format!("LanguageTool LSP ready: {}", options.endpoint()),
            )
            .await;
    }

    pub async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        log::info!(
            "Opened document {uri} language_id={} version={} bytes={}",
            params.text_document.language_id,
            params.text_document.version,
            params.text_document.text.len(),
            uri = uri.as_str()
        );
        self.documents.insert(&params.text_document).await;
        if self.options().check_on_open {
            self.check_uri_now(&uri).await;
        } else {
            log::debug!(
                "Skipping open check for {uri}: check_on_open=false",
                uri = uri.as_str()
            );
        }
    }

    pub async fn did_change(self: &Arc<Self>, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        log::debug!(
            "Received {} change(s) for {uri} version={}",
            params.content_changes.len(),
            params.text_document.version,
            uri = uri.as_str()
        );
        self.documents
            .apply_changes(&uri, params.text_document.version, params.content_changes)
            .await;

        if self.options().check_while_typing {
            self.schedule_check(uri).await;
        } else {
            log::debug!(
                "Skipping typing check for {uri}: check_while_typing=false",
                uri = uri.as_str()
            );
        }
    }

    pub async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        log::info!("Saved document {uri}", uri = uri.as_str());
        if self.options().check_on_save {
            self.check_uri_now(&uri).await;
        } else {
            log::debug!(
                "Skipping save check for {uri}: check_on_save=false",
                uri = uri.as_str()
            );
        }
    }

    pub async fn did_close(&self, params: DidCloseTextDocumentParams) {
        log::info!("Closed document {}", params.text_document.uri.as_str());
        self.documents.remove(&params.text_document.uri).await;
        self.clear_stale_diagnostics(&params.text_document.uri, None)
            .await;
    }

    pub async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> RpcResult<Option<CodeActionResponse>> {
        let mut actions = Vec::new();
        let uri = params.text_document.uri;
        let project_config_display_path = self.project_config_display_path();
        let diagnostic_count = params.context.diagnostics.len();
        log::debug!(
            "Building code actions for {uri} diagnostics={diagnostic_count}",
            uri = uri.as_str()
        );

        for diagnostic in params.context.diagnostics {
            if diagnostic.source.as_deref() != Some(SOURCE) {
                continue;
            }
            let Some(data) = parse_diagnostic_data(&diagnostic) else {
                continue;
            };

            for replacement in data.replacements {
                if replacement.is_empty() {
                    continue;
                }
                actions.push(CodeActionOrCommand::CodeAction(make_replacement_action(
                    &uri,
                    &diagnostic,
                    &replacement,
                    data.document_version,
                )));
            }

            if !data.matched_text.trim().is_empty()
                && !data.matched_text.chars().any(char::is_whitespace)
            {
                actions.push(CodeActionOrCommand::Command(make_command(
                    format!(
                        "Ignore '{}' in {}",
                        data.matched_text, project_config_display_path
                    ),
                    COMMAND_IGNORE_WORD,
                    data.matched_text.clone(),
                )));
            }

            actions.push(CodeActionOrCommand::Command(make_command(
                format!(
                    "Disable rule '{}' in {}",
                    data.rule_id, project_config_display_path
                ),
                COMMAND_DISABLE_RULE,
                data.rule_id.clone(),
            )));

            if let Some(category_id) = data.category_id {
                actions.push(CodeActionOrCommand::Command(make_command(
                    format!(
                        "Disable category '{category_id}' in {}",
                        project_config_display_path
                    ),
                    COMMAND_DISABLE_CATEGORY,
                    category_id,
                )));
            }
        }

        if actions.is_empty() {
            log::debug!("No code actions available for {uri}", uri = uri.as_str());
            Ok(None)
        } else {
            log::debug!(
                "Returning {} code action(s) for {uri}",
                actions.len(),
                uri = uri.as_str()
            );
            Ok(Some(actions))
        }
    }

    /// Applies a project-config-mutating command (ignore word / disable
    /// rule / disable category). Only writes the change to disk; the
    /// project config file watcher is responsible for noticing the write
    /// and installing it (see [`Self::handle_project_config_changed`]),
    /// so there's nothing further for the caller to install here.
    pub async fn handle_execute_command(&self, params: ExecuteCommandParams) -> RpcResult<()> {
        log::info!("Executing command {}", params.command);
        let first_arg = params.arguments.first().and_then(Value::as_str);
        match (params.command.as_str(), first_arg) {
            (COMMAND_IGNORE_WORD, Some(word)) => self.add_ignored_word(word).await,
            (COMMAND_DISABLE_RULE, Some(rule_id)) => self.add_disabled_rule(rule_id).await,
            (COMMAND_DISABLE_CATEGORY, Some(category_id)) => {
                self.add_disabled_category(category_id).await
            }
            _ => {
                log::warn!("Unknown or invalid command: {}", params.command);
                Ok(())
            }
        }
        .map_err(RpcError::invalid_params)
    }
}

/// Loads the project config file at `path`, or falls back to an empty one
/// if there's no resolvable path (see
/// [`ClientOptions::resolved_project_config_path`]).
async fn load_project_config(path: Option<&Path>) -> ProjectConfig {
    match path {
        Some(path) => ProjectConfig::load(path).await,
        None => ProjectConfig::default(),
    }
}

/// Pumps [`ProjectConfigChanged`] messages from the project config file
/// watcher into [`LanguageServerBackend::handle_project_config_changed`],
/// for as long as `changed` keeps producing them (i.e. for as long as the
/// watcher task, spawned alongside this loop in
/// [`LanguageServerBackend::new`], is alive).
async fn run_project_config_reload_loop(
    backend: Arc<LanguageServerBackend>,
    mut changed: UnboundedReceiver<ProjectConfigChanged>,
) {
    while let Some(ProjectConfigChanged { revision }) = changed.recv().await {
        backend.handle_project_config_changed(revision).await;
    }
}

struct TextSegment<'a> {
    lt_utf16: Utf16Range,
    doc_byte: ByteRange,
    text: &'a str,
}

fn completed_blocks_from_responses(
    responses: Vec<(CheckBlock, LanguageToolResponse)>,
    text: &str,
    index: &TextIndex,
    version: i32,
    options: &ClientOptions,
) -> Vec<CheckedBlock> {
    responses
        .into_iter()
        .map(|(block, response)| {
            let diagnostics =
                diagnostics_for_block(&block, response.matches, text, index, version, options);
            CheckedBlock {
                byte_range: block.byte_range,
                diagnostics,
            }
        })
        .collect()
}

fn diagnostics_for_block(
    block: &CheckBlock,
    matches: Vec<LanguageToolMatch>,
    text: &str,
    index: &TextIndex,
    version: i32,
    options: &ClientOptions,
) -> Vec<RawDiagnostic> {
    let segments = text_segments_for_block(block);
    let diagnostics = matches
        .iter()
        .filter_map(|item| match_utf16_range(item).map(|range| (item, range)))
        .filter_map(|(item, lt_range)| {
            let doc_byte_range = map_lt_range_to_doc_bytes(&segments, lt_range)?;
            let matched_text = text.get(doc_byte_range.start.0..doc_byte_range.end.0)?;
            if matched_text.trim().is_empty() || options.is_ignored_word(matched_text) {
                return None;
            }

            let utf16_start = index.utf16_offset_for_byte(doc_byte_range.start);
            let utf16_end = index.utf16_offset_for_byte(doc_byte_range.end);
            let range = Range {
                start: index.position(utf16_start),
                end: index.position(utf16_end),
            };
            let data = diagnostic_data_for_text(matched_text.to_string(), item, options, version);
            Some(RawDiagnostic {
                doc_byte_range,
                diagnostic: make_lsp_diagnostic_for_range(range, item, options),
                data,
            })
        })
        .collect::<Vec<_>>();
    log::debug!(
        "Mapped LanguageTool matches to {} diagnostic(s) for block {:?}",
        diagnostics.len(),
        block.byte_range
    );
    diagnostics
}

fn text_segments_for_block(block: &CheckBlock) -> Vec<TextSegment<'_>> {
    let mut segments = Vec::new();
    let mut lt_utf16_cursor = 0usize;
    let mut doc_byte_cursor = block.byte_range.start.0;

    for annotation in &block.annotated.annotation {
        let content = annotation_content(annotation);
        let utf16_len = content.chars().map(char::len_utf16).sum::<usize>();
        let byte_len = content.len();
        if let Annotation::Text { text } = annotation {
            segments.push(TextSegment {
                lt_utf16: Utf16Range::new(lt_utf16_cursor, lt_utf16_cursor + utf16_len),
                doc_byte: ByteRange::new(doc_byte_cursor, doc_byte_cursor + byte_len),
                text,
            });
        }
        lt_utf16_cursor += utf16_len;
        doc_byte_cursor += byte_len;
    }

    segments
}

fn annotation_content(annotation: &Annotation) -> &str {
    match annotation {
        Annotation::Text { text } => text,
        Annotation::Markup { markup, .. } => markup,
    }
}

fn map_lt_range_to_doc_bytes(
    segments: &[TextSegment<'_>],
    lt_range: Utf16Range,
) -> Option<ByteRange> {
    let segment = segments.iter().find(|segment| {
        segment.lt_utf16.start <= lt_range.start && lt_range.end <= segment.lt_utf16.end
    });
    let Some(segment) = segment else {
        log::debug!(
            "Dropping LT match at utf16 {}..{}: spans markup boundary",
            lt_range.start.0,
            lt_range.end.0
        );
        return None;
    };
    let relative_start = lt_range.start.0 - segment.lt_utf16.start.0;
    let relative_end = lt_range.end.0 - segment.lt_utf16.start.0;
    let byte_start =
        segment.doc_byte.start.0 + byte_offset_for_utf16_in_text(segment.text, relative_start)?;
    let byte_end =
        segment.doc_byte.start.0 + byte_offset_for_utf16_in_text(segment.text, relative_end)?;
    Some(ByteRange::new(byte_start, byte_end))
}

fn byte_offset_for_utf16_in_text(text: &str, target: usize) -> Option<usize> {
    let mut utf16 = 0usize;
    for (byte, ch) in text.char_indices() {
        if utf16 == target {
            return Some(byte);
        }
        utf16 += ch.len_utf16();
        if utf16 == target {
            return Some(byte + ch.len_utf8());
        }
        if utf16 > target {
            return None;
        }
    }
    (utf16 == target).then_some(text.len())
}

fn make_replacement_action(
    uri: &Uri,
    diagnostic: &Diagnostic,
    replacement: &str,
    document_version: i32,
) -> CodeAction {
    let edit = TextEdit {
        range: diagnostic.range,
        new_text: replacement.to_string(),
    };

    CodeAction {
        title: format!("Replace with '{replacement}'"),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diagnostic.clone()]),
        edit: Some(WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: Some(document_version),
                },
                edits: vec![OneOf::Left(edit)],
            }])),
            change_annotations: None,
        }),
        command: None,
        is_preferred: None,
        disabled: None,
        data: None,
    }
}

fn make_command(title: String, command: &str, argument: String) -> Command {
    Command {
        title,
        command: command.to_string(),
        arguments: Some(vec![Value::String(argument)]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, PreparedCheck};
    use crate::languagetool::{
        LanguageToolCategory, LanguageToolMatch, LanguageToolReplacement, LanguageToolRule,
    };

    struct TestRequest {
        block: CheckBlock,
        text: Arc<String>,
        index: Arc<TextIndex>,
    }

    fn prepare_test_request(mut document: Document) -> TestRequest {
        let PreparedCheck::Check(prepared) = document.prepare_check(0) else {
            panic!("document should be checkable");
        };
        let block = prepared
            .blocks
            .into_iter()
            .next()
            .expect("document should have a check block");
        TestRequest {
            block,
            text: prepared.text,
            index: prepared.index,
        }
    }
    #[test]
    fn builds_diagnostics_for_document() {
        let document = Document::new(
            "file:///tmp/test.txt".parse::<Uri>().unwrap(),
            1,
            Some("plaintext".to_string()),
            "This are a tset.".to_string(),
        );
        let options = ClientOptions::default();
        let request = prepare_test_request(document);
        let item = LanguageToolMatch {
            message: "Possible spelling mistake found.".to_string(),
            short_message: None,
            offset: 11,
            length: 4,
            replacements: vec![LanguageToolReplacement {
                value: Some("test".to_string()),
            }],
            context: Box::default(),
            sentence: String::new(),
            rule: Some(Box::new(LanguageToolRule {
                id: "MORFOLOGIK_RULE_EN_US".to_string(),
                sub_id: None,
                description: String::new(),
                urls: None,
                issue_type: Some("misspelling".to_string()),
                category: Box::new(LanguageToolCategory {
                    id: Some("TYPOS".to_string()),
                    name: None,
                }),
            })),
        };

        let diagnostics = diagnostics_for_block(
            &request.block,
            vec![item],
            &request.text,
            &request.index,
            1,
            &options,
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].diagnostic.range.start, Position::new(0, 11));
        assert_eq!(diagnostics[0].diagnostic.range.end, Position::new(0, 15));
    }

    #[test]
    fn diagnostics_use_original_document_offsets() {
        let document = Document::new(
            "file:///tmp/test.rs".parse::<Uri>().unwrap(),
            1,
            Some("rust".to_string()),
            "let value = 1; // This are a comment.".to_string(),
        );
        let options = ClientOptions::default();
        let request = prepare_test_request(document);
        let item = LanguageToolMatch {
            message: "The singular demonstrative pronoun does not agree.".to_string(),
            short_message: None,
            offset: 3,
            length: 4,
            replacements: Vec::new(),
            context: Box::default(),
            sentence: String::new(),
            rule: Some(Box::new(LanguageToolRule {
                id: "THIS_NNS".to_string(),
                sub_id: None,
                description: String::new(),
                urls: None,
                issue_type: None,
                category: Box::new(LanguageToolCategory::new()),
            })),
        };

        let diagnostics = diagnostics_for_block(
            &request.block,
            vec![item],
            &request.text,
            &request.index,
            1,
            &options,
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].diagnostic.range.start, Position::new(0, 18));
        assert_eq!(diagnostics[0].diagnostic.range.end, Position::new(0, 22));
    }

    #[test]
    fn diagnostics_drop_matches_in_markup_regions() {
        let document = Document::new(
            "file:///tmp/test.rs".parse::<Uri>().unwrap(),
            1,
            Some("rust".to_string()),
            "let typoo = 1; // This are a comment.".to_string(),
        );
        let options = ClientOptions::default();
        let request = prepare_test_request(document);
        let item = LanguageToolMatch {
            message: "Possible spelling mistake found.".to_string(),
            short_message: None,
            offset: 0,
            length: 2,
            replacements: Vec::new(),
            context: Box::default(),
            sentence: String::new(),
            rule: Some(Box::new(LanguageToolRule {
                id: "MORFOLOGIK_RULE_EN_US".to_string(),
                sub_id: None,
                description: String::new(),
                urls: None,
                issue_type: Some("misspelling".to_string()),
                category: Box::new(LanguageToolCategory {
                    id: Some("TYPOS".to_string()),
                    name: None,
                }),
            })),
        };

        let diagnostics = diagnostics_for_block(
            &request.block,
            vec![item],
            &request.text,
            &request.index,
            1,
            &options,
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diagnostics_use_languagetool_utf16_offsets() {
        let document = Document::new(
            "file:///tmp/test.txt".parse::<Uri>().unwrap(),
            1,
            Some("plaintext".to_string()),
            "😀 This are a tset.".to_string(),
        );
        let options = ClientOptions::default();
        let request = prepare_test_request(document);
        let item = LanguageToolMatch {
            message: "The verb 'are' is plural.".to_string(),
            short_message: None,
            offset: 3,
            length: 8,
            replacements: Vec::new(),
            context: Box::default(),
            sentence: String::new(),
            rule: Some(Box::new(LanguageToolRule {
                id: "PLURAL_VERB_AFTER_THIS".to_string(),
                sub_id: None,
                description: String::new(),
                urls: None,
                issue_type: Some("grammar".to_string()),
                category: Box::new(LanguageToolCategory {
                    id: Some("GRAMMAR".to_string()),
                    name: None,
                }),
            })),
        };

        let diagnostics = diagnostics_for_block(
            &request.block,
            vec![item],
            &request.text,
            &request.index,
            1,
            &options,
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].diagnostic.range.start, Position::new(0, 3));
        assert_eq!(diagnostics[0].diagnostic.range.end, Position::new(0, 11));

        assert_eq!(diagnostics[0].data.matched_text, "This are");
    }
}

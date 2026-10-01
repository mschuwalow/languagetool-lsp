use crate::config::{ClientOptions, LanguageToolBackend, ProjectConfig};
use crate::diagnostics::{
    RawDiagnostic, SOURCE, diagnostic_data_for_text, make_lsp_diagnostic_for_range,
    match_utf16_range, parse_diagnostic_data,
};
use crate::document_cache::{CheckedBlock, DocumentCache, DocumentToken, PreparedCheck};
use crate::languagetool::{
    Annotation, LanguageToolClient, LanguageToolError, LanguageToolMatch, LanguageToolResponse,
};
use crate::masking::CheckBlock;
use crate::text_index::{ByteRange, TextIndex, Utf16Range};
use arc_swap::ArcSwap;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tower_lsp_server::Client;
use tower_lsp_server::jsonrpc::{Error as RpcError, Result as RpcResult};
use tower_lsp_server::ls_types::*;

const COMMAND_IGNORE_WORD: &str = "languagetool.ignoreWordInWorkspace";
const COMMAND_DISABLE_RULE: &str = "languagetool.disableRuleInWorkspace";
const COMMAND_DISABLE_CATEGORY: &str = "languagetool.disableCategoryInWorkspace";

/// Everything the server knows once `initialize` (or the most recent
/// `workspace/didChangeConfiguration`, or the project config file watcher
/// picking up a change to the project config file on disk) has resolved
/// workspace root, client options, project config, and the merged
/// effective options. This is treated as an immutable snapshot: there's
/// no interior mutability here (unlike `documents`, which really is live
/// shared state). A configuration change produces a *new*
/// `LanguageServerBackend` (see [`LanguageServerBackend::with_new_config`])
/// rather than mutating this one in place — see that method's docs for the
/// resulting trade-off with in-flight debounced checks.
#[derive(Clone)]
pub struct LanguageServerBackend {
    client: Client,
    /// Workspace root, resolved once from the LSP workspace folders / root
    /// URI during `initialize`. `None` when the client opened a single
    /// file with no folder (legal per the LSP spec: `rootUri` and
    /// `workspaceFolders` may both be `null`).
    root: Option<PathBuf>,
    documents: DocumentCache,
    client_options: ClientOptions,
    project_config: ProjectConfig,
    /// `project_config` merged into `client_options`; the options actually
    /// used for checks.
    options: Arc<ClientOptions>,
    /// Bumped every time `options` is recomputed (i.e. on every
    /// [`LanguageServerBackend::with_new_config`] /
    /// [`LanguageServerBackend::with_new_project_config`]), so a
    /// document's per-block diagnostics cache can tell a check that ran
    /// under old options apart from one that ran under the current ones.
    options_version: u64,
    language_tool: LanguageToolClient,
}

impl LanguageServerBackend {
    pub async fn new(client: Client, root: Option<PathBuf>, client_options: ClientOptions) -> Self {
        let project_config = load_project_config(&client_options, root.as_deref()).await;
        let options = Arc::new(project_config.merged_options(&client_options));
        let language_tool = LanguageToolClient::new(&options.backend);
        Self {
            client,
            root,
            documents: DocumentCache::default(),
            client_options,
            project_config,
            options,
            options_version: 0,
            language_tool,
        }
    }

    pub fn options(&self) -> &Arc<ClientOptions> {
        &self.options
    }

    /// Rebuilds everything derived from client-provided configuration
    /// (`client_options`, `project_config` reloaded if its resolved path
    /// changed, the merged `options`, and the LanguageTool HTTP client)
    /// while reusing the existing open-document cache — document contents
    /// aren't part of "configuration" and don't need re-syncing with the
    /// client.
    ///
    /// Note: any check already scheduled (debounced `didChange` or an
    /// in-flight `recheck_all`) against the *previous* backend keeps using
    /// that snapshot's options until it completes, since it holds its own
    /// clone. The caller is expected to trigger [`Self::recheck_all`] on
    /// the returned backend to bring every open document back in sync.
    pub async fn with_new_config(&self, client_options: ClientOptions) -> Self {
        let project_config = load_project_config(&client_options, self.root.as_deref()).await;
        let options = Arc::new(project_config.merged_options(&client_options));
        let language_tool = LanguageToolClient::new(&options.backend);
        Self {
            client: self.client.clone(),
            root: self.root.clone(),
            documents: self.documents.clone(),
            client_options,
            project_config,
            options,
            options_version: self.options_version + 1,
            language_tool,
        }
    }

    /// Like [`Self::with_new_config`], but for when only `project_config`
    /// changed on disk (via a `workspace/executeCommand` that edits ignored
    /// words / disabled rules / disabled categories). `client_options` and
    /// the LanguageTool HTTP client are unaffected by that, so both are
    /// reused as-is; only the merged `options` and `options_version` change.
    fn with_new_project_config(&self, project_config: ProjectConfig) -> Self {
        let options = Arc::new(project_config.merged_options(&self.client_options));
        Self {
            client: self.client.clone(),
            root: self.root.clone(),
            documents: self.documents.clone(),
            client_options: self.client_options.clone(),
            project_config,
            options,
            options_version: self.options_version + 1,
            language_tool: self.language_tool.clone(),
        }
    }

    /// Parses `settings` as a full replacement [`ClientOptions`] value and
    /// rebuilds everything derived from it via [`Self::with_new_config`].
    /// This does *not* merge with the previous options: any field the
    /// payload doesn't mention falls back to its `#[serde(default)]`, not
    /// whatever was set at `initialize` or by an earlier config change —
    /// if a user drops a setting from their config, they expect the
    /// default to take effect, not for a stale value to linger.
    /// Returns `None` for a `null` notification (nothing changed) or a
    /// value that fails to parse (reported to the client, previous options
    /// kept as-is); the caller should install and recheck against `Some`.
    pub async fn with_new_config_from_settings(&self, settings: Value) -> Option<Self> {
        if settings == Value::Null {
            log::debug!("Ignoring null configuration change notification");
            return None;
        }

        let client_options = match ClientOptions::parse_value(settings) {
            Ok(client_options) => client_options,
            Err(err) => {
                let message = format!(
                    "Ignoring invalid LanguageTool configuration change; keeping previous options: {err}"
                );
                log::error!("{message}");
                self.client.log_message(MessageType::ERROR, message).await;
                return None;
            }
        };

        log::info!("LanguageTool configuration changed; reloading options and project config");
        Some(self.with_new_config(client_options).await)
    }

    async fn schedule_check(&self, uri: Uri) {
        let Some(token) = self.documents.token(&uri).await else {
            log::debug!(
                "Skipping check schedule for {uri}: document not cached",
                uri = uri.as_str()
            );
            return;
        };
        let debounce = self.options.debounce_ms;
        log::debug!(
            "Scheduling check for {uri} token={token:?} debounce_ms={debounce}",
            uri = uri.as_str()
        );
        let backend = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce)).await;
            let prepared = backend
                .documents
                .prepare_check_if_current(&uri, token, backend.options_version)
                .await;
            if let Some((prepared, token)) = prepared {
                log::debug!(
                    "Running debounced check for {uri} token={token:?}",
                    uri = uri.as_str()
                );
                backend
                    .run_prepared_check(prepared, token, backend.options.clone())
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
        let Some(prepared) = self
            .documents
            .prepare_check(uri, self.options_version)
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
        self.run_prepared_check(prepared, token, self.options.clone())
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
        options: Arc<ClientOptions>,
    ) {
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
                    let language_tool = self.language_tool.clone();
                    let options = Arc::clone(&options);
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

    /// Rechecks every open document against this backend's (current)
    /// options. Called after a config change has been installed as the new
    /// backend, to bring all documents back in sync.
    pub async fn recheck_all(&self) {
        let urls = self.documents.urls().await;
        log::info!("Rechecking {} open document(s)", urls.len());
        let mut tasks = tokio::task::JoinSet::new();
        for uri in urls {
            let backend = self.clone();
            tasks.spawn(async move { backend.check_uri_now(&uri).await });
        }
        while tasks.join_next().await.is_some() {}
    }

    pub fn project_config_path(&self) -> Option<PathBuf> {
        self.client_options
            .resolved_project_config_path(self.root.as_deref())
    }

    fn project_config_display_path(&self) -> String {
        self.client_options.project_config_display_path()
    }

    /// Reloads `project_config` from the currently resolved project config
    /// path and, if it actually differs from what's currently loaded,
    /// rebuilds derived options via [`Self::with_new_project_config`].
    /// Returns `None` if nothing changed (including when there's no
    /// resolvable path at all), in which case there's nothing for the
    /// caller to install or recheck against.
    ///
    /// This is how project config changes reach a running server: both
    /// the project config file watcher (picking up hand edits, or edits
    /// from another editor window) and this server's own
    /// `workspace/executeCommand` handlers (which only write to disk; see
    /// [`Self::handle_execute_command`]) rely on this being called after
    /// the file changes, rather than swapping in a new backend directly.
    pub async fn reload_project_config_if_changed(&self) -> Option<Self> {
        let project_config = load_project_config(&self.client_options, self.root.as_deref()).await;
        if project_config == self.project_config {
            return None;
        }
        Some(self.with_new_project_config(project_config))
    }

    /// Applies `update` to a copy of the current project config and, if it
    /// actually changed anything, saves the result to disk. Deliberately
    /// does *not* install the result as this backend's `project_config`:
    /// per [`Self::reload_project_config_if_changed`]'s docs, that's the
    /// project config file watcher's job once it notices the write below.
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

        let mut next_config = self.project_config.clone();
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
        let options = &self.options;
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
        if self.options.check_on_open {
            self.check_uri_now(&uri).await;
        } else {
            log::debug!(
                "Skipping open check for {uri}: check_on_open=false",
                uri = uri.as_str()
            );
        }
    }

    pub async fn did_change(&self, params: DidChangeTextDocumentParams) {
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

        if self.options.check_while_typing {
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
        if self.options.check_on_save {
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
    /// and installing it via [`Self::reload_project_config_if_changed`],
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

/// A shared, swappable handle to the server's current
/// [`LanguageServerBackend`] (or lack thereof, before `initialize`
/// completes). Cloning is cheap (an `Arc` bump) and every clone observes
/// the same underlying value, which is how the project config file
/// watcher (a background task with its own long-lived clone) and the
/// request-handling code both see backend installs from one another.
#[derive(Clone)]
pub struct SharedBackend(Arc<ArcSwap<Option<LanguageServerBackend>>>);

impl SharedBackend {
    pub fn new() -> Self {
        Self(Arc::new(ArcSwap::new(Arc::new(None))))
    }

    pub fn get(&self) -> Option<LanguageServerBackend> {
        self.0.load().as_ref().clone()
    }

    pub fn install(&self, backend: LanguageServerBackend) {
        self.0.store(Arc::new(Some(backend)));
    }

    /// Installs `backend` iff nothing has been installed yet. Returns
    /// `true` if it was installed.
    ///
    /// This guards against two concurrent `initialize` requests racing
    /// each other: `tower-lsp-server`'s own duplicate-`initialize` guard
    /// is a check-then-dispatch against an atomic flag that isn't set
    /// until the `initialize` future resolves, so both requests could
    /// otherwise reach this point. Comparing-and-swapping against the
    /// exact `Arc` most recently observed (by pointer identity, which is
    /// what `ArcSwap::compare_and_swap` checks) rather than just checking
    /// `is_some()` and then unconditionally storing closes that race.
    pub fn try_initialize(&self, backend: LanguageServerBackend) -> bool {
        let uninitialized = self.0.load();
        if uninitialized.is_some() {
            return false;
        }
        let previous = self
            .0
            .compare_and_swap(&uninitialized, Arc::new(Some(backend)));
        Arc::ptr_eq(&previous, &uninitialized)
    }
}

impl Default for SharedBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Loads the project config file `client_options` resolves to against
/// `root`, or falls back to an empty one if there's no resolvable path
/// (see [`ClientOptions::resolved_project_config_path`]).
async fn load_project_config(client_options: &ClientOptions, root: Option<&Path>) -> ProjectConfig {
    match client_options.resolved_project_config_path(root) {
        Some(path) => ProjectConfig::load(&path).await,
        None => ProjectConfig::default(),
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

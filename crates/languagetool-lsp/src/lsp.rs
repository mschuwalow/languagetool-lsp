use crate::config::{BackendKind, ClientOptions, ProjectConfig};
use crate::diagnostics::{
    diagnostic_data_for_text, make_lsp_diagnostic_for_range, match_utf16_range,
    parse_diagnostic_data, SOURCE,
};
use crate::diagnostics_cache::CachedDiagnostic;
use crate::document_cache::{
    ChangeStatus, CompletedCheckBlock, DocumentCache, DocumentToken, PreparedCheck,
};
use crate::languagetool::{
    Annotation, LanguageToolClient, LanguageToolError, LanguageToolMatch, LanguageToolResponse,
};
use crate::masking::CheckBlock;
use crate::runtime_config::RuntimeConfig;
use crate::text_index::{ByteRange, TextIndex, Utf16Range};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tower_lsp_server::jsonrpc::{Error as RpcError, Result as RpcResult};
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer};

const COMMAND_IGNORE_WORD: &str = "languagetool.ignoreWordInWorkspace";
const COMMAND_DISABLE_RULE: &str = "languagetool.disableRuleInWorkspace";
const COMMAND_DISABLE_CATEGORY: &str = "languagetool.disableCategoryInWorkspace";

#[derive(Clone)]
pub struct Backend {
    client: Client,
    root: Arc<RwLock<PathBuf>>,
    documents: DocumentCache,
    config: RuntimeConfig,
    language_tool: LanguageToolClient,
}

impl Backend {
    pub fn new(client: Client, root: PathBuf) -> Self {
        Self {
            client,
            root: Arc::new(RwLock::new(root)),
            documents: DocumentCache::default(),
            config: RuntimeConfig::default(),
            language_tool: LanguageToolClient::new(),
        }
    }

    async fn options(&self) -> ClientOptions {
        self.config.options().await
    }

    async fn schedule_check(&self, uri: Uri) {
        let Some(token) = self.documents.token(&uri) else {
            log::debug!(
                "Skipping check schedule for {uri}: document not cached",
                uri = uri.as_str()
            );
            return;
        };
        let debounce = self.options().await.debounce_ms;
        log::debug!(
            "Scheduling check for {uri} token={token:?} debounce_ms={debounce}",
            uri = uri.as_str()
        );
        let backend = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce)).await;
            let options = backend.options().await;
            let options_key = options_key(&options);
            let prepared = backend
                .documents
                .prepare_check_if_current(&uri, token, options_key);
            if let Some((prepared, token)) = prepared {
                log::debug!(
                    "Running debounced check for {uri} token={token:?}",
                    uri = uri.as_str()
                );
                backend.run_prepared_check(prepared, token, options).await;
            } else {
                log::debug!(
                    "Skipping stale debounced check for {uri} token={token:?}",
                    uri = uri.as_str()
                );
            }
        });
    }

    async fn check_uri_now(&self, uri: &Uri) {
        let options = self.options().await;
        let options_key = options_key(&options);
        let Some(prepared) = self.documents.prepare_check(uri, options_key) else {
            log::debug!(
                "Skipping immediate check for {uri}: document not cached",
                uri = uri.as_str()
            );
            return;
        };
        log::debug!("Running immediate check for {uri}", uri = uri.as_str());
        let (prepared, token) = prepared;
        self.run_prepared_check(prepared, token, options).await;
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
        options: ClientOptions,
    ) {
        match prepared {
            PreparedCheck::Check(data) => {
                let uri = data.uri;
                let version = data.version;
                let text = data.text;
                let index = data.index;
                let options = Arc::new(options);

                log::debug!(
                    "Starting check for {uri} token={token:?} version={version:?} check_blocks={}",
                    data.blocks.len(),
                    uri = uri.as_str()
                );

                let mut checks = tokio::task::JoinSet::new();
                for block in data.blocks {
                    let language_tool = self.language_tool.clone();
                    let options = Arc::clone(&options);
                    let request = CheckRequest {
                        uri: uri.clone(),
                        version,
                        token,
                        text: Arc::clone(&text),
                        index: Arc::clone(&index),
                        block,
                    };
                    checks.spawn(async move {
                        let result = language_tool
                            .check_annotated(&request.block.annotated, options.as_ref())
                            .await;
                        (request, result)
                    });
                }

                let mut responses = Vec::new();
                while let Some(result) = checks.join_next().await {
                    match result {
                        Ok((request, Ok(response))) => {
                            log::debug!(
                                "LanguageTool returned {} match(es) for {} token={:?} block={:?}",
                                response.matches.len(),
                                request.uri.as_str(),
                                request.token,
                                request.block.byte_range
                            );
                            responses.push((request, response));
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

                responses.sort_by_key(|(request, _)| request.block.byte_range.start.0);
                let checked_blocks = completed_blocks_from_responses(responses, options.as_ref());
                self.complete_and_publish_check(uri, version, token, checked_blocks, false)
                    .await;
            }
            PreparedCheck::ReuseCached { uri, version } => {
                self.complete_and_publish_check(uri, version, token, Vec::new(), true)
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
        checked_blocks: Vec<CompletedCheckBlock>,
        cached: bool,
    ) {
        let Some(diagnostics) =
            self.documents
                .complete_check_if_current(&uri, token, checked_blocks)
        else {
            if cached {
                log::debug!(
                    "Discarding stale cached check result for {} token={:?}",
                    uri.as_str(),
                    token
                );
            } else {
                log::debug!(
                    "Discarding stale check result for {} token={:?}",
                    uri.as_str(),
                    token
                );
            }
            return;
        };

        if cached {
            log::debug!(
                "Publishing {} cached diagnostic(s) for {uri} token={token:?} version={version:?}",
                diagnostics.len(),
                uri = uri.as_str()
            );
        } else {
            log::debug!(
                "Publishing {} diagnostic(s) for {uri} token={token:?} version={version:?}",
                diagnostics.len(),
                uri = uri.as_str()
            );
        }

        self.client
            .publish_diagnostics(uri, diagnostics, Some(version))
            .await;
    }

    async fn log_check_error(&self, options: &ClientOptions, err: LanguageToolError) {
        let message = match &err {
            LanguageToolError::Api { .. } if matches!(options.backend, BackendKind::Custom) => {
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

    async fn recheck_all(&self) {
        let urls = self.documents.urls();
        log::info!("Rechecking {} open document(s)", urls.len());
        for uri in urls {
            self.check_uri_now(&uri).await;
        }
    }

    async fn project_config_path(&self) -> PathBuf {
        let root = self.root.read().expect("workspace root poisoned").clone();
        self.config.project_config_path(&root).await
    }

    async fn project_config_display_path(&self) -> String {
        self.config.project_config_display_path().await
    }

    async fn update_project_config(
        &self,
        update: impl FnOnce(&mut ProjectConfig) -> bool,
    ) -> Result<bool, String> {
        let project_config_path = self.project_config_path().await;
        let updated = self
            .config
            .update_project_config(&project_config_path, update)
            .await?;

        if !updated {
            log::debug!("Project config update made no changes");
            return Ok(false);
        }

        log::info!(
            "Saved LanguageTool project config to {}",
            project_config_path.display()
        );
        Ok(true)
    }

    async fn add_ignored_word(&self, word: &str) -> Result<bool, String> {
        self.update_project_config(|project_config| project_config.add_ignored_word(word))
            .await
    }

    async fn add_disabled_rule(&self, rule_id: &str) -> Result<bool, String> {
        self.update_project_config(|project_config| project_config.add_disabled_rule(rule_id))
            .await
    }

    async fn add_disabled_category(&self, category_id: &str) -> Result<bool, String> {
        self.update_project_config(|project_config| {
            project_config.add_disabled_category(category_id)
        })
        .await
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        if let Some(root) = workspace_root(&params) {
            *self.root.write().expect("workspace root poisoned") = root;
        }
        let client_options = ClientOptions::from_value(params.initialization_options);
        let root = self.root.read().expect("workspace root poisoned").clone();
        self.config.set_client_options(client_options, &root).await;
        let options = self.options().await;
        log::info!(
            "LanguageTool LSP initialized for {} using {}",
            root.display(),
            options.endpoint()
        );

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                position_encoding: Some(PositionEncodingKind::UTF16),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..TextDocumentSyncOptions::default()
                    },
                )),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                        resolve_provider: Some(false),
                        work_done_progress_options: WorkDoneProgressOptions::default(),
                    },
                )),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        COMMAND_IGNORE_WORD.to_string(),
                        COMMAND_DISABLE_RULE.to_string(),
                        COMMAND_DISABLE_CATEGORY.to_string(),
                    ],
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "LanguageTool LSP".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            offset_encoding: None,
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let options = self.options().await;
        log::info!("LanguageTool LSP ready: {}", options.endpoint());
        self.client
            .log_message(
                MessageType::INFO,
                format!("LanguageTool LSP ready: {}", options.endpoint()),
            )
            .await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        log::info!("LanguageTool LSP shutdown requested");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        log::info!(
            "Opened document {uri} language_id={} version={} bytes={}",
            params.text_document.language_id,
            params.text_document.version,
            params.text_document.text.len(),
            uri = uri.as_str()
        );
        self.documents.insert(&params.text_document);
        if self.options().await.check_on_open {
            self.check_uri_now(&uri).await;
        } else {
            log::debug!(
                "Skipping open check for {uri}: check_on_open=false",
                uri = uri.as_str()
            );
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let had_changes = !params.content_changes.is_empty();
        log::debug!(
            "Received {} change(s) for {uri} version={}",
            params.content_changes.len(),
            params.text_document.version,
            uri = uri.as_str()
        );
        let change_status = self.documents.apply_changes(
            &uri,
            params.text_document.version,
            params.content_changes,
        );

        if change_status == ChangeStatus::OutOfSync {
            self.clear_stale_diagnostics(&uri, Some(params.text_document.version))
                .await;
            return;
        }

        if had_changes && self.options().await.check_while_typing {
            self.schedule_check(uri).await;
        } else if had_changes {
            log::debug!(
                "Skipping typing check for {uri}: check_while_typing=false",
                uri = uri.as_str()
            );
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        log::info!("Saved document {uri}", uri = uri.as_str());
        if self.options().await.check_on_save {
            self.check_uri_now(&uri).await;
        } else {
            log::debug!(
                "Skipping save check for {uri}: check_on_save=false",
                uri = uri.as_str()
            );
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        log::info!("Closed document {}", params.text_document.uri.as_str());
        self.documents.remove(&params.text_document.uri);
        self.clear_stale_diagnostics(&params.text_document.uri, None)
            .await;
    }

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        let mut actions = Vec::new();
        let uri = params.text_document.uri;
        let project_config_display_path = self.project_config_display_path().await;
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

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        if params.settings != Value::Null {
            log::info!("LanguageTool configuration changed; reloading options and project config");
            let root = self.root.read().expect("workspace root poisoned").clone();
            if let Err(err) = self
                .config
                .update_client_options(params.settings, &root)
                .await
            {
                let message = format!(
                    "Ignoring invalid LanguageTool configuration change; keeping previous options: {err}"
                );
                log::error!("{message}");
                self.client.log_message(MessageType::ERROR, message).await;
                return;
            }
            self.recheck_all().await;
        } else {
            log::debug!("Ignoring null configuration change notification");
        }
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> RpcResult<Option<Value>> {
        log::info!("Executing command {}", params.command);
        let first_arg = params.arguments.first().and_then(Value::as_str);
        let updated = match (params.command.as_str(), first_arg) {
            (COMMAND_IGNORE_WORD, Some(word)) => self.add_ignored_word(word).await,
            (COMMAND_DISABLE_RULE, Some(rule_id)) => self.add_disabled_rule(rule_id).await,
            (COMMAND_DISABLE_CATEGORY, Some(category_id)) => {
                self.add_disabled_category(category_id).await
            }
            _ => {
                log::warn!("Unknown or invalid command: {}", params.command);
                Ok(false)
            }
        }
        .map_err(RpcError::invalid_params)?;

        if updated {
            log::info!(
                "Command {} updated project config; scheduling recheck",
                params.command
            );
            let backend = self.clone();
            tokio::spawn(async move {
                backend.recheck_all().await;
            });
        } else {
            log::debug!("Command {} did not change project config", params.command);
        }
        Ok(None)
    }
}

struct CheckRequest {
    uri: Uri,
    version: i32,
    token: DocumentToken,
    text: Arc<String>,
    index: Arc<TextIndex>,
    block: CheckBlock,
}

#[derive(Debug, Clone)]
struct MappedDiagnostic {
    doc_byte_range: ByteRange,
    diagnostic: Diagnostic,
}

struct TextSegment<'a> {
    lt_utf16: Utf16Range,
    doc_byte: ByteRange,
    text: &'a str,
}

fn completed_blocks_from_responses(
    responses: Vec<(CheckRequest, LanguageToolResponse)>,
    options: &ClientOptions,
) -> Vec<CompletedCheckBlock> {
    responses
        .into_iter()
        .map(|(request, response)| {
            let diagnostics = diagnostics_for_request(&request, response.matches, options);
            CompletedCheckBlock {
                byte_range: request.block.byte_range,
                diagnostics: diagnostics
                    .into_iter()
                    .map(|diagnostic| CachedDiagnostic {
                        doc_byte_range: diagnostic.doc_byte_range,
                        diagnostic: diagnostic.diagnostic,
                    })
                    .collect(),
            }
        })
        .collect()
}

fn diagnostics_for_request(
    request: &CheckRequest,
    matches: Vec<LanguageToolMatch>,
    options: &ClientOptions,
) -> Vec<MappedDiagnostic> {
    let segments = text_segments_for_block(&request.block);
    let diagnostics = matches
        .iter()
        .filter_map(|item| match_utf16_range(item).map(|range| (item, range)))
        .filter_map(|(item, lt_range)| {
            let doc_byte_range = map_lt_range_to_doc_bytes(&segments, lt_range)?;
            let matched_text = request
                .text
                .get(doc_byte_range.start.0..doc_byte_range.end.0)?;
            if matched_text.trim().is_empty() || options.is_ignored_word(matched_text) {
                return None;
            }

            let utf16_start = request.index.utf16_offset_for_byte(doc_byte_range.start);
            let utf16_end = request.index.utf16_offset_for_byte(doc_byte_range.end);
            let range = Range {
                start: request.index.position(utf16_start),
                end: request.index.position(utf16_end),
            };
            let data = diagnostic_data_for_text(
                matched_text.to_string(),
                item,
                options,
                Some(request.version),
            );
            Some(MappedDiagnostic {
                doc_byte_range,
                diagnostic: make_lsp_diagnostic_for_range(range, item, data, options),
            })
        })
        .collect::<Vec<_>>();
    log::debug!(
        "Mapped LanguageTool matches to {} diagnostic(s) for {}",
        diagnostics.len(),
        request.uri.as_str()
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
    })?;
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

fn options_key(options: &ClientOptions) -> String {
    serde_json::to_string(options).unwrap_or_else(|_| format!("{options:?}"))
}

fn make_replacement_action(
    uri: &Uri,
    diagnostic: &Diagnostic,
    replacement: &str,
    document_version: Option<i32>,
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
                    version: document_version,
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

#[allow(deprecated)]
fn workspace_root(params: &InitializeParams) -> Option<PathBuf> {
    params
        .workspace_folders
        .as_ref()
        .and_then(|folders| folders.first())
        .and_then(|folder| folder.uri.to_file_path().map(|path| path.into_owned()))
        .or_else(|| {
            params
                .root_uri
                .as_ref()
                .and_then(|uri| uri.to_file_path().map(|path| path.into_owned()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, PreparedCheck};
    use crate::languagetool::{
        LanguageToolCategory, LanguageToolMatch, LanguageToolReplacement, LanguageToolRule,
    };

    fn check_request_for_test(mut document: Document, options: &ClientOptions) -> CheckRequest {
        let PreparedCheck::Check(prepared) = document.prepare_check(options_key(options)) else {
            panic!("document should be checkable");
        };
        let block = prepared
            .blocks
            .into_iter()
            .next()
            .expect("document should have a check block");
        CheckRequest {
            uri: prepared.uri,
            version: prepared.version,
            token: DocumentToken::new_for_test(0, prepared.version, 0),
            text: prepared.text,
            index: prepared.index,
            block,
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
        let request = check_request_for_test(document, &options);
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

        let diagnostics = diagnostics_for_request(&request, vec![item], &options);
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
        let request = check_request_for_test(document, &options);
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

        let diagnostics = diagnostics_for_request(&request, vec![item], &options);
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
        let request = check_request_for_test(document, &options);
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

        let diagnostics = diagnostics_for_request(&request, vec![item], &options);
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
        let request = check_request_for_test(document, &options);
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

        let diagnostics = diagnostics_for_request(&request, vec![item], &options);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].diagnostic.range.start, Position::new(0, 3));
        assert_eq!(diagnostics[0].diagnostic.range.end, Position::new(0, 11));

        let data = parse_diagnostic_data(&diagnostics[0].diagnostic).unwrap();
        assert_eq!(data.matched_text, "This are");
    }
}

use crate::config::{BackendKind, ClientOptions, ProjectConfig};
use crate::diagnostics::{
    diagnostic_data, make_lsp_diagnostic, match_offsets, parse_diagnostic_data, SOURCE,
};
use crate::document_cache::{ChangeStatus, DocumentCache, DocumentEntry, DocumentToken};
use crate::languagetool::{
    AnnotatedText, LanguageToolClient, LanguageToolError, LanguageToolMatch,
};
use crate::runtime_config::RuntimeConfig;
use crate::text_index::TextIndex;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tower_lsp::jsonrpc::{Error as RpcError, Result as RpcResult};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

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

struct CheckRequest {
    uri: Url,
    version: i32,
    token: DocumentToken,
    text: String,
    index: TextIndex,
    annotated: AnnotatedText,
    ignored_ranges: Vec<(usize, usize)>,
    options: ClientOptions,
}

enum PreparedCheck {
    Check(Box<CheckRequest>),
    Clear { uri: Url, version: i32 },
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

    async fn schedule_check(&self, uri: Url) {
        let Some(token) = self.documents.token(&uri) else {
            log::debug!("Skipping check schedule for {uri}: document not cached");
            return;
        };
        let debounce = self.options().await.debounce_ms;
        log::debug!("Scheduling check for {uri} token={token:?} debounce_ms={debounce}");
        let backend = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce)).await;
            let options = backend.options().await;
            let prepared = {
                backend
                    .documents
                    .with_bumped_entry_if_current(&uri, token, |entry| {
                        backend.prepare_check_entry(entry, options)
                    })
            };
            if let Some(prepared) = prepared {
                log::debug!("Running debounced check for {uri} token={token:?}");
                backend.run_prepared_check(prepared).await;
            } else {
                log::debug!("Skipping stale debounced check for {uri} token={token:?}");
            }
        });
    }

    async fn check_uri_now(&self, uri: &Url) {
        let options = self.options().await;
        let Some(prepared) = self
            .documents
            .with_bumped_entry(uri, |entry| self.prepare_check_entry(entry, options))
        else {
            log::debug!("Skipping immediate check for {uri}: document not cached");
            return;
        };
        log::debug!("Running immediate check for {uri}");
        self.run_prepared_check(prepared).await;
    }

    async fn clear_stale_diagnostics(&self, uri: &Url, version: Option<i32>) {
        log::debug!("Clearing stale diagnostics for {uri} version={version:?}");
        self.client
            .publish_diagnostics(uri.clone(), Vec::new(), version)
            .await;
    }

    async fn run_prepared_check(&self, prepared: PreparedCheck) {
        let request = match prepared {
            PreparedCheck::Check(request) => request,
            PreparedCheck::Clear { uri, version } => {
                log::debug!("Document {uri} is not checkable; clearing diagnostics");
                self.clear_stale_diagnostics(&uri, Some(version)).await;
                return;
            }
        };
        let uri = &request.uri;
        let token = request.token;

        log::debug!(
            "Starting check for {uri} token={token:?} version={:?}",
            request.version
        );
        log::debug!(
            "Sending LanguageTool request for {uri} token={token:?} annotations={} ignored_ranges={}",
            request.annotated.annotation.len(),
            request.ignored_ranges.len()
        );
        let response = match self
            .language_tool
            .check_annotated(&request.annotated, &request.options)
            .await
        {
            Ok(response) => {
                log::debug!(
                    "LanguageTool returned {} match(es) for {uri} token={token:?}",
                    response.matches.len()
                );
                response
            }
            Err(err) => {
                self.log_check_error(&request.options, err).await;
                if self.documents.is_current(&request.uri, request.token) {
                    self.clear_stale_diagnostics(&request.uri, Some(request.version))
                        .await;
                } else {
                    log::debug!(
                        "Skipping stale diagnostic clear for {} token={:?}",
                        request.uri,
                        request.token
                    );
                }
                return;
            }
        };

        if !self.documents.is_current(&request.uri, request.token) {
            log::debug!(
                "Discarding stale check result for {} token={:?}",
                request.uri,
                request.token
            );
            return;
        }
        let diagnostics = diagnostics_for_request(&request, response.matches);
        let diagnostic_count = diagnostics.len();
        log::debug!(
            "Publishing {diagnostic_count} diagnostic(s) for {uri} token={token:?} version={:?}",
            request.version
        );
        self.client
            .publish_diagnostics(request.uri.clone(), diagnostics, Some(request.version))
            .await;
    }

    fn prepare_check_entry(&self, entry: &DocumentEntry, options: ClientOptions) -> PreparedCheck {
        let document = entry.document();
        let token = entry.token();
        let Some(checkable_document) =
            document.checkable(|language| options.language_enabled(&language))
        else {
            return PreparedCheck::Clear {
                uri: document.uri().clone(),
                version: document.version(),
            };
        };

        PreparedCheck::Check(Box::new(CheckRequest {
            uri: checkable_document.uri().clone(),
            version: checkable_document.version(),
            token,
            text: checkable_document.text().to_string(),
            index: checkable_document.index().clone(),
            annotated: checkable_document.annotated().clone(),
            ignored_ranges: checkable_document.ignored_ranges().to_vec(),
            options,
        }))
    }

    async fn log_check_error(&self, options: &ClientOptions, err: LanguageToolError) {
        let message = match &err {
            LanguageToolError::Api { .. } | LanguageToolError::Request { .. }
                if matches!(options.backend, BackendKind::Custom) =>
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

fn diagnostics_for_request(
    request: &CheckRequest,
    matches: Vec<LanguageToolMatch>,
) -> Vec<Diagnostic> {
    let diagnostics = matches
        .iter()
        .filter_map(|item| match_offsets(item).map(|(offset, length)| (item, offset, length)))
        .filter(|(_, offset, length)| {
            !request
                .index
                .text_for_utf16_range(&request.text, *offset, *offset + *length)
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        })
        .filter(|(_, offset, length)| {
            !intersects_ignored_ranges(*offset, *offset + *length, &request.ignored_ranges)
        })
        .filter_map(|(item, _, _)| {
            let data = diagnostic_data(
                &request.text,
                &request.index,
                item,
                &request.options,
                Some(request.version),
            );
            (!request.options.is_ignored_word(&data.matched_text))
                .then(|| make_lsp_diagnostic(&request.index, item, data, &request.options))
        })
        .collect::<Vec<_>>();
    log::debug!(
        "Mapped LanguageTool matches to {} diagnostic(s) for {}",
        diagnostics.len(),
        request.uri
    );
    diagnostics
}

fn intersects_ignored_ranges(start: usize, end: usize, ignored_ranges: &[(usize, usize)]) -> bool {
    ignored_ranges
        .iter()
        .any(|(ignored_start, ignored_end)| start < *ignored_end && end > *ignored_start)
}

fn make_replacement_action(
    uri: &Url,
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

#[tower_lsp::async_trait]
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
            params.text_document.text.len()
        );
        self.documents.insert(&params.text_document);
        if self.options().await.check_on_open {
            self.check_uri_now(&uri).await;
        } else {
            log::debug!("Skipping open check for {uri}: check_on_open=false");
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let had_changes = !params.content_changes.is_empty();
        log::debug!(
            "Received {} change(s) for {uri} version={}",
            params.content_changes.len(),
            params.text_document.version
        );
        let change_status = self.documents.apply_changes(
            &uri,
            params.text_document.version,
            params.content_changes,
        );
        if change_status == ChangeStatus::OutOfSync {
            self.run_prepared_check(PreparedCheck::Clear {
                uri: uri.clone(),
                version: params.text_document.version,
            })
            .await;
        }
        if had_changes && self.options().await.check_while_typing {
            self.schedule_check(uri).await;
        } else if had_changes {
            log::debug!("Skipping typing check for {uri}: check_while_typing=false");
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        log::info!("Saved document {uri}");
        if self.options().await.check_on_save {
            self.check_uri_now(&uri).await;
        } else {
            log::debug!("Skipping save check for {uri}: check_on_save=false");
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        log::info!("Closed document {}", params.text_document.uri);
        self.documents.remove(&params.text_document.uri);
        self.clear_stale_diagnostics(&params.text_document.uri, None)
            .await;
    }

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        let mut actions = Vec::new();
        let uri = params.text_document.uri;
        let project_config_display_path = self.project_config_display_path().await;
        let diagnostic_count = params.context.diagnostics.len();
        log::debug!("Building code actions for {uri} diagnostics={diagnostic_count}");

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
            log::debug!("No code actions available for {uri}");
            Ok(None)
        } else {
            log::debug!("Returning {} code action(s) for {uri}", actions.len());
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

fn workspace_root(params: &InitializeParams) -> Option<PathBuf> {
    params
        .workspace_folders
        .as_ref()
        .and_then(|folders| folders.first())
        .and_then(|folder| folder.uri.to_file_path().ok())
        .or_else(|| {
            params
                .root_uri
                .as_ref()
                .and_then(|uri| uri.to_file_path().ok())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
    use crate::languagetool::{
        LanguageToolCategory, LanguageToolMatch, LanguageToolReplacement, LanguageToolRule,
    };

    fn check_request_for_test(document: &Document, options: ClientOptions) -> CheckRequest {
        let document = document
            .checkable(|language| options.language_enabled(&language))
            .unwrap();
        CheckRequest {
            uri: document.uri().clone(),
            version: document.version(),
            token: DocumentToken::new_for_test(0, document.version(), 0),
            text: document.text().to_string(),
            index: document.index().clone(),
            annotated: document.annotated().clone(),
            ignored_ranges: document.ignored_ranges().to_vec(),
            options,
        }
    }

    #[test]
    fn builds_diagnostics_for_document() {
        let document = Document::new(
            Url::parse("file:///tmp/test.txt").unwrap(),
            1,
            Some("plaintext".to_string()),
            "This are a tset.".to_string(),
        );
        let options = ClientOptions::default();
        let request = check_request_for_test(&document, options);
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

        let diagnostics = diagnostics_for_request(&request, vec![item]);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start, Position::new(0, 11));
        assert_eq!(diagnostics[0].range.end, Position::new(0, 15));
    }

    #[test]
    fn diagnostics_use_original_document_offsets() {
        let document = Document::new(
            Url::parse("file:///tmp/test.rs").unwrap(),
            1,
            Some("rust".to_string()),
            "let value = 1; // This are a comment.".to_string(),
        );
        let options = ClientOptions::default();
        let request = check_request_for_test(&document, options);
        let item = LanguageToolMatch {
            message: "The singular demonstrative pronoun does not agree.".to_string(),
            short_message: None,
            offset: 18,
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

        let diagnostics = diagnostics_for_request(&request, vec![item]);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start, Position::new(0, 18));
        assert_eq!(diagnostics[0].range.end, Position::new(0, 22));
    }

    #[test]
    fn diagnostics_use_languagetool_utf16_offsets() {
        let document = Document::new(
            Url::parse("file:///tmp/test.txt").unwrap(),
            1,
            Some("plaintext".to_string()),
            "😀 This are a tset.".to_string(),
        );
        let options = ClientOptions::default();
        let request = check_request_for_test(&document, options);
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

        let diagnostics = diagnostics_for_request(&request, vec![item]);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start, Position::new(0, 3));
        assert_eq!(diagnostics[0].range.end, Position::new(0, 11));

        let data = parse_diagnostic_data(&diagnostics[0]).unwrap();
        assert_eq!(data.matched_text, "This are");
    }
}

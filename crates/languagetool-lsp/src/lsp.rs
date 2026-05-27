use crate::config::{BackendConfig, ClientOptions, ProjectConfig};
use crate::diagnostics::{
    diagnostic_data, make_lsp_diagnostic, match_offsets, parse_diagnostic_data, SOURCE,
};
use crate::document::{ChangeStatus, CheckableDocument, Document};
use crate::document_cache::DocumentCache;
use crate::languagetool::{LanguageToolClient, LanguageToolError, LanguageToolMatch};
use serde_json::Value;
use std::path::{Path, PathBuf};
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
    initialization_options: Arc<RwLock<ClientOptions>>,
    project_config: Arc<RwLock<ProjectConfig>>,
    language_tool: LanguageToolClient,
}

impl Backend {
    pub fn new(client: Client, root: PathBuf) -> Self {
        Self {
            client,
            root: Arc::new(RwLock::new(root)),
            documents: DocumentCache::default(),
            initialization_options: Arc::new(RwLock::new(ClientOptions::default())),
            project_config: Arc::new(RwLock::new(ProjectConfig::default())),
            language_tool: LanguageToolClient::new(),
        }
    }

    fn options(&self) -> ClientOptions {
        let initialization_options = self
            .initialization_options
            .read()
            .expect("initialization options poisoned")
            .clone();
        self.project_config
            .read()
            .expect("project config poisoned")
            .merged_options(&initialization_options)
    }

    fn schedule_check(&self, uri: Url) {
        let generation = self.documents.bump_generation(&uri);
        let debounce = self.options().debounce_ms;
        log::debug!("Scheduling check for {uri} generation={generation} debounce_ms={debounce}");
        let backend = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce)).await;
            if backend.documents.generation(&uri) == generation {
                log::debug!("Running debounced check for {uri} generation={generation}");
                backend.check_uri(&uri, generation).await;
            } else {
                log::debug!("Skipping stale debounced check for {uri} generation={generation}");
            }
        });
    }

    async fn check_uri_now(&self, uri: &Url) {
        let generation = self.documents.bump_generation(uri);
        log::debug!("Running immediate check for {uri} generation={generation}");
        self.check_uri(uri, generation).await;
    }

    async fn clear_stale_diagnostics(&self, uri: &Url, version: Option<i32>) {
        log::debug!("Clearing stale diagnostics for {uri} version={version:?}");
        self.client
            .publish_diagnostics(uri.clone(), Vec::new(), version)
            .await;
    }

    async fn check_uri(&self, uri: &Url, generation: u64) {
        let Some(document) = self.documents.get(uri) else {
            log::debug!("Skipping check for {uri} generation={generation}: document not cached");
            return;
        };

        log::debug!(
            "Starting check for {uri} generation={generation} version={:?}",
            document.version()
        );
        let options = self.options();
        let Some(checkable_document) =
            document.checkable(|language| options.language_enabled(&language))
        else {
            log::debug!("Document {uri} is not checkable; clearing diagnostics");
            self.clear_stale_diagnostics(document.uri(), Some(document.version()))
                .await;
            return;
        };

        log::debug!(
            "Sending LanguageTool request for {uri} generation={generation} annotations={} ignored_ranges={}",
            checkable_document.annotated().annotation.len(),
            checkable_document.ignored_ranges().len()
        );
        let response = match self
            .language_tool
            .check_annotated(checkable_document.annotated(), &options)
            .await
        {
            Ok(response) => {
                log::debug!(
                    "LanguageTool returned {} match(es) for {uri} generation={generation}",
                    response.matches.len()
                );
                response
            }
            Err(err) => {
                self.log_check_error(&options, err).await;
                if self.document_is_current(&document, generation) {
                    self.clear_stale_diagnostics(
                        checkable_document.uri(),
                        Some(checkable_document.version()),
                    )
                    .await;
                }
                return;
            }
        };

        if !self.document_is_current(&document, generation) {
            log::debug!("Discarding stale check result for {uri} generation={generation}");
            return;
        }
        let diagnostics = diagnostics_for_document(&checkable_document, response.matches, &options);
        let diagnostic_count = diagnostics.len();
        log::debug!(
            "Publishing {diagnostic_count} diagnostic(s) for {uri} generation={generation} version={:?}",
            checkable_document.version()
        );
        self.client
            .publish_diagnostics(
                checkable_document.uri().clone(),
                diagnostics,
                Some(checkable_document.version()),
            )
            .await;
    }

    async fn log_check_error(&self, options: &ClientOptions, err: LanguageToolError) {
        let message = match &err {
            LanguageToolError::Api { .. } | LanguageToolError::Request { .. }
                if matches!(options.backend, BackendConfig::Local { .. }) =>
            {
                format!(
                    "LanguageTool is not reachable at {}. Is the local server running? {err}",
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

    fn project_config_path(&self) -> PathBuf {
        let root = self.root.read().expect("workspace root poisoned");
        self.options().project_config_path(&root)
    }

    fn project_config_display_path(&self) -> String {
        self.options().project_config_display_path()
    }

    fn save_project_config(
        &self,
        project_config: &ProjectConfig,
        path: &Path,
    ) -> Result<(), String> {
        project_config
            .save(path)
            .map_err(|err| format!("Failed to save project config: {err}"))
    }

    fn update_project_config(
        &self,
        update: impl FnOnce(&mut ProjectConfig) -> bool,
    ) -> Result<bool, String> {
        let project_config_path = self.project_config_path();
        let (updated, next_config) = {
            let project_config = self.project_config.read().expect("project config poisoned");
            let mut next_config = project_config.clone();
            let updated = update(&mut next_config);
            (updated, next_config)
        };

        if !updated {
            log::debug!("Project config update made no changes");
            return Ok(false);
        }

        log::info!(
            "Saving LanguageTool project config to {}",
            project_config_path.display()
        );
        self.save_project_config(&next_config, &project_config_path)?;
        *self
            .project_config
            .write()
            .expect("project config poisoned") = next_config;
        Ok(true)
    }

    async fn add_ignored_word(&self, word: &str) -> Result<bool, String> {
        self.update_project_config(|project_config| project_config.add_ignored_word(word))
    }

    async fn add_disabled_rule(&self, rule_id: &str) -> Result<bool, String> {
        self.update_project_config(|project_config| project_config.add_disabled_rule(rule_id))
    }

    async fn add_disabled_category(&self, category_id: &str) -> Result<bool, String> {
        self.update_project_config(|project_config| {
            project_config.add_disabled_category(category_id)
        })
    }

    fn document_is_current(&self, document: &Document, generation: u64) -> bool {
        let current = self.documents.generation(document.uri()) == generation
            && self
                .documents
                .get(document.uri())
                .is_some_and(|current| current.version() == document.version());
        if !current {
            log::debug!(
                "Document {} is stale for generation={generation} version={:?}",
                document.uri(),
                document.version()
            );
        }
        current
    }
}

fn diagnostics_for_document(
    document: &CheckableDocument<'_>,
    matches: Vec<LanguageToolMatch>,
    options: &ClientOptions,
) -> Vec<Diagnostic> {
    let index = document.index();
    let text = document.text();
    let diagnostics = matches
        .iter()
        .filter_map(|item| match_offsets(item).map(|(offset, length)| (item, offset, length)))
        .filter(|(_, offset, length)| {
            !index
                .text_for_utf16_range(text, *offset, *offset + *length)
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        })
        .filter(|(_, offset, length)| {
            !intersects_ignored_ranges(*offset, *offset + *length, document.ignored_ranges())
        })
        .filter_map(|(item, _, _)| {
            let data = diagnostic_data(text, index, item, options, Some(document.version()));
            (!options.is_ignored_word(&data.matched_text))
                .then(|| make_lsp_diagnostic(index, item, data, options))
        })
        .collect::<Vec<_>>();
    log::debug!(
        "Mapped LanguageTool matches to {} diagnostic(s) for {}",
        diagnostics.len(),
        document.uri()
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
        let options = ClientOptions::from_value(params.initialization_options);
        let root = self.root.read().expect("workspace root poisoned").clone();
        let project_config = ProjectConfig::load(&options.project_config_path(&root));
        log::info!(
            "LanguageTool LSP initialized for {} using {}",
            root.display(),
            options.endpoint()
        );
        *self
            .initialization_options
            .write()
            .expect("initialization options poisoned") = options;
        *self
            .project_config
            .write()
            .expect("project config poisoned") = project_config;

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
        let options = self.options();
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
        if self.options().check_on_open {
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
        for change in params.content_changes {
            if self
                .documents
                .apply_change(&uri, params.text_document.version, change)
                == ChangeStatus::OutOfSync
            {
                self.clear_stale_diagnostics(&uri, Some(params.text_document.version))
                    .await;
            }
        }
        if had_changes && self.options().check_while_typing {
            self.schedule_check(uri);
        } else if had_changes {
            log::debug!("Skipping typing check for {uri}: check_while_typing=false");
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        log::info!("Saved document {uri}");
        if self.options().check_on_save {
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
        let project_config_display_path = self.project_config_display_path();
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
            let options = ClientOptions::from_value(Some(params.settings));
            let root = self.root.read().expect("workspace root poisoned").clone();
            let project_config = ProjectConfig::load(&options.project_config_path(&root));
            *self
                .initialization_options
                .write()
                .expect("initialization options poisoned") = options;
            *self
                .project_config
                .write()
                .expect("project config poisoned") = project_config;
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
    use crate::languagetool::{
        LanguageToolCategory, LanguageToolMatch, LanguageToolReplacement, LanguageToolRule,
    };

    #[test]
    fn builds_diagnostics_for_document() {
        let document = Document::new(
            Url::parse("file:///tmp/test.txt").unwrap(),
            1,
            Some("plaintext".to_string()),
            "This are a tset.".to_string(),
        );
        let document = document.checkable(|_| true).unwrap();
        let options = ClientOptions::default();
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

        let diagnostics = diagnostics_for_document(&document, vec![item], &options);
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
        let document = document.checkable(|_| true).unwrap();
        let options = ClientOptions::default();
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

        let diagnostics = diagnostics_for_document(&document, vec![item], &options);
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
        let document = document.checkable(|_| true).unwrap();
        let options = ClientOptions::default();
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

        let diagnostics = diagnostics_for_document(&document, vec![item], &options);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start, Position::new(0, 3));
        assert_eq!(diagnostics[0].range.end, Position::new(0, 11));

        let data = parse_diagnostic_data(&diagnostics[0]).unwrap();
        assert_eq!(data.matched_text, "This are");
    }
}

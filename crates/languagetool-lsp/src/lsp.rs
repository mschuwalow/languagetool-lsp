use crate::config::{BackendConfig, ClientOptions, ProjectConfig};
use crate::diagnostics::{
    diagnostic_data, make_lsp_diagnostic, match_offsets, parse_diagnostic_data, SOURCE,
};
use crate::document_cache::{Document, DocumentCache};
use crate::languagetool::{
    AnnotatedText, LanguageToolClient, LanguageToolError, LanguageToolMatch,
};
use crate::masking::{annotated_for_language, ignored_ranges_for_language};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

const COMMAND_IGNORE_WORD: &str = "languagetool.ignoreWordInWorkspace";
const COMMAND_DISABLE_RULE: &str = "languagetool.disableRuleInWorkspace";
const COMMAND_DISABLE_CATEGORY: &str = "languagetool.disableCategoryInWorkspace";

#[derive(Clone)]
pub struct Backend {
    client: Client,
    root: PathBuf,
    documents: DocumentCache,
    initialization_options: Arc<RwLock<ClientOptions>>,
    project_config: Arc<RwLock<ProjectConfig>>,
    language_tool: LanguageToolClient,
}

impl Backend {
    pub fn new(client: Client, root: PathBuf) -> Self {
        Self {
            client,
            root,
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
        let backend = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce)).await;
            if backend.documents.generation(&uri) == generation {
                backend.check_uri(&uri, generation).await;
            }
        });
    }

    async fn check_uri_now(&self, uri: &Url) {
        let generation = self.documents.bump_generation(uri);
        self.check_uri(uri, generation).await;
    }

    async fn check_uri(&self, uri: &Url, generation: u64) {
        let Some(document) = self.documents.get(uri) else {
            return;
        };

        let options = self.options();
        if !options.language_enabled(document.language_id.as_deref()) {
            self.client
                .publish_diagnostics(document.uri, Vec::new(), document.version)
                .await;
            return;
        }

        let data = annotated_for_document(&document);
        let ignored_ranges = ignored_ranges_for_document(&document);
        if !data.has_text() {
            self.client
                .publish_diagnostics(document.uri, Vec::new(), document.version)
                .await;
            return;
        }

        let response = match self.language_tool.check_annotated(&data, &options).await {
            Ok(response) => response,
            Err(err) => {
                self.log_check_error(&options, err).await;
                return;
            }
        };

        if self.documents.generation(&document.uri) != generation {
            return;
        }
        if self
            .documents
            .get(&document.uri)
            .is_none_or(|current| current.version != document.version)
        {
            return;
        }
        let diagnostics =
            diagnostics_for_document(&document, response.matches, &options, &ignored_ranges);
        self.client
            .publish_diagnostics(document.uri, diagnostics, document.version)
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
        for uri in self.documents.urls() {
            self.check_uri_now(&uri).await;
        }
    }

    fn project_config_path(&self) -> PathBuf {
        self.options().project_config_path(&self.root)
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

    async fn update_project_config(&self, update: impl FnOnce(&mut ProjectConfig) -> bool) {
        let project_config_path = self.project_config_path();
        let updated = {
            let mut project_config = self
                .project_config
                .write()
                .expect("project config poisoned");
            let updated = update(&mut project_config);
            if updated {
                if let Err(err) = self.save_project_config(&project_config, &project_config_path) {
                    log::error!("{err}");
                }
            }
            updated
        };

        if updated {
            self.recheck_all().await;
        }
    }

    async fn add_ignored_word(&self, word: &str) {
        self.update_project_config(|project_config| project_config.add_ignored_word(word))
            .await;
    }

    async fn add_disabled_rule(&self, rule_id: &str) {
        self.update_project_config(|project_config| project_config.add_disabled_rule(rule_id))
            .await;
    }

    async fn add_disabled_category(&self, category_id: &str) {
        self.update_project_config(|project_config| {
            project_config.add_disabled_category(category_id)
        })
        .await;
    }
}

fn diagnostics_for_document(
    document: &Document,
    matches: Vec<LanguageToolMatch>,
    options: &ClientOptions,
    ignored_ranges: &[(usize, usize)],
) -> Vec<Diagnostic> {
    let index = &document.index;
    matches
        .iter()
        .filter_map(|item| match_offsets(item).map(|(offset, length)| (item, offset, length)))
        .filter(|(_, offset, length)| {
            !index
                .text_for_utf16_range(&document.text, *offset, *offset + *length)
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        })
        .filter(|(_, offset, length)| {
            !intersects_ignored_ranges(*offset, *offset + *length, ignored_ranges)
        })
        .filter_map(|(item, _, _)| {
            let data = diagnostic_data(&document.text, index, item, options);
            (!options.is_ignored_word(&data.matched_text))
                .then(|| make_lsp_diagnostic(index, item, data, options))
        })
        .collect()
}

fn intersects_ignored_ranges(start: usize, end: usize, ignored_ranges: &[(usize, usize)]) -> bool {
    ignored_ranges
        .iter()
        .any(|(ignored_start, ignored_end)| start < *ignored_end && end > *ignored_start)
}

fn annotated_for_document(document: &Document) -> AnnotatedText {
    let data = annotated_for_language(&document.text, document.language_id.as_deref());
    if data.has_text() {
        return data;
    }

    let extension = document.uri.to_file_path().ok().and_then(|path| {
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_string)
    });
    annotated_for_language(&document.text, extension.as_deref())
}

fn ignored_ranges_for_document(document: &Document) -> Vec<(usize, usize)> {
    let ranges = ignored_ranges_for_language(
        &document.text,
        &document.index,
        document.language_id.as_deref(),
    );
    if !ranges.is_empty() {
        return ranges;
    }

    let extension = document.uri.to_file_path().ok().and_then(|path| {
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_string)
    });
    ignored_ranges_for_language(&document.text, &document.index, extension.as_deref())
}

fn make_replacement_action(uri: &Url, diagnostic: &Diagnostic, replacement: &str) -> CodeAction {
    let mut changes = HashMap::new();
    changes.insert(
        uri.clone(),
        vec![TextEdit {
            range: diagnostic.range,
            new_text: replacement.to_string(),
        }],
    );

    CodeAction {
        title: format!("Replace with '{replacement}'"),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diagnostic.clone()]),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
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
        let options = ClientOptions::from_value(params.initialization_options);
        let project_config = ProjectConfig::load(&options.project_config_path(&self.root));
        log::info!(
            "LanguageTool LSP initialized for {} using {}",
            self.root.display(),
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
                            include_text: Some(true),
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
        self.client
            .log_message(
                MessageType::INFO,
                format!("LanguageTool LSP ready: {}", options.endpoint()),
            )
            .await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        self.documents.insert(&params.text_document);
        if self.options().check_on_open {
            self.check_uri_now(&uri).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let had_changes = !params.content_changes.is_empty();
        for change in params.content_changes {
            self.documents
                .apply_change(&uri, Some(params.text_document.version), change);
        }
        if had_changes && self.options().check_while_typing {
            self.schedule_check(uri);
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Some(text) = params.text {
            self.documents.update(&uri, None, text);
        }
        if self.options().check_on_save {
            self.check_uri_now(&uri).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri);
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        let mut actions = Vec::new();
        let uri = params.text_document.uri;
        let project_config_display_path = self.project_config_display_path();

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
            Ok(None)
        } else {
            Ok(Some(actions))
        }
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        if params.settings != Value::Null {
            let options = ClientOptions::from_value(Some(params.settings));
            let project_config = ProjectConfig::load(&options.project_config_path(&self.root));
            *self
                .initialization_options
                .write()
                .expect("initialization options poisoned") = options;
            *self
                .project_config
                .write()
                .expect("project config poisoned") = project_config;
            self.recheck_all().await;
        }
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> RpcResult<Option<Value>> {
        let first_arg = params.arguments.first().and_then(Value::as_str);
        match (params.command.as_str(), first_arg) {
            (COMMAND_IGNORE_WORD, Some(word)) => self.add_ignored_word(word).await,
            (COMMAND_DISABLE_RULE, Some(rule_id)) => self.add_disabled_rule(rule_id).await,
            (COMMAND_DISABLE_CATEGORY, Some(category_id)) => {
                self.add_disabled_category(category_id).await
            }
            _ => log::warn!("Unknown or invalid command: {}", params.command),
        }
        Ok(None)
    }
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
            Some(1),
            Some("plaintext".to_string()),
            "This are a tset.".to_string(),
        );
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

        let diagnostics = diagnostics_for_document(&document, vec![item], &options, &[]);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start, Position::new(0, 11));
        assert_eq!(diagnostics[0].range.end, Position::new(0, 15));
    }

    #[test]
    fn diagnostics_use_original_document_offsets() {
        let document = Document::new(
            Url::parse("file:///tmp/test.rs").unwrap(),
            Some(1),
            Some("rust".to_string()),
            "let value = 1; // This are a comment.".to_string(),
        );
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

        let diagnostics = diagnostics_for_document(&document, vec![item], &options, &[]);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start, Position::new(0, 18));
        assert_eq!(diagnostics[0].range.end, Position::new(0, 22));
    }

    #[test]
    fn diagnostics_use_languagetool_utf16_offsets() {
        let document = Document::new(
            Url::parse("file:///tmp/test.txt").unwrap(),
            Some(1),
            Some("plaintext".to_string()),
            "😀 This are a tset.".to_string(),
        );
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

        let diagnostics = diagnostics_for_document(&document, vec![item], &options, &[]);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start, Position::new(0, 3));
        assert_eq!(diagnostics[0].range.end, Position::new(0, 11));

        let data = parse_diagnostic_data(&diagnostics[0]).unwrap();
        assert_eq!(data.matched_text, "This are");
    }
}

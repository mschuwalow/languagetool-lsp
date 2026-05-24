use crate::config::{BackendConfig, ClientOptions, ProjectConfig};
use crate::diagnostics::{
    diagnostic_data, make_lsp_diagnostic, match_offsets, parse_diagnostic_data, SOURCE,
};
use crate::document_cache::{Document, DocumentCache};
use crate::languagetool::{
    AnnotatedText, LanguageToolClient, LanguageToolError, LanguageToolMatch,
};
use crate::line_index::LineIndex;
use crate::masking::{annotated_for_language, ignored_ranges_for_language};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
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
    generations: Arc<RwLock<HashMap<String, u64>>>,
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
            generations: Arc::new(RwLock::new(HashMap::new())),
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

    fn bump_generation(&self, uri: &Url) -> u64 {
        let mut generations = self.generations.write().expect("generations poisoned");
        let generation = generations.get(uri.as_str()).copied().unwrap_or(0) + 1;
        generations.insert(uri.to_string(), generation);
        generation
    }

    fn generation(&self, uri: &Url) -> u64 {
        self.generations
            .read()
            .expect("generations poisoned")
            .get(uri.as_str())
            .copied()
            .unwrap_or(0)
    }

    fn remove_generation(&self, uri: &Url) {
        self.generations
            .write()
            .expect("generations poisoned")
            .remove(uri.as_str());
    }

    fn schedule_check(&self, uri: Url, generation: u64) {
        let debounce = self.options().debounce_ms;
        let backend = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce)).await;
            if backend.generation(&uri) == generation {
                backend.check_uri(&uri, generation).await;
            }
        });
    }

    async fn check_uri_now(&self, uri: &Url) {
        let generation = self.bump_generation(uri);
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

        if self.generation(&document.uri) != generation {
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

    fn save_project_config(&self, project_config: &ProjectConfig) -> Result<(), String> {
        project_config
            .save(&self.root)
            .map_err(|err| format!("Failed to save project config: {err}"))
    }

    async fn add_ignored_word(&self, word: &str) {
        let updated = {
            let mut project_config = self
                .project_config
                .write()
                .expect("project config poisoned");
            let updated = project_config.add_ignored_word(word);
            if updated {
                if let Err(err) = self.save_project_config(&project_config) {
                    log::error!("{err}");
                }
            }
            updated
        };

        if updated {
            self.recheck_all().await;
        }
    }

    async fn add_disabled_rule(&self, rule_id: &str) {
        let updated = {
            let mut project_config = self
                .project_config
                .write()
                .expect("project config poisoned");
            let updated = project_config.add_disabled_rule(rule_id);
            if updated {
                if let Err(err) = self.save_project_config(&project_config) {
                    log::error!("{err}");
                }
            }
            updated
        };

        if updated {
            self.recheck_all().await;
        }
    }

    async fn add_disabled_category(&self, category_id: &str) {
        let updated = {
            let mut project_config = self
                .project_config
                .write()
                .expect("project config poisoned");
            let updated = project_config.add_disabled_category(category_id);
            if updated {
                if let Err(err) = self.save_project_config(&project_config) {
                    log::error!("{err}");
                }
            }
            updated
        };

        if updated {
            self.recheck_all().await;
        }
    }
}

fn diagnostics_for_document(
    document: &Document,
    matches: Vec<LanguageToolMatch>,
    options: &ClientOptions,
    ignored_ranges: &[(usize, usize)],
) -> Vec<Diagnostic> {
    let line_index = LineIndex::new(&document.text);
    matches
        .iter()
        .filter_map(|item| match_offsets(item).map(|(offset, length)| (item, offset, length)))
        .filter(|(_, offset, length)| {
            !utf16_text_for_range(&document.text, *offset, *offset + *length)
                .trim()
                .is_empty()
        })
        .filter(|(_, offset, length)| {
            !intersects_ignored_ranges(*offset, *offset + *length, ignored_ranges)
        })
        .filter_map(|(item, _, _)| {
            let data = diagnostic_data(&document.text, &line_index, item, options);
            (!options.is_ignored_word(&data.matched_text))
                .then(|| make_lsp_diagnostic(&line_index, item, data, options))
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
    let ranges = ignored_ranges_for_language(&document.text, document.language_id.as_deref());
    if !ranges.is_empty() {
        return ranges;
    }

    let extension = document.uri.to_file_path().ok().and_then(|path| {
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_string)
    });
    ignored_ranges_for_language(&document.text, extension.as_deref())
}

fn utf16_text_for_range(text: &str, start: usize, end: usize) -> String {
    let mut offset = 0;
    let mut output = String::new();

    for ch in text.chars() {
        let next = offset + ch.len_utf16();
        if offset >= start && next <= end {
            output.push(ch);
        }
        offset = next;
        if offset >= end {
            break;
        }
    }

    output
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
        let project_config = ProjectConfig::load(&self.root);
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
        if had_changes {
            let generation = self.bump_generation(&uri);
            if self.options().check_while_typing {
                self.schedule_check(uri, generation);
            }
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
        self.remove_generation(&params.text_document.uri);
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        let mut actions = Vec::new();
        let uri = params.text_document.uri;

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
                        data.matched_text,
                        ProjectConfig::display_path()
                    ),
                    COMMAND_IGNORE_WORD,
                    data.matched_text.clone(),
                )));
            }

            actions.push(CodeActionOrCommand::Command(make_command(
                format!(
                    "Disable rule '{}' in {}",
                    data.rule_id,
                    ProjectConfig::display_path()
                ),
                COMMAND_DISABLE_RULE,
                data.rule_id.clone(),
            )));

            if let Some(category_id) = data.category_id {
                actions.push(CodeActionOrCommand::Command(make_command(
                    format!(
                        "Disable category '{category_id}' in {}",
                        ProjectConfig::display_path()
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
            *self
                .initialization_options
                .write()
                .expect("initialization options poisoned") = options;
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
    use crate::languagetool::{LanguageToolCategory, LanguageToolMatch, LanguageToolRule};

    #[test]
    fn builds_diagnostics_for_document() {
        let document = Document {
            uri: Url::parse("file:///tmp/test.txt").unwrap(),
            version: Some(1),
            language_id: Some("plaintext".to_string()),
            text: "This are a tset.".to_string(),
        };
        let options = ClientOptions::default();
        let item = LanguageToolMatch {
            message: "Possible spelling mistake found.".to_string(),
            offset: 11,
            length: 4,
            replacements: vec!["test".to_string()],
            rule: Some(LanguageToolRule {
                id: "MORFOLOGIK_RULE_EN_US".to_string(),
                issue_type: Some("misspelling".to_string()),
                category: Some(LanguageToolCategory {
                    id: Some("TYPOS".to_string()),
                }),
            }),
        };

        let diagnostics = diagnostics_for_document(&document, vec![item], &options, &[]);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start, Position::new(0, 11));
        assert_eq!(diagnostics[0].range.end, Position::new(0, 15));
    }

    #[test]
    fn diagnostics_use_original_document_offsets() {
        let document = Document {
            uri: Url::parse("file:///tmp/test.rs").unwrap(),
            version: Some(1),
            language_id: Some("rust".to_string()),
            text: "let value = 1; // This are a comment.".to_string(),
        };
        let options = ClientOptions::default();
        let item = LanguageToolMatch {
            message: "The singular demonstrative pronoun does not agree.".to_string(),
            offset: 18,
            length: 4,
            replacements: Vec::new(),
            rule: Some(LanguageToolRule {
                id: "THIS_NNS".to_string(),
                issue_type: None,
                category: None,
            }),
        };

        let diagnostics = diagnostics_for_document(&document, vec![item], &options, &[]);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start, Position::new(0, 18));
        assert_eq!(diagnostics[0].range.end, Position::new(0, 22));
    }
}

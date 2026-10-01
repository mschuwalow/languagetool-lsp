use crate::config::ClientOptions;
use crate::config_actor::{self, ConfigActorHandle};
use crate::diagnostics::{SOURCE, parse_diagnostic_data};
use crate::document_actor::{self, DocumentActorHandle};
use crate::runtime_config::RuntimeConfig;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, watch};
use tower_lsp_server::Client;
use tower_lsp_server::jsonrpc::{Error as RpcError, Result as RpcResult};
use tower_lsp_server::ls_types::*;

const COMMAND_IGNORE_WORD: &str = "languagetool.ignoreWordInWorkspace";
const COMMAND_DISABLE_RULE: &str = "languagetool.disableRuleInWorkspace";
const COMMAND_DISABLE_CATEGORY: &str = "languagetool.disableCategoryInWorkspace";

pub struct LanguageServerBackend {
    client: Client,
    config: watch::Receiver<Arc<RuntimeConfig>>,
    config_actor: ConfigActorHandle,
    documents: Mutex<HashMap<String, DocumentActorHandle>>,
}

impl LanguageServerBackend {
    pub async fn new(client: Client, root: Option<PathBuf>, client_options: ClientOptions) -> Self {
        let (config_actor, config) = config_actor::spawn(root, client_options).await;
        Self {
            client,
            config,
            config_actor,
            documents: Mutex::new(HashMap::new()),
        }
    }

    pub fn options(&self) -> Arc<ClientOptions> {
        Arc::clone(&self.config.borrow().options)
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
        let uri = params.text_document.uri.as_str().to_string();
        let handle = document_actor::spawn(
            self.client.clone(),
            self.config.clone(),
            &params.text_document,
        );
        // Dropping a replaced handle closes its actor (see
        // `DocumentActorHandle`'s docs), so this also cleans up properly
        // if the client reopens a document it never closed.
        if self
            .documents
            .lock()
            .await
            .insert(uri.clone(), handle)
            .is_some()
        {
            log::warn!("Document {uri} was already open; replacing with a new one");
        }
    }

    pub async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let documents = self.documents.lock().await;
        let Some(handle) = documents.get(uri.as_str()) else {
            log::error!("Received change for unopened document {}", uri.as_str());
            return;
        };
        handle.changed(params.text_document.version, params.content_changes);
    }

    pub async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        log::info!("Saved document {}", uri.as_str());
        if let Some(handle) = self.documents.lock().await.get(uri.as_str()) {
            handle.saved();
        }
    }

    pub async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Dropping the handle closes the actor; see `DocumentActorHandle`.
        self.documents
            .lock()
            .await
            .remove(params.text_document.uri.as_str());
    }

    pub async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> RpcResult<Option<CodeActionResponse>> {
        let mut actions = Vec::new();
        let uri = params.text_document.uri;
        let project_config_display_path = self
            .config
            .borrow()
            .client_options
            .project_config_display_path();

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

        Ok((!actions.is_empty()).then_some(actions))
    }

    pub async fn with_new_config_from_settings(&self, settings: Value) {
        if settings == Value::Null {
            log::debug!("Ignoring null configuration change notification");
            return;
        }

        match ClientOptions::parse_value(settings) {
            Ok(client_options) => {
                log::info!("LanguageTool configuration changed; reloading options");
                self.config_actor.apply_client_options(client_options);
            }
            Err(err) => {
                let message = format!(
                    "Ignoring invalid LanguageTool configuration change; keeping previous options: {err}"
                );
                log::error!("{message}");
                self.client.log_message(MessageType::ERROR, message).await;
            }
        }
    }

    pub async fn handle_execute_command(&self, params: ExecuteCommandParams) -> RpcResult<()> {
        log::info!("Executing command {}", params.command);
        let first_arg = params.arguments.first().and_then(Value::as_str);
        let result = match (params.command.as_str(), first_arg) {
            (COMMAND_IGNORE_WORD, Some(word)) => {
                let word = word.to_string();
                self.config_actor
                    .update_project_config(move |config| config.add_ignored_word(&word))
                    .await
            }
            (COMMAND_DISABLE_RULE, Some(rule_id)) => {
                let rule_id = rule_id.to_string();
                self.config_actor
                    .update_project_config(move |config| config.add_disabled_rule(&rule_id))
                    .await
            }
            (COMMAND_DISABLE_CATEGORY, Some(category_id)) => {
                let category_id = category_id.to_string();
                self.config_actor
                    .update_project_config(move |config| config.add_disabled_category(&category_id))
                    .await
            }
            _ => {
                log::warn!("Unknown or invalid command: {}", params.command);
                Ok(())
            }
        };
        result.map_err(RpcError::invalid_params)
    }
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

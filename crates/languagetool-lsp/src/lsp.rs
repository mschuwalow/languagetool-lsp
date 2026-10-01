use crate::backend::LanguageServerBackend;
use crate::config::ClientOptions;
use serde_json::Value;
use std::borrow::Cow;
use std::path::PathBuf;
use tokio::sync::OnceCell;
use tower_lsp_server::Client;
use tower_lsp_server::jsonrpc::{Error as RpcError, ErrorCode, Result as RpcResult};
use tower_lsp_server::ls_types::*;

const COMMAND_IGNORE_WORD: &str = "languagetool.ignoreWordInWorkspace";
const COMMAND_DISABLE_RULE: &str = "languagetool.disableRuleInWorkspace";
const COMMAND_DISABLE_CATEGORY: &str = "languagetool.disableCategoryInWorkspace";

/// Error returned for requests received while the server has not (yet)
/// completed `initialize`.
///
/// The `tower-lsp-server` transport already rejects such requests before
/// they reach [`LanguageServer`] methods, so this only matters as a
/// defensive fallback per the LSP spec (`-32002 ServerNotInitialized`).
fn not_initialized_error() -> RpcError {
    RpcError {
        code: ErrorCode::ServerError(-32002),
        message: Cow::Borrowed("Server not initialized"),
        data: None,
    }
}

/// Error returned for an `initialize` request received after the server has
/// already been (or is concurrently being) initialized. The spec states
/// `initialize` may only be sent once but doesn't mandate a specific error
/// code for a violation; `InvalidRequest` matches what `tower-lsp-server`'s
/// own duplicate-`initialize` guard uses.
fn duplicate_initialize_error() -> RpcError {
    log::warn!("Received duplicate `initialize` request; rejecting");
    RpcError::invalid_request()
}

pub struct LanguageServer {
    client: Client,
    backend: OnceCell<LanguageServerBackend>,
}

impl LanguageServer {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            backend: OnceCell::new(),
        }
    }

    fn require_initialized(&self) -> RpcResult<&LanguageServerBackend> {
        self.backend.get().ok_or_else(not_initialized_error)
    }

    /// Same as [`Self::require_initialized`], but for notification
    /// handlers, which have no error channel back to the client and so
    /// just log and drop the notification instead.
    fn backend_for_notification(&self, method: &str) -> Option<&LanguageServerBackend> {
        let backend = self.backend.get();
        if backend.is_none() {
            log::warn!("Dropping `{method}` notification received before initialize");
        }
        backend
    }
}

impl tower_lsp_server::LanguageServer for LanguageServer {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        let root = workspace_root(&params);
        if root.is_none() {
            log::warn!(
                "Client opened no workspace folder; project config will only be used if \
                 `languagetool.projectConfigPath` is set to an absolute path"
            );
        }
        let client_options = ClientOptions::from_value(params.initialization_options);
        let root_display = root
            .as_deref()
            .map(|root| root.display().to_string())
            .unwrap_or_else(|| "<no workspace folder>".to_string());
        let backend = LanguageServerBackend::new(self.client.clone(), root, client_options).await;
        log::info!(
            "LanguageTool LSP initialized for {root_display} using {}",
            backend.options().endpoint()
        );

        if self.backend.set(backend).is_err() {
            return Err(duplicate_initialize_error());
        }

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
        let Some(backend) = self.backend_for_notification("initialized") else {
            return;
        };
        backend.log_ready().await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        log::info!("LanguageTool LSP shutdown requested");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let Some(backend) = self.backend_for_notification("textDocument/didOpen") else {
            return;
        };
        backend.did_open(params).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(backend) = self.backend_for_notification("textDocument/didChange") else {
            return;
        };
        backend.did_change(params).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let Some(backend) = self.backend_for_notification("textDocument/didSave") else {
            return;
        };
        backend.did_save(params).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let Some(backend) = self.backend_for_notification("textDocument/didClose") else {
            return;
        };
        backend.did_close(params).await;
    }

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        self.require_initialized()?.code_action(params).await
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let Some(backend) = self.backend_for_notification("workspace/didChangeConfiguration")
        else {
            return;
        };
        backend.with_new_config_from_settings(params.settings).await;
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> RpcResult<Option<Value>> {
        let backend = self.require_initialized()?;
        backend.handle_execute_command(params).await?;
        Ok(None)
    }
}

// `root_uri` and `root_path` are deprecated in LSP in favour of
// `workspaceFolders`, but we fall back to them for older clients.
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
        .or_else(|| params.root_path.as_deref().map(PathBuf::from))
}

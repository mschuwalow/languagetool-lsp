use crate::backend::{LanguageServerBackend, run_project_config_reload_loop};
use crate::config::ClientOptions;
use crate::project_config_watcher::{ProjectConfigWatcher, ProjectConfigWatcherHandle};
use arc_swap::ArcSwapOption;
use serde_json::Value;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::unbounded_channel;
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
    backend: Arc<ArcSwapOption<LanguageServerBackend>>,
    /// Handle for telling the project config file watcher what to watch;
    /// cloned into every [`LanguageServerBackend`] that gets constructed
    /// so it can (re-)point the watcher itself (see that struct's docs).
    project_config_watcher_handle: ProjectConfigWatcherHandle,
    /// Kept alive for as long as `LanguageServer` is; dropping either of
    /// these would stop the corresponding background task (the file
    /// watcher itself, and the loop that reacts to its messages).
    _project_config_watcher: ProjectConfigWatcher,
    _project_config_reload_task: tokio::task::JoinHandle<()>,
}

impl LanguageServer {
    pub fn new(client: Client) -> Self {
        let backend: Arc<ArcSwapOption<LanguageServerBackend>> =
            Arc::new(ArcSwapOption::from(None));
        let (changed_tx, changed_rx) = unbounded_channel();
        let (project_config_watcher, project_config_watcher_handle) =
            ProjectConfigWatcher::spawn(changed_tx);
        let reload_task = tokio::spawn(run_project_config_reload_loop(
            Arc::clone(&backend),
            changed_rx,
        ));
        Self {
            client,
            backend,
            project_config_watcher_handle,
            _project_config_watcher: project_config_watcher,
            _project_config_reload_task: reload_task,
        }
    }

    fn current_backend(&self) -> Option<LanguageServerBackend> {
        self.backend.load_full().map(|backend| (*backend).clone())
    }

    fn require_initialized(&self) -> RpcResult<LanguageServerBackend> {
        self.current_backend().ok_or_else(not_initialized_error)
    }
}

impl tower_lsp_server::LanguageServer for LanguageServer {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        // The `initialize` request may only be sent once per the spec. Fail
        // fast if we're obviously already past that point.
        if self.current_backend().is_some() {
            return Err(duplicate_initialize_error());
        }

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
        let initialized = LanguageServerBackend::new(
            self.client.clone(),
            root,
            client_options,
            self.project_config_watcher_handle.clone(),
        )
        .await;
        log::info!(
            "LanguageTool LSP initialized for {root_display} using {}",
            initialized.options().endpoint()
        );

        // `tower-lsp-server`'s own duplicate-`initialize` guard is a
        // check-then-dispatch against an atomic flag that isn't set until
        // our future resolves, so two `initialize` requests racing each
        // other could both reach this point. `compare_and_swap` only
        // installs `initialized` if the slot is *still* empty by the time
        // the swap actually happens; if the returned previous value is
        // `Some` instead, another request's `initialize` call won the
        // race.
        let previous = self.backend.compare_and_swap(
            &None::<Arc<LanguageServerBackend>>,
            Some(Arc::new(initialized)),
        );
        if previous.is_some() {
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
        let Some(backend) = self.current_backend() else {
            log::warn!("Received `initialized` notification before `initialize` completed");
            return;
        };
        backend.log_ready().await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        log::info!("LanguageTool LSP shutdown requested");
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let Some(backend) = self.current_backend() else {
            log::warn!("Dropping `textDocument/didOpen` notification received before initialize");
            return;
        };
        backend.did_open(params).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(backend) = self.current_backend() else {
            log::warn!("Dropping `textDocument/didChange` notification received before initialize");
            return;
        };
        backend.did_change(params).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let Some(backend) = self.current_backend() else {
            log::warn!("Dropping `textDocument/didSave` notification received before initialize");
            return;
        };
        backend.did_save(params).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let Some(backend) = self.current_backend() else {
            log::warn!("Dropping `textDocument/didClose` notification received before initialize");
            return;
        };
        backend.did_close(params).await;
    }

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        self.require_initialized()?.code_action(params).await
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let Some(backend) = self.current_backend() else {
            log::warn!(
                "Dropping `workspace/didChangeConfiguration` notification received before initialize"
            );
            return;
        };
        let Some(new_backend) = backend.with_new_config_from_settings(params.settings).await else {
            return;
        };
        // `with_new_config_from_settings` already told the project config
        // file watcher where to look next (see `LanguageServerBackend`'s
        // docs), so installing the new backend and rechecking is all
        // that's left to do here.
        self.backend.store(Some(Arc::new(new_backend.clone())));
        new_backend.recheck_all().await;
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> RpcResult<Option<Value>> {
        let backend = self.require_initialized()?;
        backend.handle_execute_command(params).await?;
        // `handle_execute_command` only writes to disk; the project config
        // file watcher notices the write and installs+rechecks it (see
        // `ProjectConfigWatcher`'s docs), so there's nothing further to do
        // here.
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

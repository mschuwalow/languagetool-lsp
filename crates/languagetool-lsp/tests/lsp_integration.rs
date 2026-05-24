use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{duplex, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tower_lsp::lsp_types::Url;
use tower_lsp::{LspService, Server};

use languagetool_lsp::lsp::Backend;

fn encode_message(message: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", message.len(), message)
}

struct TestContext {
    request_tx: DuplexStream,
    response_rx: BufReader<DuplexStream>,
    _server: tokio::task::JoinHandle<()>,
    next_id: i64,
    workspace: TempDir,
}

impl TestContext {
    fn new() -> Self {
        let workspace = TempDir::new().expect("test workspace should be created");
        let root = workspace.path().to_path_buf();
        let (request_tx, server_rx) = duplex(1024 * 1024);
        let (server_tx, response_rx) = duplex(1024 * 1024);
        let response_rx = BufReader::new(response_rx);
        let (service, socket) = LspService::new(|client| Backend::new(client, root.clone()));
        let server = tokio::spawn(Server::new(server_rx, server_tx, socket).serve(service));

        Self {
            request_tx,
            response_rx,
            _server: server,
            next_id: 1,
            workspace,
        }
    }

    fn root_uri(&self) -> Url {
        Url::from_file_path(self.workspace.path()).expect("workspace path should be a file URL")
    }

    fn doc_uri(&self, path: &str) -> Url {
        Url::from_file_path(self.workspace.path().join(path))
            .expect("document path should be a file URL")
    }

    fn project_config_path(&self) -> PathBuf {
        self.workspace
            .path()
            .join(Path::new(".zed/languagetool.json"))
    }

    async fn initialize(&mut self) -> Value {
        let root_uri = self.root_uri();
        let response = self
            .request(
                "initialize",
                json!({
                    "capabilities": {
                        "textDocument": {
                            "codeAction": {
                                "codeActionLiteralSupport": {
                                    "codeActionKind": { "valueSet": ["quickfix"] }
                                },
                                "dataSupport": true
                            },
                            "publishDiagnostics": { "versionSupport": true }
                        },
                        "workspace": { "executeCommand": { "dynamicRegistration": false } }
                    },
                    "processId": null,
                    "rootUri": root_uri,
                    "workspaceFolders": [{ "name": "test", "uri": root_uri }],
                    "initializationOptions": {
                        "backend": { "type": "local", "url": "http://localhost:8081" },
                        "language": "en-US",
                        "checkOnOpen": true,
                        "checkOnSave": true,
                        "checkWhileTyping": false,
                        "debounceMs": 10,
                        "maxReplacements": 8
                    }
                }),
            )
            .await;
        self.notify("initialized", json!({})).await;
        response
    }

    async fn open_document(&mut self, uri: &Url, language_id: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text
                }
            }),
        )
        .await;
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .await;
        self.wait_response(id).await
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
        .await;
    }

    async fn send(&mut self, message: Value) {
        let message = serde_json::to_string(&message).expect("JSON-RPC message should serialize");
        self.request_tx
            .write_all(encode_message(&message).as_bytes())
            .await
            .expect("JSON-RPC message should be written");
    }

    async fn wait_response(&mut self, id: i64) -> Value {
        self.wait_for(|message| {
            (message.get("id") == Some(&json!(id))).then(|| {
                assert!(
                    message.get("error").is_none(),
                    "unexpected JSON-RPC error: {message}"
                );
                message.get("result").cloned().unwrap_or(Value::Null)
            })
        })
        .await
    }

    async fn wait_notification(&mut self, method: &str) -> Value {
        self.wait_for(|message| {
            (message.get("method").and_then(Value::as_str) == Some(method))
                .then(|| message.get("params").cloned().unwrap_or(Value::Null))
        })
        .await
    }

    async fn wait_for<T>(&mut self, mut accept: impl FnMut(Value) -> Option<T>) -> T {
        let deadline = Duration::from_secs(15);
        loop {
            let message = tokio::time::timeout(deadline, self.read_message())
                .await
                .expect("timed out waiting for an LSP message");
            if message.get("method").and_then(Value::as_str) == Some("window/logMessage") {
                continue;
            }
            if let Some(value) = accept(message) {
                return value;
            }
        }
    }

    async fn read_message(&mut self) -> Value {
        let mut content_length = None;
        loop {
            let mut header = String::new();
            self.response_rx
                .read_line(&mut header)
                .await
                .expect("LSP header should be readable");
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some(length) = header.strip_prefix("Content-Length: ") {
                content_length = Some(
                    length
                        .parse::<usize>()
                        .expect("Content-Length should be a usize"),
                );
            }
        }

        let content_length = content_length.expect("LSP message should include Content-Length");
        let mut content = vec![0; content_length];
        self.response_rx
            .read_exact(&mut content)
            .await
            .expect("LSP body should be readable");
        serde_json::from_slice(&content).expect("LSP body should be JSON")
    }
}

#[tokio::test]
async fn initialize_reports_expected_capabilities() {
    let mut ctx = TestContext::new();

    let result = ctx.initialize().await;
    let capabilities = &result["capabilities"];

    assert_eq!(
        capabilities["positionEncoding"],
        json!("utf-16"),
        "the server should report UTF-16 positions"
    );
    assert_eq!(
        capabilities["codeActionProvider"]["codeActionKinds"],
        json!(["quickfix"])
    );
    assert_eq!(
        capabilities["executeCommandProvider"]["commands"],
        json!([
            "languagetool.ignoreWordInWorkspace",
            "languagetool.disableRuleInWorkspace",
            "languagetool.disableCategoryInWorkspace"
        ])
    );
}

#[tokio::test]
async fn did_open_publishes_languagetool_diagnostics() {
    let mut ctx = TestContext::new();
    ctx.initialize().await;
    let uri = ctx.doc_uri("document.txt");

    ctx.open_document(&uri, "plaintext", "This are a tset.")
        .await;
    let params = ctx
        .wait_notification("textDocument/publishDiagnostics")
        .await;
    let diagnostics = params["diagnostics"]
        .as_array()
        .expect("diagnostics should be an array");

    assert!(!diagnostics.is_empty(), "expected LanguageTool diagnostics");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["source"] == "LanguageTool"
            && diagnostic["data"]["replacements"]
                .as_array()
                .is_some_and(|replacements| replacements.iter().any(|value| value == "test"))
    }));
}

#[tokio::test]
async fn code_action_returns_replacement_and_workspace_commands() {
    let mut ctx = TestContext::new();
    ctx.initialize().await;
    let uri = ctx.doc_uri("document.txt");

    ctx.open_document(&uri, "plaintext", "This are a tset.")
        .await;
    let params = ctx
        .wait_notification("textDocument/publishDiagnostics")
        .await;
    let diagnostic = params["diagnostics"]
        .as_array()
        .expect("diagnostics should be an array")
        .iter()
        .find(|diagnostic| {
            diagnostic["data"]["replacements"]
                .as_array()
                .is_some_and(|replacements| replacements.iter().any(|value| value == "test"))
        })
        .expect("expected diagnostic with 'test' replacement")
        .clone();

    let actions = ctx
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": uri },
                "range": diagnostic["range"],
                "context": { "diagnostics": [diagnostic] }
            }),
        )
        .await;
    let actions = actions
        .as_array()
        .expect("code action result should be an array");

    assert!(actions.iter().any(|action| {
        action["title"] == "Replace with 'test'"
            && action["edit"]["changes"][uri.as_str()][0]["newText"] == "test"
    }));
    assert!(actions.iter().any(|action| {
        action["command"] == "languagetool.ignoreWordInWorkspace"
            && action["arguments"] == json!(["tset"])
    }));
}

#[tokio::test]
async fn execute_ignore_word_command_writes_project_config() {
    let mut ctx = TestContext::new();
    ctx.initialize().await;

    let result = ctx
        .request(
            "workspace/executeCommand",
            json!({
                "command": "languagetool.ignoreWordInWorkspace",
                "arguments": ["tset"]
            }),
        )
        .await;

    assert_eq!(result, Value::Null);
    let config = std::fs::read_to_string(ctx.project_config_path())
        .expect("ignore command should write project config");
    let config: Value = serde_json::from_str(&config).expect("project config should be JSON");
    assert_eq!(config["ignored_words"], json!(["tset"]));
}

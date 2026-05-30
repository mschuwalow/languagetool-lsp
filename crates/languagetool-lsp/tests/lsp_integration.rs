use httpmock::prelude::*;
use languagetool_lsp::lsp::Backend;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{duplex, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tower_lsp_server::ls_types::Uri;
use tower_lsp_server::{LspService, Server};

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
        let (request_tx, server_rx) = duplex(1024 * 1024);
        let (server_tx, response_rx) = duplex(1024 * 1024);
        let response_rx = BufReader::new(response_rx);
        let (service, socket) = LspService::new(Backend::new);
        let server = tokio::spawn(Server::new(server_rx, server_tx, socket).serve(service));

        Self {
            request_tx,
            response_rx,
            _server: server,
            next_id: 1,
            workspace,
        }
    }

    fn root_uri(&self) -> Uri {
        Uri::from_file_path(self.workspace.path()).expect("workspace path should be a file URL")
    }

    fn doc_uri(&self, path: &str) -> Uri {
        Uri::from_file_path(self.workspace.path().join(path))
            .expect("document path should be a file URL")
    }

    fn project_config_path(&self) -> PathBuf {
        self.workspace
            .path()
            .join(Path::new(".zed/languagetool.json"))
    }

    async fn initialize(&mut self) -> Value {
        self.initialize_with_options(json!({})).await
    }

    async fn initialize_with_options(&mut self, extra_options: Value) -> Value {
        let root_uri = self.root_uri();
        let mut initialization_options = json!({
            "backend": "custom",
            "customBackendUrl": "http://localhost:8081",
            "language": "en-US",
            "checkOnOpen": true,
            "checkOnSave": true,
            "checkWhileTyping": false,
            "debounceMs": 10,
            "maxReplacements": 8
        });
        merge_json(&mut initialization_options, extra_options);

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
                    "initializationOptions": initialization_options
                }),
            )
            .await;
        response
    }

    async fn open_document(&mut self, uri: &Uri, language_id: &str, text: &str) {
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

    async fn request_error(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .await;
        self.wait_for(|message| {
            (message.get("id") == Some(&json!(id))).then(|| {
                message
                    .get("error")
                    .cloned()
                    .expect("expected JSON-RPC error")
            })
        })
        .await
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

impl TestContext {
    async fn change_document(&mut self, uri: &Uri, version: i64, changes: serde_json::Value) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": changes
            }),
        )
        .await;
    }

    async fn save_document(&mut self, uri: &Uri) {
        self.notify(
            "textDocument/didSave",
            json!({
                "textDocument": { "uri": uri }
            }),
        )
        .await;
    }
}

/// Sends a sequence of incremental didChange edits and then a didSave trigger.
/// This test does not require a LanguageTool server; it only validates that the
/// incremental-edit machinery in DocumentCache doesn't corrupt state.
#[tokio::test]
async fn incremental_did_change_keeps_document_consistent() {
    let mut ctx = TestContext::new();
    ctx.initialize_with_options(json!({
        "checkOnOpen": false,
        "checkOnSave": false,
        "checkWhileTyping": false,
    }))
    .await;
    let uri = ctx.doc_uri("document.txt");

    // Open with initial content.
    ctx.open_document(&uri, "plaintext", "hello world\nfoo bar")
        .await;

    // Replace "world" → "zed" on line 0.
    ctx.change_document(
        &uri,
        2,
        json!([{
            "range": {
                "start": { "line": 0, "character": 6 },
                "end":   { "line": 0, "character": 11 }
            },
            "text": "zed"
        }]),
    )
    .await;

    // Insert a newline inside "foo bar" on line 1: "foo\n bar".
    ctx.change_document(
        &uri,
        3,
        json!([{
            "range": {
                "start": { "line": 1, "character": 3 },
                "end":   { "line": 1, "character": 3 }
            },
            "text": "\n"
        }]),
    )
    .await;

    // Delete the inserted newline, merging back to "foo bar".
    ctx.change_document(
        &uri,
        4,
        json!([{
            "range": {
                "start": { "line": 1, "character": 3 },
                "end":   { "line": 2, "character": 0 }
            },
            "text": ""
        }]),
    )
    .await;

    // Apply an edit that is only valid if the index correctly reflects the
    // post-edit layout: replace " bar" on line 1 (characters 3-7) with " baz".
    // With a corrupt index this would either panic or silently truncate.
    ctx.change_document(
        &uri,
        5,
        json!([{
            "range": {
                "start": { "line": 1, "character": 3 },
                "end":   { "line": 1, "character": 7 }
            },
            "text": " baz"
        }]),
    )
    .await;

    // Save should only trigger checking; document state comes from didOpen/didChange.
    ctx.save_document(&uri).await;

    // Perform one more incremental edit on top of the saved text to confirm the
    // index is still usable.  If the index is corrupt, replace_range will panic
    // inside the server task, causing the next request to time out.
    ctx.change_document(
        &uri,
        6,
        json!([{
            "range": {
                "start": { "line": 0, "character": 6 },
                "end":   { "line": 0, "character": 9 }
            },
            "text": "world"
        }]),
    )
    .await;

    // If we reach here without a timeout the server is still alive and has not
    // panicked.  Issue a benign request to confirm the JSON-RPC channel works.
    let result = ctx
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end":   { "line": 0, "character": 0 }
                },
                "context": { "diagnostics": [] }
            }),
        )
        .await;
    // No diagnostics → no actions, but the server must not have crashed.
    assert!(result.is_null() || result.as_array().is_some());
}

/// Verifies that incremental edits involving non-ASCII (emoji) characters resolve
/// to the correct byte positions via the UTF-16 index.
#[tokio::test]
async fn incremental_did_change_handles_non_ascii() {
    let mut ctx = TestContext::new();
    ctx.initialize_with_options(json!({
        "checkOnOpen": false,
        "checkOnSave": false,
        "checkWhileTyping": false,
    }))
    .await;
    let uri = ctx.doc_uri("emoji.txt");

    ctx.open_document(&uri, "plaintext", "hi 😀 there").await;

    // Replace the emoji (UTF-16 chars 3-5) with ASCII.
    ctx.change_document(
        &uri,
        2,
        json!([{
            "range": {
                "start": { "line": 0, "character": 3 },
                "end":   { "line": 0, "character": 5 }
            },
            "text": "x"
        }]),
    )
    .await;

    // Now insert another emoji at position 4 (after 'x').
    ctx.change_document(
        &uri,
        3,
        json!([{
            "range": {
                "start": { "line": 0, "character": 4 },
                "end":   { "line": 0, "character": 4 }
            },
            "text": "😂"
        }]),
    )
    .await;

    // Replace the second emoji with ASCII again; correct index required.
    ctx.change_document(
        &uri,
        4,
        json!([{
            "range": {
                "start": { "line": 0, "character": 4 },
                "end":   { "line": 0, "character": 6 }
            },
            "text": "y"
        }]),
    )
    .await;

    // Confirm the server is still alive.
    let result = ctx
        .request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end":   { "line": 0, "character": 0 }
                },
                "context": { "diagnostics": [] }
            }),
        )
        .await;
    assert!(result.is_null() || result.as_array().is_some());
}

fn merge_json(base: &mut Value, patch: Value) {
    let (Some(base), Some(patch)) = (base.as_object_mut(), patch.as_object()) else {
        return;
    };
    for (key, value) in patch {
        base.insert(key.clone(), value.clone());
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
        capabilities["textDocumentSync"]["save"]["includeText"],
        json!(false),
        "save should be a check trigger, not a full-text sync path"
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
            && action["edit"]["documentChanges"][0]["textDocument"]["uri"] == uri.as_str()
            && action["edit"]["documentChanges"][0]["textDocument"]["version"] == 1
            && action["edit"]["documentChanges"][0]["edits"][0]["newText"] == "test"
    }));
    assert!(actions.iter().any(|action| {
        action["command"] == "languagetool.ignoreWordInWorkspace"
            && action["arguments"] == json!(["tset"])
    }));
}

#[tokio::test]
async fn malformed_incremental_change_clears_diagnostics() {
    let mut ctx = TestContext::new();
    ctx.initialize_with_options(json!({
        "checkOnOpen": false,
        "checkOnSave": false,
        "checkWhileTyping": true,
    }))
    .await;
    let uri = ctx.doc_uri("emoji.txt");

    ctx.open_document(&uri, "plaintext", "a😀b").await;
    ctx.change_document(
        &uri,
        2,
        json!([{
            "range": {
                "start": { "line": 0, "character": 2 },
                "end":   { "line": 0, "character": 2 }
            },
            "text": "x"
        }]),
    )
    .await;

    let params = ctx
        .wait_notification("textDocument/publishDiagnostics")
        .await;
    assert_eq!(params["uri"], uri.as_str());
    assert_eq!(params["version"], json!(2));
    assert_eq!(params["diagnostics"], json!([]));
}

#[tokio::test]
async fn initialize_workspace_root_controls_project_config_location() {
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
    assert!(ctx.project_config_path().exists());
}

#[tokio::test]
async fn execute_command_reports_project_config_save_failure() {
    let mut ctx = TestContext::new();
    ctx.initialize().await;
    tokio::fs::create_dir_all(ctx.project_config_path())
        .await
        .expect("directory at config path should be created");

    let error = ctx
        .request_error(
            "workspace/executeCommand",
            json!({
                "command": "languagetool.ignoreWordInWorkspace",
                "arguments": ["tset"]
            }),
        )
        .await;

    assert_eq!(error["code"], -32602);
}

#[tokio::test]
async fn check_failure_clears_stale_diagnostics() {
    let server = MockServer::start();
    let _check = server.mock(|when, then| {
        when.method(POST).path("/v2/check");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{
                    "matches": [{
                        "message": "Possible spelling mistake found.",
                        "offset": 11,
                        "length": 4,
                        "replacements": [{"value": "test"}],
                        "context": {"text": "This are a tset.", "offset": 11, "length": 4},
                        "sentence": "This are a tset.",
                        "rule": {
                            "id": "MORFOLOGIK_RULE_EN_US",
                            "description": "Possible spelling mistake",
                            "issueType": "misspelling",
                            "category": {"id": "TYPOS", "name": "Possible Typo"}
                        }
                    }]
                }"#,
            );
    });
    let mut ctx = TestContext::new();
    ctx.initialize_with_options(json!({
        "backend": "custom",
        "customBackendUrl": server.base_url()
    }))
    .await;
    let uri = ctx.doc_uri("document.txt");

    ctx.open_document(&uri, "plaintext", "This are a tset.")
        .await;
    let params = ctx
        .wait_notification("textDocument/publishDiagnostics")
        .await;
    assert!(!params["diagnostics"].as_array().unwrap().is_empty());

    ctx.notify(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "backend": "custom",
                "customBackendUrl": format!("{}/missing", server.base_url()),
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
    let params = ctx
        .wait_notification("textDocument/publishDiagnostics")
        .await;
    assert!(params["diagnostics"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn cached_block_diagnostics_are_republished_without_rechecking() {
    let server = MockServer::start();
    let check = server.mock(|when, then| {
        when.method(POST).path("/v2/check");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{
                    "matches": [{
                        "message": "Possible issue found.",
                        "offset": 3,
                        "length": 5,
                        "replacements": [],
                        "context": {"text": "dummy", "offset": 3, "length": 5},
                        "sentence": "dummy",
                        "rule": {
                            "id": "TEST_RULE",
                            "description": "Test rule",
                            "issueType": "grammar",
                            "category": {"id": "GRAMMAR", "name": "Grammar"}
                        }
                    }]
                }"#,
            );
    });
    let mut ctx = TestContext::new();
    ctx.initialize_with_options(json!({
        "backend": "custom",
        "customBackendUrl": server.base_url(),
        "checkWhileTyping": false,
        "checkOnSave": true
    }))
    .await;
    let uri = ctx.doc_uri("document.rs");

    ctx.open_document(&uri, "rust", "// First are bad.\n\n// Second are bad.\n")
        .await;
    let params = ctx
        .wait_notification("textDocument/publishDiagnostics")
        .await;
    let diagnostics = params["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(check.calls(), 2);

    ctx.change_document(
        &uri,
        2,
        json!([{
            "range": {
                "start": { "line": 2, "character": 3 },
                "end":   { "line": 2, "character": 9 }
            },
            "text": "Third!"
        }]),
    )
    .await;
    ctx.save_document(&uri).await;

    let params = ctx
        .wait_notification("textDocument/publishDiagnostics")
        .await;
    let diagnostics = params["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(diagnostics[0]["range"]["start"]["line"], json!(0));
    assert_eq!(diagnostics[1]["range"]["start"]["line"], json!(2));
    assert_eq!(check.calls(), 3);
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
    let config = tokio::fs::read_to_string(ctx.project_config_path())
        .await
        .expect("ignore command should write project config");
    let config: Value = serde_json::from_str(&config).expect("project config should be JSON");
    assert_eq!(config["ignored_words"], json!(["tset"]));
}

#[tokio::test]
async fn execute_command_uses_configured_project_config_path() {
    let mut ctx = TestContext::new();
    ctx.initialize_with_options(json!({
        "projectConfigPath": ".idea/languagetool.json"
    }))
    .await;

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
    assert!(!ctx.project_config_path().exists());
    let config_path = ctx.workspace.path().join(".idea/languagetool.json");
    let config = tokio::fs::read_to_string(config_path)
        .await
        .expect("ignore command should write configured project config");
    let config: Value = serde_json::from_str(&config).expect("project config should be JSON");
    assert_eq!(config["ignored_words"], json!(["tset"]));
}

#[tokio::test]
async fn partial_configuration_change_preserves_existing_options() {
    let mut ctx = TestContext::new();
    ctx.initialize_with_options(json!({
        "projectConfigPath": ".idea/languagetool.json"
    }))
    .await;

    ctx.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "debounceMs": 100 } }),
    )
    .await;
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
    assert!(!ctx.project_config_path().exists());
    assert!(ctx
        .workspace
        .path()
        .join(".idea/languagetool.json")
        .exists());
}

#[tokio::test]
async fn invalid_configuration_change_keeps_previous_options() {
    let mut ctx = TestContext::new();
    ctx.initialize_with_options(json!({
        "projectConfigPath": ".idea/languagetool.json"
    }))
    .await;

    ctx.notify(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "checkOnSave": "not a bool" } }),
    )
    .await;
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
    assert!(!ctx.project_config_path().exists());
    assert!(ctx
        .workspace
        .path()
        .join(".idea/languagetool.json")
        .exists());
}

#[tokio::test]
async fn existing_project_config_is_loaded_on_initialize() {
    let mut ctx = TestContext::new();
    let config_path = ctx.workspace.path().join(".idea/languagetool.json");
    tokio::fs::create_dir_all(config_path.parent().unwrap())
        .await
        .expect("project config directory should be created");
    tokio::fs::write(
        &config_path,
        serde_json::to_string_pretty(&json!({ "ignored_words": ["tset"] })).unwrap(),
    )
    .await
    .expect("project config should be written");

    ctx.initialize_with_options(json!({
        "projectConfigPath": ".idea/languagetool.json"
    }))
    .await;
    let uri = ctx.doc_uri("document.txt");

    ctx.open_document(&uri, "plaintext", "This are a tset.")
        .await;
    let params = ctx
        .wait_notification("textDocument/publishDiagnostics")
        .await;
    let diagnostics = params["diagnostics"]
        .as_array()
        .expect("diagnostics should be an array");

    assert!(diagnostics.iter().all(|diagnostic| {
        diagnostic["data"]["matchedText"] != "tset"
            && diagnostic["data"]["replacements"]
                .as_array()
                .is_none_or(|replacements| replacements.iter().all(|value| value != "test"))
    }));
}

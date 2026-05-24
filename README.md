# LanguageTool LSP

Rust language server for LanguageTool-backed diagnostics and quick fixes.

## Workspace

- `crates/languagetool-client`: checked-in Rust client generated from the official LanguageTool Swagger document.
- `crates/languagetool-lsp`: LSP server using `tower-lsp` and the generated client crate.

## Generate Client

The official Swagger document is checked in at `openapi/languagetool-swagger.json`.

Regenerate the client with cargo-make:

```sh
cargo make generate-client
```

The Makefile writes a temporary normalized Swagger file under `target/openapi/` that adds `application/x-www-form-urlencoded` as the consumed form encoding. This makes OpenAPI Generator emit `reqwest::RequestBuilder::form(...)`, which works with LanguageTool.

## Verify

```sh
cargo make verify
```

Or run the checks directly:

```sh
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Health Check

With a local LanguageTool server running on `localhost:8081`:

```sh
cargo run -p languagetool-lsp -- --root . health
```

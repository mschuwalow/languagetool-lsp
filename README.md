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

## Verify

```sh
cargo make check
```
